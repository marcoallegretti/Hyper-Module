//! Shared, bounded scene-performance capture and Runtime handoff contracts.
//!
//! The crate deliberately contains only control-side validation and a durable,
//! provenance-bound update format. Audio callbacks publish their compact,
//! committed observations through their own preallocated channels; callers
//! hand the resulting primitive fields to [`ScenePerformanceCapture`].

#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
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

const FORMAT: &str = "hyper-runtime-scene-performance-update";
const FORMAT_VERSION: u8 = 1;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// Maximum accepted version-one Runtime scene-performance update byte length.
///
/// The bound accommodates the version-one maximum of 65,536 canonical launch
/// records with stable 36-byte scene identifiers while keeping an external
/// import distinctly smaller than a project package.
pub const MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_BYTES: u64 = 8 * 1_024 * 1_024;

/// Maximum accepted version-one Runtime scene-performance launch count.
///
/// This intentionally matches the current version-one arrangement scene-launch
/// document limit. Callers still pass their current remaining capacity before
/// accepting a capture or importing an update.
pub const MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES: usize = 65_536;

/// Encodes one SHA-256 digest using the lowercase hexadecimal representation
/// required by the Runtime scene-performance provenance contract.
#[must_use]
pub fn sha256_hex(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;

        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

/// One verified scene available to a scene-performance capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenePerformanceScene {
    id: String,
    grid_quantized: bool,
}

impl ScenePerformanceScene {
    /// Creates one stable scene mapping for a capture snapshot.
    ///
    /// [`ScenePerformanceCapture::try_new`] validates the complete immutable
    /// catalog so it can report the precise invalid entry or duplicate.
    #[must_use]
    pub fn new(id: String, grid_quantized: bool) -> Self {
        Self { id, grid_quantized }
    }

    /// Returns the stable project scene identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns whether the scene can produce a grid-representable launch.
    #[must_use]
    pub const fn grid_quantized(&self) -> bool {
        self.grid_quantized
    }
}

/// One stable, tick-addressed captured scene launch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenePerformanceLaunch {
    scene_id: String,
    tick: u64,
}

impl ScenePerformanceLaunch {
    /// Creates one stable, tick-addressed scene launch.
    ///
    /// # Errors
    ///
    /// Returns an update-contract rejection when the scene identity or tick
    /// cannot be represented by version-one canonical JSON.
    pub fn try_new(
        scene_id: String,
        tick: u64,
    ) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        if !is_valid_entity_id(&scene_id) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance launch scene identity is invalid",
            ));
        }
        if tick > MAX_SAFE_INTEGER {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance launch tick is outside the version-one JSON domain",
            ));
        }
        Ok(Self { scene_id, tick })
    }

    /// Returns the stable project scene identity.
    #[must_use]
    pub fn scene_id(&self) -> &str {
        &self.scene_id
    }

    /// Returns the exact arrangement tick.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexedScenePerformanceLaunch {
    scene_index: usize,
    tick: u64,
}

/// A classified refusal from the shared scene-performance capture contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenePerformanceCaptureError {
    /// The configured capture capacity was zero or above the version-one cap.
    InvalidMaximumLaunchCount,
    /// A capture catalog contains an invalid stable scene identifier.
    InvalidSceneIdentity {
        /// Index of the invalid catalog entry.
        scene_index: usize,
    },
    /// A capture catalog repeats a stable scene identifier.
    DuplicateSceneIdentity {
        /// Index of the duplicate catalog entry.
        scene_index: usize,
    },
    /// Capture received a scene index not present in its immutable catalog.
    UnknownScene {
        /// Unrecognized scene index.
        scene_index: usize,
    },
    /// Capture received a launch for a non-grid scene.
    NonGridScene {
        /// Scene index whose launch is not grid-representable.
        scene_index: usize,
    },
    /// Capture received an immediate launch with no exact arrangement tick.
    ImmediateLaunch {
        /// Scene index for the immediate launch.
        scene_index: usize,
    },
    /// Capture received a tick outside the version-one canonical JSON domain.
    TickOutOfRange {
        /// Scene index for the out-of-range launch.
        scene_index: usize,
        /// Exact unsupported arrangement tick.
        tick: u64,
    },
    /// Capture received a launch after loop playback became enabled.
    LoopEnabledDuringCapture,
    /// The committed launch sequence skipped or reordered an observation.
    SequenceDiscontinuity {
        /// Sequence expected after the prior committed observation.
        expected: u64,
        /// Sequence actually received.
        actual: u64,
    },
    /// Two accepted launches resolve to the same arrangement tick.
    DuplicateTick {
        /// Repeated exact arrangement tick.
        tick: u64,
    },
    /// The bounded capture cannot accept another launch.
    CaptureCapacityReached {
        /// Maximum number of launches accepted by this capture.
        maximum_launch_count: usize,
    },
    /// Control-side allocation required to preserve a complete capture failed.
    CapacityUnavailable,
    /// A completed capture contains no recordable launches.
    NoLaunchesCaptured,
}

