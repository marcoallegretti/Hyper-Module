//! Shared, bounded Runtime-to-IDE finalized-recording handback contracts.
//!
//! Runtime may publish one complete recording handback only after it owns a
//! finalized capture and the shared recording authority has produced a
//! verified descriptor. The artifact binds that handoff to the exact verified
//! Runtime deployment that created it. The IDE remains the sole project
//! writer: it must validate source provenance before asking project storage to
//! import the referenced media.

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
#[cfg(test)]
use hyper_recording_metadata::MAX_FINALIZED_RECORDING_BYTE_LENGTH;
use hyper_recording_metadata::{FinalizedRecordingHandoff, MAX_FINALIZED_RECORDING_HANDOFF_BYTES};
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-runtime-recording-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const OUTER_ENVELOPE_MAXIMUM_BYTES: u64 = 512;

/// Maximum byte size accepted for one version-one Runtime recording update.
///
/// The embedded finalized-recording handoff retains its established two-MiB
/// boundary. The outer envelope adds a 36-byte update ID, a 36-byte source
/// project identity, two 64-byte digests, and two JSON-safe integers. Its
/// maximum canonical overhead is below 448 bytes; the remaining 64 bytes keep
/// that wire bound explicit without accepting an unbounded wrapper around the
/// handoff.
pub const MAX_RUNTIME_RECORDING_UPDATE_BYTES: u64 =
    MAX_FINALIZED_RECORDING_HANDOFF_BYTES as u64 + OUTER_ENVELOPE_MAXIMUM_BYTES;

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

/// Exact verified project and Runtime provenance for one recording update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeRecordingUpdateSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeRecordingUpdateSource {
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
    ) -> Result<Self, RuntimeRecordingUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update source deployment generation is outside the version-one JSON domain",
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

/// A complete, append-only finalized-recording handback emitted by Runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct RuntimeRecordingUpdate {
    id: String,
    source: RuntimeRecordingUpdateSource,
    handoff: FinalizedRecordingHandoff,
}

