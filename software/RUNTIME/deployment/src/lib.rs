#![doc = "Native selection and health transition of installed Runtime bundles."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use hyper_fs_authority::{
    identity, open_syncable_directory_ambient, open_syncable_directory_at, read_stable_file,
    rename_replace, sync_directory,
};
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_project_model::{is_valid_entity_id, is_valid_extension_key};
use hyper_runtime_bundle::{RuntimeBundleError, VerifiedRuntimeBundle, prepare_runtime_bundle};
use same_file::Handle;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

const STATE_NAME: &str = "state.json";
const BUNDLES_NAME: &str = "bundles";
const LOCK_NAME: &str = "deployment.lock";
const LOCK_OWNER_NAME: &str = "owner.json";
const MAXIMUM_METADATA_BYTES: u64 = 64 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeploymentStartupErrorKind {
    Io,
    Locked,
    StateInvalid,
    StateNonCanonical,
    CandidateRejected,
    Publication,
    AuthorityChanged,
}

#[derive(Debug)]
pub struct DeploymentStartupError {
    kind: DeploymentStartupErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl DeploymentStartupError {
    #[must_use]
    pub const fn kind(&self) -> DeploymentStartupErrorKind {
        self.kind
    }
}

impl fmt::Display for DeploymentStartupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DeploymentStartupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug)]
struct CandidateFailures {
    active: CandidateError,
    previous: CandidateError,
}

impl fmt::Display for CandidateFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "active candidate: {}; previous candidate: {}",
            error_chain(&self.active),
            error_chain(&self.previous)
        )
    }
}

impl Error for CandidateFailures {}

#[derive(Debug)]
enum CandidateError {
    Bundle(RuntimeBundleError),
    Deployment(DeploymentStartupError),
}

impl fmt::Display for CandidateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bundle(error) => error.fmt(formatter),
            Self::Deployment(error) => error.fmt(formatter),
        }
    }
}

impl Error for CandidateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bundle(error) => Some(error),
            Self::Deployment(error) => Some(error),
        }
    }
}

impl From<RuntimeBundleError> for CandidateError {
    fn from(error: RuntimeBundleError) -> Self {
        Self::Bundle(error)
    }
}

#[derive(Debug)]
pub struct ResolvedRuntimeDeployment {
    bundle: VerifiedRuntimeBundle,
    generation: u64,
    recovered_from_bundle_sha256: Option<[u8; 32]>,
}

