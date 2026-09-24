//! Shared, bounded Runtime-to-IDE automation-take handback contracts.
//!
//! Runtime emits one complete, provenance-bound automation take only after a
//! finished capture. The artifact retains stable project identities rather
//! than ephemeral mixer slots, and the IDE remains the only project writer.

#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeMap;
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
use hyper_playback_json::{
    AutomationChannelParameter, AutomationInterpolation, AutomationMainParameter, AutomationPoint,
    AutomationSendParameter, AutomationTarget, AutomationValue,
};
use hyper_project_model::is_valid_entity_id;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-runtime-automation-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_TAKE_NAME_CHARACTERS: usize = 120;

/// Maximum byte size accepted for a version-one Runtime automation update.
///
/// Version one captures one bounded lane with at most 8,192 points. Even with
/// JSON-safe maximum tick and integer values, the largest canonical payload is
/// materially smaller than this one-MiB input boundary. The headroom makes the
/// transport limit explicit without inheriting the project document's full
/// unbounded surface.
pub const MAX_RUNTIME_AUTOMATION_UPDATE_BYTES: u64 = 1_048_576;

/// Maximum number of captured points in one version-one Runtime take.
///
/// The IDE additionally constrains a capture to its remaining aggregate
/// project capacity. This artifact-level bound prevents a single handback
/// from monopolizing an otherwise valid project document.
pub const MAX_RUNTIME_AUTOMATION_UPDATE_POINTS: usize = 8_192;

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

/// Exact verified project and Runtime provenance for one automation update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAutomationUpdateSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeAutomationUpdateSource {
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
    ) -> Result<Self, RuntimeAutomationUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update source deployment generation is outside the version-one JSON domain",
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

/// Stable authored mixer target for one Runtime automation take.
///
/// Channel and send identifiers remain stable project identities. Runtime-only
/// mixer slot indexes are deliberately absent from this boundary and must be
/// resolved anew by both Runtime capture preparation and IDE import.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeAutomationTarget {
    /// One stable mixer-channel parameter.
    Channel {
        /// Stable project track identity.
        track_id: String,
        /// Captured channel parameter.
        parameter: AutomationChannelParameter,
    },
    /// One stable mixer-send parameter.
    Send {
        /// Stable project send identity.
        send_id: String,
        /// Captured send parameter.
        parameter: AutomationSendParameter,
    },
    /// One main-output parameter.
    Main {
        /// Captured main-output parameter.
        parameter: AutomationMainParameter,
    },
}

impl RuntimeAutomationTarget {
    /// Creates one stable channel target.
    ///
    /// # Errors
    ///
    /// Refuses a track identity outside the project entity domain.
    pub fn try_channel(
        track_id: String,
        parameter: AutomationChannelParameter,
    ) -> Result<Self, RuntimeAutomationUpdateError> {
        let target = Self::Channel {
            track_id,
            parameter,
        };
        target.validate()?;
        Ok(target)
    }

    /// Creates one stable send target.
    ///
    /// # Errors
    ///
    /// Refuses a send identity outside the project entity domain.
    pub fn try_send(
        send_id: String,
        parameter: AutomationSendParameter,
    ) -> Result<Self, RuntimeAutomationUpdateError> {
        let target = Self::Send { send_id, parameter };
        target.validate()?;
        Ok(target)
    }

    /// Creates one main-output target.
    #[must_use]
    pub const fn main(parameter: AutomationMainParameter) -> Self {
        Self::Main { parameter }
    }

    /// Returns whether this target accepts Boolean automation points.
    #[must_use]
    pub const fn is_boolean(&self) -> bool {
        self.validation_target().is_boolean()
    }

    /// Returns inclusive native integer bounds for a continuous target.
    #[must_use]
    pub const fn integer_bounds(&self) -> Option<(i32, i32)> {
        self.validation_target().integer_bounds()
    }

    /// Returns the sole version-one interpolation accepted for this target.
    #[must_use]
    pub const fn interpolation(&self) -> AutomationInterpolation {
        if self.is_boolean() {
            AutomationInterpolation::Hold
        } else {
            AutomationInterpolation::Linear
        }
    }

