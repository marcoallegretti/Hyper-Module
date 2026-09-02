//! Shared, bounded Runtime-to-IDE loop-update contracts.
//!
//! Runtime emits one complete, provenance-bound arrangement-loop state only
//! after an explicit operator request and a successful audio-cycle
//! acknowledgement. The IDE remains the only project writer: an update is a
//! deliberate handoff, never background synchronization.

#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
#[cfg(target_os = "linux")]
use hyper_fs_authority::publish_bytes_no_replace;
use hyper_fs_authority::{
    confirm_stable_absolute_file, open_syncable_directory_ambient, read_stable_absolute_file,
    sync_directory,
};
#[cfg(windows)]
use hyper_fs_authority::{identity, remove_file_if_identity, rename_no_replace};
use hyper_project_model::is_valid_entity_id;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-runtime-loop-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Maximum byte size accepted for a version-one Runtime loop update.
///
/// Every variable-width version-one value has a fixed contract bound: entity
/// identities are 36 bytes, digests are 64 bytes, and JSON numbers are capped
/// at the RFC 8259 interoperable integer range. The largest canonical update
/// is well below one KiB; the round 1 KiB boundary leaves bounded format
/// headroom while rejecting unrelated file contents before deserialization.
pub const MAX_RUNTIME_LOOP_UPDATE_BYTES: u64 = 1_024;

/// Encodes one SHA-256 digest using the lowercase hexadecimal form required by
/// Runtime handback provenance.
#[must_use]
pub fn sha256_hex(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;

        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

/// Exact verified project and Runtime provenance for one loop update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLoopUpdateSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeLoopUpdateSource {
    /// Creates provenance copied from a verified Runtime deployment.
    ///
    /// # Errors
    ///
    /// Refuses identities or digests outside the version-one contract.
    pub fn try_new(
        project_id: String,
        project_revision: u64,
        project_sha256: String,
        runtime_bundle_sha256: String,
        deployment_generation: u64,
    ) -> Result<Self, RuntimeLoopUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update source deployment generation is outside the version-one JSON domain",
            ));
        }
        Ok(Self {
            project_id,
            project_revision,
            project_sha256,
            runtime_bundle_sha256,
            deployment_generation,
        })
    }

    /// Returns the exact source project identity.
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Returns the exact source project revision.
    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    /// Returns the canonical source project-manifest SHA-256.
    #[must_use]
    pub fn project_sha256(&self) -> &str {
        &self.project_sha256
    }

    /// Returns the verified Runtime bundle SHA-256.
    #[must_use]
    pub fn runtime_bundle_sha256(&self) -> &str {
        &self.runtime_bundle_sha256
    }

    /// Returns the verified Runtime deployment generation.
    #[must_use]
    pub const fn deployment_generation(&self) -> u64 {
        self.deployment_generation
    }
}

/// One persistable Runtime arrangement-loop state.
///
/// A present range retains its enabled state, so an operator can hand back a
/// remembered but disengaged loop. An absent range means to clear the durable
/// project loop. Runtime's live state can represent an enabled loop with no
/// range; that incoherent state deliberately has no value in this type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeLoopRange {
    start_frame: u64,
    end_frame: u64,
    enabled: bool,
}

impl RuntimeLoopRange {
    /// Creates one finite, nonempty loop range in Runtime frames.
    ///
    /// # Errors
    ///
    /// Refuses collapsed, inverted, or JSON-unsafe frame positions.
    pub fn try_new(
        start_frame: u64,
        end_frame: u64,
        enabled: bool,
    ) -> Result<Self, RuntimeLoopUpdateError> {
        if start_frame > MAX_SAFE_INTEGER || end_frame > MAX_SAFE_INTEGER {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update frame position is outside the version-one JSON domain",
            ));
        }
        if end_frame <= start_frame {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update range must end after it starts",
            ));
        }
        Ok(Self {
            start_frame,
            end_frame,
            enabled,
        })
    }

    /// Returns the inclusive start boundary in Runtime frames.
    #[must_use]
    pub const fn start_frame(self) -> u64 {
        self.start_frame
    }

    /// Returns the exclusive end boundary in Runtime frames.
    #[must_use]
    pub const fn end_frame(self) -> u64 {
        self.end_frame
    }

    /// Returns whether Runtime transport had the retained range engaged.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

/// A complete, append-only arrangement-loop update emitted by Runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLoopUpdate {
    id: String,
    source: RuntimeLoopUpdateSource,
    loop_range: Option<RuntimeLoopRange>,
}

