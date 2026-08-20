#![doc = "Deterministic native Runtime bundle construction for the Desktop IDE."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_fs_authority::{
    identity, open_syncable_directory_ambient, open_syncable_directory_at, remove_directory_entry,
    rename_no_replace, sync_directory,
};
use hyper_project_loader::{ProjectPlaybackError, ProjectPlaybackLimits, prepare_project_playback};
use hyper_project_model::{AudioMediaType, HardwareCapabilities, ProjectManifest, UsbMode};
use hyper_runtime_bundle::{RuntimeBundleError, prepare_runtime_bundle, prepare_runtime_bundle_at};
use semver::Version;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

const BUILDER_ID: &str = "hyper-bundle-builder";
const ENTRYPOINT: &str = "project.json";
const MANIFEST_NAME: &str = "manifest.json";
const MAX_MANIFEST_BYTES: usize = 1_048_576;
const MAX_FILES: usize = 4_096;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Default upper bound shared with the version-1 conformance builder.
pub const DEFAULT_MAXIMUM_RUNTIME_BUNDLE_BYTES: u64 = 1_024 * 1_024 * 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBundleBuildErrorKind {
    InvalidRequest,
    Source,
    Limit,
    Filesystem,
    Serialization,
    Verification,
    OutputConflict,
    ArtifactReadyButCleanupFailed,
    PublishedButUnsynced,
}

#[derive(Debug)]
pub struct RuntimeBundleBuildError {
    kind: RuntimeBundleBuildErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
    published: Option<Box<PublishedRuntimeBundle>>,
}

impl RuntimeBundleBuildError {
    #[must_use]
    pub const fn kind(&self) -> RuntimeBundleBuildErrorKind {
        self.kind
    }

    /// Returns the verified artifact when publication or reuse succeeded but a
    /// later durability or cleanup operation failed. The caller must not assume
    /// the artifact was lost.
    #[must_use]
    pub fn published(&self) -> Option<&PublishedRuntimeBundle> {
        self.published.as_deref()
    }
}

impl fmt::Display for RuntimeBundleBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeBundleBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedRuntimeBundle {
    output_path: PathBuf,
    bundle_sha256: [u8; 32],
    bundle_byte_length: u64,
    reused: bool,
}

impl PublishedRuntimeBundle {
    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    #[must_use]
    pub const fn bundle_sha256(&self) -> &[u8; 32] {
        &self.bundle_sha256
    }

    #[must_use]
    pub const fn bundle_byte_length(&self) -> u64 {
        self.bundle_byte_length
    }

    #[must_use]
    pub const fn reused(&self) -> bool {
        self.reused
    }
}

#[derive(Serialize)]
struct RuntimeManifest<'a> {
    format: &'static str,
    format_version: u8,
    project_id: &'a str,
    project_revision: u64,
    source_project_sha256: String,
    minimum_runtime_version: &'a str,
    builder: Builder,
    entrypoint: &'static str,
    required_capabilities: CapabilityManifest<'a>,
    files: Vec<RuntimeFile>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Serialize)]
struct Builder {
    id: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "this wire type mirrors the version-1 hardware capability contract"
)]
struct CapabilityManifest<'a> {
    format: &'static str,
    format_version: u8,
    audio_inputs: u8,
    audio_outputs: u8,
    cv_inputs: u8,
    cv_outputs: u8,
    gate_outputs: u8,
    has_clock_input: bool,
    has_reset_input: bool,
    has_clock_output: bool,
    usb_mode: &'static str,
    has_ethernet: bool,
    extensions: &'a BTreeMap<String, Value>,
}

impl<'a> From<&'a HardwareCapabilities> for CapabilityManifest<'a> {
    fn from(capabilities: &'a HardwareCapabilities) -> Self {
        Self {
            format: "hyper-hardware-capabilities",
            format_version: 1,
            audio_inputs: capabilities.audio_inputs(),
            audio_outputs: capabilities.audio_outputs(),
            cv_inputs: capabilities.cv_inputs(),
            cv_outputs: capabilities.cv_outputs(),
            gate_outputs: capabilities.gate_outputs(),
            has_clock_input: capabilities.has_clock_input(),
            has_reset_input: capabilities.has_reset_input(),
            has_clock_output: capabilities.has_clock_output(),
            usb_mode: match capabilities.usb_mode() {
                UsbMode::None => "none",
                UsbMode::Host => "host",
                UsbMode::Service => "service",
                UsbMode::HostAndService => "host-and-service",
            },
            has_ethernet: capabilities.has_ethernet(),
            extensions: capabilities.extensions(),
        }
    }
}