    const fn validation_target(&self) -> AutomationTarget {
        match self {
            Self::Channel { parameter, .. } => AutomationTarget::Channel {
                slot: 0,
                parameter: *parameter,
            },
            Self::Send { parameter, .. } => AutomationTarget::Send {
                slot: 0,
                send: 0,
                parameter: *parameter,
            },
            Self::Main { parameter } => AutomationTarget::Main {
                parameter: *parameter,
            },
        }
    }

    fn validate(&self) -> Result<(), RuntimeAutomationUpdateError> {
        match self {
            Self::Channel { track_id, .. } if !is_valid_entity_id(track_id) => {
                Err(RuntimeAutomationUpdateError::invalid(
                    "automation-update channel target identity is invalid",
                ))
            }
            Self::Send { send_id, .. } if !is_valid_entity_id(send_id) => {
                Err(RuntimeAutomationUpdateError::invalid(
                    "automation-update send target identity is invalid",
                ))
            }
            Self::Channel { .. } | Self::Send { .. } | Self::Main { .. } => Ok(()),
        }
    }

    fn value_matches(&self, value: AutomationValue) -> bool {
        match (self.is_boolean(), value) {
            (true, AutomationValue::Boolean(_)) => true,
            (false, AutomationValue::Integer(value)) => match self.integer_bounds() {
                Some((minimum, maximum)) => (minimum..=maximum).contains(&value),
                None => false,
            },
            (true, AutomationValue::Integer(_)) | (false, AutomationValue::Boolean(_)) => false,
        }
    }
}

/// A complete, append-only automation take emitted by Runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeAutomationUpdate {
    id: String,
    source: RuntimeAutomationUpdateSource,
    name: String,
    target: RuntimeAutomationTarget,
    start_tick: u64,
    end_tick: u64,
    points: Vec<AutomationPoint>,
}