impl RuntimeRecordingUpdate {
    /// Creates one version-one update with an explicitly supplied unique ID.
    ///
    /// The shared finalized-recording handoff remains intact inside this
    /// envelope. Its metadata must identify the exact same project as the
    /// verified Runtime source; the handoff never selects a destination.
    ///
    /// # Errors
    ///
    /// Refuses an invalid update ID or a recording handoff from another
    /// source project.
    pub fn try_new(
        id: String,
        source: RuntimeRecordingUpdateSource,
        handoff: FinalizedRecordingHandoff,
    ) -> Result<Self, RuntimeRecordingUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update identity is invalid",
            ));
        }
        if handoff.metadata().project_id() != source.project_id() {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update metadata project identity does not match the Runtime source",
            ));
        }
        Ok(Self {
            id,
            source,
            handoff,
        })
    }

    /// Parses one complete canonical version-one recording update.
    ///
    /// # Errors
    ///
    /// Refuses malformed, noncanonical, oversized, or incompatible artifacts
    /// before callers can use their recording handoff.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeRecordingUpdateError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_RECORDING_UPDATE_BYTES {
            return Err(RuntimeRecordingUpdateError::limit(
                "recording update exceeds the version-one byte capacity",
            ));
        }
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_RECORDING_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::JsonInvalid,
                "recording update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                "recording update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeRecordingUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                "recording update shape is invalid",
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
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeRecordingUpdateError> {
        let handoff: Value =
            serde_json::from_slice(self.handoff.canonical_bytes()).map_err(|source| {
                RuntimeRecordingUpdateError::with_source(
                    RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                    "recording update retained handoff cannot be decoded",
                    source,
                )
            })?;
        let value =
            serde_json::to_value(RawUpdate::from_update(self, handoff)).map_err(|source| {
                RuntimeRecordingUpdateError::with_source(
                    RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                    "recording update cannot be serialized",
                    source,
                )
            })?;
        let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                "recording update cannot be canonicalized",
                source,
            )
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_RECORDING_UPDATE_BYTES {
            return Err(RuntimeRecordingUpdateError::limit(
                "recording update exceeds the version-one byte capacity",
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
    pub const fn source(&self) -> &RuntimeRecordingUpdateSource {
        &self.source
    }

    /// Returns the complete validated recording handoff.
    #[must_use]
    pub const fn handoff(&self) -> &FinalizedRecordingHandoff {
        &self.handoff
    }

    /// Transfers the complete validated recording handoff.
    #[must_use]
    pub fn into_handoff(self) -> FinalizedRecordingHandoff {
        self.handoff
    }

    fn from_raw(raw: RawUpdate) -> Result<Self, RuntimeRecordingUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeRecordingUpdateError::invalid(
                "recording update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeRecordingUpdateSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let handoff_bytes = serde_json_canonicalizer::to_vec(&raw.handoff).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                "recording update handoff cannot be canonicalized",
                source,
            )
        })?;
        let handoff = FinalizedRecordingHandoff::parse(&handoff_bytes).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::InvalidUpdate,
                "recording update handoff is invalid",
                source,
            )
        })?;
        Self::try_new(raw.id, source, handoff)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    format: String,
    format_version: u8,
    id: String,
    source: RawSource,
    handoff: Value,
    extensions: BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeRecordingUpdate, handoff: Value) -> Self {
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
            handoff,
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

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeRecordingUpdateErrorKind {
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

/// A classified Runtime recording-update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeRecordingUpdateError {
    kind: RuntimeRecordingUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeRecordingUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeRecordingUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(RuntimeRecordingUpdateErrorKind::InvalidUpdate, message)
    }

    fn limit(message: &'static str) -> Self {
        Self::new(RuntimeRecordingUpdateErrorKind::LimitExceeded, message)
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeRecordingUpdateErrorKind::NonCanonicalJson,
            "recording update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeRecordingUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeRecordingUpdateErrorKind,
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
        Self::with_source(RuntimeRecordingUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeRecordingUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeRecordingUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime recording-update output
/// directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeRecordingUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeRecordingUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeRecordingUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeRecordingUpdateError::new(
                RuntimeRecordingUpdateErrorKind::OutputDirectoryInvalid,
                "recording-update output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeRecordingUpdateError::io("inspect recording-update output directory", source)
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeRecordingUpdateError::new(
                RuntimeRecordingUpdateErrorKind::OutputDirectoryInvalid,
                "recording-update output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeRecordingUpdateError::io(
                "retain recording-update output directory identity",
                source,
            )
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeRecordingUpdateError::io("open recording-update output directory", source)
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeRecordingUpdateError::io(
                        "clone recording-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeRecordingUpdateError::io(
                "inspect retained recording-update output directory",
                source,
            )
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
    /// is returned as [`RuntimeRecordingUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(
        &self,
        update: &RuntimeRecordingUpdate,
    ) -> Result<PathBuf, RuntimeRecordingUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime recording-update publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeRecordingUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeRecordingUpdateError::io(
                        "clone retained recording-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeRecordingUpdateError::io(
                "inspect retained recording-update output directory",
                source,
            )
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeRecordingUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeRecordingUpdateError::io("reinspect recording-update output directory", source)
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeRecordingUpdateError::io(
                "reopen recording-update output directory identity",
                source,
            )
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeRecordingUpdateError::new(
                RuntimeRecordingUpdateErrorKind::AuthorityChanged,
                "recording-update output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeRecordingUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeRecordingUpdateErrorKind::PublicationConflict
            } else {
                RuntimeRecordingUpdateErrorKind::Io
            };
            RuntimeRecordingUpdateError::with_source(
                kind,
                "publish recording update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::PublicationUncertain,
                "recording update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeRecordingUpdateError> {
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
                RuntimeRecordingUpdateError::io("create recording-update staging file", source)
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeRecordingUpdateError::io("inspect recording-update staging file", source)
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeRecordingUpdateError::new(
                        RuntimeRecordingUpdateErrorKind::Io,
                        "recording-update staging entry is not a regular file",
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
            .map_err(|source| {
                RuntimeRecordingUpdateError::io("write recording-update staging file", source)
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeRecordingUpdateError::io(
                        "synchronize recording-update staging file",
                        source,
                    )
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
                RuntimeRecordingUpdateErrorKind::PublicationConflict
            } else {
                RuntimeRecordingUpdateErrorKind::Io
            };
            let primary = RuntimeRecordingUpdateError::with_source(
                kind,
                "publish recording update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeRecordingUpdateError::with_source(
                RuntimeRecordingUpdateErrorKind::PublicationUncertain,
                "recording update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeRecordingUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeRecordingUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeRecordingUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime recording update from a stable absolute regular
/// non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable update.
pub fn read_runtime_recording_update(
    path: &Path,
) -> Result<RuntimeRecordingUpdate, RuntimeRecordingUpdateError> {
    let snapshot =
        read_stable_absolute_file(path, MAX_RUNTIME_RECORDING_UPDATE_BYTES).map_err(|source| {
            RuntimeRecordingUpdateError::io("read stable recording update", source)
        })?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeRecordingUpdate::parse(&bytes)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_RECORDING_UPDATE_BYTES, &evidence).map_err(
        |source| RuntimeRecordingUpdateError::io("confirm stable recording update", source),
    )?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-recording-update.json")
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
    use super::*;

    #[cfg(unix)]
    const ABSOLUTE_PREFIX: &str = "/";
    #[cfg(windows)]
    const ABSOLUTE_PREFIX: &str = "C:\\";

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const OTHER_PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334466";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334477";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-recording-update-{}",
                uuid::Uuid::new_v4()
            ));
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

    fn source() -> RuntimeRecordingUpdateSource {
        RuntimeRecordingUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn handoff_for(project_id: &str) -> FinalizedRecordingHandoff {
        let metadata = serde_json::json!({
            "asset_id": "018f47a2-6b2d-7c11-8a44-001122334478",
            "capture_kind": "new",
            "captured_at_utc": "2026-09-04T12:00:00.000Z",
            "channels": 2,
            "extensions": {},
            "format": "hyper-recording-metadata",
            "format_version": 1,
            "frame_count": 48_000,
            "id": "018f47a2-6b2d-7c11-8a44-001122334479",
            "project_id": project_id,
            "sample_rate_hz": 48_000,
            "source_input_indices": [0, 1],
            "timeline_start_tick": 0,
            "track_id": "018f47a2-6b2d-7c11-8a44-001122334480",
        });
        let bytes = serde_json_canonicalizer::to_vec(&serde_json::json!({
            "extensions": {},
            "format": "hyper-finalized-recording-handoff",
            "format_version": 1,
            "metadata": metadata,
            "recording": {
                "byte_length": 96,
                "media_type": "audio/wav",
                "path": format!("{ABSOLUTE_PREFIX}recorded-take.wav"),
                "sha256": "c".repeat(64),
            },
        }))
        .unwrap();
        FinalizedRecordingHandoff::parse(&bytes).unwrap()
    }

    fn update() -> RuntimeRecordingUpdate {
        RuntimeRecordingUpdate::try_new(UPDATE_ID.to_owned(), source(), handoff_for(PROJECT_ID))
            .unwrap()
    }

    #[test]
    fn sha256_hex_is_fixed_width_lowercase() {
        assert_eq!(sha256_hex(&[0xab; 32]), "ab".repeat(32));
    }

    #[test]
    fn source_requires_exact_json_safe_runtime_provenance() {
        assert_eq!(
            RuntimeRecordingUpdateSource::try_new(
                "not-an-entity-id".to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeRecordingUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER + 1,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeRecordingUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                PROJECT_DIGEST.to_uppercase(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeRecordingUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                MAX_SAFE_INTEGER + 1,
            )
            .unwrap_err()
            .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_round_trips_the_complete_shared_recording_handoff() {
        let update = update();
        let bytes = update.canonical_bytes().unwrap();

        assert_eq!(RuntimeRecordingUpdate::parse(&bytes).unwrap(), update);
        assert_eq!(
            update.handoff().canonical_bytes(),
            handoff_for(PROJECT_ID).canonical_bytes()
        );
        assert_eq!(update.handoff().recording_byte_length(), 96);
        assert_eq!(
            update.handoff().metadata().track_id(),
            "018f47a2-6b2d-7c11-8a44-001122334480"
        );
    }

    #[test]
    fn update_binds_the_embedded_recording_to_the_source_project() {
        assert_eq!(
            RuntimeRecordingUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                handoff_for(OTHER_PROJECT_ID),
            )
            .unwrap_err()
            .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );

        let mut value: Value =
            serde_json::from_slice(&update().canonical_bytes().unwrap()).unwrap();
        value["handoff"]["metadata"]["project_id"] = serde_json::json!(OTHER_PROJECT_ID);
        let bytes = serde_json_canonicalizer::to_vec(&value).unwrap();
        assert_eq!(
            RuntimeRecordingUpdate::parse(&bytes).unwrap_err().kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_refuses_noncanonical_and_augmented_artifacts() {
        let bytes = update().canonical_bytes().unwrap();
        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeRecordingUpdate::parse(&noncanonical)
                .unwrap_err()
                .kind(),
            RuntimeRecordingUpdateErrorKind::NonCanonicalJson
        );

        let mut extension: Value = serde_json::from_slice(&bytes).unwrap();
        extension["extensions"] = serde_json::json!({"unrecognized": true});
        let extension = serde_json_canonicalizer::to_vec(&extension).unwrap();
        assert_eq!(
            RuntimeRecordingUpdate::parse(&extension)
                .unwrap_err()
                .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );

        let mut injected: Value = serde_json::from_slice(&bytes).unwrap();
        injected["destination_project"] = serde_json::json!("must-not-be-accepted");
        let injected = serde_json_canonicalizer::to_vec(&injected).unwrap();
        assert_eq!(
            RuntimeRecordingUpdate::parse(&injected).unwrap_err().kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_refuses_handoff_recording_lengths_outside_the_shared_contract() {
        let bytes = update().canonical_bytes().unwrap();

        let mut empty: Value = serde_json::from_slice(&bytes).unwrap();
        empty["handoff"]["recording"]["byte_length"] = serde_json::json!(0);
        let empty = serde_json_canonicalizer::to_vec(&empty).unwrap();
        assert_eq!(
            RuntimeRecordingUpdate::parse(&empty).unwrap_err().kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );

        let mut oversized: Value = serde_json::from_slice(&bytes).unwrap();
        oversized["handoff"]["recording"]["byte_length"] =
            serde_json::json!(MAX_FINALIZED_RECORDING_BYTE_LENGTH + 1);
        let oversized = serde_json_canonicalizer::to_vec(&oversized).unwrap();
        assert_eq!(
            RuntimeRecordingUpdate::parse(&oversized)
                .unwrap_err()
                .kind(),
            RuntimeRecordingUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_writer_stays_within_the_reader_boundary() {
        let bytes = update().canonical_bytes().unwrap();
        assert!(
            u64::try_from(bytes.len()).unwrap() <= MAX_RUNTIME_RECORDING_UPDATE_BYTES,
            "the writer must never emit a reader-oversized artifact"
        );
        assert_eq!(
            MAX_RUNTIME_RECORDING_UPDATE_BYTES,
            MAX_FINALIZED_RECORDING_HANDOFF_BYTES as u64 + OUTER_ENVELOPE_MAXIMUM_BYTES
        );
    }

    #[test]
    fn maximum_source_envelope_stays_inside_the_fixed_headroom() {
        let source = RuntimeRecordingUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            MAX_SAFE_INTEGER,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            MAX_SAFE_INTEGER,
        )
        .unwrap();
        let handoff = handoff_for(PROJECT_ID);
        let handoff_bytes = handoff.canonical_bytes().len();
        let update =
            RuntimeRecordingUpdate::try_new(UPDATE_ID.to_owned(), source, handoff).unwrap();
        let update_bytes = update.canonical_bytes().unwrap();
        let overhead = update_bytes.len() - handoff_bytes;

        assert!(
            overhead <= 448,
            "maximum version-one source values must retain the documented envelope bound"
        );
        assert!(
            u64::try_from(overhead).unwrap() <= OUTER_ENVELOPE_MAXIMUM_BYTES,
            "the 512-byte reader headroom must cover every outer-envelope field"
        );
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeRecordingUpdateDirectory::open(temporary.path()).unwrap();
        let update = update();
        let output = directory.publish(&update).unwrap();

        assert!(output.ends_with(format!("{UPDATE_ID}.hyper-recording-update.json")));
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(read_runtime_recording_update(&output).unwrap(), update);
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeRecordingUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_directory_refuses_relative_authority() {
        let Err(error) = RuntimeRecordingUpdateDirectory::open(Path::new("recording-updates"))
        else {
            panic!("a relative recording-update directory must be rejected");
        };
        assert_eq!(
            error.kind(),
            RuntimeRecordingUpdateErrorKind::OutputDirectoryInvalid
        );
    }
}