#[derive(Clone, Serialize)]
struct RuntimeFile {
    path: String,
    role: &'static str,
    media_type: &'static str,
    byte_length: u64,
    sha256: String,
}

struct Payload {
    descriptor: RuntimeFile,
    source: PayloadSource,
}

enum PayloadSource {
    ProjectManifest,
    Resource(String),
    Asset(String),
}

/// Builds, verifies, durably publishes, and returns one version-1 Runtime
/// bundle without modifying the editable source project.
///
/// A pre-existing output is accepted only when it independently verifies to
/// the exact identity that this invocation generated. The source is fully
/// prepared for playback before any output is created and reconfirmed before
/// publication.
///
/// # Errors
///
/// Returns a classified error for an invalid request, unplayable or changing
/// source, exceeded bound, filesystem failure, output collision, generated
/// bundle verification failure, or publication whose final parent-directory
/// durability barrier failed.
pub fn build_project_runtime_bundle(
    project_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
) -> Result<PublishedRuntimeBundle, RuntimeBundleBuildError> {
    validate_request(minimum_runtime_version, maximum_bundle_bytes)?;
    let project =
        prepare_project_playback(project_path.as_ref(), playback_limits).map_err(source_error)?;
    let (package, _, _, _, _) = project.into_parts();
    let project_manifest = package.manifest();
    let mut payloads = describe_payloads(&package, project_manifest)?;
    payloads.sort_by(|left, right| left.descriptor.path.cmp(&right.descriptor.path));

    let manifest = RuntimeManifest {
        format: "hyper-runtime-bundle",
        format_version: 1,
        project_id: project_manifest.project_id(),
        project_revision: project_manifest.revision(),
        source_project_sha256: encode_digest(package.manifest_sha256()),
        minimum_runtime_version,
        builder: Builder {
            id: BUILDER_ID,
            version: env!("CARGO_PKG_VERSION"),
        },
        entrypoint: ENTRYPOINT,
        required_capabilities: project_manifest.required_capabilities().into(),
        files: payloads
            .iter()
            .map(|payload| payload.descriptor.clone())
            .collect(),
        extensions: BTreeMap::new(),
    };
    let manifest_bytes = serde_json_canonicalizer::to_vec(&manifest)
        .map_err(|source| serialization_error("canonicalize runtime manifest", source))?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(limit_error(
            "generated runtime manifest exceeds one mebibyte",
        ));
    }
    let bundle_byte_length = total_bundle_bytes(&payloads, manifest_bytes.len())?;
    if bundle_byte_length > maximum_bundle_bytes {
        return Err(limit_error(
            "runtime bundle exceeds its configured byte limit",
        ));
    }
    let bundle_sha256: [u8; 32] = Sha256::digest(&manifest_bytes).into();

    publish_prepared_bundle(
        &package,
        output_path.as_ref(),
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
        &payloads,
        &manifest_bytes,
        bundle_sha256,
        bundle_byte_length,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "publication binds the complete prepared identity and verification policy"
)]
fn publish_prepared_bundle(
    package: &hyper_storage_engine::StructurallyVerifiedProject,
    output_path: &Path,
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
    payloads: &[Payload],
    manifest_bytes: &[u8],
    bundle_sha256: [u8; 32],
    bundle_byte_length: u64,
) -> Result<PublishedRuntimeBundle, RuntimeBundleBuildError> {
    let (output_parent_path, output_name) = output_parts(output_path)?;
    let output_parent = open_syncable_directory_ambient(output_parent_path)
        .map_err(|source| filesystem_error("open Runtime bundle output parent", source))?;
    let stage_name = format!(".runtime-staging-{}", Uuid::new_v4());
    let (stage, stage_identity) = create_and_open_stage(&output_parent, &stage_name)?;
    let stage_path = output_parent_path.join(&stage_name);

    let result = build_in_stage(
        package,
        &stage,
        &stage_path,
        payloads,
        manifest_bytes,
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
        bundle_sha256,
        bundle_byte_length,
    );
    drop(stage);
    let generated = match result {
        Ok(generated) => generated,
        Err(primary) => {
            return cleanup_after_failure(primary, &output_parent, &stage_name, stage_identity);
        }
    };

    match rename_no_replace(
        &output_parent,
        &stage_name,
        stage_identity,
        &output_parent,
        &output_name,
    ) {
        Ok(()) => {
            let mut published = generated;
            output_path.clone_into(&mut published.output_path);
            finish_publication(&output_parent, published)
        }
        Err(rename_error) => {
            match output_parent.symlink_metadata(&output_name) {
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    let primary = filesystem_error("publish Runtime bundle", rename_error);
                    return cleanup_after_failure(
                        primary,
                        &output_parent,
                        &stage_name,
                        stage_identity,
                    );
                }
                Err(source) => {
                    let primary = filesystem_error("inspect Runtime bundle output", source);
                    return cleanup_after_failure(
                        primary,
                        &output_parent,
                        &stage_name,
                        stage_identity,
                    );
                }
                Ok(_) => {}
            }
            let existing = verify_existing_output(
                output_path,
                minimum_runtime_version,
                maximum_bundle_bytes,
                playback_limits,
                &generated,
            );
            let cleanup = remove_directory_entry(&output_parent, &stage_name, stage_identity);
            match (existing, cleanup) {
                (Ok(existing), Ok(())) => finish_publication(&output_parent, existing),
                (Ok(existing), Err(cleanup)) => {
                    Err(artifact_ready_but_cleanup_failed(existing, cleanup))
                }
                (Err(conflict), Ok(())) => Err(conflict.with_rename_source(rename_error)),
                (Err(conflict), Err(cleanup)) => Err(filesystem_error_with_source(
                    &format!("{conflict}; staging cleanup also failed"),
                    cleanup,
                )),
            }
        }
    }
}