impl ResolvedRuntimeDeployment {
    #[must_use]
    pub const fn bundle(&self) -> &VerifiedRuntimeBundle {
        &self.bundle
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn recovered_from_bundle_sha256(&self) -> Option<&[u8; 32]> {
        self.recovered_from_bundle_sha256.as_ref()
    }

    #[must_use]
    pub fn into_bundle(self) -> VerifiedRuntimeBundle {
        self.bundle
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum ActivationStatus {
    Pending,
    Healthy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawActivation {
    bundle_sha256: String,
    project_id: String,
    project_revision: u64,
    minimum_runtime_version: String,
    status: ActivationStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawDeploymentState {
    format: String,
    format_version: u64,
    generation: u64,
    active: RawActivation,
    previous_bundle_sha256: Option<String>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug)]
struct Activation {
    bundle_sha256: [u8; 32],
    project_id: Option<String>,
    project_revision: Option<u64>,
    minimum_runtime_version: Option<Version>,
    status: ActivationStatus,
}

#[derive(Clone, Debug)]
struct DeploymentState {
    raw: RawDeploymentState,
    active: Activation,
    previous_bundle_sha256: Option<[u8; 32]>,
    original_bytes: Vec<u8>,
}

struct TargetAuthority {
    published_root: PathBuf,
    root: Dir,
    root_identity: Handle,
}

impl TargetAuthority {
    fn open(root: &Path) -> Result<Self, DeploymentStartupError> {
        let metadata = fs::symlink_metadata(root)
            .map_err(|source| io_error("inspect deployment target", source))?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(reject(
                DeploymentStartupErrorKind::StateInvalid,
                "deployment target must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(root)
            .map_err(|source| io_error("retain deployment target identity", source))?;
        let directory = open_syncable_directory_ambient(root)
            .map_err(|source| io_error("open deployment target", source))?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| io_error("clone deployment target", source))?
                .into_std_file(),
        )
        .map_err(|source| io_error("inspect retained deployment target", source))?;
        Self::confirm_path(root, &root_identity, &opened)?;
        Ok(Self {
            published_root: root.to_owned(),
            root: directory,
            root_identity,
        })
    }

    fn confirm_unchanged(&self) -> Result<(), DeploymentStartupError> {
        let opened = Handle::from_file(
            self.root
                .try_clone()
                .map_err(|source| io_error("clone deployment target", source))?
                .into_std_file(),
        )
        .map_err(|source| io_error("inspect retained deployment target", source))?;
        Self::confirm_path(&self.published_root, &self.root_identity, &opened)
    }

    fn confirm_path(
        root: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), DeploymentStartupError> {
        let metadata = fs::symlink_metadata(root)
            .map_err(|source| io_error("reinspect deployment target", source))?;
        let published = Handle::from_path(root)
            .map_err(|source| io_error("reopen deployment target identity", source))?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(reject(
                DeploymentStartupErrorKind::AuthorityChanged,
                "deployment target identity changed",
            ));
        }
        Ok(())
    }
}

struct DeploymentLock<'a> {
    target: &'a TargetAuthority,
    directory: Dir,
    identity: (u64, u64),
    owner_bytes: Vec<u8>,
}

impl<'a> DeploymentLock<'a> {
    fn acquire(target: &'a TargetAuthority) -> Result<Self, DeploymentStartupError> {
        let builder = private_directory_builder();
        match target.root.create_dir_with(LOCK_NAME, &builder) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                return Err(reject(
                    DeploymentStartupErrorKind::Locked,
                    "another deployment transaction is active",
                ));
            }
            Err(source) => return Err(io_error("create deployment lock", source)),
        }
        let directory = match open_syncable_directory_at(&target.root, LOCK_NAME) {
            Ok(directory) => directory,
            Err(source) => {
                let primary = io_error("retain deployment lock", source);
                return match target
                    .root
                    .remove_dir(LOCK_NAME)
                    .and_then(|()| sync_directory(&target.root))
                {
                    Ok(()) => Err(primary),
                    Err(cleanup) => {
                        Err(cleanup_error(primary, "clean up deployment lock", &cleanup))
                    }
                };
            }
        };
        let initialized = Self::initialize(target, &directory);
        match initialized {
            Ok((lock_identity, owner_bytes)) => Ok(Self {
                target,
                directory,
                identity: lock_identity,
                owner_bytes,
            }),
            Err(primary) => match directory
                .remove_open_dir_all()
                .and_then(|()| sync_directory(&target.root))
            {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(cleanup_error(primary, "clean up deployment lock", &cleanup)),
            },
        }
    }

    fn initialize(
        target: &TargetAuthority,
        directory: &Dir,
    ) -> Result<((u64, u64), Vec<u8>), DeploymentStartupError> {
        let lock_metadata = directory
            .dir_metadata()
            .map_err(|source| io_error("inspect deployment lock", source))?;
        let owner = serde_json::json!({
            "format": "hyper-deployment-lock",
            "format_version": 1,
            "owner_pid": std::process::id(),
            "owner_token": Uuid::new_v4().to_string(),
            "extensions": {}
        });
        let owner_bytes = serde_json_canonicalizer::to_vec(&owner).map_err(|source| {
            boxed_error(
                DeploymentStartupErrorKind::Publication,
                "canonicalize deployment lock owner",
                source,
            )
        })?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut owner_file = directory
            .open_with(LOCK_OWNER_NAME, &options)
            .map_err(|source| io_error("create deployment lock owner", source))?;
        owner_file
            .write_all(&owner_bytes)
            .and_then(|()| owner_file.sync_all())
            .map_err(|source| io_error("publish deployment lock owner", source))?;
        sync_directory(directory)
            .and_then(|()| sync_directory(&target.root))
            .map_err(|source| io_error("synchronize deployment lock", source))?;
        target.confirm_unchanged()?;
        Ok((identity(&lock_metadata), owner_bytes))
    }

    fn release(self) -> Result<(), DeploymentStartupError> {
        let owner = read_stable_file(&self.directory, LOCK_OWNER_NAME, MAXIMUM_METADATA_BYTES)
            .map_err(|source| io_error("verify deployment lock owner", source))?;
        let published = self
            .target
            .root
            .symlink_metadata(LOCK_NAME)
            .map_err(|source| io_error("reinspect deployment lock", source))?;
        if owner != self.owner_bytes
            || published.file_type().is_symlink()
            || !published.is_dir()
            || identity(&published) != self.identity
        {
            return Err(reject(
                DeploymentStartupErrorKind::AuthorityChanged,
                "deployment lock ownership changed",
            ));
        }
        self.target.confirm_unchanged()?;
        self.directory
            .remove_open_dir_all()
            .and_then(|()| sync_directory(&self.target.root))
            .map_err(|source| io_error("release deployment lock", source))?;
        self.target.confirm_unchanged()
    }
}