impl fmt::Display for ScenePerformanceCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaximumLaunchCount => {
                formatter.write_str("scene-performance capture capacity is invalid")
            }
            Self::InvalidSceneIdentity { scene_index } => {
                write!(
                    formatter,
                    "scene-performance scene {scene_index} has an invalid identity"
                )
            }
            Self::DuplicateSceneIdentity { scene_index } => write!(
                formatter,
                "scene-performance scene {scene_index} repeats a stable identity"
            ),
            Self::UnknownScene { scene_index } => {
                write!(
                    formatter,
                    "scene-performance scene {scene_index} is unknown"
                )
            }
            Self::NonGridScene { scene_index } => write!(
                formatter,
                "scene-performance scene {scene_index} is not grid-quantized"
            ),
            Self::ImmediateLaunch { scene_index } => write!(
                formatter,
                "scene-performance scene {scene_index} has no arrangement tick"
            ),
            Self::TickOutOfRange { scene_index, tick } => write!(
                formatter,
                "scene-performance scene {scene_index} has out-of-range arrangement tick {tick}"
            ),
            Self::LoopEnabledDuringCapture => {
                formatter.write_str("scene-performance capture cannot represent looped transport")
            }
            Self::SequenceDiscontinuity { expected, actual } => write!(
                formatter,
                "scene-performance observation sequence changed from {expected} to {actual}"
            ),
            Self::DuplicateTick { tick } => {
                write!(
                    formatter,
                    "scene-performance tick {tick} was captured more than once"
                )
            }
            Self::CaptureCapacityReached {
                maximum_launch_count,
            } => write!(
                formatter,
                "scene-performance capture reached its {maximum_launch_count}-launch limit"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("scene-performance capture capacity is unavailable")
            }
            Self::NoLaunchesCaptured => {
                formatter.write_str("scene-performance capture contains no launches")
            }
        }
    }
}

impl Error for ScenePerformanceCaptureError {}

/// Control-side, lossless validation state for one scene performance.
///
/// It retains compact scene indexes until finalization so accepted callback
/// observations do not repeat stable scene identities. Callers own their
/// transport lifecycle and must stop/finalize only at a committed boundary.
#[derive(Debug)]
pub struct ScenePerformanceCapture {
    scenes: Vec<ScenePerformanceScene>,
    launches: Vec<IndexedScenePerformanceLaunch>,
    maximum_launch_count: usize,
    last_sequence: Option<u64>,
}

impl ScenePerformanceCapture {
    /// Creates a bounded capture from an immutable verified scene snapshot.
    ///
    /// # Errors
    ///
    /// Refuses invalid or duplicate stable identities and a capacity outside
    /// the version-one scene-launch domain.
    pub fn try_new(
        scenes: Vec<ScenePerformanceScene>,
        maximum_launch_count: usize,
    ) -> Result<Self, ScenePerformanceCaptureError> {
        if maximum_launch_count == 0
            || maximum_launch_count > MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
        {
            return Err(ScenePerformanceCaptureError::InvalidMaximumLaunchCount);
        }
        for (scene_index, scene) in scenes.iter().enumerate() {
            if !is_valid_entity_id(scene.id()) {
                return Err(ScenePerformanceCaptureError::InvalidSceneIdentity { scene_index });
            }
            if scenes[..scene_index]
                .iter()
                .any(|prior| prior.id() == scene.id())
            {
                return Err(ScenePerformanceCaptureError::DuplicateSceneIdentity { scene_index });
            }
        }
        Ok(Self {
            scenes,
            launches: Vec::new(),
            maximum_launch_count,
            last_sequence: None,
        })
    }