fn validate_request(
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
) -> Result<(), RuntimeBundleBuildError> {
    if minimum_runtime_version.is_empty() || Version::parse(minimum_runtime_version).is_err() {
        return Err(request_error("minimum Runtime version is invalid"));
    }
    if maximum_bundle_bytes == 0 || maximum_bundle_bytes > MAX_SAFE_INTEGER {
        return Err(request_error(
            "maximum Runtime bundle bytes must be a positive safe integer",
        ));
    }
    Ok(())
}

fn describe_payloads(
    package: &hyper_storage_engine::StructurallyVerifiedProject,
    manifest: &ProjectManifest,
) -> Result<Vec<Payload>, RuntimeBundleBuildError> {
    let file_count = 1_usize
        .checked_add(manifest.resources().len())
        .and_then(|count| count.checked_add(manifest.assets().len()))
        .ok_or_else(|| limit_error("runtime file count overflowed"))?;
    if file_count > MAX_FILES {
        return Err(limit_error("runtime file count exceeds its limit"));
    }
    let mut payloads = Vec::new();
    payloads
        .try_reserve_exact(file_count)
        .map_err(|_| limit_error("runtime payload capacity is unavailable"))?;
    payloads.push(Payload {
        descriptor: RuntimeFile {
            path: ENTRYPOINT.to_owned(),
            role: "runtime-project",
            media_type: "application/json",
            byte_length: package.manifest_byte_length(),
            sha256: encode_digest(package.manifest_sha256()),
        },
        source: PayloadSource::ProjectManifest,
    });
    for resource in manifest.resources() {
        payloads.push(Payload {
            descriptor: RuntimeFile {
                path: resource.path().to_owned(),
                role: "runtime-data",
                media_type: "application/json",
                byte_length: resource.byte_length(),
                sha256: resource.sha256().to_owned(),
            },
            source: PayloadSource::Resource(resource.id().to_owned()),
        });
    }
    for asset in manifest.assets() {
        payloads.push(Payload {
            descriptor: RuntimeFile {
                path: asset.path().to_owned(),
                role: "audio",
                media_type: match asset.media_type() {
                    AudioMediaType::AudioFlac => "audio/flac",
                    AudioMediaType::AudioWav => "audio/wav",
                },
                byte_length: asset.byte_length(),
                sha256: asset.sha256().to_owned(),
            },
            source: PayloadSource::Asset(asset.id().to_owned()),
        });
    }
    Ok(payloads)
}