impl RuntimeAutomationUpdate {
    /// Creates one version-one automation update with an explicitly supplied
    /// unique update ID.
    ///
    /// The version-one interpolation is derived from the target, rather than
    /// caller selected: continuous parameters are linear and Boolean
    /// parameters are held. This prevents an artifact from describing a take
    /// the IDE's live capture path would never create.
    ///
    /// # Errors
    ///
    /// Refuses identities, take metadata, points, or target values outside the
    /// version-one contract.
    pub fn try_new(
        id: String,
        source: RuntimeAutomationUpdateSource,
        name: String,
        target: RuntimeAutomationTarget,
        start_tick: u64,
        end_tick: u64,
        points: Vec<AutomationPoint>,
    ) -> Result<Self, RuntimeAutomationUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update identity is invalid",
            ));
        }
        if !take_name_is_valid(&name) {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update take name is outside the version-one domain",
            ));
        }
        target.validate()?;
        if start_tick >= end_tick || end_tick > MAX_SAFE_INTEGER {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update interval is empty or outside the version-one JSON domain",
            ));
        }
        if points.is_empty() {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update declares no captured points",
            ));
        }
        if points.len() > MAX_RUNTIME_AUTOMATION_UPDATE_POINTS {
            return Err(RuntimeAutomationUpdateError::limit(
                "automation-update exceeds the version-one point capacity",
            ));
        }
        let mut previous_tick = None;
        for point in &points {
            if point.tick() < start_tick
                || point.tick() >= end_tick
                || point.tick() > MAX_SAFE_INTEGER
                || previous_tick.is_some_and(|previous| previous >= point.tick())
            {
                return Err(RuntimeAutomationUpdateError::invalid(
                    "automation-update points are outside their ordered interval",
                ));
            }
            if !target.value_matches(point.value()) {
                return Err(RuntimeAutomationUpdateError::invalid(
                    "automation-update point value differs from its target domain",
                ));
            }
            previous_tick = Some(point.tick());
        }
        Ok(Self {
            id,
            source,
            name,
            target,
            start_tick,
            end_tick,
            points,
        })
    }

    /// Parses one complete canonical version-one automation update.
    ///
    /// # Errors
    ///
    /// Refuses malformed, noncanonical, oversized, or incompatible artifacts
    /// before callers can use the captured take.
    pub fn parse(bytes: &[u8]) -> Result<Self, RuntimeAutomationUpdateError> {
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_AUTOMATION_UPDATE_BYTES {
            return Err(RuntimeAutomationUpdateError::limit(
                "automation-update exceeds the version-one byte capacity",
            ));
        }
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_AUTOMATION_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::JsonInvalid,
                "automation-update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::InvalidUpdate,
                "automation-update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeAutomationUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::InvalidUpdate,
                "automation-update shape is invalid",
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
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeAutomationUpdateError> {
        let value = serde_json::to_value(RawUpdate::from_update(self)).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::InvalidUpdate,
                "automation-update cannot be serialized",
                source,
            )
        })?;
        let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::InvalidUpdate,
                "automation-update cannot be canonicalized",
                source,
            )
        })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_RUNTIME_AUTOMATION_UPDATE_BYTES {
            return Err(RuntimeAutomationUpdateError::limit(
                "automation-update exceeds the version-one byte capacity",
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
    pub const fn source(&self) -> &RuntimeAutomationUpdateSource {
        &self.source
    }

    /// Returns the validated, user-visible take name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the stable authored mixer target.
    #[must_use]
    pub const fn target(&self) -> &RuntimeAutomationTarget {
        &self.target
    }

    /// Returns the target-derived version-one interpolation.
    #[must_use]
    pub const fn interpolation(&self) -> AutomationInterpolation {
        self.target.interpolation()
    }

    /// Returns the inclusive captured take boundary in project ticks.
    #[must_use]
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    /// Returns the exclusive captured take boundary in project ticks.
    #[must_use]
    pub const fn end_tick(&self) -> u64 {
        self.end_tick
    }

    /// Returns the strictly ordered captured parameter points.
    #[must_use]
    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    fn from_raw(raw: RawUpdate) -> Result<Self, RuntimeAutomationUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeAutomationUpdateSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let target = raw.target.try_into_target()?;
        if raw.interpolation.try_into_interpolation() != target.interpolation() {
            return Err(RuntimeAutomationUpdateError::invalid(
                "automation-update interpolation does not match its target",
            ));
        }
        if raw.points.len() > MAX_RUNTIME_AUTOMATION_UPDATE_POINTS {
            return Err(RuntimeAutomationUpdateError::limit(
                "automation-update exceeds the version-one point capacity",
            ));
        }
        let mut points = Vec::new();
        points.try_reserve_exact(raw.points.len()).map_err(|_| {
            RuntimeAutomationUpdateError::limit("automation-update point capacity is unavailable")
        })?;
        for point in raw.points {
            points.push(point.into_point());
        }
        Self::try_new(
            raw.id,
            source,
            raw.name,
            target,
            raw.start_tick,
            raw.end_tick,
            points,
        )
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUpdate {
    format: String,
    format_version: u8,
    id: String,
    source: RawSource,
    name: String,
    target: RawTarget,
    interpolation: RawInterpolation,
    start_tick: u64,
    end_tick: u64,
    points: Vec<RawPoint>,
    extensions: BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeAutomationUpdate) -> Self {
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
            name: update.name.clone(),
            target: RawTarget::from_target(&update.target),
            interpolation: RawInterpolation::from_interpolation(update.interpolation()),
            start_tick: update.start_tick,
            end_tick: update.end_tick,
            points: update
                .points
                .iter()
                .copied()
                .map(RawPoint::from_point)
                .collect(),
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
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum RawTarget {
    #[serde(rename = "mixer-channel")]
    Channel {
        track_id: String,
        parameter: RawChannelParameter,
    },
    #[serde(rename = "mixer-send")]
    Send {
        send_id: String,
        parameter: RawSendParameter,
    },
    #[serde(rename = "mixer-main")]
    Main { parameter: RawMainParameter },
}

impl RawTarget {
    fn from_target(target: &RuntimeAutomationTarget) -> Self {
        match target {
            RuntimeAutomationTarget::Channel {
                track_id,
                parameter,
            } => Self::Channel {
                track_id: track_id.clone(),
                parameter: RawChannelParameter::from_parameter(*parameter),
            },
            RuntimeAutomationTarget::Send { send_id, parameter } => Self::Send {
                send_id: send_id.clone(),
                parameter: RawSendParameter::from_parameter(*parameter),
            },
            RuntimeAutomationTarget::Main { parameter } => Self::Main {
                parameter: RawMainParameter::from_parameter(*parameter),
            },
        }
    }

    fn try_into_target(self) -> Result<RuntimeAutomationTarget, RuntimeAutomationUpdateError> {
        match self {
            Self::Channel {
                track_id,
                parameter,
            } => RuntimeAutomationTarget::try_channel(track_id, parameter.into_parameter()),
            Self::Send { send_id, parameter } => {
                RuntimeAutomationTarget::try_send(send_id, parameter.into_parameter())
            }
            Self::Main { parameter } => {
                Ok(RuntimeAutomationTarget::main(parameter.into_parameter()))
            }
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RawChannelParameter {
    Gain,
    Pan,
    Mute,
    Solo,
}

impl RawChannelParameter {
    const fn from_parameter(parameter: AutomationChannelParameter) -> Self {
        match parameter {
            AutomationChannelParameter::Gain => Self::Gain,
            AutomationChannelParameter::Pan => Self::Pan,
            AutomationChannelParameter::Mute => Self::Mute,
            AutomationChannelParameter::Solo => Self::Solo,
        }
    }

    const fn into_parameter(self) -> AutomationChannelParameter {
        match self {
            Self::Gain => AutomationChannelParameter::Gain,
            Self::Pan => AutomationChannelParameter::Pan,
            Self::Mute => AutomationChannelParameter::Mute,
            Self::Solo => AutomationChannelParameter::Solo,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RawSendParameter {
    Gain,
    Pan,
    Enabled,
}

impl RawSendParameter {
    const fn from_parameter(parameter: AutomationSendParameter) -> Self {
        match parameter {
            AutomationSendParameter::Gain => Self::Gain,
            AutomationSendParameter::Pan => Self::Pan,
            AutomationSendParameter::Enabled => Self::Enabled,
        }
    }

    const fn into_parameter(self) -> AutomationSendParameter {
        match self {
            Self::Gain => AutomationSendParameter::Gain,
            Self::Pan => AutomationSendParameter::Pan,
            Self::Enabled => AutomationSendParameter::Enabled,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RawMainParameter {
    Gain,
    Mute,
}

impl RawMainParameter {
    const fn from_parameter(parameter: AutomationMainParameter) -> Self {
        match parameter {
            AutomationMainParameter::Gain => Self::Gain,
            AutomationMainParameter::Mute => Self::Mute,
        }
    }

    const fn into_parameter(self) -> AutomationMainParameter {
        match self {
            Self::Gain => AutomationMainParameter::Gain,
            Self::Mute => AutomationMainParameter::Mute,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum RawInterpolation {
    Hold,
    Linear,
}

impl RawInterpolation {
    const fn from_interpolation(interpolation: AutomationInterpolation) -> Self {
        match interpolation {
            AutomationInterpolation::Hold => Self::Hold,
            AutomationInterpolation::Linear => Self::Linear,
        }
    }

    const fn try_into_interpolation(self) -> AutomationInterpolation {
        match self {
            Self::Hold => AutomationInterpolation::Hold,
            Self::Linear => AutomationInterpolation::Linear,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPoint {
    tick: u64,
    value: RawValue,
}

impl RawPoint {
    fn from_point(point: AutomationPoint) -> Self {
        Self {
            tick: point.tick(),
            value: RawValue::from_value(point.value()),
        }
    }

    const fn into_point(self) -> AutomationPoint {
        AutomationPoint::new(self.tick, self.value.into_value())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum RawValue {
    Integer(i32),
    Boolean(bool),
}

impl RawValue {
    const fn from_value(value: AutomationValue) -> Self {
        match value {
            AutomationValue::Integer(value) => Self::Integer(value),
            AutomationValue::Boolean(value) => Self::Boolean(value),
        }
    }

    const fn into_value(self) -> AutomationValue {
        match self {
            Self::Integer(value) => AutomationValue::Integer(value),
            Self::Boolean(value) => AutomationValue::Boolean(value),
        }
    }
}

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeAutomationUpdateErrorKind {
    /// An external filesystem operation failed.
    Io,
    /// Input was not valid strict JSON.
    JsonInvalid,
    /// Input was valid JSON but not canonical RFC 8785 bytes.
    NonCanonicalJson,
    /// Input or a requested operation is outside the version-one contract.
    InvalidUpdate,
    /// A configured byte or point limit was exceeded.
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

/// A classified automation-update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeAutomationUpdateError {
    kind: RuntimeAutomationUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeAutomationUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeAutomationUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(RuntimeAutomationUpdateErrorKind::InvalidUpdate, message)
    }

    fn limit(message: &'static str) -> Self {
        Self::new(RuntimeAutomationUpdateErrorKind::LimitExceeded, message)
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeAutomationUpdateErrorKind::NonCanonicalJson,
            "automation-update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeAutomationUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeAutomationUpdateErrorKind,
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
        Self::with_source(RuntimeAutomationUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeAutomationUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeAutomationUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime automation-update output
/// directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeAutomationUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeAutomationUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeAutomationUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeAutomationUpdateError::new(
                RuntimeAutomationUpdateErrorKind::OutputDirectoryInvalid,
                "automation-update output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeAutomationUpdateError::io("inspect automation-update output directory", source)
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeAutomationUpdateError::new(
                RuntimeAutomationUpdateErrorKind::OutputDirectoryInvalid,
                "automation-update output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeAutomationUpdateError::io(
                "retain automation-update output directory identity",
                source,
            )
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeAutomationUpdateError::io("open automation-update output directory", source)
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeAutomationUpdateError::io(
                        "clone automation-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeAutomationUpdateError::io(
                "inspect retained automation-update output directory",
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
    /// is returned as [`RuntimeAutomationUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(
        &self,
        update: &RuntimeAutomationUpdate,
    ) -> Result<PathBuf, RuntimeAutomationUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime automation-update publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeAutomationUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeAutomationUpdateError::io(
                        "clone retained automation-update output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeAutomationUpdateError::io(
                "inspect retained automation-update output directory",
                source,
            )
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeAutomationUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeAutomationUpdateError::io("reinspect automation-update output directory", source)
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeAutomationUpdateError::io(
                "reopen automation-update output directory identity",
                source,
            )
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeAutomationUpdateError::new(
                RuntimeAutomationUpdateErrorKind::AuthorityChanged,
                "automation-update output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(&self, name: &str, bytes: &[u8]) -> Result<(), RuntimeAutomationUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeAutomationUpdateErrorKind::PublicationConflict
            } else {
                RuntimeAutomationUpdateErrorKind::Io
            };
            RuntimeAutomationUpdateError::with_source(
                kind,
                "publish automation update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::PublicationUncertain,
                "automation update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeAutomationUpdateError> {
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
                RuntimeAutomationUpdateError::io("create automation-update staging file", source)
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeAutomationUpdateError::io("inspect automation-update staging file", source)
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeAutomationUpdateError::new(
                        RuntimeAutomationUpdateErrorKind::Io,
                        "automation-update staging entry is not a regular file",
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
                RuntimeAutomationUpdateError::io("write automation-update staging file", source)
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeAutomationUpdateError::io(
                        "synchronize automation-update staging file",
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
                RuntimeAutomationUpdateErrorKind::PublicationConflict
            } else {
                RuntimeAutomationUpdateErrorKind::Io
            };
            let primary = RuntimeAutomationUpdateError::with_source(
                kind,
                "publish automation update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeAutomationUpdateError::with_source(
                RuntimeAutomationUpdateErrorKind::PublicationUncertain,
                "automation update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeAutomationUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeAutomationUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeAutomationUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime automation update from a stable absolute regular
/// non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable automation take.
pub fn read_runtime_automation_update(
    path: &Path,
) -> Result<RuntimeAutomationUpdate, RuntimeAutomationUpdateError> {
    let snapshot =
        read_stable_absolute_file(path, MAX_RUNTIME_AUTOMATION_UPDATE_BYTES).map_err(|source| {
            RuntimeAutomationUpdateError::io("read stable automation update", source)
        })?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeAutomationUpdate::parse(&bytes)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_AUTOMATION_UPDATE_BYTES, &evidence).map_err(
        |source| RuntimeAutomationUpdateError::io("confirm stable automation update", source),
    )?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-automation-update.json")
}

fn take_name_is_valid(name: &str) -> bool {
    let character_count = name.chars().count();
    name.trim() == name && (1..=MAX_TAKE_NAME_CHARACTERS).contains(&character_count)
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
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-automation-update-{}",
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

    fn source() -> RuntimeAutomationUpdateSource {
        RuntimeAutomationUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn channel_target(parameter: AutomationChannelParameter) -> RuntimeAutomationTarget {
        RuntimeAutomationTarget::try_channel(TRACK_ID.to_owned(), parameter).unwrap()
    }

    fn gain_points() -> Vec<AutomationPoint> {
        vec![
            AutomationPoint::new(1920, AutomationValue::Integer(1_000_000)),
            AutomationPoint::new(3840, AutomationValue::Integer(1_500_000)),
        ]
    }

    fn update() -> RuntimeAutomationUpdate {
        RuntimeAutomationUpdate::try_new(
            UPDATE_ID.to_owned(),
            source(),
            "Runtime gain pass".to_owned(),
            channel_target(AutomationChannelParameter::Gain),
            1920,
            5760,
            gain_points(),
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
    fn source_requires_exact_json_safe_runtime_provenance() {
        assert_eq!(
            RuntimeAutomationUpdateSource::try_new(
                "not-an-entity-id".to_owned(),
                7,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                MAX_SAFE_INTEGER + 1,
                PROJECT_DIGEST.to_owned(),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdateSource::try_new(
                PROJECT_ID.to_owned(),
                7,
                "A".repeat(64),
                BUNDLE_DIGEST.to_owned(),
                3,
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn stable_targets_preserve_the_existing_automation_domains() {
        let pan = channel_target(AutomationChannelParameter::Pan);
        assert_eq!(pan.integer_bounds(), Some((-1_000_000, 1_000_000)));
        assert_eq!(pan.interpolation(), AutomationInterpolation::Linear);

        let mute = channel_target(AutomationChannelParameter::Mute);
        assert!(mute.is_boolean());
        assert_eq!(mute.integer_bounds(), None);
        assert_eq!(mute.interpolation(), AutomationInterpolation::Hold);

        assert_eq!(
            RuntimeAutomationTarget::try_send(
                SEND_ID.to_owned(),
                AutomationSendParameter::Enabled,
            )
            .unwrap()
            .interpolation(),
            AutomationInterpolation::Hold
        );
        assert_eq!(
            RuntimeAutomationTarget::main(AutomationMainParameter::Gain).integer_bounds(),
            Some((0, 2_000_000))
        );
        assert_eq!(
            RuntimeAutomationTarget::try_channel(
                "not-an-entity-id".to_owned(),
                AutomationChannelParameter::Gain,
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_round_trips_exact_stable_authorship_and_points() {
        let update = update();
        let bytes = update.canonical_bytes().unwrap();

        assert_eq!(RuntimeAutomationUpdate::parse(&bytes).unwrap(), update);
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["target"]["kind"], "mixer-channel");
        assert_eq!(value["target"]["track_id"], TRACK_ID);
        assert_eq!(value["target"]["parameter"], "gain");
        assert_eq!(value["interpolation"], "linear");
        assert_eq!(value["points"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn update_rejects_invalid_names_intervals_points_and_domains() {
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                " leading space".to_owned(),
                channel_target(AutomationChannelParameter::Gain),
                0,
                3,
                vec![AutomationPoint::new(0, AutomationValue::Integer(0))],
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                "Valid".to_owned(),
                channel_target(AutomationChannelParameter::Gain),
                2,
                2,
                vec![AutomationPoint::new(2, AutomationValue::Integer(0))],
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                "Valid".to_owned(),
                channel_target(AutomationChannelParameter::Gain),
                0,
                3,
                vec![
                    AutomationPoint::new(1, AutomationValue::Integer(0)),
                    AutomationPoint::new(1, AutomationValue::Integer(0)),
                ],
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                "Valid".to_owned(),
                channel_target(AutomationChannelParameter::Gain),
                0,
                3,
                vec![AutomationPoint::new(1, AutomationValue::Boolean(false))],
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                "Valid".to_owned(),
                channel_target(AutomationChannelParameter::Pan),
                0,
                3,
                vec![AutomationPoint::new(1, AutomationValue::Integer(1_000_001))],
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_rejects_noncanonical_unknown_and_inconsistent_raw_values() {
        let bytes = update().canonical_bytes().unwrap();
        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeAutomationUpdate::parse(&noncanonical)
                .unwrap_err()
                .kind(),
            RuntimeAutomationUpdateErrorKind::NonCanonicalJson
        );

        let mut interpolation: Value = serde_json::from_slice(&bytes).unwrap();
        interpolation["interpolation"] = json!("hold");
        let interpolation = serde_json_canonicalizer::to_vec(&interpolation).unwrap();
        assert_eq!(
            RuntimeAutomationUpdate::parse(&interpolation)
                .unwrap_err()
                .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );

        let mut extension: Value = serde_json::from_slice(&bytes).unwrap();
        extension["extensions"] = json!({"unrecognized": true});
        let extension = serde_json_canonicalizer::to_vec(&extension).unwrap();
        assert_eq!(
            RuntimeAutomationUpdate::parse(&extension)
                .unwrap_err()
                .kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );

        let mut unknown: Value = serde_json::from_slice(&bytes).unwrap();
        unknown["unexpected"] = json!(true);
        let unknown = serde_json_canonicalizer::to_vec(&unknown).unwrap();
        assert_eq!(
            RuntimeAutomationUpdate::parse(&unknown).unwrap_err().kind(),
            RuntimeAutomationUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn maximum_version_one_point_count_is_bounded_well_below_the_byte_limit() {
        let points = (0..MAX_RUNTIME_AUTOMATION_UPDATE_POINTS)
            .map(|index| {
                AutomationPoint::new(
                    MAX_SAFE_INTEGER - u64::try_from(MAX_RUNTIME_AUTOMATION_UPDATE_POINTS).unwrap()
                        + u64::try_from(index).unwrap(),
                    AutomationValue::Integer(2_000_000),
                )
            })
            .collect();
        let maximum = RuntimeAutomationUpdate::try_new(
            UPDATE_ID.to_owned(),
            source(),
            "N".repeat(MAX_TAKE_NAME_CHARACTERS),
            channel_target(AutomationChannelParameter::Gain),
            MAX_SAFE_INTEGER - u64::try_from(MAX_RUNTIME_AUTOMATION_UPDATE_POINTS).unwrap(),
            MAX_SAFE_INTEGER,
            points,
        )
        .unwrap();
        let bytes = maximum.canonical_bytes().unwrap();
        assert!(
            bytes.len() <= 786_432,
            "the bounded version-one schema must retain at least 256 KiB below its one-MiB reader limit"
        );
        assert!(
            u64::try_from(bytes.len()).unwrap() <= MAX_RUNTIME_AUTOMATION_UPDATE_BYTES,
            "the writer must never emit a reader-oversized artifact"
        );

        let excessive = (0..=MAX_RUNTIME_AUTOMATION_UPDATE_POINTS)
            .map(|index| {
                AutomationPoint::new(u64::try_from(index).unwrap(), AutomationValue::Integer(0))
            })
            .collect();
        assert_eq!(
            RuntimeAutomationUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                "Valid".to_owned(),
                channel_target(AutomationChannelParameter::Gain),
                0,
                u64::try_from(MAX_RUNTIME_AUTOMATION_UPDATE_POINTS + 1).unwrap(),
                excessive,
            )
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::LimitExceeded
        );
        assert_eq!(
            RuntimeAutomationUpdate::parse(&vec![
                b' ';
                usize::try_from(
                    MAX_RUNTIME_AUTOMATION_UPDATE_BYTES + 1
                )
                .unwrap()
            ],)
            .unwrap_err()
            .kind(),
            RuntimeAutomationUpdateErrorKind::LimitExceeded
        );
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let update = update();
        let output = directory.publish(&update).unwrap();
        assert!(output.ends_with(format!("{UPDATE_ID}.hyper-automation-update.json")));
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(read_runtime_automation_update(&output).unwrap(), update);
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeAutomationUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_directory_refuses_relative_authority() {
        let Err(error) = RuntimeAutomationUpdateDirectory::open(Path::new("automation-updates"))
        else {
            panic!("a relative automation-update directory must be rejected");
        };
        assert_eq!(
            error.kind(),
            RuntimeAutomationUpdateErrorKind::OutputDirectoryInvalid
        );
    }
}