    /// Returns how many launches have been accepted so far.
    #[must_use]
    pub fn captured_launch_count(&self) -> usize {
        self.launches.len()
    }

    /// Returns whether the capture has no accepted launches.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.launches.is_empty()
    }

    /// Validates one committed interactive-scene launch observation.
    ///
    /// `arrangement_tick` is `None` only for an immediate launch. The caller
    /// must supply observations in the exact sequence published by its
    /// committed callback handoff.
    ///
    /// # Errors
    ///
    /// Refuses every observation which cannot be losslessly represented by the
    /// bounded version-one arrangement-scene-launch contract.
    pub fn observe(
        &mut self,
        scene_index: usize,
        arrangement_tick: Option<u64>,
        sequence: u64,
        loop_enabled: bool,
    ) -> Result<(), ScenePerformanceCaptureError> {
        if loop_enabled {
            return Err(ScenePerformanceCaptureError::LoopEnabledDuringCapture);
        }
        let Some(scene) = self.scenes.get(scene_index) else {
            return Err(ScenePerformanceCaptureError::UnknownScene { scene_index });
        };
        if !scene.grid_quantized() {
            return Err(ScenePerformanceCaptureError::NonGridScene { scene_index });
        }
        let Some(tick) = arrangement_tick else {
            return Err(ScenePerformanceCaptureError::ImmediateLaunch { scene_index });
        };
        if tick > MAX_SAFE_INTEGER {
            return Err(ScenePerformanceCaptureError::TickOutOfRange { scene_index, tick });
        }
        if let Some(previous) = self.last_sequence {
            let expected = previous.wrapping_add(1);
            if sequence != expected {
                return Err(ScenePerformanceCaptureError::SequenceDiscontinuity {
                    expected,
                    actual: sequence,
                });
            }
        }
        if self.launches.iter().any(|recorded| recorded.tick == tick) {
            return Err(ScenePerformanceCaptureError::DuplicateTick { tick });
        }
        if self.launches.len() >= self.maximum_launch_count {
            return Err(ScenePerformanceCaptureError::CaptureCapacityReached {
                maximum_launch_count: self.maximum_launch_count,
            });
        }
        self.launches
            .try_reserve(1)
            .map_err(|_| ScenePerformanceCaptureError::CapacityUnavailable)?;
        self.launches
            .push(IndexedScenePerformanceLaunch { scene_index, tick });
        self.last_sequence = Some(sequence);
        Ok(())
    }

    /// Converts a completed nonempty capture into stable scene-ID launches.
    ///
    /// # Errors
    ///
    /// Returns [`ScenePerformanceCaptureError::NoLaunchesCaptured`] for an
    /// explicit empty cancellation or [`ScenePerformanceCaptureError::CapacityUnavailable`]
    /// when finalization cannot retain every stable launch identity.
    pub fn into_launches(
        self,
    ) -> Result<Vec<ScenePerformanceLaunch>, ScenePerformanceCaptureError> {
        if self.launches.is_empty() {
            return Err(ScenePerformanceCaptureError::NoLaunchesCaptured);
        }
        let mut launches = Vec::new();
        launches
            .try_reserve_exact(self.launches.len())
            .map_err(|_| ScenePerformanceCaptureError::CapacityUnavailable)?;
        for recorded in self.launches {
            let scene = self.scenes.get(recorded.scene_index).ok_or(
                ScenePerformanceCaptureError::UnknownScene {
                    scene_index: recorded.scene_index,
                },
            )?;
            launches.push(ScenePerformanceLaunch {
                scene_id: scene.id.clone(),
                tick: recorded.tick,
            });
        }
        Ok(launches)
    }
}

/// Exact verified project and Runtime provenance for one performance update.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeScenePerformanceSource {
    project_id: String,
    project_revision: u64,
    project_sha256: String,
    runtime_bundle_sha256: String,
    deployment_generation: u64,
}