impl RuntimeLoopUpdate {
    /// Creates one version-one update with an explicitly supplied unique ID.
    ///
    /// An absent range is an explicit durable clear. A caller cannot represent
    /// an enabled range without bounds because [`RuntimeLoopRange`] always
    /// supplies both boundaries.
    ///
    /// # Errors
    ///
    /// Refuses an identity outside the version-one project-ID domain.
    pub fn try_new(
        id: String,
        source: RuntimeLoopUpdateSource,
        loop_range: Option<RuntimeLoopRange>,
    ) -> Result<Self, RuntimeLoopUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop-update identity is invalid",
            ));
        }
        Ok(Self {
            id,
            source,
            loop_range,
        })
    }

    /// Parses one complete canonical version-one update.
    ///
    /// # Errors
    ///
    /// Refuses malformed, noncanonical, oversized, or incompatible artifacts
    /// before callers can use their loop state.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeLoopUpdateError> {
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_LOOP_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::JsonInvalid,
                "loop update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::InvalidUpdate,
                "loop update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeLoopUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::InvalidUpdate,
                "loop update shape is invalid",
                source,
            )
        })?;
        Self::from_raw(raw)
    }

    /// Returns canonical RFC 8785 JSON bytes for durable publication.
    ///
    /// # Errors
    ///
    /// Returns a serialization refusal if canonicalization unexpectedly cannot
    /// represent this already validated version-one update.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeLoopUpdateError> {
        let value = serde_json::to_value(RawUpdate::from_update(self)).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::InvalidUpdate,
                "loop update cannot be serialized",
                source,
            )
        })?;
        let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::InvalidUpdate,
                "loop update cannot be canonicalized",
                source,
            )
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_LOOP_UPDATE_BYTES {
            return Err(RuntimeLoopUpdateError::limit(
                "loop update exceeds the version-one byte capacity",
            ));
        }
        Ok(bytes)
    }

    /// Returns the unique update identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns exact Runtime source provenance.
    #[must_use]
    pub const fn source(&self) -> &RuntimeLoopUpdateSource {
        &self.source
    }

    /// Returns the complete persisted loop state requested by the update.
    #[must_use]
    pub const fn loop_range(&self) -> Option<RuntimeLoopRange> {
        self.loop_range
    }

    fn from_raw(raw: RawUpdate) -> Result<Self, RuntimeLoopUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeLoopUpdateError::invalid(
                "loop update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeLoopUpdateSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let loop_range = raw
            .loop_range
            .map(RawLoopRange::try_into_range)
            .transpose()?;
        Self::try_new(raw.id, source, loop_range)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    format: String,
    format_version: u8,
    id: String,
    source: RawSource,
    loop_range: Option<RawLoopRange>,
    extensions: BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeLoopUpdate) -> Self {
        Self {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
            id: update.id.clone(),
            source: RawSource {
                project_id: update.source.project_id.clone(),
                project_revision: update.source.project_revision,
                project_sha256: update.source.project_sha256.clone(),
                runtime_bundle_sha256: update.source.runtime_bundle_sha256.clone(),
                deployment_generation: update.source.deployment_generation,
            },
            loop_range: update.loop_range.map(RawLoopRange::from_range),
            extensions: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawLoopRange {
    start_frame: u64,
    end_frame: u64,
    enabled: bool,
}

impl RawLoopRange {
    fn from_range(range: RuntimeLoopRange) -> Self {
        Self {
            start_frame: range.start_frame,
            end_frame: range.end_frame,
            enabled: range.enabled,
        }
    }

    fn try_into_range(self) -> Result<RuntimeLoopRange, RuntimeLoopUpdateError> {
        RuntimeLoopRange::try_new(self.start_frame, self.end_frame, self.enabled)
    }
}

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeLoopUpdateErrorKind {
    /// An external filesystem operation failed.
    Io,
    /// Input was not valid strict JSON.
    JsonInvalid,
    /// Input was valid JSON but not canonical RFC 8785 bytes.
    NonCanonicalJson,
    /// Input or a requested operation is outside the version-one contract.
    InvalidUpdate,
    /// A configured byte limit was exceeded.
    LimitExceeded,
    /// The explicit Runtime output directory is invalid.
    OutputDirectoryInvalid,
    /// A retained output directory authority changed during publication.
    AuthorityChanged,
    /// Publication would replace an already existing update.
    PublicationConflict,
    /// An update may exist, but its durability cannot be established.
    PublicationUncertain,
}

/// A classified loop-update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeLoopUpdateError {
    kind: RuntimeLoopUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeLoopUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeLoopUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(RuntimeLoopUpdateErrorKind::InvalidUpdate, message)
    }

    fn limit(message: &'static str) -> Self {
        Self::new(RuntimeLoopUpdateErrorKind::LimitExceeded, message)
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeLoopUpdateErrorKind::NonCanonicalJson,
            "loop update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeLoopUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeLoopUpdateErrorKind,
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }

    fn io(message: impl Into<String>, source: io::Error) -> Self {
        Self::with_source(RuntimeLoopUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeLoopUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeLoopUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime loop-update output directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeLoopUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeLoopUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeLoopUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeLoopUpdateError::new(
                RuntimeLoopUpdateErrorKind::OutputDirectoryInvalid,
                "loop-update output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeLoopUpdateError::io("inspect loop-update output directory", source)
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeLoopUpdateError::new(
                RuntimeLoopUpdateErrorKind::OutputDirectoryInvalid,
                "loop-update output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeLoopUpdateError::io("retain loop-update output directory identity", source)
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeLoopUpdateError::io("open loop-update output directory", source)
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeLoopUpdateError::io("clone loop-update output directory", source)
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeLoopUpdateError::io("inspect retained loop-update output directory", source)
        })?;
        Self::confirm_path(path, &root_identity, &opened)?;
        Ok(Self {
            root_path: path.to_owned(),
            directory,
            root_identity,
        })
    }

    /// Returns the absolute path retained for publication.
    #[must_use]
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }

    /// Publishes one canonical update without replacing an existing entry.
    ///
    /// The returned path is meaningful only after the complete file and
    /// directory durability sequence succeeds. A later directory-sync failure
    /// is returned as [`RuntimeLoopUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(&self, update: &RuntimeLoopUpdate) -> Result<PathBuf, RuntimeLoopUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime loop-update publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeLoopUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeLoopUpdateError::io(
                        "clone retained loop-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeLoopUpdateError::io("inspect retained loop-update output directory", source)
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeLoopUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeLoopUpdateError::io("reinspect loop-update output directory", source)
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeLoopUpdateError::io("reopen loop-update output directory identity", source)
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeLoopUpdateError::new(
                RuntimeLoopUpdateErrorKind::AuthorityChanged,
                "loop-update output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeLoopUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeLoopUpdateErrorKind::PublicationConflict
            } else {
                RuntimeLoopUpdateErrorKind::Io
            };
            RuntimeLoopUpdateError::with_source(
                kind,
                "publish loop update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::PublicationUncertain,
                "loop update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeLoopUpdateError> {
        use uuid::Uuid;

        let stage_name = format!(".{name}-{}.pending", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = self
            .directory
            .open_with(&stage_name, &options)
            .map_err(|source| {
                RuntimeLoopUpdateError::io("create loop-update staging file", source)
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeLoopUpdateError::io("inspect loop-update staging file", source)
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeLoopUpdateError::new(
                        RuntimeLoopUpdateErrorKind::Io,
                        "loop-update staging entry is not a regular file",
                    ))
                }
            });
        let staged_identity = match staged_identity {
            Ok(identity) => identity,
            Err(primary) => {
                drop(file);
                return self.cleanup_windows_stage(primary, &stage_name, None);
            }
        };
        let stage_result = file
            .write_all(bytes)
            .map_err(|source| RuntimeLoopUpdateError::io("write loop-update staging file", source))
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeLoopUpdateError::io("synchronize loop-update staging file", source)
                })
            });
        drop(file);
        if let Err(primary) = stage_result {
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        if let Err(primary) = self.confirm_unchanged() {
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        if let Err(source) = rename_no_replace(
            &self.directory,
            &stage_name,
            staged_identity,
            &self.directory,
            name,
        ) {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeLoopUpdateErrorKind::PublicationConflict
            } else {
                RuntimeLoopUpdateErrorKind::Io
            };
            let primary = RuntimeLoopUpdateError::with_source(
                kind,
                "publish loop update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeLoopUpdateError::with_source(
                RuntimeLoopUpdateErrorKind::PublicationUncertain,
                "loop update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeLoopUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeLoopUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeLoopUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime loop update from a stable absolute regular
/// non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable update.
pub fn read_runtime_loop_update(path: &Path) -> Result<RuntimeLoopUpdate, RuntimeLoopUpdateError> {
    let snapshot = read_stable_absolute_file(path, MAX_RUNTIME_LOOP_UPDATE_BYTES)
        .map_err(|source| RuntimeLoopUpdateError::io("read stable loop update", source))?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeLoopUpdate::parse(&bytes)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_LOOP_UPDATE_BYTES, &evidence)
        .map_err(|source| RuntimeLoopUpdateError::io("confirm stable loop update", source))?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-loop-update.json")
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_absolute_nonaliased(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hyper-runtime-loop-update-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn source() -> RuntimeLoopUpdateSource {
        RuntimeLoopUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn range(enabled: bool) -> RuntimeLoopRange {
        RuntimeLoopRange::try_new(48_000, 192_000, enabled).unwrap()
    }

    fn update(loop_range: Option<RuntimeLoopRange>) -> RuntimeLoopUpdate {
        RuntimeLoopUpdate::try_new(UPDATE_ID.to_owned(), source(), loop_range).unwrap()
    }

    #[test]
    fn sha256_hex_is_fixed_width_lowercase() {
        let mut digest = [0_u8; 32];
        digest[0] = 0x0a;
        digest[31] = 0xff;

        assert_eq!(sha256_hex(&digest), format!("0a{}ff", "00".repeat(30)));
    }

    #[test]
    fn source_requires_exact_json_safe_runtime_provenance() {
        assert_eq!(
            RuntimeLoopUpdateSource::try_new(
                "not-an-entity-id".to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeLoopUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER + 1,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeLoopUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                PROJECT_DIGEST.to_uppercase(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeLoopUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                MAX_SAFE_INTEGER + 1,
            )
            .unwrap_err()
            .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_losslessly_represents_clear_and_disengaged_loop_states() {
        let cleared = update(None);
        let cleared_bytes = cleared.canonical_bytes().unwrap();
        assert_eq!(RuntimeLoopUpdate::parse(&cleared_bytes).unwrap(), cleared);
        let cleared_value: Value = serde_json::from_slice(&cleared_bytes).unwrap();
        assert_eq!(cleared_value["loop_range"], Value::Null);

        let disengaged = update(Some(range(false)));
        let disengaged_bytes = disengaged.canonical_bytes().unwrap();
        assert_eq!(
            RuntimeLoopUpdate::parse(&disengaged_bytes).unwrap(),
            disengaged
        );
        let parsed_range = disengaged.loop_range().unwrap();
        assert_eq!(parsed_range.start_frame(), 48_000);
        assert_eq!(parsed_range.end_frame(), 192_000);
        assert!(!parsed_range.enabled());
    }

    #[test]
    fn update_requires_canonical_nonempty_json_safe_ranges() {
        let update = update(Some(range(true)));
        let bytes = update.canonical_bytes().unwrap();
        assert_eq!(RuntimeLoopUpdate::parse(&bytes).unwrap(), update);

        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeLoopUpdate::parse(&noncanonical).unwrap_err().kind(),
            RuntimeLoopUpdateErrorKind::NonCanonicalJson
        );

        let mut inverted: Value = serde_json::from_slice(&bytes).unwrap();
        inverted["loop_range"]["end_frame"] = json!(48_000);
        let inverted = serde_json_canonicalizer::to_vec(&inverted).unwrap();
        assert_eq!(
            RuntimeLoopUpdate::parse(&inverted).unwrap_err().kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );

        let mut extension: Value = serde_json::from_slice(&bytes).unwrap();
        extension["extensions"] = json!({"unrecognized": true});
        let extension = serde_json_canonicalizer::to_vec(&extension).unwrap();
        assert_eq!(
            RuntimeLoopUpdate::parse(&extension).unwrap_err().kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );

        let mut unknown_field: Value = serde_json::from_slice(&bytes).unwrap();
        unknown_field["unexpected"] = json!(true);
        let unknown_field = serde_json_canonicalizer::to_vec(&unknown_field).unwrap();
        assert_eq!(
            RuntimeLoopUpdate::parse(&unknown_field).unwrap_err().kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );

        assert_eq!(
            RuntimeLoopRange::try_new(MAX_SAFE_INTEGER, MAX_SAFE_INTEGER, true)
                .unwrap_err()
                .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeLoopRange::try_new(MAX_SAFE_INTEGER + 1, u64::MAX, true)
                .unwrap_err()
                .kind(),
            RuntimeLoopUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn maximum_version_one_values_remain_well_within_the_byte_limit() {
        let source = RuntimeLoopUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            MAX_SAFE_INTEGER,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            MAX_SAFE_INTEGER,
        )
        .unwrap();
        let maximum = RuntimeLoopUpdate::try_new(
            UPDATE_ID.to_owned(),
            source,
            Some(RuntimeLoopRange::try_new(MAX_SAFE_INTEGER - 1, MAX_SAFE_INTEGER, false).unwrap()),
        )
        .unwrap();

        let bytes = maximum.canonical_bytes().unwrap();
        assert!(
            bytes.len() <= 768,
            "the bounded version-one schema must retain at least 256 bytes below its 1 KiB reader limit"
        );
        assert!(
            u64::try_from(bytes.len()).unwrap() <= MAX_RUNTIME_LOOP_UPDATE_BYTES,
            "the writer must never emit a reader-oversized artifact"
        );
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let update = update(Some(range(true)));
        let output = directory.publish(&update).unwrap();
        assert!(output.ends_with(format!("{UPDATE_ID}.hyper-loop-update.json")));
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(read_runtime_loop_update(&output).unwrap(), update);
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeLoopUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_directory_refuses_relative_authority() {
        let Err(error) = RuntimeLoopUpdateDirectory::open(Path::new("loop-updates")) else {
            panic!("a relative loop-update directory must be rejected");
        };
        assert_eq!(
            error.kind(),
            RuntimeLoopUpdateErrorKind::OutputDirectoryInvalid
        );
    }
}
