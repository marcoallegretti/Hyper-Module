//! Shared, bounded Runtime-to-IDE mixer-update contracts.
//!
//! Runtime emits a complete, provenance-bound mixer base state only after an
//! explicit operator request and a successful audio-cycle acknowledgement.
//! The IDE remains the sole project writer: it verifies this artifact against
//! the exact source revision and refuses local mixer divergence rather than
//! merging two live states.

#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
#[cfg(windows)]
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::Dir;
#[cfg(windows)]
use cap_std::fs::OpenOptions;
use hyper_audio_engine::{LinearGain, Pan};
#[cfg(target_os = "linux")]
use hyper_fs_authority::publish_bytes_no_replace;
use hyper_fs_authority::{
    confirm_stable_absolute_file, open_syncable_directory_ambient, read_stable_absolute_file,
    sync_directory,
};
#[cfg(windows)]
use hyper_fs_authority::{identity, remove_file_if_identity, rename_no_replace};
use hyper_playback_json::{
    LoaderLimits, MixerBaseState, MixerChannelBaseState, MixerSendBaseState,
};
use hyper_project_model::is_valid_entity_id;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-runtime-mixer-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Maximum byte size accepted for a version-one Runtime mixer update.
///
/// This matches the verified version-one mixer-document byte limit. The
/// artifact carries only complete base state, so it is materially smaller than
/// a project package while remaining bounded independently of filesystem
/// metadata.
pub const MAX_RUNTIME_MIXER_UPDATE_BYTES: u64 = 1_048_576;

/// Maximum mixer-channel count accepted by the version-one update contract.
pub const MAX_RUNTIME_MIXER_UPDATE_CHANNELS: usize = LoaderLimits::VERSION_1.max_mixer_channels;

/// Maximum total mixer-send count accepted by the version-one update contract.
pub const MAX_RUNTIME_MIXER_UPDATE_SENDS: usize = LoaderLimits::VERSION_1.max_mixer_sends;

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

/// Exact verified project and Runtime provenance for one mixer update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMixerUpdateSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeMixerUpdateSource {
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
    ) -> Result<Self, RuntimeMixerUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update source deployment generation is outside the version-one JSON domain",
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

/// A complete, append-only mixer base-state update emitted by Runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeMixerUpdate {
    id: String,
    source: RuntimeMixerUpdateSource,
    mixer_id: String,
    state: MixerBaseState,
}

impl RuntimeMixerUpdate {
    /// Creates one version-one update with an explicitly supplied unique ID.
    ///
    /// Runtime is expected to generate this ID before publication. Keeping the
    /// identity explicit makes deterministic tests and other verified Runtime
    /// frontends independent of hidden randomness authority.
    ///
    /// # Errors
    ///
    /// Refuses invalid identifiers, over-capacity state, or ambiguous stable
    /// track/send identities.
    pub fn try_new(
        id: String,
        source: RuntimeMixerUpdateSource,
        mixer_id: String,
        state: MixerBaseState,
    ) -> Result<Self, RuntimeMixerUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update identity is invalid",
            ));
        }
        if !is_valid_entity_id(&mixer_id) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer-update mixer identity is invalid",
            ));
        }
        validate_mixer_state(&state)?;
        Ok(Self {
            id,
            source,
            mixer_id,
            state,
        })
    }

    /// Parses one complete canonical version-one update.
    ///
    /// # Errors
    ///
    /// Refuses malformed, noncanonical, oversized, or incompatible artifacts
    /// before callers can use their base state.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeMixerUpdateError> {
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_MIXER_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::JsonInvalid,
                "mixer update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::InvalidUpdate,
                "mixer update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeMixerUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::InvalidUpdate,
                "mixer update shape is invalid",
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
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeMixerUpdateError> {
        let value = serde_json::to_value(RawUpdate::from_update(self)).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::InvalidUpdate,
                "mixer update cannot be serialized",
                source,
            )
        })?;
        serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::InvalidUpdate,
                "mixer update cannot be canonicalized",
                source,
            )
        })
    }

    /// Returns the unique update identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns exact Runtime source provenance.
    #[must_use]
    pub const fn source(&self) -> &RuntimeMixerUpdateSource {
        &self.source
    }

    /// Returns the exact mixer resource identity whose base state is carried.
    #[must_use]
    pub fn mixer_id(&self) -> &str {
        &self.mixer_id
    }

    /// Returns the complete stable mixer base state.
    #[must_use]
    pub const fn state(&self) -> &MixerBaseState {
        &self.state
    }

    /// Transfers the already validated complete mixer base state without a
    /// second allocation.
    #[must_use]
    pub fn into_state(self) -> MixerBaseState {
        self.state
    }

    fn from_raw(raw: RawUpdate) -> Result<Self, RuntimeMixerUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeMixerUpdateSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let state = raw.mixer.try_into_state()?;
        Self::try_new(raw.id, source, raw.mixer_id, state)
    }
}

