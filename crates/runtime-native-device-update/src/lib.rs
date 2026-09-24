//! Shared, bounded Runtime-to-IDE native-device state handback contracts.
//!
//! Runtime emits one complete, provenance-bound state update for one existing
//! device-chain resource only after an explicit operator request and a
//! successful audio-cycle acknowledgement. The artifact carries no runtime
//! handles, slots, extensions, or structural edits. The IDE remains the sole
//! project writer and may replace only the exact source chain after proving
//! that its current topology and source bytes have not diverged.

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
#[cfg(target_os = "linux")]
use hyper_fs_authority::publish_bytes_no_replace;
use hyper_fs_authority::{
    confirm_stable_absolute_file, open_syncable_directory_ambient, read_stable_absolute_file,
    sync_directory,
};
#[cfg(windows)]
use hyper_fs_authority::{identity, remove_file_if_identity, rename_no_replace};
use hyper_native_device::{
    MAX_DEVICES_PER_CHAIN, NATIVE_COMPRESSOR_DEVICE_ID, NATIVE_COMPRESSOR_DSP_VERSION,
    NATIVE_COMPRESSOR_STATE_VERSION, NATIVE_FILTER_DEVICE_ID, NATIVE_FILTER_DSP_VERSION,
    NATIVE_FILTER_STATE_VERSION, NativeDeviceInstance, NativeDeviceState,
};
use hyper_project_model::is_valid_entity_id;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-runtime-native-device-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Maximum byte size accepted for one version-one native-device update.
///
/// Version one permits at most eight devices, each with a fixed supported
/// state schema and fixed-width identities. The accompanying maximum-value
/// test makes this deliberately small bound part of the wire contract rather
/// than inheriting the much larger project-document limit.
pub const MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES: u64 = 4_096;

/// Maximum device count accepted by one version-one native-device update.
pub const MAX_RUNTIME_NATIVE_DEVICE_UPDATE_DEVICES: usize = MAX_DEVICES_PER_CHAIN;

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

/// Exact verified project and Runtime provenance for one native-device update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNativeDeviceUpdateSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeNativeDeviceUpdateSource {
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
    ) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source deployment generation is outside the version-one JSON domain",
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

/// One ordered native-device state record in a Runtime handback.
///
/// The record binds live state to the existing device instance identity and
/// exact supported type/version tuple. It deliberately has no extension field:
/// an import must preserve extensions from the verified source chain instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNativeDeviceUpdateDevice {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeDeviceState,
}

impl RuntimeNativeDeviceUpdateDevice {
    /// Creates one validated state record for a supported native device.
    ///
    /// # Errors
    ///
    /// Refuses an invalid instance identity, mismatched type/version/state
    /// tuple, or a state outside the native-device domain.
    pub fn try_new(
        id: String,
        device_id: String,
        state_version: u16,
        dsp_version: u16,
        enabled: bool,
        state: NativeDeviceState,
    ) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        validate_device_record(&id, &device_id, state_version, dsp_version, enabled, state)?;
        Ok(Self {
            id,
            device_id,
            state_version,
            dsp_version,
            enabled,
            state,
        })
    }

    /// Returns the stable source instance identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the supported native device identity.
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Returns the state-document version bound into this update.
    #[must_use]
    pub const fn state_version(&self) -> u16 {
        self.state_version
    }

    /// Returns the DSP implementation version bound into this update.
    #[must_use]
    pub const fn dsp_version(&self) -> u16 {
        self.dsp_version
    }

    /// Returns whether the device was enabled in the acknowledged Runtime state.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the validated supported native-device state.
    #[must_use]
    pub const fn state(&self) -> NativeDeviceState {
        self.state
    }
}

/// A complete, append-only state update for exactly one existing device chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeNativeDeviceUpdate {
    id: String,
    source: RuntimeNativeDeviceUpdateSource,
    chain_id: String,
    track_id: String,
    source_chain_sha256: String,
    devices: Vec<RuntimeNativeDeviceUpdateDevice>,
}