#[allow(clippy::too_many_arguments)]
fn build_in_stage(
    source: &hyper_storage_engine::StructurallyVerifiedProject,
    stage: &Dir,
    stage_path: &Path,
    payloads: &[Payload],
    manifest_bytes: &[u8],
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
    bundle_sha256: [u8; 32],
    bundle_byte_length: u64,
) -> Result<PublishedRuntimeBundle, RuntimeBundleBuildError> {
    let mut directories = BTreeSet::new();
    for payload in payloads {
        let bytes = match &payload.source {
            PayloadSource::ProjectManifest => source.read_manifest(),
            PayloadSource::Resource(resource_id) => source.read_resource(resource_id),
            PayloadSource::Asset(asset_id) => source.read_asset(asset_id, maximum_bundle_bytes),
        }
        .map_err(source_storage_error)?;
        write_payload(stage, &payload.descriptor.path, &bytes, &mut directories)?;
    }
    write_file(stage, MANIFEST_NAME, manifest_bytes)?;
    verify_generated(
        stage,
        stage_path,
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
        bundle_sha256,
        bundle_byte_length,
    )?;
    sync_created_directories(stage, &directories)?;
    source.confirm_unchanged().map_err(source_storage_error)?;
    verify_generated(
        stage,
        stage_path,
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
        bundle_sha256,
        bundle_byte_length,
    )?;
    Ok(PublishedRuntimeBundle {
        output_path: stage_path.to_owned(),
        bundle_sha256,
        bundle_byte_length,
        reused: false,
    })
}

fn output_parts(output_path: &Path) -> Result<(&Path, String), RuntimeBundleBuildError> {
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| request_error("Runtime bundle output must have an existing parent"))?;
    let name = output_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| request_error("Runtime bundle output name is not portable UTF-8"))?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(request_error(
            "Runtime bundle output name must be one normal component",
        ));
    }
    Ok((parent, name.to_owned()))
}

fn create_stage(parent: &Dir, name: &str) -> Result<(), RuntimeBundleBuildError> {
    #[cfg(unix)]
    let builder = {
        use cap_std::fs::DirBuilderExt;
        let mut builder = cap_std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = cap_std::fs::DirBuilder::new();
    parent
        .create_dir_with(name, &builder)
        .map_err(|source| filesystem_error("create Runtime bundle staging directory", source))
}

fn create_and_open_stage(
    parent: &Dir,
    name: &str,
) -> Result<(Dir, (u64, u64)), RuntimeBundleBuildError> {
    create_stage(parent, name)?;
    let stage = match open_syncable_directory_at(parent, name) {
        Ok(stage) => stage,
        Err(source) => {
            let primary = filesystem_error("open Runtime bundle staging directory", source);
            return cleanup_created_stage(parent, name, primary);
        }
    };
    let stage_identity = match stage.dir_metadata() {
        Ok(metadata) => identity(&metadata),
        Err(source) => {
            drop(stage);
            let primary = filesystem_error("inspect Runtime bundle staging directory", source);
            return cleanup_created_stage(parent, name, primary);
        }
    };
    Ok((stage, stage_identity))
}

fn cleanup_created_stage<T>(
    parent: &Dir,
    name: &str,
    primary: RuntimeBundleBuildError,
) -> Result<T, RuntimeBundleBuildError> {
    let stage_identity = match parent.symlink_metadata(name) {
        Ok(metadata) => identity(&metadata),
        Err(cleanup) => {
            return Err(filesystem_error_with_source(
                &format!(
                    "{primary}; could not identify the Runtime bundle staging entry for cleanup"
                ),
                cleanup,
            ));
        }
    };
    cleanup_after_failure(primary, parent, name, stage_identity)
}

fn write_payload(
    stage: &Dir,
    path: &str,
    bytes: &[u8],
    directories: &mut BTreeSet<String>,
) -> Result<(), RuntimeBundleBuildError> {
    let components: Vec<_> = path.split('/').collect();
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| request_error("runtime payload path is empty"))?;
    let mut current = stage
        .try_clone()
        .map_err(|source| filesystem_error("retain Runtime bundle staging directory", source))?;
    let mut relative = String::new();
    for component in parents {
        if !relative.is_empty() {
            relative.push('/');
        }
        relative.push_str(component);
        if directories.insert(relative.clone()) {
            current
                .create_dir(component)
                .map_err(|source| filesystem_error("create Runtime payload directory", source))?;
        }
        current = current
            .open_dir_nofollow(component)
            .map_err(|source| filesystem_error("open Runtime payload directory", source))?;
    }
    write_file(&current, file_name, bytes)
}