impl RuntimeScenePerformanceSource {
    /// Creates provenance copied from the verified Runtime deployment.
    ///
    /// # Errors
    ///
    /// Refuses any identity, digest, revision, or generation that cannot be
    /// represented by the version-one canonical JSON contract.
    pub fn try_new(
        project_id: String,
        project_revision: u64,
        project_sha256: String,
        runtime_bundle_sha256: String,
        deployment_generation: u64,
    ) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        if !is_valid_entity_id(&project_id) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance source project identity is invalid",
            ));
        }
        if project_revision > MAX_SAFE_INTEGER {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance source project revision is outside the version-one JSON domain",
            ));
        }
        if !valid_sha256(&project_sha256) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance source project digest is invalid",
            ));
        }
        if !valid_sha256(&runtime_bundle_sha256) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance source Runtime bundle digest is invalid",
            ));
        }
        if deployment_generation > MAX_SAFE_INTEGER {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance source deployment generation is outside the version-one JSON domain",
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

/// A fully validated, append-only update emitted by Runtime capture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeScenePerformanceUpdate {
    id: String,
    source: RuntimeScenePerformanceSource,
    launches: Vec<ScenePerformanceLaunch>,
}

impl RuntimeScenePerformanceUpdate {
    /// Creates one version-one update with an explicitly supplied unique ID.
    ///
    /// Runtime is expected to generate this ID before publication. Accepting it
    /// as an argument keeps deterministic tests and alternative verified
    /// frontends independent from a hidden randomness authority.
    ///
    /// # Errors
    ///
    /// Refuses invalid update identity, empty/over-limit launch sets, repeated
    /// ticks, or a caller-provided limit outside this version's domain.
    pub fn try_new(
        id: String,
        source: RuntimeScenePerformanceSource,
        launches: Vec<ScenePerformanceLaunch>,
        maximum_launch_count: usize,
    ) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        if !is_valid_entity_id(&id) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance update identity is invalid",
            ));
        }
        validate_update_launches(&launches, maximum_launch_count)?;
        Ok(Self {
            id,
            source,
            launches,
        })
    }

    /// Parses one complete canonical version-one update.
    ///
    /// # Errors
    ///
    /// Refuses a malformed, noncanonical, oversized, out-of-domain, or
    /// otherwise incompatible artifact before callers can use its launches.
    pub fn parse(
        bytes: &[u8],
        maximum_launch_count: usize,
    ) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        let value: Value = hyper_strict_json::parse(
            bytes,
            usize::try_from(MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_BYTES).unwrap_or(usize::MAX),
        )
        .map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::JsonInvalid,
                "scene-performance update is not strict JSON",
                source,
            )
        })?;
        let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate,
                "scene-performance update cannot be canonicalized",
                source,
            )
        })?;
        if canonical != bytes {
            return Err(RuntimeScenePerformanceUpdateError::noncanonical());
        }
        let raw: RawUpdate = serde_json::from_value(value).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate,
                "scene-performance update shape is invalid",
                source,
            )
        })?;
        Self::from_raw(raw, maximum_launch_count)
    }

    /// Returns canonical RFC 8785 JSON bytes for durable publication.
    ///
    /// # Errors
    ///
    /// Returns a serialization refusal if canonicalization unexpectedly cannot
    /// represent this already validated version-one update.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, RuntimeScenePerformanceUpdateError> {
        let raw = RawUpdate::from_update(self);
        let value = serde_json::to_value(raw).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate,
                "scene-performance update cannot be serialized",
                source,
            )
        })?;
        serde_json_canonicalizer::to_vec(&value).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate,
                "scene-performance update cannot be canonicalized",
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
    pub const fn source(&self) -> &RuntimeScenePerformanceSource {
        &self.source
    }

    /// Returns the complete stable, tick-addressed launch set.
    #[must_use]
    pub fn launches(&self) -> &[ScenePerformanceLaunch] {
        &self.launches
    }

    /// Transfers the already validated stable launch set without copying it.
    #[must_use]
    pub fn into_launches(self) -> Vec<ScenePerformanceLaunch> {
        self.launches
    }

    fn from_raw(
        raw: RawUpdate,
        maximum_launch_count: usize,
    ) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        if raw.format != FORMAT || raw.format_version != FORMAT_VERSION {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance update format is unsupported",
            ));
        }
        if !raw.extensions.is_empty() {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance update extensions are unsupported by version one",
            ));
        }
        let source = RuntimeScenePerformanceSource::try_new(
            raw.source.project_id,
            raw.source.project_revision,
            raw.source.project_sha256,
            raw.source.runtime_bundle_sha256,
            raw.source.deployment_generation,
        )?;
        let mut launches = Vec::new();
        launches
            .try_reserve_exact(raw.launches.len())
            .map_err(|_| RuntimeScenePerformanceUpdateError::capacity())?;
        for launch in raw.launches {
            launches.push(ScenePerformanceLaunch::try_new(
                launch.scene_id,
                launch.tick,
            )?);
        }
        Self::try_new(raw.id, source, launches, maximum_launch_count)
    }
}