/// Resolves the active installed Runtime bundle, performs native software
/// health preparation, promotes pending state, and recovers through the
/// previous installed identity when necessary.
///
/// # Errors
///
/// Returns a classified failure for malformed state, lock contention,
/// candidate rejection, durable state publication, or retained-authority loss.
pub fn resolve_runtime_deployment(
    target_root: impl AsRef<Path>,
    runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
) -> Result<ResolvedRuntimeDeployment, DeploymentStartupError> {
    Version::parse(runtime_version).map_err(|source| {
        boxed_error(
            DeploymentStartupErrorKind::StateInvalid,
            "executing Runtime version is invalid",
            source,
        )
    })?;
    let target = TargetAuthority::open(target_root.as_ref())?;
    let lock = DeploymentLock::acquire(&target)?;
    let outcome = resolve_locked(
        &target,
        runtime_version,
        maximum_bundle_bytes,
        playback_limits,
    );
    let release = lock.release();
    match (outcome, release) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(operation), Err(release)) => Err(DeploymentStartupError {
            kind: operation.kind,
            message: format!(
                "{}; deployment lock release also failed: {}",
                error_chain(&operation),
                error_chain(&release)
            ),
            source: Some(Box::new(operation)),
        }),
    }
}

fn resolve_locked(
    target: &TargetAuthority,
    runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
) -> Result<ResolvedRuntimeDeployment, DeploymentStartupError> {
    let state = read_state(target)?;
    let bundles = target
        .root
        .open_dir_nofollow(BUNDLES_NAME)
        .map_err(|source| io_error("open installed bundles directory", source))?;
    let bundles_metadata = bundles
        .dir_metadata()
        .map_err(|source| io_error("inspect installed bundles directory", source))?;
    if !bundles_metadata.is_dir() {
        return Err(reject(
            DeploymentStartupErrorKind::StateInvalid,
            "installed bundles path is not a directory",
        ));
    }
    let bundles_identity = identity(&bundles_metadata);
    let active = prepare_candidate(
        target,
        &bundles,
        &state.active,
        runtime_version,
        maximum_bundle_bytes,
        playback_limits,
    );
    match active {
        Ok(bundle) => {
            confirm_directory_entry(target, BUNDLES_NAME, bundles_identity)?;
            if state.active.status == ActivationStatus::Healthy {
                target.confirm_unchanged()?;
                return Ok(ResolvedRuntimeDeployment {
                    bundle,
                    generation: state.raw.generation,
                    recovered_from_bundle_sha256: None,
                });
            }
            let generation = next_generation(state.raw.generation)?;
            let mut promoted = state.raw.clone();
            promoted.generation = generation;
            promoted.active.status = ActivationStatus::Healthy;
            publish_state(target, &state.original_bytes, &promoted)?;
            Ok(ResolvedRuntimeDeployment {
                bundle,
                generation,
                recovered_from_bundle_sha256: None,
            })
        }
        Err(active_error) => {
            let Some(previous_identity) = state.previous_bundle_sha256 else {
                return Err(DeploymentStartupError {
                    kind: DeploymentStartupErrorKind::CandidateRejected,
                    message: "active bundle failed and no previous bundle is available".to_owned(),
                    source: Some(Box::new(active_error)),
                });
            };
            let previous_activation = activation_for_previous(previous_identity);
            let previous = prepare_candidate(
                target,
                &bundles,
                &previous_activation,
                runtime_version,
                maximum_bundle_bytes,
                playback_limits,
            );
            match previous {
                Ok(bundle) => {
                    confirm_directory_entry(target, BUNDLES_NAME, bundles_identity)?;
                    let generation = next_generation(state.raw.generation)?;
                    let recovered = recovered_state(&state, &bundle, generation);
                    publish_state(target, &state.original_bytes, &recovered)?;
                    Ok(ResolvedRuntimeDeployment {
                        bundle,
                        generation,
                        recovered_from_bundle_sha256: Some(state.active.bundle_sha256),
                    })
                }
                Err(previous) => Err(DeploymentStartupError {
                    kind: DeploymentStartupErrorKind::CandidateRejected,
                    message: "active and previous bundles both failed startup health checks"
                        .to_owned(),
                    source: Some(Box::new(CandidateFailures {
                        active: active_error,
                        previous,
                    })),
                }),
            }
        }
    }
}