impl RuntimeNativeDeviceUpdate {
    /// Creates one version-one update with an explicitly supplied unique ID.
    ///
    /// Runtime must generate this ID before durable publication. The update
    /// has one nonempty ordered device list and cannot express a structural
    /// chain change.
    ///
    /// # Errors
    ///
    /// Refuses invalid identities or digests, an empty/over-capacity device
    /// list, or repeated device identities.
    pub fn try_new(
        id: String,
        source: RuntimeNativeDeviceUpdateSource,
        chain_id: String,
        track_id: String,
        source_chain_sha256: String,
        devices: Vec<RuntimeNativeDeviceUpdateDevice>,
    ) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update identity is invalid",
            ));
        }
        if !is_valid_entity_id(&chain_id) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update chain identity is invalid",
            ));
        }
        if !is_valid_entity_id(&track_id) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update track identity is invalid",
            ));
        }
        if !valid_sha256(&source_chain_sha256) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update source chain digest is invalid",
            ));
        }
        validate_devices(&devices)?;
        Ok(Self {
            id,
            source,
            chain_id,
            track_id,
            source_chain_sha256,
            devices,
        })
    }

    /// Parses one complete canonical version-one update.
    ///
    /// # Errors
    ///
    /// Refuses malformed, noncanonical, oversized, or incompatible artifacts
    /// before callers can use the state records.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES {
            return Err(RuntimeNativeDeviceUpdateError::limit(
                "native-device update exceeds the version-one byte capacity",
            ));
        }
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::JsonInvalid,
                "native-device update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate,
                "native-device update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeNativeDeviceUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate,
                "native-device update shape is invalid",
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
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeNativeDeviceUpdateError> {
        let value = serde_json::to_value(RawUpdate::from_update(self)).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate,
                "native-device update cannot be serialized",
                source,
            )
        })?;
        let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate,
                "native-device update cannot be canonicalized",
                source,
            )
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES {
            return Err(RuntimeNativeDeviceUpdateError::limit(
                "native-device update exceeds the version-one byte capacity",
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
    pub const fn source(&self) -> &RuntimeNativeDeviceUpdateSource {
        &self.source
    }

    /// Returns the exact device-chain resource identity whose state is carried.
    #[must_use]
    pub fn chain_id(&self) -> &str {
        &self.chain_id
    }

    /// Returns the exact owning track identity from the verified source chain.
    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    /// Returns the descriptor digest of the exact source chain resource.
    #[must_use]
    pub fn source_chain_sha256(&self) -> &str {
        &self.source_chain_sha256
    }

    /// Returns the ordered complete state records for the source chain.
    #[must_use]
    pub fn devices(&self) -> &[RuntimeNativeDeviceUpdateDevice] {
        &self.devices
    }

    /// Transfers the validated complete state records without a second allocation.
    #[must_use]
    pub fn into_devices(self) -> Vec<RuntimeNativeDeviceUpdateDevice> {
        self.devices
    }

    fn from_raw(raw: RawUpdate) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeNativeDeviceUpdateSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(raw.devices.len())
            .map_err(|_| RuntimeNativeDeviceUpdateError::capacity())?;
        for device in raw.devices {
            devices.push(device.try_into_device()?);
        }
        Self::try_new(
            raw.id,
            source,
            raw.chain_id,
            raw.track_id,
            raw.source_chain_sha256,
            devices,
        )
    }
}

fn validate_device_record(
    id: &str,
    device_id: &str,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeDeviceState,
) -> Result<(), RuntimeNativeDeviceUpdateError> {
    if !is_valid_entity_id(id) {
        return Err(RuntimeNativeDeviceUpdateError::invalid(
            "native-device update device identity is invalid",
        ));
    }
    let validation = match state {
        NativeDeviceState::Filter(filter) => {
            if device_id != NATIVE_FILTER_DEVICE_ID
                || state_version != NATIVE_FILTER_STATE_VERSION
                || dsp_version != NATIVE_FILTER_DSP_VERSION
            {
                return Err(RuntimeNativeDeviceUpdateError::invalid(
                    "native-device update filter type or version is unsupported",
                ));
            }
            NativeDeviceInstance::try_new_filter(id.to_owned(), enabled, filter).map(|_| ())
        }
        NativeDeviceState::Compressor(compressor) => {
            if device_id != NATIVE_COMPRESSOR_DEVICE_ID
                || state_version != NATIVE_COMPRESSOR_STATE_VERSION
                || dsp_version != NATIVE_COMPRESSOR_DSP_VERSION
            {
                return Err(RuntimeNativeDeviceUpdateError::invalid(
                    "native-device update compressor type or version is unsupported",
                ));
            }
            NativeDeviceInstance::try_new_compressor(id.to_owned(), enabled, compressor).map(|_| ())
        }
    };
    validation.map_err(|source| {
        RuntimeNativeDeviceUpdateError::with_source(
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate,
            "native-device update state is outside the supported device domain",
            source,
        )
    })
}

fn validate_devices(
    devices: &[RuntimeNativeDeviceUpdateDevice],
) -> Result<(), RuntimeNativeDeviceUpdateError> {
    if devices.is_empty() {
        return Err(RuntimeNativeDeviceUpdateError::invalid(
            "native-device update must contain at least one device",
        ));
    }
    if devices.len() > MAX_RUNTIME_NATIVE_DEVICE_UPDATE_DEVICES {
        return Err(RuntimeNativeDeviceUpdateError::limit(
            "native-device update exceeds the version-one device capacity",
        ));
    }
    let mut ids = BTreeSet::new();
    for device in devices {
        if !ids.insert(device.id.as_str()) {
            return Err(RuntimeNativeDeviceUpdateError::invalid(
                "native-device update repeats a device identity",
            ));
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
    chain_id: String,
    track_id: String,
    source_chain_sha256: String,
    devices: Vec<RawDevice>,
    extensions: BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeNativeDeviceUpdate) -> Self {
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
            chain_id: update.chain_id.clone(),
            track_id: update.track_id.clone(),
            source_chain_sha256: update.source_chain_sha256.clone(),
            devices: update.devices.iter().map(RawDevice::from_device).collect(),
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
struct RawDevice {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeDeviceState,
}

impl RawDevice {
    fn from_device(device: &RuntimeNativeDeviceUpdateDevice) -> Self {
        Self {
            id: device.id.clone(),
            device_id: device.device_id.clone(),
            state_version: device.state_version,
            dsp_version: device.dsp_version,
            enabled: device.enabled,
            state: device.state,
        }
    }

    fn try_into_device(
        self,
    ) -> Result<RuntimeNativeDeviceUpdateDevice, RuntimeNativeDeviceUpdateError> {
        RuntimeNativeDeviceUpdateDevice::try_new(
            self.id,
            self.device_id,
            self.state_version,
            self.dsp_version,
            self.enabled,
            self.state,
        )
    }
}

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeNativeDeviceUpdateErrorKind {
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

/// A classified native-device-update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeNativeDeviceUpdateError {
    kind: RuntimeNativeDeviceUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeNativeDeviceUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeNativeDeviceUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate, message)
    }

    fn limit(message: &'static str) -> Self {
        Self::new(RuntimeNativeDeviceUpdateErrorKind::LimitExceeded, message)
    }

    fn capacity() -> Self {
        Self::new(
            RuntimeNativeDeviceUpdateErrorKind::LimitExceeded,
            "native-device update capacity is unavailable",
        )
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeNativeDeviceUpdateErrorKind::NonCanonicalJson,
            "native-device update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeNativeDeviceUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeNativeDeviceUpdateErrorKind,
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
        Self::with_source(RuntimeNativeDeviceUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeNativeDeviceUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeNativeDeviceUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime native-device-update output directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeNativeDeviceUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeNativeDeviceUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeNativeDeviceUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeNativeDeviceUpdateError::new(
                RuntimeNativeDeviceUpdateErrorKind::OutputDirectoryInvalid,
                "native-device-update output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "inspect native-device-update output directory",
                source,
            )
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeNativeDeviceUpdateError::new(
                RuntimeNativeDeviceUpdateErrorKind::OutputDirectoryInvalid,
                "native-device-update output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "retain native-device-update output directory identity",
                source,
            )
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeNativeDeviceUpdateError::io("open native-device-update output directory", source)
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeNativeDeviceUpdateError::io(
                        "clone native-device-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "inspect retained native-device-update output directory",
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
    /// is returned as [`RuntimeNativeDeviceUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(
        &self,
        update: &RuntimeNativeDeviceUpdate,
    ) -> Result<PathBuf, RuntimeNativeDeviceUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime native-device-update publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeNativeDeviceUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeNativeDeviceUpdateError::io(
                        "clone retained native-device-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "inspect retained native-device-update output directory",
                source,
            )
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeNativeDeviceUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "reinspect native-device-update output directory",
                source,
            )
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeNativeDeviceUpdateError::io(
                "reopen native-device-update output directory identity",
                source,
            )
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeNativeDeviceUpdateError::new(
                RuntimeNativeDeviceUpdateErrorKind::AuthorityChanged,
                "native-device-update output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeNativeDeviceUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeNativeDeviceUpdateErrorKind::PublicationConflict
            } else {
                RuntimeNativeDeviceUpdateErrorKind::Io
            };
            RuntimeNativeDeviceUpdateError::with_source(
                kind,
                "publish native-device update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::PublicationUncertain,
                "native-device update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeNativeDeviceUpdateError> {
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
                RuntimeNativeDeviceUpdateError::io(
                    "create native-device-update staging file",
                    source,
                )
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeNativeDeviceUpdateError::io(
                    "inspect native-device-update staging file",
                    source,
                )
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeNativeDeviceUpdateError::new(
                        RuntimeNativeDeviceUpdateErrorKind::Io,
                        "native-device-update staging entry is not a regular file",
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
                RuntimeNativeDeviceUpdateError::io(
                    "write native-device-update staging file",
                    source,
                )
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeNativeDeviceUpdateError::io(
                        "synchronize native-device-update staging file",
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
                RuntimeNativeDeviceUpdateErrorKind::PublicationConflict
            } else {
                RuntimeNativeDeviceUpdateErrorKind::Io
            };
            let primary = RuntimeNativeDeviceUpdateError::with_source(
                kind,
                "publish native-device update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeNativeDeviceUpdateError::with_source(
                RuntimeNativeDeviceUpdateErrorKind::PublicationUncertain,
                "native-device update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeNativeDeviceUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeNativeDeviceUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeNativeDeviceUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime native-device update from a stable absolute
/// regular non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable update.
pub fn read_runtime_native_device_update(
    path: &Path,
) -> Result<RuntimeNativeDeviceUpdate, RuntimeNativeDeviceUpdateError> {
    let snapshot = read_stable_absolute_file(path, MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES)
        .map_err(|source| {
            RuntimeNativeDeviceUpdateError::io("read stable native-device update", source)
        })?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeNativeDeviceUpdate::parse(&bytes)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES, &evidence).map_err(
        |source| RuntimeNativeDeviceUpdateError::io("confirm stable native-device update", source),
    )?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-native-device-update.json")
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

    use hyper_native_device::{
        MAX_COMPRESSOR_ATTACK_MICROSECONDS, MAX_COMPRESSOR_RATIO_MILLIONTHS,
        MAX_COMPRESSOR_RELEASE_MICROSECONDS, MAX_FILTER_CUTOFF_MILLIHERTZ,
        MAX_FILTER_RESONANCE_MILLIONTHS, MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS,
        MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS, NativeCompressorState, NativeFilterMode,
        NativeFilterState,
    };
    use serde_json::{Value, json};
    use uuid::Uuid;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const FILTER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const COMPRESSOR_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334459";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334460";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CHAIN_DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-native-device-update-{}",
                Uuid::new_v4()
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

    fn source() -> RuntimeNativeDeviceUpdateSource {
        RuntimeNativeDeviceUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn filter_state() -> NativeFilterState {
        NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_000).unwrap()
    }

    fn compressor_state() -> NativeCompressorState {
        NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 150_000, 1_500).unwrap()
    }

    fn filter_device(id: &str) -> RuntimeNativeDeviceUpdateDevice {
        RuntimeNativeDeviceUpdateDevice::try_new(
            id.to_owned(),
            NATIVE_FILTER_DEVICE_ID.to_owned(),
            NATIVE_FILTER_STATE_VERSION,
            NATIVE_FILTER_DSP_VERSION,
            true,
            NativeDeviceState::Filter(filter_state()),
        )
        .unwrap()
    }

    fn compressor_device(id: &str) -> RuntimeNativeDeviceUpdateDevice {
        RuntimeNativeDeviceUpdateDevice::try_new(
            id.to_owned(),
            NATIVE_COMPRESSOR_DEVICE_ID.to_owned(),
            NATIVE_COMPRESSOR_STATE_VERSION,
            NATIVE_COMPRESSOR_DSP_VERSION,
            false,
            NativeDeviceState::Compressor(compressor_state()),
        )
        .unwrap()
    }

    fn update() -> RuntimeNativeDeviceUpdate {
        RuntimeNativeDeviceUpdate::try_new(
            UPDATE_ID.to_owned(),
            source(),
            CHAIN_ID.to_owned(),
            TRACK_ID.to_owned(),
            CHAIN_DIGEST.to_owned(),
            vec![filter_device(FILTER_ID), compressor_device(COMPRESSOR_ID)],
        )
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
    fn update_requires_canonical_complete_state_only_topology() {
        let update = update();
        let bytes = update.canonical_bytes().unwrap();
        assert_eq!(RuntimeNativeDeviceUpdate::parse(&bytes).unwrap(), update);

        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&noncanonical)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::NonCanonicalJson
        );

        let oversized =
            vec![b' '; usize::try_from(MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES + 1).unwrap()];
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&oversized)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::LimitExceeded
        );

        let mut extension: Value = serde_json::from_slice(&bytes).unwrap();
        extension["extensions"] = json!({"unrecognized": true});
        let extension = serde_json_canonicalizer::to_vec(&extension).unwrap();
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&extension)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        let mut unknown_field: Value = serde_json::from_slice(&bytes).unwrap();
        unknown_field["unexpected"] = json!(true);
        let unknown_field = serde_json_canonicalizer::to_vec(&unknown_field).unwrap();
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&unknown_field)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        let mut mismatched_kind: Value = serde_json::from_slice(&bytes).unwrap();
        mismatched_kind["devices"][0]["device_id"] = json!(NATIVE_COMPRESSOR_DEVICE_ID);
        let mismatched_kind = serde_json_canonicalizer::to_vec(&mismatched_kind).unwrap();
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&mismatched_kind)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        let mut duplicate_device: Value = serde_json::from_slice(&bytes).unwrap();
        duplicate_device["devices"][1]["id"] = json!(FILTER_ID);
        let duplicate_device = serde_json_canonicalizer::to_vec(&duplicate_device).unwrap();
        assert_eq!(
            RuntimeNativeDeviceUpdate::parse(&duplicate_device)
                .unwrap_err()
                .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn constructor_refuses_non_state_only_or_ambiguous_records() {
        assert_eq!(
            RuntimeNativeDeviceUpdateDevice::try_new(
                FILTER_ID.to_owned(),
                NATIVE_FILTER_DEVICE_ID.to_owned(),
                NATIVE_FILTER_STATE_VERSION,
                NATIVE_FILTER_DSP_VERSION + 1,
                true,
                NativeDeviceState::Filter(filter_state()),
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        assert_eq!(
            RuntimeNativeDeviceUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                CHAIN_ID.to_owned(),
                TRACK_ID.to_owned(),
                CHAIN_DIGEST.to_owned(),
                Vec::new(),
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        let filter = filter_device(FILTER_ID);
        assert_eq!(
            RuntimeNativeDeviceUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                CHAIN_ID.to_owned(),
                TRACK_ID.to_owned(),
                CHAIN_DIGEST.to_owned(),
                vec![filter.clone(), filter],
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );

        assert_eq!(
            RuntimeNativeDeviceUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                CHAIN_ID.to_owned(),
                TRACK_ID.to_owned(),
                PROJECT_DIGEST.to_uppercase(),
                vec![filter_device(FILTER_ID)],
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn source_requires_exact_json_safe_runtime_provenance() {
        assert_eq!(
            RuntimeNativeDeviceUpdateSource::try_new(
                "not-an-entity-id".to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeNativeDeviceUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER + 1,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeNativeDeviceUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_uppercase(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeNativeDeviceUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn maximum_version_one_device_state_is_bounded_by_contract() {
        let devices = (0..MAX_RUNTIME_NATIVE_DEVICE_UPDATE_DEVICES)
            .map(|index| {
                let id = format!("018f47a2-6b2d-7c11-8a44-{index:012x}");
                RuntimeNativeDeviceUpdateDevice::try_new(
                    id,
                    NATIVE_COMPRESSOR_DEVICE_ID.to_owned(),
                    NATIVE_COMPRESSOR_STATE_VERSION,
                    NATIVE_COMPRESSOR_DSP_VERSION,
                    true,
                    NativeDeviceState::Compressor(
                        NativeCompressorState::try_new(
                            MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS,
                            MAX_COMPRESSOR_RATIO_MILLIONTHS,
                            MAX_COMPRESSOR_ATTACK_MICROSECONDS,
                            MAX_COMPRESSOR_RELEASE_MICROSECONDS,
                            MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS,
                        )
                        .unwrap(),
                    ),
                )
                .unwrap()
            })
            .collect();
        let update = RuntimeNativeDeviceUpdate::try_new(
            UPDATE_ID.to_owned(),
            RuntimeNativeDeviceUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                MAX_SAFE_INTEGER,
            )
            .unwrap(),
            CHAIN_ID.to_owned(),
            TRACK_ID.to_owned(),
            CHAIN_DIGEST.to_owned(),
            devices,
        )
        .unwrap();
        assert!(
            u64::try_from(update.canonical_bytes().unwrap().len()).unwrap()
                <= MAX_RUNTIME_NATIVE_DEVICE_UPDATE_BYTES
        );

        let filter = NativeFilterState::try_new(
            NativeFilterMode::HighPass,
            MAX_FILTER_CUTOFF_MILLIHERTZ,
            MAX_FILTER_RESONANCE_MILLIONTHS,
        )
        .unwrap();
        assert_eq!(filter.cutoff_millihertz(), MAX_FILTER_CUTOFF_MILLIHERTZ);
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeNativeDeviceUpdateDirectory::open(temporary.path()).unwrap();
        let update = update();
        let output = directory.publish(&update).unwrap();
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(read_runtime_native_device_update(&output).unwrap(), update);
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeNativeDeviceUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_directory_refuses_relative_authority() {
        let Err(error) =
            RuntimeNativeDeviceUpdateDirectory::open(Path::new("native-device-updates"))
        else {
            panic!("a relative native-device-update directory must be rejected");
        };
        assert_eq!(
            error.kind(),
            RuntimeNativeDeviceUpdateErrorKind::OutputDirectoryInvalid
        );
    }
}