fn write_file(directory: &Dir, name: &str, bytes: &[u8]) -> Result<(), RuntimeBundleBuildError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(name, &options)
        .map_err(|source| filesystem_error("create Runtime bundle file", source))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| filesystem_error("write Runtime bundle file", source))
}

fn sync_created_directories(
    stage: &Dir,
    directories: &BTreeSet<String>,
) -> Result<(), RuntimeBundleBuildError> {
    let mut ordered: Vec<_> = directories.iter().collect();
    ordered.sort_by_key(|path| std::cmp::Reverse(path.matches('/').count()));
    for path in ordered {
        let directory = open_relative_directory(stage, path)?;
        sync_directory(&directory)
            .map_err(|source| filesystem_error("synchronize Runtime payload directory", source))?;
    }
    sync_directory(stage)
        .map_err(|source| filesystem_error("synchronize Runtime bundle staging directory", source))
}

fn open_relative_directory(root: &Dir, path: &str) -> Result<Dir, RuntimeBundleBuildError> {
    let mut current = root
        .try_clone()
        .map_err(|source| filesystem_error("retain Runtime bundle directory", source))?;
    for component in path.split('/') {
        current = open_syncable_directory_at(&current, component)
            .map_err(|source| filesystem_error("open syncable Runtime bundle directory", source))?;
    }
    Ok(current)
}

fn verify_generated(
    root: &Dir,
    path: &Path,
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
    expected_sha256: [u8; 32],
    expected_bytes: u64,
) -> Result<(), RuntimeBundleBuildError> {
    let directory = root
        .try_clone()
        .map_err(|source| filesystem_error("retain Runtime bundle staging directory", source))?;
    let verified = prepare_runtime_bundle_at(
        path,
        directory,
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
    )
    .map_err(verification_error)?;
    if verified.bundle_sha256() != &expected_sha256
        || verified.bundle_byte_length() != expected_bytes
    {
        return Err(rejection(
            RuntimeBundleBuildErrorKind::Verification,
            "generated Runtime bundle identity does not match its manifest",
        ));
    }
    Ok(())
}

fn verify_existing_output(
    output_path: &Path,
    minimum_runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
    generated: &PublishedRuntimeBundle,
) -> Result<PublishedRuntimeBundle, RuntimeBundleBuildError> {
    let existing = prepare_runtime_bundle(
        output_path,
        minimum_runtime_version,
        maximum_bundle_bytes,
        playback_limits,
    )
    .map_err(|source| RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::OutputConflict,
        message: "Runtime bundle output already exists and is not the requested bundle".to_owned(),
        source: Some(Box::new(source)),
        published: None,
    })?;
    if existing.bundle_sha256() != generated.bundle_sha256()
        || existing.bundle_byte_length() != generated.bundle_byte_length()
    {
        return Err(rejection(
            RuntimeBundleBuildErrorKind::OutputConflict,
            "Runtime bundle output already exists with a different identity",
        ));
    }
    let directory = open_syncable_directory_ambient(output_path)
        .map_err(|source| filesystem_error("open existing Runtime bundle output", source))?;
    sync_directory(&directory)
        .map_err(|source| filesystem_error("synchronize existing Runtime bundle output", source))?;
    Ok(PublishedRuntimeBundle {
        output_path: output_path.to_owned(),
        bundle_sha256: *generated.bundle_sha256(),
        bundle_byte_length: generated.bundle_byte_length(),
        reused: true,
    })
}

fn finish_publication(
    parent: &Dir,
    published: PublishedRuntimeBundle,
) -> Result<PublishedRuntimeBundle, RuntimeBundleBuildError> {
    if let Err(source) = sync_directory(parent) {
        return Err(RuntimeBundleBuildError {
            kind: RuntimeBundleBuildErrorKind::PublishedButUnsynced,
            message: format!(
                "Runtime bundle was published at {} but its parent durability barrier failed",
                published.output_path.display()
            ),
            source: Some(Box::new(source)),
            published: Some(Box::new(published)),
        });
    }
    Ok(published)
}