fn validate_update_launches(
    launches: &[ScenePerformanceLaunch],
    maximum_launch_count: usize,
) -> Result<(), RuntimeScenePerformanceUpdateError> {
    if maximum_launch_count == 0
        || maximum_launch_count > MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
    {
        return Err(RuntimeScenePerformanceUpdateError::invalid(
            "scene-performance update launch capacity is invalid",
        ));
    }
    if launches.is_empty() {
        return Err(RuntimeScenePerformanceUpdateError::invalid(
            "scene-performance update contains no launches",
        ));
    }
    if launches.len() > maximum_launch_count {
        return Err(RuntimeScenePerformanceUpdateError::limit(
            "scene-performance update exceeds its accepted launch capacity",
        ));
    }
    let mut ticks = BTreeSet::new();
    for launch in launches {
        if !is_valid_entity_id(launch.scene_id()) || launch.tick() > MAX_SAFE_INTEGER {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance update launch is outside the version-one domain",
            ));
        }
        if !ticks.insert(launch.tick()) {
            return Err(RuntimeScenePerformanceUpdateError::invalid(
                "scene-performance update repeats an arrangement tick",
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
    launches: Vec<RawLaunch>,
    extensions: std::collections::BTreeMap<String, Value>,
}

impl RawUpdate {
    fn from_update(update: &RuntimeScenePerformanceUpdate) -> Self {
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
            launches: update
                .launches
                .iter()
                .map(|launch| RawLaunch {
                    scene_id: launch.scene_id.clone(),
                    tick: launch.tick,
                })
                .collect(),
            extensions: std::collections::BTreeMap::new(),
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
struct RawLaunch {
    scene_id: String,
    tick: u64,
}

/// Classifies malformed artifacts and durable-output failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeScenePerformanceUpdateErrorKind {
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

/// A classified scene-performance update error with its underlying cause.
#[derive(Debug)]
pub struct RuntimeScenePerformanceUpdateError {
    kind: RuntimeScenePerformanceUpdateErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeScenePerformanceUpdateError {
    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> RuntimeScenePerformanceUpdateErrorKind {
        self.kind
    }

    fn invalid(message: &'static str) -> Self {
        Self::new(
            RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate,
            message,
        )
    }

    fn limit(message: &'static str) -> Self {
        Self::new(
            RuntimeScenePerformanceUpdateErrorKind::LimitExceeded,
            message,
        )
    }

    fn capacity() -> Self {
        Self::new(
            RuntimeScenePerformanceUpdateErrorKind::LimitExceeded,
            "scene-performance update capacity is unavailable",
        )
    }

    fn noncanonical() -> Self {
        Self::new(
            RuntimeScenePerformanceUpdateErrorKind::NonCanonicalJson,
            "scene-performance update is not canonical RFC 8785 JSON",
        )
    }

    fn new(kind: RuntimeScenePerformanceUpdateErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    fn with_source(
        kind: RuntimeScenePerformanceUpdateErrorKind,
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
        Self::with_source(RuntimeScenePerformanceUpdateErrorKind::Io, message, source)
    }
}

impl fmt::Display for RuntimeScenePerformanceUpdateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeScenePerformanceUpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

/// Retained authority for an explicit Runtime update-output directory.
///
/// The directory must already exist, be absolute and non-link, and remain the
/// same filesystem object from opening through durable publication.
pub struct RuntimeScenePerformanceUpdateDirectory {
    root_path: PathBuf,
    directory: Dir,
    root_identity: Handle,
}

impl RuntimeScenePerformanceUpdateDirectory {
    /// Opens an explicit pre-existing output directory without following links.
    ///
    /// # Errors
    ///
    /// Refuses relative or lexically aliased paths, link-like or non-directory
    /// endpoints, and directories which cannot be retained for durable output.
    pub fn open(path: &Path) -> Result<Self, RuntimeScenePerformanceUpdateError> {
        if !is_absolute_nonaliased(path) {
            return Err(RuntimeScenePerformanceUpdateError::new(
                RuntimeScenePerformanceUpdateErrorKind::OutputDirectoryInvalid,
                "scene-performance output directory must be an absolute non-aliased path",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "inspect scene-performance output directory",
                source,
            )
        })?;
        if is_link_like(&metadata) || !metadata.is_dir() {
            return Err(RuntimeScenePerformanceUpdateError::new(
                RuntimeScenePerformanceUpdateErrorKind::OutputDirectoryInvalid,
                "scene-performance output directory must be a regular non-link directory",
            ));
        }
        let root_identity = Handle::from_path(path).map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "retain scene-performance output directory identity",
                source,
            )
        })?;
        let directory = open_syncable_directory_ambient(path).map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "open scene-performance output directory",
                source,
            )
        })?;
        let opened = Handle::from_file(
            directory
                .try_clone()
                .map_err(|source| {
                    RuntimeScenePerformanceUpdateError::io(
                        "clone scene-performance output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "inspect retained scene-performance output directory",
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
    /// is returned as [`RuntimeScenePerformanceUpdateErrorKind::PublicationUncertain`]
    /// because the no-replace rename may already be durable.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal for directory replacement, existing update
    /// ID, filesystem failure, or a durability boundary that cannot be proven.
    pub fn publish(
        &self,
        update: &RuntimeScenePerformanceUpdate,
    ) -> Result<PathBuf, RuntimeScenePerformanceUpdateError> {
        let bytes = update.canonical_bytes()?;
        self.confirm_unchanged()?;
        let name = update_file_name(update.id());
        #[cfg(target_os = "linux")]
        self.publish_linux(&name, &bytes)?;
        #[cfg(windows)]
        self.publish_windows(&name, &bytes)?;
        #[cfg(not(any(target_os = "linux", windows)))]
        compile_error!("Runtime scene-performance publication supports Linux and Windows only");
        self.confirm_unchanged()?;
        Ok(self.root_path.join(name))
    }

    fn confirm_unchanged(&self) -> Result<(), RuntimeScenePerformanceUpdateError> {
        let opened = Handle::from_file(
            self.directory
                .try_clone()
                .map_err(|source| {
                    RuntimeScenePerformanceUpdateError::io(
                        "clone retained scene-performance output directory",
                        source,
                    )
                })?
                .into_std_file(),
        )
        .map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "inspect retained scene-performance output directory",
                source,
            )
        })?;
        Self::confirm_path(&self.root_path, &self.root_identity, &opened)
    }

    fn confirm_path(
        path: &Path,
        expected: &Handle,
        opened: &Handle,
    ) -> Result<(), RuntimeScenePerformanceUpdateError> {
        let metadata = fs::symlink_metadata(path).map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "reinspect scene-performance output directory",
                source,
            )
        })?;
        let published = Handle::from_path(path).map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "reopen scene-performance output directory identity",
                source,
            )
        })?;
        if is_link_like(&metadata)
            || !metadata.is_dir()
            || &published != expected
            || opened != expected
        {
            return Err(RuntimeScenePerformanceUpdateError::new(
                RuntimeScenePerformanceUpdateErrorKind::AuthorityChanged,
                "scene-performance output directory identity changed",
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn publish_linux(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeScenePerformanceUpdateError> {
        publish_bytes_no_replace(&self.directory, name, bytes).map_err(|source| {
            let kind = if source.kind() == io::ErrorKind::AlreadyExists {
                RuntimeScenePerformanceUpdateErrorKind::PublicationConflict
            } else {
                RuntimeScenePerformanceUpdateErrorKind::Io
            };
            RuntimeScenePerformanceUpdateError::with_source(
                kind,
                "publish scene-performance update without replacement",
                source,
            )
        })?;
        sync_directory(&self.directory).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::PublicationUncertain,
                "scene-performance update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn publish_windows(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), RuntimeScenePerformanceUpdateError> {
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
                RuntimeScenePerformanceUpdateError::io(
                    "create scene-performance update staging file",
                    source,
                )
            })?;
        let staged_identity = file
            .metadata()
            .map_err(|source| {
                RuntimeScenePerformanceUpdateError::io(
                    "inspect scene-performance update staging file",
                    source,
                )
            })
            .and_then(|metadata| {
                if metadata.is_file() {
                    Ok(identity(&metadata))
                } else {
                    Err(RuntimeScenePerformanceUpdateError::new(
                        RuntimeScenePerformanceUpdateErrorKind::Io,
                        "scene-performance update staging entry is not a regular file",
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
                RuntimeScenePerformanceUpdateError::io(
                    "write scene-performance update staging file",
                    source,
                )
            })
            .and_then(|()| {
                file.sync_all().map_err(|source| {
                    RuntimeScenePerformanceUpdateError::io(
                        "synchronize scene-performance update staging file",
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
                RuntimeScenePerformanceUpdateErrorKind::PublicationConflict
            } else {
                RuntimeScenePerformanceUpdateErrorKind::Io
            };
            let primary = RuntimeScenePerformanceUpdateError::with_source(
                kind,
                "publish scene-performance update without replacement",
                source,
            );
            return self.cleanup_windows_stage(primary, &stage_name, Some(staged_identity));
        }
        sync_directory(&self.directory).map_err(|source| {
            RuntimeScenePerformanceUpdateError::with_source(
                RuntimeScenePerformanceUpdateErrorKind::PublicationUncertain,
                "scene-performance update was published but output-directory durability is uncertain",
                source,
            )
        })
    }

    #[cfg(windows)]
    fn cleanup_windows_stage<T>(
        &self,
        primary: RuntimeScenePerformanceUpdateError,
        stage_name: &str,
        expected_identity: Option<(u64, u64)>,
    ) -> Result<T, RuntimeScenePerformanceUpdateError> {
        let Some(expected_identity) = expected_identity else {
            return Err(primary);
        };
        match remove_file_if_identity(&self.directory, stage_name, expected_identity) {
            Ok(()) => Err(primary),
            Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(primary),
            Err(cleanup) => Err(RuntimeScenePerformanceUpdateError::with_source(
                primary.kind,
                format!("{primary}; staging cleanup also failed"),
                cleanup,
            )),
        }
    }
}

/// Reads one explicit Runtime update from a stable absolute regular non-link file.
///
/// # Errors
///
/// Refuses path aliases, links, unstable file contents, malformed or
/// noncanonical JSON, and all unsupported version-one fields before returning
/// a usable update.
pub fn read_runtime_scene_performance_update(
    path: &Path,
    maximum_launch_count: usize,
) -> Result<RuntimeScenePerformanceUpdate, RuntimeScenePerformanceUpdateError> {
    let snapshot = read_stable_absolute_file(path, MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_BYTES)
        .map_err(|source| {
            RuntimeScenePerformanceUpdateError::io("read stable scene-performance update", source)
        })?;
    let (bytes, evidence) = snapshot.into_parts();
    let update = RuntimeScenePerformanceUpdate::parse(&bytes, maximum_launch_count)?;
    confirm_stable_absolute_file(path, MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_BYTES, &evidence)
        .map_err(|source| {
            RuntimeScenePerformanceUpdateError::io(
                "confirm stable scene-performance update",
                source,
            )
        })?;
    Ok(update)
}

fn update_file_name(id: &str) -> String {
    format!("{id}.hyper-scene-performance.json")
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
    const FIRST_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const SECOND_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const UPDATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hyper-scene-performance-{}", Uuid::new_v4()));
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

    fn scene(id: &str, grid_quantized: bool) -> ScenePerformanceScene {
        ScenePerformanceScene::new(id.to_owned(), grid_quantized)
    }

    fn source() -> RuntimeScenePerformanceSource {
        RuntimeScenePerformanceSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn launch(id: &str, tick: u64) -> ScenePerformanceLaunch {
        ScenePerformanceLaunch::try_new(id.to_owned(), tick).unwrap()
    }

    fn update() -> RuntimeScenePerformanceUpdate {
        RuntimeScenePerformanceUpdate::try_new(
            UPDATE_ID.to_owned(),
            source(),
            vec![launch(FIRST_SCENE_ID, 960), launch(SECOND_SCENE_ID, 1_920)],
            MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES,
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
    fn capture_accepts_only_contiguous_grid_commits_and_finalizes_stable_ids() {
        let mut capture = ScenePerformanceCapture::try_new(
            vec![scene(FIRST_SCENE_ID, true), scene(SECOND_SCENE_ID, true)],
            2,
        )
        .unwrap();
        capture.observe(1, Some(1_920), u64::MAX, false).unwrap();
        capture.observe(0, Some(960), 0, false).unwrap();

        assert_eq!(capture.captured_launch_count(), 2);
        assert_eq!(
            capture.into_launches().unwrap(),
            vec![launch(SECOND_SCENE_ID, 1_920), launch(FIRST_SCENE_ID, 960)]
        );
    }

    #[test]
    fn capture_refuses_lossy_or_ambiguous_observations_without_advancing_state() {
        let mut capture = ScenePerformanceCapture::try_new(
            vec![scene(FIRST_SCENE_ID, true), scene(SECOND_SCENE_ID, false)],
            2,
        )
        .unwrap();

        assert_eq!(
            capture.observe(1, Some(960), 1, false),
            Err(ScenePerformanceCaptureError::NonGridScene { scene_index: 1 })
        );
        assert_eq!(
            capture.observe(0, None, 1, false),
            Err(ScenePerformanceCaptureError::ImmediateLaunch { scene_index: 0 })
        );
        assert_eq!(
            capture.observe(0, Some(MAX_SAFE_INTEGER + 1), 1, false),
            Err(ScenePerformanceCaptureError::TickOutOfRange {
                scene_index: 0,
                tick: MAX_SAFE_INTEGER + 1,
            })
        );
        capture.observe(0, Some(960), 1, false).unwrap();
        assert_eq!(
            capture.observe(0, Some(960), 2, false),
            Err(ScenePerformanceCaptureError::DuplicateTick { tick: 960 })
        );
        assert_eq!(
            capture.observe(0, Some(1_920), 3, false),
            Err(ScenePerformanceCaptureError::SequenceDiscontinuity {
                expected: 2,
                actual: 3,
            })
        );
        assert_eq!(capture.captured_launch_count(), 1);
    }

    #[test]
    fn update_requires_canonical_strict_complete_provenance() {
        let update = update();
        let bytes = update.canonical_bytes().unwrap();
        assert_eq!(
            RuntimeScenePerformanceUpdate::parse(
                &bytes,
                MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
            )
            .unwrap(),
            update
        );

        let mut noncanonical = Vec::with_capacity(bytes.len() + 1);
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&bytes);
        assert_eq!(
            RuntimeScenePerformanceUpdate::parse(
                &noncanonical,
                MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
            )
            .unwrap_err()
            .kind(),
            RuntimeScenePerformanceUpdateErrorKind::NonCanonicalJson
        );

        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["launches"] = json!([
            {"scene_id": FIRST_SCENE_ID, "tick": 960},
            {"scene_id": SECOND_SCENE_ID, "tick": 960}
        ]);
        let duplicate_tick = serde_json_canonicalizer::to_vec(&value).unwrap();
        assert_eq!(
            RuntimeScenePerformanceUpdate::parse(
                &duplicate_tick,
                MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
            )
            .unwrap_err()
            .kind(),
            RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate
        );
    }

    #[test]
    fn update_directory_publishes_once_and_reader_rechecks_the_file() {
        let temporary = TemporaryDirectory::new();
        let directory = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let update = update();
        let output = directory.publish(&update).unwrap();
        assert_eq!(
            fs::read(&output).unwrap(),
            update.canonical_bytes().unwrap()
        );
        assert_eq!(
            read_runtime_scene_performance_update(
                &output,
                MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES
            )
            .unwrap(),
            update
        );
        assert_eq!(
            directory.publish(&update).unwrap_err().kind(),
            RuntimeScenePerformanceUpdateErrorKind::PublicationConflict
        );
    }

    #[test]
    fn update_refuses_an_empty_or_over_limit_launch_set() {
        assert_eq!(
            RuntimeScenePerformanceUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                Vec::new(),
                MAX_RUNTIME_SCENE_PERFORMANCE_UPDATE_LAUNCHES,
            )
            .unwrap_err()
            .kind(),
            RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate
        );
        assert_eq!(
            RuntimeScenePerformanceUpdate::try_new(
                UPDATE_ID.to_owned(),
                source(),
                vec![launch(FIRST_SCENE_ID, 960)],
                0,
            )
            .unwrap_err()
            .kind(),
            RuntimeScenePerformanceUpdateErrorKind::InvalidUpdate
        );
    }
}