fn prepare_candidate(
    target: &TargetAuthority,
    bundles: &Dir,
    expected: &Activation,
    runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
) -> Result<VerifiedRuntimeBundle, CandidateError> {
    let name = encode_digest(&expected.bundle_sha256);
    let candidate = bundles.open_dir_nofollow(&name).map_err(|source| {
        CandidateError::Deployment(io_error("open installed candidate", source))
    })?;
    let metadata = candidate.dir_metadata().map_err(|source| {
        CandidateError::Deployment(io_error("inspect installed candidate", source))
    })?;
    if !metadata.is_dir() {
        return Err(CandidateError::Deployment(reject(
            DeploymentStartupErrorKind::CandidateRejected,
            "installed candidate is not a directory",
        )));
    }
    let root = retained_child_path(target, bundles, &name);
    let bundle = prepare_runtime_bundle(
        &root,
        runtime_version,
        maximum_bundle_bytes,
        playback_limits,
    )?;
    bind_activation(expected, &bundle)?;
    let published = bundles.symlink_metadata(&name).map_err(|source| {
        CandidateError::Deployment(io_error("reinspect installed candidate", source))
    })?;
    if published.file_type().is_symlink()
        || !published.is_dir()
        || identity(&published) != identity(&metadata)
    {
        return Err(CandidateError::Deployment(reject(
            DeploymentStartupErrorKind::AuthorityChanged,
            "installed candidate identity changed",
        )));
    }
    target
        .confirm_unchanged()
        .map_err(CandidateError::Deployment)?;
    Ok(bundle)
}