fn artifact_ready_but_cleanup_failed(
    published: PublishedRuntimeBundle,
    source: io::Error,
) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::ArtifactReadyButCleanupFailed,
        message: format!(
            "verified Runtime bundle remains available at {} but staging cleanup failed",
            published.output_path().display()
        ),
        source: Some(Box::new(source)),
        published: Some(Box::new(published)),
    }
}

fn cleanup_after_failure<T>(
    primary: RuntimeBundleBuildError,
    parent: &Dir,
    stage_name: &str,
    stage_identity: (u64, u64),
) -> Result<T, RuntimeBundleBuildError> {
    match remove_directory_entry(parent, stage_name, stage_identity) {
        Ok(()) => Err(primary),
        Err(cleanup) => Err(filesystem_error_with_source(
            &format!("{primary}; Runtime bundle staging cleanup also failed"),
            cleanup,
        )),
    }
}

fn total_bundle_bytes(
    payloads: &[Payload],
    manifest_bytes: usize,
) -> Result<u64, RuntimeBundleBuildError> {
    let mut total = u64::try_from(manifest_bytes)
        .map_err(|_| limit_error("runtime manifest byte count is unavailable"))?;
    for payload in payloads {
        total = total
            .checked_add(payload.descriptor.byte_length)
            .ok_or_else(|| limit_error("runtime bundle byte accounting overflowed"))?;
    }
    if total > MAX_SAFE_INTEGER {
        return Err(limit_error(
            "runtime bundle byte accounting exceeds the safe integer range",
        ));
    }
    Ok(total)
}

fn encode_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

impl RuntimeBundleBuildError {
    fn with_rename_source(mut self, source: io::Error) -> Self {
        if self.source.is_none() {
            self.source = Some(Box::new(source));
        }
        self
    }
}

fn rejection(kind: RuntimeBundleBuildErrorKind, message: &str) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind,
        message: message.to_owned(),
        source: None,
        published: None,
    }
}

fn request_error(message: &str) -> RuntimeBundleBuildError {
    rejection(RuntimeBundleBuildErrorKind::InvalidRequest, message)
}

fn limit_error(message: &str) -> RuntimeBundleBuildError {
    rejection(RuntimeBundleBuildErrorKind::Limit, message)
}

fn source_error(source: ProjectPlaybackError) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::Source,
        message: "source project is not ready for Runtime playback".to_owned(),
        source: Some(Box::new(source)),
        published: None,
    }
}

fn source_storage_error(source: hyper_storage_engine::StorageError) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::Source,
        message: "source project changed during Runtime bundle construction".to_owned(),
        source: Some(Box::new(source)),
        published: None,
    }
}

fn filesystem_error(action: &str, source: io::Error) -> RuntimeBundleBuildError {
    filesystem_error_with_source(action, source)
}

fn filesystem_error_with_source(
    action: &str,
    source: impl Error + Send + Sync + 'static,
) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::Filesystem,
        message: action.to_owned(),
        source: Some(Box::new(source)),
        published: None,
    }
}

fn serialization_error(
    action: &str,
    source: impl Error + Send + Sync + 'static,
) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::Serialization,
        message: action.to_owned(),
        source: Some(Box::new(source)),
        published: None,
    }
}

fn verification_error(source: RuntimeBundleError) -> RuntimeBundleBuildError {
    RuntimeBundleBuildError {
        kind: RuntimeBundleBuildErrorKind::Verification,
        message: "generated Runtime bundle failed independent verification".to_owned(),
        source: Some(Box::new(source)),
        published: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_preserves_verified_artifact_evidence() {
        let published = PublishedRuntimeBundle {
            output_path: PathBuf::from("prepared.runtime"),
            bundle_sha256: [7; 32],
            bundle_byte_length: 42,
            reused: true,
        };

        let error = artifact_ready_but_cleanup_failed(
            published,
            io::Error::other("injected cleanup failure"),
        );

        assert_eq!(
            error.kind(),
            RuntimeBundleBuildErrorKind::ArtifactReadyButCleanupFailed
        );
        let retained = error.published().expect("artifact evidence must survive");
        assert_eq!(retained.output_path(), Path::new("prepared.runtime"));
        assert_eq!(retained.bundle_byte_length(), 42);
        assert!(retained.reused());
    }
}