fn validate_mixer_state(state: &MixerBaseState) -> Result<(), RuntimeMixerUpdateError> {
    if state.channels.len() > MAX_RUNTIME_MIXER_UPDATE_CHANNELS {
        return Err(RuntimeMixerUpdateError::limit(
            "mixer update exceeds the version-one channel capacity",
        ));
    }
    let mut track_ids = BTreeSet::new();
    let mut send_ids = BTreeSet::new();
    let mut total_sends = 0_usize;
    for channel in &state.channels {
        if !is_valid_entity_id(&channel.track_id) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer update channel track identity is invalid",
            ));
        }
        if !track_ids.insert(channel.track_id.as_str()) {
            return Err(RuntimeMixerUpdateError::invalid(
                "mixer update repeats a channel track identity",
            ));
        }
        total_sends = total_sends
            .checked_add(channel.sends.len())
            .ok_or_else(|| {
                RuntimeMixerUpdateError::limit("mixer update send capacity is unavailable")
            })?;
        if total_sends > MAX_RUNTIME_MIXER_UPDATE_SENDS {
            return Err(RuntimeMixerUpdateError::limit(
                "mixer update exceeds the version-one send capacity",
            ));
        }
        for send in &channel.sends {
            if !is_valid_entity_id(&send.id) {
                return Err(RuntimeMixerUpdateError::invalid(
                    "mixer update send identity is invalid",
                ));
            }
            if !send_ids.insert(send.id.as_str()) {
                return Err(RuntimeMixerUpdateError::invalid(
                    "mixer update repeats a send identity",
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    format: String,
    format_version: u8,
    id: String,
    source: RawSource,
    mixer_id: String,
    mixer: RawMixerState,
    extensions: BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeMixerUpdate) -> Self {
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
            mixer_id: update.mixer_id.clone(),
            mixer: RawMixerState::from_state(&update.state),
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
struct RawMixerState {
    channels: Vec<RawMixerChannel>,
    main_gain_millionths: u32,
    main_mute: bool,
}

impl RawMixerState {
    fn from_state(state: &MixerBaseState) -> Self {
        Self {
            channels: state
                .channels
                .iter()
                .map(RawMixerChannel::from_state)
                .collect(),
            main_gain_millionths: state.main_gain.to_millionths(),
            main_mute: state.main_muted,
        }
    }

    fn try_into_state(self) -> Result<MixerBaseState, RuntimeMixerUpdateError> {
        if self.channels.len() > MAX_RUNTIME_MIXER_UPDATE_CHANNELS {
            return Err(RuntimeMixerUpdateError::limit(
                "mixer update exceeds the version-one channel capacity",
            ));
        }
        let mut channels = Vec::new();
        channels
            .try_reserve_exact(self.channels.len())
            .map_err(|_| RuntimeMixerUpdateError::capacity())?;
        for channel in self.channels {
            channels.push(channel.try_into_state()?);
        }
        let state = MixerBaseState {
            channels,
            main_gain: LinearGain::from_millionths(self.main_gain_millionths).map_err(|_| {
                RuntimeMixerUpdateError::invalid("mixer update main gain is invalid")
            })?,
            main_muted: self.main_mute,
        };
        validate_mixer_state(&state)?;
        Ok(state)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMixerChannel {
    track_id: String,
    gain_millionths: u32,
    pan_millionths: i32,
    mute: bool,
    solo: bool,
    sends: Vec<RawMixerSend>,
}

impl RawMixerChannel {
    fn from_state(channel: &MixerChannelBaseState) -> Self {
        Self {
            track_id: channel.track_id.clone(),
            gain_millionths: channel.gain.to_millionths(),
            pan_millionths: channel.pan.to_millionths(),
            mute: channel.muted,
            solo: channel.solo,
            sends: channel.sends.iter().map(RawMixerSend::from_state).collect(),
        }
    }

    fn try_into_state(self) -> Result<MixerChannelBaseState, RuntimeMixerUpdateError> {
        let mut sends = Vec::new();
        sends
            .try_reserve_exact(self.sends.len())
            .map_err(|_| RuntimeMixerUpdateError::capacity())?;
        for send in self.sends {
            sends.push(send.try_into_state()?);
        }
        Ok(MixerChannelBaseState {
            track_id: self.track_id,
            gain: LinearGain::from_millionths(self.gain_millionths).map_err(|_| {
                RuntimeMixerUpdateError::invalid("mixer update channel gain is invalid")
            })?,
            pan: Pan::from_millionths(self.pan_millionths).map_err(|_| {
                RuntimeMixerUpdateError::invalid("mixer update channel pan is invalid")
            })?,
            muted: self.mute,
            solo: self.solo,
            sends,
        })
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawMixerSend {
    id: String,
    gain_millionths: u32,
    pan_millionths: i32,
    enabled: bool,
}

impl RawMixerSend {
    fn from_state(send: &MixerSendBaseState) -> Self {
        Self {
            id: send.id.clone(),
            gain_millionths: send.gain.to_millionths(),
            pan_millionths: send.pan.to_millionths(),
            enabled: send.enabled,
        }
    }

    fn try_into_state(self) -> Result<MixerSendBaseState, RuntimeMixerUpdateError> {
        Ok(MixerSendBaseState {
            id: self.id,
            gain: LinearGain::from_millionths(self.gain_millionths).map_err(|_| {
                RuntimeMixerUpdateError::invalid("mixer update send gain is invalid")
            })?,
            pan: Pan::from_millionths(self.pan_millionths).map_err(|_| {
                RuntimeMixerUpdateError::invalid("mixer update send pan is invalid")
            })?,
            enabled: self.enabled,
        })
    }
}

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMixerUpdateErrorKind {
    /// An external filesystem operation failed.
    Io,
    /// Input was not valid strict JSON.
    JsonInvalid,
    /// Input was valid JSON but not canonical RFC 8785 bytes.
    NonCanonicalJson,
    /// Input or a requested operation is outside the version-one contract.
    InvalidUpdate,
    /// A configured byte/count limit was exceeded.
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

/// A classified mixer-update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeMixerUpdateError {
    kind: RuntimeMixerUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeMixerUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeMixerUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(RuntimeMixerUpdateErrorKind::InvalidUpdate, message)
    }

    fn limit(message: &'static str) -> Self {
        Self::new(RuntimeMixerUpdateErrorKind::LimitExceeded, message)
    }

    fn capacity() -> Self {
        Self::new(
            RuntimeMixerUpdateErrorKind::LimitExceeded,
            "mixer update capacity is unavailable",
        )
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeMixerUpdateErrorKind::NonCanonicalJson,
            "mixer update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeMixerUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeMixerUpdateErrorKind,
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
        Self::with_source(RuntimeMixerUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeMixerUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeMixerUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime mixer-update output directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeMixerUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeMixerUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeMixerUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeMixerUpdateError::new(
                RuntimeMixerUpdateErrorKind::OutputDirectoryInvalid,
                "mixer-update output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeMixerUpdateError::io("inspect mixer-update output directory", source)
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeMixerUpdateError::new(
                RuntimeMixerUpdateErrorKind::OutputDirectoryInvalid,
                "mixer-update output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeMixerUpdateError::io("retain mixer-update output directory identity", source)
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeMixerUpdateError::io("open mixer-update output directory", source)
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeMixerUpdateError::io("clone mixer-update output directory", source)
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeMixerUpdateError::io("inspect retained mixer-update output directory", source)
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
    /// is returned as [`RuntimeMixerUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(&self, update: &RuntimeMixerUpdate) -> Result<PathBuf, RuntimeMixerUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime mixer-update publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeMixerUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeMixerUpdateError::io(
                        "clone retained mixer-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeMixerUpdateError::io("inspect retained mixer-update output directory", source)
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeMixerUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeMixerUpdateError::io("reinspect mixer-update output directory", source)
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeMixerUpdateError::io("reopen mixer-update output directory identity", source)
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeMixerUpdateError::new(
                RuntimeMixerUpdateErrorKind::AuthorityChanged,
                "mixer-update output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeMixerUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeMixerUpdateErrorKind::PublicationConflict
            } else {
                RuntimeMixerUpdateErrorKind::Io
            };
            RuntimeMixerUpdateError::with_source(
                kind,
                "publish mixer update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::PublicationUncertain,
                "mixer update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeMixerUpdateError> {
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
                RuntimeMixerUpdateError::io("create mixer-update staging file", source)
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeMixerUpdateError::io("inspect mixer-update staging file", source)
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeMixerUpdateError::new(
                        RuntimeMixerUpdateErrorKind::Io,
                        "mixer-update staging entry is not a regular file",
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
                RuntimeMixerUpdateError::io("write mixer-update staging file", source)
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeMixerUpdateError::io("synchronize mixer-update staging file", source)
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
                RuntimeMixerUpdateErrorKind::PublicationConflict
            } else {
                RuntimeMixerUpdateErrorKind::Io
            };
            let primary = RuntimeMixerUpdateError::with_source(
                kind,
                "publish mixer update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeMixerUpdateError::with_source(
                RuntimeMixerUpdateErrorKind::PublicationUncertain,
                "mixer update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeMixerUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeMixerUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeMixerUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime mixer update from a stable absolute regular
/// non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable update.
pub fn read_runtime_mixer_update(
    path: &Path,
) -> Result<RuntimeMixerUpdate, RuntimeMixerUpdateError> {
    let snapshot = read_stable_absolute_file(path, MAX_RUNTIME_MIXER_UPDATE_BYTES)
        .map_err(|source| RuntimeMixerUpdateError::io("read stable mixer update", source))?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeMixerUpdate::parse(&bytes)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_MIXER_UPDATE_BYTES, &evidence)
        .map_err(|source| RuntimeMixerUpdateError::io("confirm stable mixer update", source))?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-mixer-update.json")
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
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const TRACK_A: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const TRACK_B: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const SEND_A: &str = "018f47a2-6b2d-7c11-8a44-001122334459";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334460";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hyper-runtime-mixer-update-{}", Uuid::new_v4()));
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

    fn source() -> RuntimeMixerUpdateSource {
        RuntimeMixerUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn state() -> MixerBaseState {
        MixerBaseState {
            channels: vec![
                MixerChannelBaseState {
                    track_id: TRACK_A.to_owned(),
                    gain: LinearGain::from_millionths(725_000).unwrap(),
                    pan: Pan::from_millionths(-125_000).unwrap(),
                    muted: false,
                    solo: true,
                    sends: vec![MixerSendBaseState {
                        id: SEND_A.to_owned(),
                        gain: LinearGain::from_millionths(500_000).unwrap(),
                        pan: Pan::from_millionths(250_000).unwrap(),
                        enabled: true,
                    }],
                },
                MixerChannelBaseState {
                    track_id: TRACK_B.to_owned(),
                    gain: LinearGain::from_millionths(1_125_000).unwrap(),
                    pan: Pan::from_millionths(0).unwrap(),
                    muted: true,
                    solo: false,
                    sends: Vec::new(),
                },
            ],
            main_gain: LinearGain::from_millionths(900_000).unwrap(),
            main_muted: false,
        }
    }

    fn update() -> RuntimeMixerUpdate {
        RuntimeMixerUpdate::try_new(UPDATE_ID.to_owned(), source(), MIXER_ID.to_owned(), state())
            .unwrap()
    }

    #[test]
    fn sha256_hex_is_fixed_width_lowercase() {
        let mut digest = [0_u8; 32];
        digest[0] = 0x0a;
        digest[31] = 0xff;

        assert_eq!(sha256_hex(&digest), format!("0a{}ff", "00".repeat(30)));
    }

    #[test]
    fn update_requires_canonical_complete_unambiguous_state() {
        let update = update();
        let bytes = update.canonical_bytes().unwrap();
        assert_eq!(RuntimeMixerUpdate::parse(&bytes).unwrap(), update);

        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeMixerUpdate::parse(&noncanonical).unwrap_err().kind(),
            RuntimeMixerUpdateErrorKind::NonCanonicalJson
        );

        let mut duplicate_track: Value = serde_json::from_slice(&bytes).unwrap();
        duplicate_track["mixer"]["channels"][1]["track_id"] = json!(TRACK_A);
        let duplicate_track = serde_json_canonicalizer::to_vec(&duplicate_track).unwrap();
        assert_eq!(
            RuntimeMixerUpdate::parse(&duplicate_track)
                .unwrap_err()
                .kind(),
            RuntimeMixerUpdateErrorKind::InvalidUpdate
        );

        let mut invalid_gain: Value = serde_json::from_slice(&bytes).unwrap();
        invalid_gain["mixer"]["channels"][0]["gain_millionths"] = json!(2_000_001);
        let invalid_gain = serde_json_canonicalizer::to_vec(&invalid_gain).unwrap();
        assert_eq!(
            RuntimeMixerUpdate::parse(&invalid_gain).unwrap_err().kind(),
            RuntimeMixerUpdateErrorKind::InvalidUpdate
        );

        let mut extension: Value = serde_json::from_slice(&bytes).unwrap();
        extension["extensions"] = json!({"unrecognized": true});
        let extension = serde_json_canonicalizer::to_vec(&extension).unwrap();
        assert_eq!(
            RuntimeMixerUpdate::parse(&extension).unwrap_err().kind(),
            RuntimeMixerUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeMixerUpdateDirectory::open(temporary.path()).unwrap();
        let update = update();
        let output = directory.publish(&update).unwrap();
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(read_runtime_mixer_update(&output).unwrap(), update);
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeMixerUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_refuses_over_capacity_and_ambiguous_identities() {
        assert_eq!(
            RuntimeMixerUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER + 1,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeMixerUpdateErrorKind::InvalidUpdate
        );

        let mut over_capacity = state();
        over_capacity.channels = (0..=MAX_RUNTIME_MIXER_UPDATE_CHANNELS)
            .map(|index| MixerChannelBaseState {
                track_id: format!("018f47a2-6b2d-7c11-8a44-{index:012x}"),
                gain: LinearGain::UNITY,
                pan: Pan::CENTER,
                muted: false,
                solo: false,
                sends: Vec::new(),
            })
            .collect();
        assert_eq!(
            RuntimeMixerUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                MIXER_ID.to_owned(),
                over_capacity,
            )
            .unwrap_err()
            .kind(),
            RuntimeMixerUpdateErrorKind::LimitExceeded
        );

        let mut duplicate_send = state();
        duplicate_send.channels[1].sends.push(MixerSendBaseState {
            id: SEND_A.to_owned(),
            gain: LinearGain::UNITY,
            pan: Pan::CENTER,
            enabled: true,
        });
        assert_eq!(
            RuntimeMixerUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                MIXER_ID.to_owned(),
                duplicate_send,
            )
            .unwrap_err()
            .kind(),
            RuntimeMixerUpdateErrorKind::InvalidUpdate
        );
    }
}