fn confirm_directory_entry(
    target: &TargetAuthority,
    name: &str,
    expected_identity: (u64, u64),
) -> Result<(), DeploymentStartupError> {
    let published = target
        .root
        .symlink_metadata(name)
        .map_err(|source| io_error("reinspect deployment directory", source))?;
    if published.file_type().is_symlink()
        || !published.is_dir()
        || identity(&published) != expected_identity
    {
        return Err(reject(
            DeploymentStartupErrorKind::AuthorityChanged,
            "deployment directory identity changed",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn retained_child_path(_target: &TargetAuthority, bundles: &Dir, name: &str) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}/{}", bundles.as_raw_fd(), name))
}

#[cfg(windows)]
fn retained_child_path(target: &TargetAuthority, _bundles: &Dir, name: &str) -> PathBuf {
    target.published_root.join(BUNDLES_NAME).join(name)
}

fn bind_activation(
    expected: &Activation,
    bundle: &VerifiedRuntimeBundle,
) -> Result<(), CandidateError> {
    let manifest = bundle.project().package().manifest();
    if bundle.bundle_sha256() != &expected.bundle_sha256
        || expected
            .project_id
            .as_deref()
            .is_some_and(|project_id| manifest.project_id() != project_id)
        || expected
            .project_revision
            .is_some_and(|revision| manifest.revision() != revision)
        || expected
            .minimum_runtime_version
            .as_ref()
            .is_some_and(|minimum| bundle.minimum_runtime_version() != minimum)
    {
        return Err(CandidateError::Deployment(reject(
            DeploymentStartupErrorKind::CandidateRejected,
            "deployment activation does not match the installed bundle",
        )));
    }
    Ok(())
}

fn read_state(target: &TargetAuthority) -> Result<DeploymentState, DeploymentStartupError> {
    let bytes = read_stable_file(&target.root, STATE_NAME, MAXIMUM_METADATA_BYTES)
        .map_err(|source| io_error("read deployment state", source))?;
    let value = hyper_strict_json::parse(&bytes, metadata_limit()).map_err(|source| {
        boxed_error(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment state is not strict JSON",
            source,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
        boxed_error(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment state cannot be canonicalized",
            source,
        )
    })?;
    if canonical != bytes {
        return Err(reject(
            DeploymentStartupErrorKind::StateNonCanonical,
            "deployment state is not canonical RFC 8785 JSON",
        ));
    }
    let raw: RawDeploymentState = serde_json::from_value(value).map_err(|source| {
        boxed_error(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment state shape is invalid",
            source,
        )
    })?;
    let active = validate_state(&raw)?;
    let previous_bundle_sha256 = raw
        .previous_bundle_sha256
        .as_deref()
        .map(parse_digest)
        .transpose()?;
    target.confirm_unchanged()?;
    Ok(DeploymentState {
        raw,
        active,
        previous_bundle_sha256,
        original_bytes: bytes,
    })
}

fn validate_state(raw: &RawDeploymentState) -> Result<Activation, DeploymentStartupError> {
    if raw.format != "hyper-deployment-state"
        || raw.format_version != 1
        || raw.generation == 0
        || raw.generation > MAX_SAFE_INTEGER
        || raw.active.project_revision > MAX_SAFE_INTEGER
        || raw
            .extensions
            .keys()
            .any(|key| !is_valid_extension_key(key))
    {
        return Err(reject(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment state domain is invalid",
        ));
    }
    if !is_valid_entity_id(&raw.active.project_id) {
        return Err(reject(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment project identifier is invalid",
        ));
    }
    let minimum_runtime_version =
        Version::parse(&raw.active.minimum_runtime_version).map_err(|source| {
            boxed_error(
                DeploymentStartupErrorKind::StateInvalid,
                "deployment minimum Runtime version is invalid",
                source,
            )
        })?;
    Ok(Activation {
        bundle_sha256: parse_digest(&raw.active.bundle_sha256)?,
        project_id: Some(raw.active.project_id.clone()),
        project_revision: Some(raw.active.project_revision),
        minimum_runtime_version: Some(minimum_runtime_version),
        status: raw.active.status,
    })
}

fn activation_for_previous(bundle_sha256: [u8; 32]) -> Activation {
    Activation {
        bundle_sha256,
        project_id: None,
        project_revision: None,
        minimum_runtime_version: None,
        status: ActivationStatus::Pending,
    }
}

fn recovered_state(
    state: &DeploymentState,
    bundle: &VerifiedRuntimeBundle,
    generation: u64,
) -> RawDeploymentState {
    let manifest = bundle.project().package().manifest();
    RawDeploymentState {
        format: "hyper-deployment-state".to_owned(),
        format_version: 1,
        generation,
        active: RawActivation {
            bundle_sha256: encode_digest(bundle.bundle_sha256()),
            project_id: manifest.project_id().to_owned(),
            project_revision: manifest.revision(),
            minimum_runtime_version: bundle.minimum_runtime_version().to_string(),
            status: ActivationStatus::Healthy,
        },
        previous_bundle_sha256: Some(encode_digest(&state.active.bundle_sha256)),
        extensions: BTreeMap::new(),
    }
}

fn publish_state(
    target: &TargetAuthority,
    expected_current: &[u8],
    state: &RawDeploymentState,
) -> Result<(), DeploymentStartupError> {
    let current = read_stable_file(&target.root, STATE_NAME, MAXIMUM_METADATA_BYTES)
        .map_err(|source| io_error("recheck deployment state", source))?;
    if current != expected_current {
        return Err(reject(
            DeploymentStartupErrorKind::AuthorityChanged,
            "deployment state changed during startup resolution",
        ));
    }
    let bytes = canonical_state_bytes(state)?;
    let transaction_name = format!(".state-{}", Uuid::new_v4());
    target
        .root
        .create_dir_with(&transaction_name, &private_directory_builder())
        .map_err(|source| io_error("create deployment state transaction", source))?;
    let transaction = match target.root.open_dir_nofollow(&transaction_name) {
        Ok(directory) => directory,
        Err(source) => {
            let primary = io_error("retain deployment state transaction", source);
            return match target.root.remove_dir(&transaction_name) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(cleanup_error(
                    primary,
                    "clean up deployment state transaction",
                    &cleanup,
                )),
            };
        }
    };
    let transaction_identity = match transaction.dir_metadata() {
        Ok(metadata) => identity(&metadata),
        Err(source) => {
            let primary = io_error("inspect deployment state transaction", source);
            return match transaction
                .remove_open_dir_all()
                .and_then(|()| sync_directory(&target.root))
            {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(cleanup_error(
                    primary,
                    "clean up deployment state transaction",
                    &cleanup,
                )),
            };
        }
    };
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let operation = (|| {
        let mut file = transaction
            .open_with(STATE_NAME, &options)
            .map_err(|source| io_error("create deployment state transaction file", source))?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| io_error("write deployment state transaction", source))?;
        let metadata = transaction
            .symlink_metadata(STATE_NAME)
            .map_err(|source| io_error("inspect deployment state transaction file", source))?;
        target.confirm_unchanged()?;
        rename_replace(
            &transaction,
            STATE_NAME,
            identity(&metadata),
            &target.root,
            STATE_NAME,
        )
        .and_then(|()| sync_directory(&target.root))
        .map_err(|source| io_error("publish deployment state", source))?;
        target.confirm_unchanged()
    })();
    let cleanup =
        remove_state_transaction(target, transaction, &transaction_name, transaction_identity);
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(operation), Err(cleanup)) => Err(DeploymentStartupError {
            kind: operation.kind,
            message: format!(
                "{}; deployment state transaction cleanup also failed: {}",
                error_chain(&operation),
                error_chain(&cleanup)
            ),
            source: Some(Box::new(operation)),
        }),
    }
}

fn canonical_state_bytes(state: &RawDeploymentState) -> Result<Vec<u8>, DeploymentStartupError> {
    let bytes = serde_json_canonicalizer::to_vec(state).map_err(|source| {
        boxed_error(
            DeploymentStartupErrorKind::Publication,
            "canonicalize deployment state",
            source,
        )
    })?;
    if bytes.len() > metadata_limit() {
        return Err(reject(
            DeploymentStartupErrorKind::Publication,
            "deployment state exceeds its byte limit",
        ));
    }
    Ok(bytes)
}

fn remove_state_transaction(
    target: &TargetAuthority,
    transaction: Dir,
    name: &str,
    expected_identity: (u64, u64),
) -> Result<(), DeploymentStartupError> {
    let published = target
        .root
        .symlink_metadata(name)
        .map_err(|source| io_error("reinspect deployment state transaction", source))?;
    if published.file_type().is_symlink()
        || !published.is_dir()
        || identity(&published) != expected_identity
    {
        return Err(reject(
            DeploymentStartupErrorKind::AuthorityChanged,
            "deployment state transaction identity changed",
        ));
    }
    transaction
        .remove_open_dir_all()
        .and_then(|()| sync_directory(&target.root))
        .map_err(|source| io_error("clean up deployment state transaction", source))
}

fn next_generation(current: u64) -> Result<u64, DeploymentStartupError> {
    if current >= MAX_SAFE_INTEGER {
        return Err(reject(
            DeploymentStartupErrorKind::Publication,
            "deployment generation is exhausted",
        ));
    }
    Ok(current + 1)
}

fn metadata_limit() -> usize {
    usize::try_from(MAXIMUM_METADATA_BYTES)
        .expect("the 64 KiB deployment metadata limit fits every supported platform")
}

fn parse_digest(value: &str) -> Result<[u8; 32], DeploymentStartupError> {
    if value.len() != 64 {
        return Err(reject(
            DeploymentStartupErrorKind::StateInvalid,
            "deployment bundle SHA-256 is invalid",
        ));
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hexadecimal(pair[0]);
        let low = hexadecimal(pair[1]);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(reject(
                DeploymentStartupErrorKind::StateInvalid,
                "deployment bundle SHA-256 is invalid",
            ));
        };
        digest[index] = (high << 4) | low;
    }
    Ok(digest)
}

const fn hexadecimal(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn encode_digest(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn private_directory_builder() -> DirBuilder {
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    }
    #[cfg(not(unix))]
    {
        DirBuilder::new()
    }
}

fn error_chain(error: &(dyn Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use core::fmt::Write;
        write!(&mut rendered, ": {cause}").expect("writing to a string cannot fail");
        source = cause.source();
    }
    rendered
}

fn reject(kind: DeploymentStartupErrorKind, message: &str) -> DeploymentStartupError {
    DeploymentStartupError {
        kind,
        message: message.to_owned(),
        source: None,
    }
}

fn io_error(operation: &str, source: io::Error) -> DeploymentStartupError {
    DeploymentStartupError {
        kind: DeploymentStartupErrorKind::Io,
        message: operation.to_owned(),
        source: Some(Box::new(source)),
    }
}

fn boxed_error(
    kind: DeploymentStartupErrorKind,
    message: &str,
    source: impl Error + Send + Sync + 'static,
) -> DeploymentStartupError {
    DeploymentStartupError {
        kind,
        message: message.to_owned(),
        source: Some(Box::new(source)),
    }
}

fn cleanup_error(
    primary: DeploymentStartupError,
    cleanup_operation: &str,
    cleanup: &io::Error,
) -> DeploymentStartupError {
    DeploymentStartupError {
        kind: primary.kind,
        message: format!(
            "{}; {cleanup_operation} also failed: {cleanup}",
            error_chain(&primary)
        ),
        source: Some(Box::new(primary)),
    }
}

#[cfg(unix)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn is_link_like(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}
