#![doc = "Strict bounded JSON adapter for verified Hyper Module playback resources."]
#![forbid(unsafe_code)]

mod automation;

pub use automation::{
    AutomationChannelParameter, AutomationCompileError, AutomationEvaluationError,
    AutomationInterpolation, AutomationLane, AutomationMainParameter, AutomationPoint,
    AutomationSendParameter, AutomationTake, AutomationTarget, AutomationValue, CompiledAutomation,
    LoadedAutomationSet,
};

use core::error::Error;
use core::fmt;
use std::collections::{BTreeSet, HashMap};

use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
use hyper_clip_engine::compile_native_clip_gain;
use hyper_playback_compiler::graph::{
    ArrangementResource, AssetResource, ClipResource, CompiledSchedule, GraphError, GraphInput,
    GraphLimits, PlacementResource, TrackKind, TrackResource, compile_arrangement,
};
use hyper_playback_compiler::scene::{
    CompiledScenePerformance, LaunchQuantization, SceneClipResource, SceneCompileError,
    SceneLaunchResource, SceneLimits, ScenePerformanceInput, SceneResource, SceneSlotAction,
    SceneSlotResource, SceneTrackResource, compile_scene_performance,
};
use hyper_playback_compiler::{
    ClipCompileSpec, CompileError, CompiledPlacement, CompiledSceneAudioClip, PlacementCompileSpec,
    TempoEvent, TickRange, Timeline, TimelineError, WarpMarker,
    compile_interactive_scene_audio_clip, compile_placement, compile_scene_audio_clip,
};
use hyper_project_model::{is_valid_entity_id, is_valid_extension_key};
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use hyper_strict_json::{self as strict, StrictJsonError, StrictJsonErrorKind};

const VERSION_1_SAMPLE_RATE_HZ: u32 = 48_000;
const VERSION_1_TICKS_PER_QUARTER: u64 = 1_920;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_VERSION_1_TIMELINE_EVENTS: usize = 65_536;
const MAX_VERSION_1_NAME_CHARACTERS: usize = 120;
const MAX_VERSION_1_SENDS_PER_CHANNEL: usize = 2;

#[derive(Debug)]
struct ExtensionsDocument(BTreeSet<String>);

impl<'de> Deserialize<'de> for ExtensionsDocument {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ExtensionsVisitor)
    }
}

struct ExtensionsVisitor;

impl<'de> Visitor<'de> for ExtensionsVisitor {
    type Value = ExtensionsDocument;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a version 1 extensions object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some((key, _)) = map.next_entry::<String, IgnoredAny>()? {
            keys.insert(key);
        }
        Ok(ExtensionsDocument(keys))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentKind {
    Timeline,
    Track,
    Clip,
    Scene,
    Arrangement,
    Mixer,
    AutomationTake,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentLocation {
    pub kind: DocumentKind,
    pub index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonRejection {
    EmptyDocument,
    DocumentTooLarge,
    Utf8Bom,
    InvalidJson,
    DuplicateObjectName,
    NonInteroperableInteger,
    NegativeZero,
    TypedDocumentMismatch,
}

impl From<StrictJsonErrorKind> for JsonRejection {
    fn from(kind: StrictJsonErrorKind) -> Self {
        match kind {
            StrictJsonErrorKind::EmptyDocument => Self::EmptyDocument,
            StrictJsonErrorKind::DocumentTooLarge => Self::DocumentTooLarge,
            StrictJsonErrorKind::Utf8Bom => Self::Utf8Bom,
            StrictJsonErrorKind::InvalidJson => Self::InvalidJson,
            StrictJsonErrorKind::DuplicateObjectName => Self::DuplicateObjectName,
            StrictJsonErrorKind::NonInteroperableInteger => Self::NonInteroperableInteger,
            StrictJsonErrorKind::NegativeZero => Self::NegativeZero,
            StrictJsonErrorKind::TypedDocumentMismatch => Self::TypedDocumentMismatch,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionLimit {
    TrackDocuments,
    ClipDocuments,
    SceneDocuments,
    TrackClipReferences,
    WarpMarkers,
    SceneSlots,
    Placements,
    SceneLaunches,
    MixerChannels,
    MixerSends,
    ReturnBuses,
    AutomationDocuments,
    AutomationLanes,
    AutomationPoints,
}

#[derive(Debug, Eq, PartialEq)]
pub enum LoadErrorKind {
    Json {
        rejection: JsonRejection,
        line: usize,
        column: usize,
    },
    LimitExceeded {
        collection: CollectionLimit,
        actual: usize,
        maximum: usize,
    },
    InvalidProjectIdentity,
    ForeignProject,
    InvalidFormat,
    UnsupportedVersion,
    InvalidVersionOneDomain,
    DuplicateEntityId,
    CapacityUnavailable,
}

#[derive(Debug, Eq, PartialEq)]
pub struct LoadError {
    pub location: Option<DocumentLocation>,
    pub kind: LoadErrorKind,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(location) = self.location {
            write!(
                formatter,
                "{:?} document {}: {}",
                location.kind, location.index, self.message
            )
        } else {
            formatter.write_str(&self.message)
        }
    }
}

impl Error for LoadError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoaderLimits {
    pub max_document_bytes: usize,
    pub max_track_documents: usize,
    pub max_clip_documents: usize,
    pub max_scene_documents: usize,
    pub max_track_clip_references: usize,
    pub max_warp_markers: usize,
    pub max_scene_slots: usize,
    pub max_placements: usize,
    pub max_scene_launches: usize,
    pub max_mixer_channels: usize,
    pub max_mixer_sends: usize,
    pub max_return_buses: usize,
    pub max_automation_documents: usize,
    pub max_automation_lanes: usize,
    pub max_automation_points: usize,
}

impl LoaderLimits {
    pub const VERSION_1: Self = Self {
        max_document_bytes: 1_048_576,
        max_track_documents: 8,
        max_clip_documents: 4_096,
        max_scene_documents: 8,
        max_track_clip_references: 4_096,
        max_warp_markers: 65_536,
        max_scene_slots: 8,
        max_placements: 65_536,
        max_scene_launches: 65_536,
        max_mixer_channels: 8,
        max_mixer_sends: 16,
        max_return_buses: 2,
        max_automation_documents: 4_096,
        max_automation_lanes: 4_096,
        max_automation_points: 65_536,
    };
}

#[derive(Clone, Copy, Debug)]
pub struct PlaybackJsonInput<'a> {
    pub project_id: &'a str,
    pub sample_rate_hz: u32,
    pub timeline: &'a [u8],
    pub tracks: &'a [&'a [u8]],
    pub clips: &'a [&'a [u8]],
    pub scenes: &'a [&'a [u8]],
    pub arrangement: Option<&'a [u8]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaAssetFact<'a> {
    pub id: &'a str,
    pub sample_rate_hz: u32,
    pub frame_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioClipGainValidationView<'a> {
    asset_id: &'a str,
    source_start_frame: usize,
    source_frame_count: usize,
}

impl AudioClipGainValidationView<'_> {
    #[must_use]
    pub const fn asset_id(&self) -> &str {
        self.asset_id
    }

    #[must_use]
    pub const fn source_start_frame(&self) -> usize {
        self.source_start_frame
    }

    #[must_use]
    pub const fn source_frame_count(&self) -> usize {
        self.source_frame_count
    }
}

#[derive(Clone, Debug)]
struct LoadedTrack {
    id: String,
    name: String,
    kind: TrackKind,
    order: u8,
    active: bool,
    parent_track_id: Option<String>,
    clip_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct LoadedClip {
    id: String,
    name: String,
    track_id: String,
    asset_id: String,
    enabled: bool,
    duration_ticks: u64,
    source_start_frame: usize,
    source_frame_count: usize,
    loop_enabled: bool,
    loop_range: TickRange,
    reverse: bool,
    gain_decibels: f64,
    transpose_cents: i64,
    fade_in_ticks: u64,
    fade_out_ticks: u64,
    warp_markers: Vec<WarpMarker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LoadedPlacement {
    id: String,
    track_id: String,
    clip_id: String,
    start_tick: u64,
    clip_start_tick: u64,
    duration_ticks: u64,
    enabled: bool,
}

#[derive(Clone, Debug)]
enum LoadedSceneSlotAction {
    Play { clip_id: String },
    Stop,
}

#[derive(Clone, Debug)]
struct LoadedSceneSlot {
    track_id: String,
    action: LoadedSceneSlotAction,
}

#[derive(Clone, Debug)]
struct LoadedScene {
    id: String,
    name: String,
    enabled: bool,
    order: u8,
    launch_quantization: LaunchQuantization,
    slots: Vec<LoadedSceneSlot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LoadedSceneLaunch {
    id: String,
    tick: u64,
    scene_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LoadedArrangement {
    id: String,
    enabled: bool,
    placements: Vec<LoadedPlacement>,
    scene_launches: Vec<LoadedSceneLaunch>,
}

#[derive(Clone, Debug)]
pub struct LoadedPlaybackProject {
    project_id: String,
    sample_rate_hz: u32,
    timeline_id: String,
    tempo_events: Vec<TempoEvent>,
    tracks: Vec<LoadedTrack>,
    clips: Vec<LoadedClip>,
    scenes: Vec<LoadedScene>,
    arrangement: Option<LoadedArrangement>,
}

#[derive(Clone, Debug)]
enum MixerDestination {
    Main,
    Track(String),
}

#[derive(Clone, Debug)]
struct LoadedMixerChannel {
    track_id: String,
    track_kind: TrackKind,
    parameters: ChannelParameters,
    solo: bool,
    destination: MixerDestination,
    sends: Vec<LoadedMixerSend>,
}

#[derive(Clone, Debug)]
struct LoadedMixerSend {
    id: String,
    bus: MixerBus,
    tap: MixerTap,
    parameters: ChannelParameters,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum MixerBus {
    A,
    B,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MixerTap {
    PreFader,
    PostFader,
}

#[derive(Clone, Debug)]
pub struct LoadedMixer {
    project_id: String,
    id: String,
    channels: Vec<LoadedMixerChannel>,
    return_buses: Vec<(MixerBus, String)>,
    main_gain: LinearGain,
    main_muted: bool,
    cue_source: CueSource,
    cue_gain: LinearGain,
    cue_muted: bool,
}

/// Persistable base parameters for one mixer send.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerSendBaseState {
    pub id: String,
    pub gain: LinearGain,
    pub pan: Pan,
    pub enabled: bool,
}

impl Eq for MixerSendBaseState {}

/// Persistable base parameters for one mixer channel in compiled order.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerChannelBaseState {
    pub track_id: String,
    pub gain: LinearGain,
    pub pan: Pan,
    pub muted: bool,
    pub solo: bool,
    pub sends: Vec<MixerSendBaseState>,
}

impl Eq for MixerChannelBaseState {}

/// Complete persistable live base state for one mixer resource.
#[derive(Clone, Debug, PartialEq)]
pub struct MixerBaseState {
    pub channels: Vec<MixerChannelBaseState>,
    pub main_gain: LinearGain,
    pub main_muted: bool,
}

impl Eq for MixerBaseState {}

#[derive(Debug)]
pub enum MixerStateSerializationError {
    Source(LoadError),
    NonCanonicalSource,
    MixerIdentityMismatch,
    ChannelIdentityMismatch {
        channel_index: usize,
    },
    SendIdentityMismatch {
        channel_index: usize,
        send_index: usize,
    },
    ChannelCountMismatch {
        document: usize,
        state: usize,
    },
    SendCountMismatch {
        channel_index: usize,
        document: usize,
        state: usize,
    },
    InvalidDocumentShape,
    Serialization(serde_json::Error),
}

impl fmt::Display for MixerStateSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => source.fmt(formatter),
            Self::NonCanonicalSource => {
                formatter.write_str("source mixer is not canonical RFC 8785 JSON")
            }
            Self::MixerIdentityMismatch => {
                formatter.write_str("source mixer identity does not match the save request")
            }
            Self::ChannelIdentityMismatch { channel_index } => write!(
                formatter,
                "mixer channel {channel_index} identity does not match the live layout"
            ),
            Self::SendIdentityMismatch {
                channel_index,
                send_index,
            } => write!(
                formatter,
                "mixer channel {channel_index} send {send_index} identity does not match the live layout"
            ),
            Self::ChannelCountMismatch { document, state } => write!(
                formatter,
                "mixer state has {state} channels; source mixer has {document}"
            ),
            Self::SendCountMismatch {
                channel_index,
                document,
                state,
            } => write!(
                formatter,
                "mixer channel {channel_index} state has {state} sends; source mixer has {document}"
            ),
            Self::InvalidDocumentShape => {
                formatter.write_str("source mixer shape changed after typed validation")
            }
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for MixerStateSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::NonCanonicalSource
            | Self::MixerIdentityMismatch
            | Self::ChannelIdentityMismatch { .. }
            | Self::SendIdentityMismatch { .. }
            | Self::ChannelCountMismatch { .. }
            | Self::SendCountMismatch { .. }
            | Self::InvalidDocumentShape => None,
        }
    }
}

/// Canonical version-1 resources required to append one audio track.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackAdditionDocuments {
    track: Vec<u8>,
    mixer: Vec<u8>,
}

impl AudioTrackAdditionDocuments {
    #[must_use]
    pub fn track(&self) -> &[u8] {
        &self.track
    }

    #[must_use]
    pub fn mixer(&self) -> &[u8] {
        &self.mixer
    }

    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, Vec<u8>) {
        (self.track, self.mixer)
    }
}

#[derive(Debug)]
pub enum AudioTrackAdditionSerializationError {
    Source(LoadError),
    TrackCapacity,
    InvalidTrackIdentity,
    MixerIdentityMismatch,
    InvalidTrackName,
    InvalidDocumentShape,
    Serialization(serde_json::Error),
}

#[derive(Debug)]
pub enum AudioTrackRenameSerializationError {
    InvalidTrackName,
    Document(AudioTrackDocumentSerializationError),
}

#[derive(Debug)]
pub enum AudioTrackDocumentSerializationError {
    Source(LoadError),
    NonCanonicalSource,
    TrackMissing,
    TrackNotAudio,
    SourceTrackMismatch,
    InvalidDocumentShape,
    Serialization(serde_json::Error),
}

#[derive(Debug)]
pub enum ArrangementPlacementDocumentSerializationError {
    Source(LoadError),
    Compilation(CompileError),
    NonCanonicalSource,
    ArrangementMissing,
    SourceArrangementMismatch,
    PlacementMissing,
    InvalidDocumentShape,
    Serialization(serde_json::Error),
}

pub type ArrangementPlacementActivationSerializationError =
    ArrangementPlacementDocumentSerializationError;
pub type ArrangementPlacementMoveSerializationError =
    ArrangementPlacementDocumentSerializationError;
pub type ArrangementPlacementDurationSerializationError =
    ArrangementPlacementDocumentSerializationError;
pub type ArrangementPlacementFrontTrimSerializationError =
    ArrangementPlacementDocumentSerializationError;

#[derive(Debug)]
pub enum AudioClipDocumentSerializationError {
    Source(LoadError),
    NonCanonicalSource,
    ClipMissing,
    SourceClipMismatch,
    InvalidDocumentShape,
    Serialization(serde_json::Error),
}

#[derive(Debug)]
pub enum AudioClipGainSerializationError {
    UnsupportedGain,
    Document(AudioClipDocumentSerializationError),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AudioTrackRemovalBlockers {
    pub clip_references: usize,
    pub child_tracks: usize,
    pub scene_slots: usize,
    pub arrangement_placements: usize,
}

impl AudioTrackRemovalBlockers {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.clip_references == 0
            && self.child_tracks == 0
            && self.scene_slots == 0
            && self.arrangement_placements == 0
    }
}

#[derive(Debug)]
pub enum AudioTrackRemovalSerializationError {
    TrackMissing,
    TrackNotAudio,
    Blocked(AudioTrackRemovalBlockers),
    MixerIdentityMismatch,
    InvalidDocumentShape,
    Source(LoadError),
    Serialization(serde_json::Error),
}

impl fmt::Display for AudioTrackRemovalSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrackMissing => formatter.write_str("the audio track does not exist"),
            Self::TrackNotAudio => formatter.write_str("the selected track is not an audio track"),
            Self::Blocked(blockers) => write!(
                formatter,
                "the audio track is still referenced by {} clips, {} child tracks, {} scene slots, and {} arrangement placements",
                blockers.clip_references,
                blockers.child_tracks,
                blockers.scene_slots,
                blockers.arrangement_placements
            ),
            Self::MixerIdentityMismatch => {
                formatter.write_str("source mixer identity does not match the removal request")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("source mixer has an invalid document shape")
            }
            Self::Source(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackRemovalSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::TrackMissing
            | Self::TrackNotAudio
            | Self::Blocked(_)
            | Self::MixerIdentityMismatch
            | Self::InvalidDocumentShape => None,
        }
    }
}

impl fmt::Display for AudioTrackRenameSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTrackName => {
                formatter.write_str("the audio-track name is outside the version-1 domain")
            }
            Self::Document(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackRenameSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Document(source) => Some(source),
            Self::InvalidTrackName => None,
        }
    }
}

impl fmt::Display for AudioTrackDocumentSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => source.fmt(formatter),
            Self::NonCanonicalSource => {
                formatter.write_str("source audio track is not canonical RFC 8785 JSON")
            }
            Self::TrackMissing => {
                formatter.write_str("the selected audio track is not present in the project")
            }
            Self::TrackNotAudio => {
                formatter.write_str("the selected structural track is not an audio track")
            }
            Self::SourceTrackMismatch => formatter
                .write_str("source audio-track bytes do not match the loaded project structure"),
            Self::InvalidDocumentShape => {
                formatter.write_str("source audio-track shape changed after typed validation")
            }
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackDocumentSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::NonCanonicalSource
            | Self::TrackMissing
            | Self::TrackNotAudio
            | Self::SourceTrackMismatch
            | Self::InvalidDocumentShape => None,
        }
    }
}

impl fmt::Display for ArrangementPlacementDocumentSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => source.fmt(formatter),
            Self::Compilation(source) => source.fmt(formatter),
            Self::NonCanonicalSource => {
                formatter.write_str("source arrangement is not canonical RFC 8785 JSON")
            }
            Self::ArrangementMissing => {
                formatter.write_str("the project has no arrangement resource")
            }
            Self::SourceArrangementMismatch => formatter
                .write_str("source arrangement bytes do not match the loaded project structure"),
            Self::PlacementMissing => {
                formatter.write_str("the selected arrangement placement is not present")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("source arrangement shape changed after typed validation")
            }
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for ArrangementPlacementDocumentSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Compilation(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::NonCanonicalSource
            | Self::ArrangementMissing
            | Self::SourceArrangementMismatch
            | Self::PlacementMissing
            | Self::InvalidDocumentShape => None,
        }
    }
}

impl fmt::Display for AudioClipDocumentSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => source.fmt(formatter),
            Self::NonCanonicalSource => {
                formatter.write_str("source audio clip is not canonical RFC 8785 JSON")
            }
            Self::ClipMissing => {
                formatter.write_str("the selected audio clip is not present in the project")
            }
            Self::SourceClipMismatch => {
                formatter.write_str("source audio-clip bytes do not match the loaded project")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("source audio-clip shape changed after typed validation")
            }
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioClipDocumentSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::NonCanonicalSource
            | Self::ClipMissing
            | Self::SourceClipMismatch
            | Self::InvalidDocumentShape => None,
        }
    }
}

impl fmt::Display for AudioClipGainSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedGain => formatter.write_str(
                "audio-clip gain must be finite and no greater than the native +6.0206 dB limit",
            ),
            Self::Document(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioClipGainSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Document(source) => Some(source),
            Self::UnsupportedGain => None,
        }
    }
}

impl fmt::Display for AudioTrackAdditionSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Source(source) => source.fmt(formatter),
            Self::TrackCapacity => {
                formatter.write_str("the version-1 project cannot append another audio track")
            }
            Self::InvalidTrackIdentity => {
                formatter.write_str("the new audio-track identity is invalid or already in use")
            }
            Self::MixerIdentityMismatch => {
                formatter.write_str("source mixer identity does not match the edit request")
            }
            Self::InvalidTrackName => {
                formatter.write_str("the new audio-track name is outside the version-1 domain")
            }
            Self::InvalidDocumentShape => {
                formatter.write_str("source mixer shape changed after typed validation")
            }
            Self::Serialization(source) => source.fmt(formatter),
        }
    }
}

impl Error for AudioTrackAdditionSerializationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::TrackCapacity
            | Self::InvalidTrackIdentity
            | Self::MixerIdentityMismatch
            | Self::InvalidTrackName
            | Self::InvalidDocumentShape => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueSource {
    Main,
    Solo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMainMixer {
    project_id: String,
    slots: Vec<CompiledMixerSlot>,
    master_gain: LinearGain,
}

impl CompiledMainMixer {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn slots(&self) -> &[CompiledMixerSlot] {
        &self.slots
    }

    #[must_use]
    pub const fn master_gain(&self) -> LinearGain {
        self.master_gain
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMixerSlot {
    track_id: String,
    parameters: ChannelParameters,
}

impl CompiledMixerSlot {
    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub const fn parameters(&self) -> ChannelParameters {
        self.parameters
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMixerGraph {
    project_id: String,
    slots: Vec<CompiledGraphSlot>,
    processing_order: Vec<usize>,
    master_gain: LinearGain,
    main_gain: LinearGain,
    main_muted: bool,
}

impl CompiledMixerGraph {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn slots(&self) -> &[CompiledGraphSlot] {
        &self.slots
    }

    #[must_use]
    pub fn processing_order(&self) -> &[usize] {
        &self.processing_order
    }

    #[must_use]
    pub const fn master_gain(&self) -> LinearGain {
        self.master_gain
    }

    #[must_use]
    pub const fn main_gain(&self) -> LinearGain {
        self.main_gain
    }

    #[must_use]
    pub const fn main_muted(&self) -> bool {
        self.main_muted
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledGraphSlot {
    track_id: String,
    kind: CompiledMixerTrackKind,
    parameters: ChannelParameters,
    muted: bool,
    solo: bool,
    destination: CompiledMixerDestination,
    sends: Vec<CompiledMixerSend>,
}

impl CompiledGraphSlot {
    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub const fn kind(&self) -> CompiledMixerTrackKind {
        self.kind
    }

    #[must_use]
    pub const fn parameters(&self) -> ChannelParameters {
        self.parameters
    }

    #[must_use]
    pub const fn muted(&self) -> bool {
        self.muted
    }

    #[must_use]
    pub const fn solo(&self) -> bool {
        self.solo
    }

    #[must_use]
    pub const fn destination(&self) -> CompiledMixerDestination {
        self.destination
    }

    #[must_use]
    pub fn sends(&self) -> &[CompiledMixerSend] {
        &self.sends
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledMixerTrackKind {
    Audio,
    Group,
    Return,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledMixerDestination {
    Main,
    Slot(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMixerSend {
    id: String,
    target_slot: usize,
    tap: CompiledMixerTap,
    parameters: ChannelParameters,
    enabled: bool,
}

impl CompiledMixerSend {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn target_slot(&self) -> usize {
        self.target_slot
    }

    #[must_use]
    pub const fn tap(&self) -> CompiledMixerTap {
        self.tap
    }

    #[must_use]
    pub const fn parameters(&self) -> ChannelParameters {
        self.parameters
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompiledMixerTap {
    PreFader,
    PostFader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerCompileError {
    CapacityUnavailable,
    InvalidGraph,
    UnsupportedPrimaryRouting,
    UnsupportedSendsOrReturns,
    UnsupportedExternalInput,
}

impl fmt::Display for MixerCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("compiled mixer capacity is unavailable")
            }
            Self::InvalidGraph => {
                formatter.write_str("validated mixer graph could not be resolved")
            }
            Self::UnsupportedPrimaryRouting => {
                formatter.write_str("track or group primary routing is not executable")
            }
            Self::UnsupportedSendsOrReturns => {
                formatter.write_str("send and return routing is not executable")
            }
            Self::UnsupportedExternalInput => {
                formatter.write_str("external-input mixer sources are not executable")
            }
        }
    }
}

impl Error for MixerCompileError {}

/// Rewrites only persisted base parameters in one canonical mixer document.
///
/// Channel, send, project, and mixer identities must match the live compiled
/// layout supplied by the caller. Routing, return buses, cue state, and
/// extension values are preserved. The returned document is canonical RFC
/// 8785 JSON and is re-parsed through the strict typed boundary before it is
/// returned.
///
/// # Errors
///
/// Returns a classified error for invalid or noncanonical source bytes,
/// identity or layout disagreement, an exceeded version-1 bound, or failed
/// canonical serialization.
#[allow(
    clippy::too_many_lines,
    reason = "identity, bounded layout validation, selective mutation, and strict output reparse remain visible as one serialization boundary"
)]
pub fn serialize_mixer_base_state(
    source: &[u8],
    project_id: &str,
    mixer_id: &str,
    state: &MixerBaseState,
    limits: LoaderLimits,
) -> Result<Vec<u8>, MixerStateSerializationError> {
    let location = DocumentLocation {
        kind: DocumentKind::Mixer,
        index: 0,
    };
    let document: MixerDocument = parse_document(source, location, limits.max_document_bytes)
        .map_err(MixerStateSerializationError::Source)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-mixer",
        &document.project_id,
        project_id,
        location,
    )
    .map_err(MixerStateSerializationError::Source)?;
    validate_contract_document(&document.id, None, &document.extensions, location)
        .map_err(MixerStateSerializationError::Source)?;
    if document.id != mixer_id {
        return Err(MixerStateSerializationError::MixerIdentityMismatch);
    }
    enforce_loader_limit(
        CollectionLimit::MixerChannels,
        document.channels.len(),
        limits.max_mixer_channels,
        Some(location),
    )
    .map_err(MixerStateSerializationError::Source)?;
    let send_count = document
        .channels
        .iter()
        .try_fold(0_usize, |total, channel| {
            total.checked_add(channel.sends.len())
        })
        .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
    enforce_loader_limit(
        CollectionLimit::MixerSends,
        send_count,
        limits.max_mixer_sends,
        Some(location),
    )
    .map_err(MixerStateSerializationError::Source)?;
    if document.channels.len() != state.channels.len() {
        return Err(MixerStateSerializationError::ChannelCountMismatch {
            document: document.channels.len(),
            state: state.channels.len(),
        });
    }
    for (channel_index, (document_channel, state_channel)) in
        document.channels.iter().zip(&state.channels).enumerate()
    {
        if document_channel.track_id != state_channel.track_id {
            return Err(MixerStateSerializationError::ChannelIdentityMismatch { channel_index });
        }
        if document_channel.sends.len() != state_channel.sends.len() {
            return Err(MixerStateSerializationError::SendCountMismatch {
                channel_index,
                document: document_channel.sends.len(),
                state: state_channel.sends.len(),
            });
        }
        for (send_index, (document_send, state_send)) in document_channel
            .sends
            .iter()
            .zip(&state_channel.sends)
            .enumerate()
        {
            if document_send.id != state_send.id {
                return Err(MixerStateSerializationError::SendIdentityMismatch {
                    channel_index,
                    send_index,
                });
            }
        }
    }

    let mut value: Value =
        serde_json::from_slice(source).map_err(MixerStateSerializationError::Serialization)?;
    let canonical_source = serde_json_canonicalizer::to_vec(&value)
        .map_err(MixerStateSerializationError::Serialization)?;
    if canonical_source != source {
        return Err(MixerStateSerializationError::NonCanonicalSource);
    }
    let channels = value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
    for (channel, state_channel) in channels.iter_mut().zip(&state.channels) {
        let channel = channel
            .as_object_mut()
            .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
        channel.insert(
            "gain_millionths".to_owned(),
            state_channel.gain.to_millionths().into(),
        );
        channel.insert(
            "pan_millionths".to_owned(),
            state_channel.pan.to_millionths().into(),
        );
        channel.insert("mute".to_owned(), state_channel.muted.into());
        channel.insert("solo".to_owned(), state_channel.solo.into());
        let sends = channel
            .get_mut("sends")
            .and_then(Value::as_array_mut)
            .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
        for (send, state_send) in sends.iter_mut().zip(&state_channel.sends) {
            let send = send
                .as_object_mut()
                .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
            send.insert(
                "gain_millionths".to_owned(),
                state_send.gain.to_millionths().into(),
            );
            send.insert(
                "pan_millionths".to_owned(),
                state_send.pan.to_millionths().into(),
            );
            send.insert("enabled".to_owned(), state_send.enabled.into());
        }
    }
    let main = value
        .get_mut("main")
        .and_then(Value::as_object_mut)
        .ok_or(MixerStateSerializationError::InvalidDocumentShape)?;
    main.insert(
        "gain_millionths".to_owned(),
        state.main_gain.to_millionths().into(),
    );
    main.insert("mute".to_owned(), state.main_muted.into());

    let output = serde_json_canonicalizer::to_vec(&value)
        .map_err(MixerStateSerializationError::Serialization)?;
    let _: MixerDocument = parse_document(&output, location, limits.max_document_bytes)
        .map_err(MixerStateSerializationError::Source)?;
    Ok(output)
}

/// Builds and validates the canonical track and mixer documents for appending
/// one active audio track at the end of a loaded version-1 project.
///
/// The supplied source mixer is preserved while a new unity-gain, center-pan
/// channel is appended. Routing, sends, cue state, and namespaced extension
/// values are unchanged.
///
/// # Errors
///
/// Returns a classified rejection for invalid identity or name input, exhausted
/// version-1 capacity, malformed source bytes, or a
/// candidate mixer that does not resolve against the extended project.
#[allow(
    clippy::too_many_arguments,
    reason = "the edit boundary explicitly binds project, media, mixer, live-state, and new-track identities"
)]
pub fn serialize_audio_track_addition(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source_mixer: &[u8],
    mixer_id: &str,
    track_id: &str,
    track_name: &str,
    limits: LoaderLimits,
) -> Result<AudioTrackAdditionDocuments, AudioTrackAdditionSerializationError> {
    let next_order = project
        .tracks
        .iter()
        .map(|track| track.order)
        .max()
        .map_or(Some(0), |order| order.checked_add(1))
        .filter(|order| *order <= 7)
        .ok_or(AudioTrackAdditionSerializationError::TrackCapacity)?;
    if project.tracks.len() >= limits.max_track_documents {
        return Err(AudioTrackAdditionSerializationError::TrackCapacity);
    }
    let name_length = track_name.chars().count();
    if track_name.trim() != track_name
        || !(1..=MAX_VERSION_1_NAME_CHARACTERS).contains(&name_length)
    {
        return Err(AudioTrackAdditionSerializationError::InvalidTrackName);
    }
    if !is_valid_entity_id(track_id)
        || track_id == project.project_id
        || project_contains_identity(project, track_id)
        || assets.iter().any(|asset| asset.id == track_id)
    {
        return Err(AudioTrackAdditionSerializationError::InvalidTrackIdentity);
    }

    let loaded_mixer = project
        .load_mixer(source_mixer, assets, limits)
        .map_err(AudioTrackAdditionSerializationError::Source)?;
    if loaded_mixer.id() != mixer_id {
        return Err(AudioTrackAdditionSerializationError::MixerIdentityMismatch);
    }

    let track = serde_json_canonicalizer::to_vec(&json!({
        "format": "hyper-track",
        "format_version": 1,
        "id": track_id,
        "project_id": project.project_id,
        "name": track_name,
        "kind": "audio",
        "order": next_order,
        "active": true,
        "clip_ids": [],
        "extensions": {}
    }))
    .map_err(AudioTrackAdditionSerializationError::Serialization)?;
    let added = load_track(
        &track,
        project.tracks.len(),
        &project.project_id,
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackAdditionSerializationError::Source)?;
    let mut extended = project.clone();
    extended
        .tracks
        .try_reserve(1)
        .map_err(|_| AudioTrackAdditionSerializationError::Source(capacity_error()))?;
    extended.tracks.push(added);
    validate_track_hierarchy(&extended.tracks)
        .map_err(AudioTrackAdditionSerializationError::Source)?;
    validate_loaded_identities(&extended).map_err(AudioTrackAdditionSerializationError::Source)?;

    let mut mixer_value: Value = serde_json::from_slice(source_mixer)
        .map_err(AudioTrackAdditionSerializationError::Serialization)?;
    let channels = mixer_value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackAdditionSerializationError::InvalidDocumentShape)?;
    channels.push(json!({
        "track_id": track_id,
        "gain_millionths": 1_000_000,
        "pan_millionths": 0,
        "mute": false,
        "solo": false,
        "destination": {"kind": "main"},
        "sends": []
    }));
    let mixer = serde_json_canonicalizer::to_vec(&mixer_value)
        .map_err(AudioTrackAdditionSerializationError::Serialization)?;
    extended
        .load_mixer(&mixer, assets, limits)
        .map_err(AudioTrackAdditionSerializationError::Source)?;
    Ok(AudioTrackAdditionDocuments { track, mixer })
}

/// Builds the canonical mixer document produced by removing one empty,
/// structurally unreferenced audio track.
///
/// # Errors
///
/// Returns a classified rejection when the track is missing, is not audio, is
/// referenced by another playback document, or the source mixer cannot be
/// validated and reduced by exactly one matching channel.
pub fn serialize_audio_track_removal(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    source_mixer: &[u8],
    mixer_id: &str,
    track_id: &str,
    limits: LoaderLimits,
) -> Result<Vec<u8>, AudioTrackRemovalSerializationError> {
    let track_index = project
        .track_index(track_id)
        .ok_or(AudioTrackRemovalSerializationError::TrackMissing)?;
    if !project.is_audio_track(track_id) {
        return Err(AudioTrackRemovalSerializationError::TrackNotAudio);
    }
    let blockers = project.audio_track_removal_blockers(track_id);
    if !blockers.is_empty() {
        return Err(AudioTrackRemovalSerializationError::Blocked(blockers));
    }
    let loaded_mixer = project
        .load_mixer(source_mixer, assets, limits)
        .map_err(AudioTrackRemovalSerializationError::Source)?;
    if loaded_mixer.id() != mixer_id {
        return Err(AudioTrackRemovalSerializationError::MixerIdentityMismatch);
    }
    let channel_index = loaded_mixer
        .channels
        .iter()
        .position(|channel| channel.track_id == track_id)
        .ok_or(AudioTrackRemovalSerializationError::InvalidDocumentShape)?;
    if loaded_mixer.channels.iter().any(|channel| {
        channel.track_id != track_id
            && matches!(&channel.destination, MixerDestination::Track(id) if id == track_id)
    }) {
        return Err(AudioTrackRemovalSerializationError::InvalidDocumentShape);
    }
    let mut reduced = project.clone();
    reduced.tracks.remove(track_index);
    validate_track_hierarchy(&reduced.tracks)
        .map_err(AudioTrackRemovalSerializationError::Source)?;
    validate_loaded_identities(&reduced).map_err(AudioTrackRemovalSerializationError::Source)?;

    let mut mixer_value: Value = serde_json::from_slice(source_mixer)
        .map_err(AudioTrackRemovalSerializationError::Serialization)?;
    let channels = mixer_value
        .get_mut("channels")
        .and_then(Value::as_array_mut)
        .ok_or(AudioTrackRemovalSerializationError::InvalidDocumentShape)?;
    if channels
        .get(channel_index)
        .and_then(|channel| channel["track_id"].as_str())
        != Some(track_id)
    {
        return Err(AudioTrackRemovalSerializationError::InvalidDocumentShape);
    }
    channels.remove(channel_index);
    let mixer = serde_json_canonicalizer::to_vec(&mixer_value)
        .map_err(AudioTrackRemovalSerializationError::Serialization)?;
    reduced
        .load_mixer(&mixer, assets, limits)
        .map_err(AudioTrackRemovalSerializationError::Source)?;
    Ok(mixer)
}

/// Replaces only the display name in one validated canonical audio-track
/// document while preserving its identity, structure, clip ownership, and
/// namespaced extensions.
///
/// # Errors
///
/// Returns a classified rejection when the selected track is absent or not
/// audio, the supplied bytes disagree with the loaded project, the new name is
/// outside the version-1 domain, or the resulting project structure fails
/// validation.
pub fn serialize_audio_track_rename(
    project: &LoadedPlaybackProject,
    source_track: &[u8],
    track_id: &str,
    track_name: &str,
    limits: LoaderLimits,
) -> Result<Vec<u8>, AudioTrackRenameSerializationError> {
    let name_length = track_name.chars().count();
    if track_name.trim() != track_name
        || !(1..=MAX_VERSION_1_NAME_CHARACTERS).contains(&name_length)
    {
        return Err(AudioTrackRenameSerializationError::InvalidTrackName);
    }
    serialize_audio_track_document_edit(project, source_track, track_id, limits, |document| {
        document.insert("name".to_owned(), track_name.into());
    })
    .map_err(AudioTrackRenameSerializationError::Document)
}

/// Replaces only the activation state in one validated canonical audio-track
/// document while preserving its identity, structure, clip ownership, and
/// namespaced extensions.
///
/// # Errors
///
/// Returns a classified rejection when the selected track is absent or not
/// audio, the supplied bytes disagree with the loaded project, or the resulting
/// project structure fails validation.
pub fn serialize_audio_track_activation(
    project: &LoadedPlaybackProject,
    source_track: &[u8],
    track_id: &str,
    active: bool,
    limits: LoaderLimits,
) -> Result<Vec<u8>, AudioTrackDocumentSerializationError> {
    serialize_audio_track_document_edit(project, source_track, track_id, limits, |document| {
        document.insert("active".to_owned(), active.into());
    })
}

/// Replaces only the activation state of one placement in a validated
/// canonical arrangement document while preserving timing, ownership, scene
/// launches, and namespaced extensions.
///
/// # Errors
///
/// Returns a classified rejection when the project has no arrangement, the
/// placement is absent, the supplied bytes disagree with the loaded project,
/// or the resulting document fails version-1 validation.
pub fn serialize_arrangement_placement_activation(
    project: &LoadedPlaybackProject,
    source_arrangement: &[u8],
    placement_id: &str,
    active: bool,
    limits: LoaderLimits,
) -> Result<Vec<u8>, ArrangementPlacementActivationSerializationError> {
    serialize_arrangement_placement_document_edit(
        project,
        source_arrangement,
        placement_id,
        limits,
        |_, document| {
            document.insert("enabled".to_owned(), active.into());
            Ok(())
        },
        |_| Ok(()),
    )
}

/// Moves one validated arrangement placement to an exact project tick while
/// preserving its stable identity, clip offset, duration, activation, and
/// namespaced extensions.
///
/// Placements are restored to canonical `(start_tick, track_id, id)` order
/// before the edited arrangement is reloaded. The strict arrangement boundary
/// therefore rejects overlap, range overflow, and every other invalid result.
///
/// # Errors
///
/// Returns a classified rejection when the project has no arrangement, the
/// placement is absent, the supplied bytes disagree with the loaded project,
/// or the moved placement makes the version-1 arrangement invalid.
pub fn serialize_arrangement_placement_move(
    project: &LoadedPlaybackProject,
    source_arrangement: &[u8],
    placement_id: &str,
    start_tick: u64,
    limits: LoaderLimits,
) -> Result<Vec<u8>, ArrangementPlacementMoveSerializationError> {
    serialize_arrangement_placement_document_edit(
        project,
        source_arrangement,
        placement_id,
        limits,
        |_, document| {
            document.insert("start_tick".to_owned(), start_tick.into());
            Ok(())
        },
        |_| Ok(()),
    )
}

/// Replaces the exact parent-timeline duration of one validated arrangement
/// placement while preserving its start, clip-local offset, stable identity,
/// activation, and namespaced extensions.
///
/// This is a right-edge resize: it does not change the placement start or the
/// clip-local start. The strict arrangement boundary rejects zero duration,
/// same-track overlap, safe-range overflow, and non-looping source overrun.
///
/// # Errors
///
/// Returns a classified rejection when the project has no arrangement, the
/// placement is absent, the supplied bytes disagree with the loaded project,
/// or the resized placement makes the version-1 arrangement invalid.
pub fn serialize_arrangement_placement_duration(
    project: &LoadedPlaybackProject,
    source_arrangement: &[u8],
    placement_id: &str,
    duration_ticks: u64,
    limits: LoaderLimits,
) -> Result<Vec<u8>, ArrangementPlacementDurationSerializationError> {
    serialize_arrangement_placement_document_edit(
        project,
        source_arrangement,
        placement_id,
        limits,
        |_, document| {
            document.insert("duration_ticks".to_owned(), duration_ticks.into());
            Ok(())
        },
        |edited_project| validate_arrangement_placement_execution(edited_project, placement_id),
    )
}

/// Trims or extends the front of one validated arrangement placement to an
/// exact project tick while preserving its right edge and playback content.
///
/// The clip-local offset advances or retreats by the same tick distance as the
/// parent-timeline start. Looped offsets remain unfolded so an extension can
/// reverse an earlier trim without losing the pre-loop pass. Extending before
/// clip-local tick zero is rejected.
///
/// # Errors
///
/// Returns a classified rejection when the project has no arrangement, the
/// placement is absent, the supplied bytes disagree with the loaded project,
/// the requested start reaches or crosses the fixed right edge, or the edited
/// placement cannot execute through the version-1 compiler.
pub fn serialize_arrangement_placement_front_trim(
    project: &LoadedPlaybackProject,
    source_arrangement: &[u8],
    placement_id: &str,
    start_tick: u64,
    limits: LoaderLimits,
) -> Result<Vec<u8>, ArrangementPlacementFrontTrimSerializationError> {
    serialize_arrangement_placement_document_edit(
        project,
        source_arrangement,
        placement_id,
        limits,
        |placement, document| {
            let end_tick = placement
                .start_tick
                .checked_add(placement.duration_ticks)
                .ok_or(ArrangementPlacementDocumentSerializationError::Compilation(
                    CompileError::ArithmeticOverflow,
                ))?;
            if start_tick >= end_tick {
                return Err(ArrangementPlacementDocumentSerializationError::Compilation(
                    CompileError::InvalidPlacement,
                ));
            }
            let clip_start_tick = if start_tick >= placement.start_tick {
                placement
                    .clip_start_tick
                    .checked_add(start_tick - placement.start_tick)
                    .ok_or(ArrangementPlacementDocumentSerializationError::Compilation(
                        CompileError::ArithmeticOverflow,
                    ))?
            } else {
                placement
                    .clip_start_tick
                    .checked_sub(placement.start_tick - start_tick)
                    .ok_or(ArrangementPlacementDocumentSerializationError::Compilation(
                        CompileError::InvalidPlacement,
                    ))?
            };
            document.insert("start_tick".to_owned(), start_tick.into());
            document.insert("clip_start_tick".to_owned(), clip_start_tick.into());
            document.insert("duration_ticks".to_owned(), (end_tick - start_tick).into());
            Ok(())
        },
        |edited_project| validate_arrangement_placement_execution(edited_project, placement_id),
    )
}

fn validate_arrangement_placement_execution(
    project: &LoadedPlaybackProject,
    placement_id: &str,
) -> Result<(), ArrangementPlacementDocumentSerializationError> {
    let placement = project
        .arrangement
        .as_ref()
        .and_then(|arrangement| {
            arrangement
                .placements
                .iter()
                .find(|placement| placement.id == placement_id)
        })
        .ok_or(ArrangementPlacementDocumentSerializationError::PlacementMissing)?;
    let clip = project
        .clips
        .iter()
        .find(|clip| clip.id == placement.clip_id)
        .ok_or(ArrangementPlacementDocumentSerializationError::Compilation(
            CompileError::InvalidPlacement,
        ))?;
    let asset_frame_count = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(ArrangementPlacementDocumentSerializationError::Compilation(
            CompileError::ArithmeticOverflow,
        ))?;
    let timeline = Timeline::try_new(project.sample_rate_hz, &project.tempo_events)
        .map_err(CompileError::InvalidTimeline)
        .map_err(ArrangementPlacementDocumentSerializationError::Compilation)?;
    compile_placement(
        timeline,
        ClipCompileSpec {
            enabled: clip.enabled,
            duration_ticks: clip.duration_ticks,
            source_start_frame: clip.source_start_frame,
            source_frame_count: clip.source_frame_count,
            asset_frame_count,
            source_sample_rate_hz: project.sample_rate_hz,
            loop_enabled: clip.loop_enabled,
            loop_range: clip.loop_range,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            transpose_cents: clip.transpose_cents,
            fade_in_ticks: clip.fade_in_ticks,
            fade_out_ticks: clip.fade_out_ticks,
            warp_markers: &clip.warp_markers,
        },
        PlacementCompileSpec {
            enabled: placement.enabled,
            start_tick: placement.start_tick,
            clip_start_tick: placement.clip_start_tick,
            duration_ticks: placement.duration_ticks,
        },
    )
    .map(|_| ())
    .map_err(ArrangementPlacementDocumentSerializationError::Compilation)
}

fn serialize_arrangement_placement_document_edit(
    project: &LoadedPlaybackProject,
    source_arrangement: &[u8],
    placement_id: &str,
    limits: LoaderLimits,
    edit: impl FnOnce(
        &LoadedPlacement,
        &mut serde_json::Map<String, Value>,
    ) -> Result<(), ArrangementPlacementDocumentSerializationError>,
    validate_edit: impl FnOnce(
        &LoadedPlaybackProject,
    ) -> Result<(), ArrangementPlacementDocumentSerializationError>,
) -> Result<Vec<u8>, ArrangementPlacementDocumentSerializationError> {
    let current = project
        .arrangement
        .as_ref()
        .ok_or(ArrangementPlacementDocumentSerializationError::ArrangementMissing)?;
    let current_placement = current
        .placements
        .iter()
        .find(|placement| placement.id == placement_id)
        .ok_or(ArrangementPlacementDocumentSerializationError::PlacementMissing)?;
    let source = load_arrangement(
        source_arrangement,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_placements,
        limits.max_scene_launches,
    )
    .map_err(ArrangementPlacementDocumentSerializationError::Source)?;
    if &source != current {
        return Err(ArrangementPlacementDocumentSerializationError::SourceArrangementMismatch);
    }

    let mut value: Value = serde_json::from_slice(source_arrangement)
        .map_err(ArrangementPlacementDocumentSerializationError::Serialization)?;
    let canonical_source = serde_json_canonicalizer::to_vec(&value)
        .map_err(ArrangementPlacementDocumentSerializationError::Serialization)?;
    if canonical_source != source_arrangement {
        return Err(ArrangementPlacementDocumentSerializationError::NonCanonicalSource);
    }
    let placements = value
        .get_mut("placements")
        .and_then(Value::as_array_mut)
        .ok_or(ArrangementPlacementDocumentSerializationError::InvalidDocumentShape)?;
    {
        let document = placements
            .iter_mut()
            .find(|placement| placement.get("id").and_then(Value::as_str) == Some(placement_id))
            .and_then(Value::as_object_mut)
            .ok_or(ArrangementPlacementDocumentSerializationError::InvalidDocumentShape)?;
        edit(current_placement, document)?;
    }
    if placements
        .iter()
        .any(|placement| placement_sort_key(placement).is_none())
    {
        return Err(ArrangementPlacementDocumentSerializationError::InvalidDocumentShape);
    }
    placements.sort_unstable_by(|left, right| {
        match (placement_sort_key(left), placement_sort_key(right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => core::cmp::Ordering::Equal,
        }
    });
    let output = serde_json_canonicalizer::to_vec(&value)
        .map_err(ArrangementPlacementDocumentSerializationError::Serialization)?;
    let edited = load_arrangement(
        &output,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_placements,
        limits.max_scene_launches,
    )
    .map_err(ArrangementPlacementDocumentSerializationError::Source)?;
    let mut edited_project = project.clone();
    edited_project.arrangement = Some(edited);
    validate_loaded_identities(&edited_project)
        .map_err(ArrangementPlacementDocumentSerializationError::Source)?;
    validate_edit(&edited_project)?;
    Ok(output)
}

fn placement_sort_key(placement: &Value) -> Option<(u64, &str, &str)> {
    Some((
        placement.get("start_tick")?.as_u64()?,
        placement.get("track_id")?.as_str()?,
        placement.get("id")?.as_str()?,
    ))
}

/// Replaces only the constant playback gain in one validated canonical audio
/// clip while preserving source, timing, looping, fades, pitch, identity, and
/// namespaced extensions.
///
/// # Errors
///
/// Returns a classified rejection when the clip is absent, the supplied bytes
/// disagree with the loaded project, or the requested gain cannot execute in
/// the native clip engine without consolidation.
pub fn serialize_audio_clip_gain(
    project: &LoadedPlaybackProject,
    source_clip: &[u8],
    clip_id: &str,
    gain_decibels: f64,
    limits: LoaderLimits,
) -> Result<Vec<u8>, AudioClipGainSerializationError> {
    if !is_native_audio_clip_gain_decibels(gain_decibels) {
        return Err(AudioClipGainSerializationError::UnsupportedGain);
    }
    serialize_audio_clip_document_edit(project, source_clip, clip_id, limits, |document| {
        let playback = document
            .get_mut("playback")
            .and_then(Value::as_object_mut)
            .ok_or(AudioClipDocumentSerializationError::InvalidDocumentShape)?;
        playback.insert("gain_decibels".to_owned(), gain_decibels.into());
        Ok(())
    })
    .map_err(AudioClipGainSerializationError::Document)
}

/// Replaces only the global activation state in one validated canonical audio
/// clip while preserving source, timing, looping, fades, playback parameters,
/// identity, and namespaced extensions.
///
/// # Errors
///
/// Returns a classified rejection when the clip is absent, the supplied bytes
/// disagree with the loaded project, or the resulting document fails
/// version-1 validation.
pub fn serialize_audio_clip_activation(
    project: &LoadedPlaybackProject,
    source_clip: &[u8],
    clip_id: &str,
    active: bool,
    limits: LoaderLimits,
) -> Result<Vec<u8>, AudioClipDocumentSerializationError> {
    serialize_audio_clip_document_edit(project, source_clip, clip_id, limits, |document| {
        document.insert("enabled".to_owned(), active.into());
        Ok(())
    })
}

fn serialize_audio_clip_document_edit(
    project: &LoadedPlaybackProject,
    source_clip: &[u8],
    clip_id: &str,
    limits: LoaderLimits,
    edit: impl FnOnce(
        &mut serde_json::Map<String, Value>,
    ) -> Result<(), AudioClipDocumentSerializationError>,
) -> Result<Vec<u8>, AudioClipDocumentSerializationError> {
    let clip_index = project
        .clips
        .iter()
        .position(|clip| clip.id == clip_id)
        .ok_or(AudioClipDocumentSerializationError::ClipMissing)?;
    let source = load_clip(
        source_clip,
        clip_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_warp_markers,
    )
    .map_err(AudioClipDocumentSerializationError::Source)?;
    if source != project.clips[clip_index] {
        return Err(AudioClipDocumentSerializationError::SourceClipMismatch);
    }

    let mut value: Value = serde_json::from_slice(source_clip)
        .map_err(AudioClipDocumentSerializationError::Serialization)?;
    let canonical_source = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioClipDocumentSerializationError::Serialization)?;
    if canonical_source != source_clip {
        return Err(AudioClipDocumentSerializationError::NonCanonicalSource);
    }
    let document = value
        .as_object_mut()
        .ok_or(AudioClipDocumentSerializationError::InvalidDocumentShape)?;
    edit(document)?;
    let output = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioClipDocumentSerializationError::Serialization)?;
    let edited = load_clip(
        &output,
        clip_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_warp_markers,
    )
    .map_err(AudioClipDocumentSerializationError::Source)?;
    let mut edited_project = project.clone();
    edited_project.clips[clip_index] = edited;
    validate_loaded_identities(&edited_project)
        .map_err(AudioClipDocumentSerializationError::Source)?;
    Ok(output)
}

/// Returns whether a decibel gain can execute in the native clip engine
/// without consolidation.
#[must_use]
pub fn is_native_audio_clip_gain_decibels(gain_decibels: f64) -> bool {
    gain_decibels.is_finite() && compile_native_clip_gain(gain_decibels).is_ok()
}

fn serialize_audio_track_document_edit(
    project: &LoadedPlaybackProject,
    source_track: &[u8],
    track_id: &str,
    limits: LoaderLimits,
    edit: impl FnOnce(&mut serde_json::Map<String, Value>),
) -> Result<Vec<u8>, AudioTrackDocumentSerializationError> {
    let track_index = project
        .track_index(track_id)
        .ok_or(AudioTrackDocumentSerializationError::TrackMissing)?;
    if !project.is_audio_track(track_id) {
        return Err(AudioTrackDocumentSerializationError::TrackNotAudio);
    }

    let source = load_track(
        source_track,
        track_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackDocumentSerializationError::Source)?;
    let current = project
        .tracks
        .get(track_index)
        .ok_or(AudioTrackDocumentSerializationError::TrackMissing)?;
    if source.id != current.id
        || source.name != current.name
        || source.kind != current.kind
        || source.order != current.order
        || source.active != current.active
        || source.parent_track_id != current.parent_track_id
        || source.clip_ids != current.clip_ids
    {
        return Err(AudioTrackDocumentSerializationError::SourceTrackMismatch);
    }

    let mut value: Value = serde_json::from_slice(source_track)
        .map_err(AudioTrackDocumentSerializationError::Serialization)?;
    let canonical_source = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioTrackDocumentSerializationError::Serialization)?;
    if canonical_source != source_track {
        return Err(AudioTrackDocumentSerializationError::NonCanonicalSource);
    }
    let document = value
        .as_object_mut()
        .ok_or(AudioTrackDocumentSerializationError::InvalidDocumentShape)?;
    edit(document);
    let output = serde_json_canonicalizer::to_vec(&value)
        .map_err(AudioTrackDocumentSerializationError::Serialization)?;
    let edited = load_track(
        &output,
        track_index,
        project.project_id(),
        limits.max_document_bytes,
        limits.max_track_clip_references,
    )
    .map_err(AudioTrackDocumentSerializationError::Source)?;
    let mut renamed_project = project.clone();
    renamed_project.tracks[track_index] = edited;
    validate_track_hierarchy(&renamed_project.tracks)
        .map_err(AudioTrackDocumentSerializationError::Source)?;
    validate_loaded_identities(&renamed_project)
        .map_err(AudioTrackDocumentSerializationError::Source)?;
    Ok(output)
}

impl LoadedMixer {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    #[must_use]
    pub const fn cue_source(&self) -> CueSource {
        self.cue_source
    }

    #[must_use]
    pub const fn cue_gain(&self) -> LinearGain {
        self.cue_gain
    }

    #[must_use]
    pub const fn cue_muted(&self) -> bool {
        self.cue_muted
    }

    /// Compiles the complete supported direct-to-main mixer subset.
    ///
    /// Cue state remains loaded and preserved but is not part of this named
    /// main-output compilation.
    ///
    /// # Errors
    ///
    /// Returns [`MixerCompileError`] for unavailable capacity or mixer graph
    /// features that the current flat stereo engine cannot execute.
    pub fn compile_flat_main(&self) -> Result<CompiledMainMixer, MixerCompileError> {
        if self
            .channels
            .iter()
            .any(|channel| !matches!(channel.destination, MixerDestination::Main))
        {
            return Err(MixerCompileError::UnsupportedPrimaryRouting);
        }
        if !self.return_buses.is_empty()
            || self
                .channels
                .iter()
                .any(|channel| !channel.sends.is_empty())
        {
            return Err(MixerCompileError::UnsupportedSendsOrReturns);
        }
        if self
            .channels
            .iter()
            .any(|channel| channel.track_kind == TrackKind::ExternalInput)
        {
            return Err(MixerCompileError::UnsupportedExternalInput);
        }

        let any_solo = self.channels.iter().any(|channel| channel.solo);
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(self.channels.len())
            .map_err(|_| MixerCompileError::CapacityUnavailable)?;
        for channel in &self.channels {
            let mut parameters = channel.parameters;
            parameters.audible &= !any_solo || channel.solo;
            let mut track_id = String::new();
            track_id
                .try_reserve_exact(channel.track_id.len())
                .map_err(|_| MixerCompileError::CapacityUnavailable)?;
            track_id.push_str(&channel.track_id);
            slots.push(CompiledMixerSlot {
                track_id,
                parameters,
            });
        }
        Ok(CompiledMainMixer {
            project_id: try_owned_compile(&self.project_id)?,
            slots,
            master_gain: if self.main_muted {
                LinearGain::SILENCE
            } else {
                self.main_gain
            },
        })
    }

    /// Compiles the complete bounded version-1 primary, send, and return graph.
    ///
    /// Cue state remains loaded and preserved but is not part of this named
    /// main-output compilation.
    ///
    /// # Errors
    ///
    /// Returns [`MixerCompileError`] for unavailable capacity, an unexpected
    /// post-validation graph inconsistency, or external-input sources whose
    /// physical signal contract is not available.
    pub fn compile_main_graph(&self) -> Result<CompiledMixerGraph, MixerCompileError> {
        if self
            .channels
            .iter()
            .any(|channel| channel.track_kind == TrackKind::ExternalInput)
        {
            return Err(MixerCompileError::UnsupportedExternalInput);
        }

        let eligible = self.solo_eligibility()?;
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(self.channels.len())
            .map_err(|_| MixerCompileError::CapacityUnavailable)?;
        for (channel_index, channel) in self.channels.iter().enumerate() {
            let kind = match channel.track_kind {
                TrackKind::Audio => CompiledMixerTrackKind::Audio,
                TrackKind::Group => CompiledMixerTrackKind::Group,
                TrackKind::Return => CompiledMixerTrackKind::Return,
                TrackKind::ExternalInput => {
                    return Err(MixerCompileError::UnsupportedExternalInput);
                }
                TrackKind::Midi | TrackKind::CvGate => {
                    return Err(MixerCompileError::InvalidGraph);
                }
            };
            let destination = match &channel.destination {
                MixerDestination::Main => CompiledMixerDestination::Main,
                MixerDestination::Track(track_id) => CompiledMixerDestination::Slot(
                    self.channel_index(track_id)
                        .ok_or(MixerCompileError::InvalidGraph)?,
                ),
            };
            let mut sends = Vec::new();
            sends
                .try_reserve_exact(channel.sends.len())
                .map_err(|_| MixerCompileError::CapacityUnavailable)?;
            for send in &channel.sends {
                let return_track = self
                    .return_buses
                    .iter()
                    .find(|(bus, _)| *bus == send.bus)
                    .map(|(_, track_id)| track_id)
                    .ok_or(MixerCompileError::InvalidGraph)?;
                let mut parameters = send.parameters;
                parameters.audible &= channel.parameters.audible && eligible[channel_index];
                sends.push(CompiledMixerSend {
                    id: try_owned_compile(&send.id)?,
                    target_slot: self
                        .channel_index(return_track)
                        .ok_or(MixerCompileError::InvalidGraph)?,
                    tap: match send.tap {
                        MixerTap::PreFader => CompiledMixerTap::PreFader,
                        MixerTap::PostFader => CompiledMixerTap::PostFader,
                    },
                    parameters,
                    enabled: send.parameters.audible,
                });
            }
            let mut parameters = channel.parameters;
            parameters.audible &= eligible[channel_index];
            slots.push(CompiledGraphSlot {
                track_id: try_owned_compile(&channel.track_id)?,
                kind,
                parameters,
                muted: !channel.parameters.audible,
                solo: channel.solo,
                destination,
                sends,
            });
        }
        let processing_order = compile_processing_order(&slots)?;
        Ok(CompiledMixerGraph {
            project_id: try_owned_compile(&self.project_id)?,
            slots,
            processing_order,
            master_gain: if self.main_muted {
                LinearGain::SILENCE
            } else {
                self.main_gain
            },
            main_gain: self.main_gain,
            main_muted: self.main_muted,
        })
    }

    fn channel_index(&self, track_id: &str) -> Option<usize> {
        self.channels
            .iter()
            .position(|channel| channel.track_id == track_id)
    }

    fn solo_eligibility(&self) -> Result<Vec<bool>, MixerCompileError> {
        let any_solo = self.channels.iter().any(|channel| channel.solo);
        let mut eligible = Vec::new();
        eligible
            .try_reserve_exact(self.channels.len())
            .map_err(|_| MixerCompileError::CapacityUnavailable)?;
        eligible.resize(self.channels.len(), !any_solo);
        if !any_solo {
            return Ok(eligible);
        }

        let mut pending = Vec::new();
        pending
            .try_reserve_exact(self.channels.len())
            .map_err(|_| MixerCompileError::CapacityUnavailable)?;
        for (index, channel) in self.channels.iter().enumerate() {
            if channel.solo {
                eligible[index] = true;
                pending.push(index);
            }
        }
        let mut cursor = 0_usize;
        while let Some(&channel_index) = pending.get(cursor) {
            cursor += 1;
            let channel = &self.channels[channel_index];
            if let MixerDestination::Track(track_id) = &channel.destination {
                let target = self
                    .channel_index(track_id)
                    .ok_or(MixerCompileError::InvalidGraph)?;
                enqueue_eligible(&mut eligible, &mut pending, target);
            }
            for send in channel.sends.iter().filter(|send| send.parameters.audible) {
                let return_track = self
                    .return_buses
                    .iter()
                    .find(|(bus, _)| *bus == send.bus)
                    .map(|(_, track_id)| track_id)
                    .ok_or(MixerCompileError::InvalidGraph)?;
                let target = self
                    .channel_index(return_track)
                    .ok_or(MixerCompileError::InvalidGraph)?;
                enqueue_eligible(&mut eligible, &mut pending, target);
            }
        }
        Ok(eligible)
    }
}

fn enqueue_eligible(eligible: &mut [bool], pending: &mut Vec<usize>, target: usize) {
    if !eligible[target] {
        eligible[target] = true;
        pending.push(target);
    }
}

fn compile_processing_order(slots: &[CompiledGraphSlot]) -> Result<Vec<usize>, MixerCompileError> {
    let mut indegree = Vec::new();
    indegree
        .try_reserve_exact(slots.len())
        .map_err(|_| MixerCompileError::CapacityUnavailable)?;
    indegree.resize(slots.len(), 0_usize);
    for slot in slots {
        if let CompiledMixerDestination::Slot(target) = slot.destination {
            indegree[target] = indegree[target]
                .checked_add(1)
                .ok_or(MixerCompileError::InvalidGraph)?;
        }
        for send in &slot.sends {
            indegree[send.target_slot] = indegree[send.target_slot]
                .checked_add(1)
                .ok_or(MixerCompileError::InvalidGraph)?;
        }
    }

    let mut emitted = Vec::new();
    emitted
        .try_reserve_exact(slots.len())
        .map_err(|_| MixerCompileError::CapacityUnavailable)?;
    emitted.resize(slots.len(), false);
    let mut order = Vec::new();
    order
        .try_reserve_exact(slots.len())
        .map_err(|_| MixerCompileError::CapacityUnavailable)?;
    while order.len() < slots.len() {
        let channel = (0..slots.len())
            .find(|index| !emitted[*index] && indegree[*index] == 0)
            .ok_or(MixerCompileError::InvalidGraph)?;
        emitted[channel] = true;
        order.push(channel);
        if let CompiledMixerDestination::Slot(target) = slots[channel].destination {
            indegree[target] = indegree[target]
                .checked_sub(1)
                .ok_or(MixerCompileError::InvalidGraph)?;
        }
        for send in &slots[channel].sends {
            indegree[send.target_slot] = indegree[send.target_slot]
                .checked_sub(1)
                .ok_or(MixerCompileError::InvalidGraph)?;
        }
    }
    Ok(order)
}

fn try_owned_compile(value: &str) -> Result<String, MixerCompileError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| MixerCompileError::CapacityUnavailable)?;
    output.push_str(value);
    Ok(output)
}

impl LoadedPlaybackProject {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Returns the validated version-1 step-tempo map in project order.
    #[must_use]
    pub fn tempo_events(&self) -> &[TempoEvent] {
        &self.tempo_events
    }

    #[must_use]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn track_index(&self, track_id: &str) -> Option<usize> {
        self.tracks.iter().position(|track| track.id == track_id)
    }

    /// Returns a validated structural track name by stable identity.
    #[must_use]
    pub fn track_name(&self, track_id: &str) -> Option<&str> {
        self.tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.name.as_str())
    }

    /// Returns the validated structural activation state by stable identity.
    #[must_use]
    pub fn track_active(&self, track_id: &str) -> Option<bool> {
        self.tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.active)
    }

    #[must_use]
    pub fn is_audio_track(&self, track_id: &str) -> bool {
        self.tracks
            .iter()
            .any(|track| track.id == track_id && track.kind == TrackKind::Audio)
    }

    #[must_use]
    pub fn audio_track_removal_blockers(&self, track_id: &str) -> AudioTrackRemovalBlockers {
        let clip_references = self
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .map_or(0, |track| track.clip_ids.len());
        let child_tracks = self
            .tracks
            .iter()
            .filter(|track| track.parent_track_id.as_deref() == Some(track_id))
            .count();
        let scene_slots = self
            .scenes
            .iter()
            .flat_map(|scene| &scene.slots)
            .filter(|slot| slot.track_id == track_id)
            .count();
        let arrangement_placements = self.arrangement.as_ref().map_or(0, |arrangement| {
            arrangement
                .placements
                .iter()
                .filter(|placement| placement.track_id == track_id)
                .count()
        });
        AudioTrackRemovalBlockers {
            clip_references,
            child_tracks,
            scene_slots,
            arrangement_placements,
        }
    }

    #[must_use]
    pub fn clip_count(&self) -> usize {
        self.clips.len()
    }

    /// Returns a validated audio-clip name by canonical project order.
    #[must_use]
    pub fn clip_name_at(&self, clip_index: usize) -> Option<&str> {
        self.clips.get(clip_index).map(|clip| clip.name.as_str())
    }

    /// Returns a validated audio-clip identity by canonical project order.
    #[must_use]
    pub fn clip_id_at(&self, clip_index: usize) -> Option<&str> {
        self.clips.get(clip_index).map(|clip| clip.id.as_str())
    }

    /// Returns the owning structural-track identity for one audio clip.
    #[must_use]
    pub fn clip_track_id_at(&self, clip_index: usize) -> Option<&str> {
        self.clips
            .get(clip_index)
            .map(|clip| clip.track_id.as_str())
    }

    /// Returns the persisted global activation state for one audio clip.
    #[must_use]
    pub fn clip_active_at(&self, clip_index: usize) -> Option<bool> {
        self.clips.get(clip_index).map(|clip| clip.enabled)
    }

    /// Returns the persisted constant playback gain for one audio clip.
    #[must_use]
    pub fn clip_gain_decibels_at(&self, clip_index: usize) -> Option<f64> {
        self.clips.get(clip_index).map(|clip| clip.gain_decibels)
    }

    /// Returns the immutable decoded-media view used to validate a candidate
    /// gain for one stable audio-clip identity.
    #[must_use]
    pub fn audio_clip_gain_validation_view(
        &self,
        clip_id: &str,
    ) -> Option<AudioClipGainValidationView<'_>> {
        self.clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .map(|clip| AudioClipGainValidationView {
                asset_id: &clip.asset_id,
                source_start_frame: clip.source_start_frame,
                source_frame_count: clip.source_frame_count,
            })
    }

    /// Returns a validated structural-track name by canonical project order.
    #[must_use]
    pub fn track_name_at(&self, track_index: usize) -> Option<&str> {
        self.tracks
            .get(track_index)
            .map(|track| track.name.as_str())
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn scene_index(&self, scene_id: &str) -> Option<usize> {
        self.scenes.iter().position(|scene| scene.id == scene_id)
    }

    /// Returns a validated scene name by its canonical order.
    #[must_use]
    pub fn scene_name(&self, scene_index: usize) -> Option<&str> {
        self.scenes
            .get(scene_index)
            .map(|scene| scene.name.as_str())
    }

    #[must_use]
    pub fn placement_count(&self) -> usize {
        self.arrangement
            .as_ref()
            .map_or(0, |arrangement| arrangement.placements.len())
    }

    /// Loads and resolves one strict version 1 mixer resource against this
    /// project's structural tracks and complete verified asset inventory.
    /// `assets` must contain every asset declared by the enclosing package;
    /// identity validation is intentionally performed before graph publication.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] for strict JSON, collection, identity, ordering,
    /// routing, send, return, solo, or parameter-domain failure.
    pub fn load_mixer(
        &self,
        bytes: &[u8],
        assets: &[MediaAssetFact<'_>],
        limits: LoaderLimits,
    ) -> Result<LoadedMixer, LoadError> {
        load_mixer_document(self, bytes, assets, limits)
    }

    /// Resolves loaded documents and verified media facts into an owned native
    /// arrangement schedule.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterCompileError`] for invalid timeline or asset identity,
    /// temporary allocation failure, graph resolution, unsupported authority,
    /// or a placement that requires consolidation.
    pub fn compile(
        &self,
        assets: &[MediaAssetFact<'_>],
        limits: GraphLimits,
    ) -> Result<OwnedCompiledSchedule, AdapterCompileError> {
        let timeline = Timeline::try_new(self.sample_rate_hz, &self.tempo_events)
            .map_err(AdapterCompileError::Timeline)?;
        compile_loaded_arrangement(self, assets, limits, true, timeline)
    }

    /// Atomically compiles explicit placements, recorded scene authority, and
    /// every scene-play media binding.
    ///
    /// # Errors
    ///
    /// Returns [`AdapterCompileError`] without publishing a partial graph when
    /// any timeline, identity, ownership, authority, scene, or native-audio
    /// requirement fails.
    pub fn compile_playback<'project>(
        &'project self,
        assets: &[MediaAssetFact<'_>],
        graph_limits: GraphLimits,
        scene_limits: SceneLimits,
    ) -> Result<CompiledPlaybackGraph<'project>, AdapterCompileError> {
        let timeline = Timeline::try_new(self.sample_rate_hz, &self.tempo_events)
            .map_err(AdapterCompileError::Timeline)?;
        let arrangement = compile_loaded_arrangement(self, assets, graph_limits, false, timeline)?;
        validate_single_track_authority(self)?;
        let scene_tracks = build_scene_track_resources(&self.tracks)?;
        let scene_clips = build_scene_clip_resources(&self.clips)?;
        let scene_slots = build_scene_slot_resources(&self.scenes)?;
        let scenes = build_scene_resources(&self.scenes, &scene_slots)?;
        let launches = build_scene_launch_resources(self.arrangement.as_ref())?;
        let scene_authority = compile_scene_performance(
            timeline,
            ScenePerformanceInput {
                enabled: self
                    .arrangement
                    .as_ref()
                    .is_none_or(|arrangement| arrangement.enabled),
                tracks: &scene_tracks,
                clips: &scene_clips,
                scenes: &scenes,
                launches: &launches,
            },
            scene_limits,
        )
        .map_err(AdapterCompileError::Scene)?;
        let scene_audio = compile_scene_audio_bindings(self, assets, timeline, &scene_authority)?;
        let interactive_scene_audio =
            compile_interactive_scene_audio_bindings(self, assets, timeline, &scene_authority)?;
        Ok(CompiledPlaybackGraph {
            project_id: &self.project_id,
            arrangement,
            scene_authority,
            scene_audio,
            interactive_scene_audio,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnedScheduleEntry {
    pub placement_id: String,
    pub placement_start_tick: u64,
    pub placement_clip_start_tick: u64,
    pub placement_duration_ticks: u64,
    pub placement_enabled: bool,
    pub track_id: String,
    pub track_index: usize,
    pub clip_id: String,
    pub clip_index: usize,
    pub asset_id: String,
    pub compiled: CompiledPlacement,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnedCompiledSchedule {
    project_id: String,
    entries: Vec<OwnedScheduleEntry>,
}

impl OwnedCompiledSchedule {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn entries(&self) -> &[OwnedScheduleEntry] {
        &self.entries
    }

    #[must_use]
    pub fn end_frame(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.compiled.arrangement_end_frame())
            .max()
            .unwrap_or(0)
    }

    #[must_use]
    pub fn into_entries(self) -> Vec<OwnedScheduleEntry> {
        self.entries
    }
}

#[derive(Debug)]
pub struct CompiledPlaybackGraph<'project> {
    project_id: &'project str,
    arrangement: OwnedCompiledSchedule,
    scene_authority: CompiledScenePerformance<'project>,
    scene_audio: Vec<CompiledSceneAudioBinding<'project>>,
    interactive_scene_audio: Vec<CompiledInteractiveSceneAudioBinding<'project>>,
}

impl<'project> CompiledPlaybackGraph<'project> {
    #[must_use]
    pub const fn project_id(&self) -> &'project str {
        self.project_id
    }

    #[must_use]
    pub const fn arrangement(&self) -> &OwnedCompiledSchedule {
        &self.arrangement
    }

    #[must_use]
    pub const fn scene_authority(&self) -> &CompiledScenePerformance<'project> {
        &self.scene_authority
    }

    #[must_use]
    pub fn into_arrangement(self) -> OwnedCompiledSchedule {
        self.arrangement
    }

    #[must_use]
    pub fn scene_audio(&self) -> &[CompiledSceneAudioBinding<'project>] {
        &self.scene_audio
    }

    #[must_use]
    pub fn interactive_scene_audio(&self) -> &[CompiledInteractiveSceneAudioBinding<'project>] {
        &self.interactive_scene_audio
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledSceneAudioBinding<'project> {
    pub track_id: &'project str,
    pub launch_id: &'project str,
    pub scene_id: &'project str,
    pub clip_id: &'project str,
    pub asset_id: &'project str,
    pub compiled: CompiledSceneAudioClip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractiveSceneAudioAvailability {
    Ready,
    ConsolidationRequired,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledInteractiveSceneAudioBinding<'project> {
    pub scene_index: usize,
    pub slot_index: usize,
    pub track_id: &'project str,
    pub scene_id: &'project str,
    pub clip_id: &'project str,
    pub asset_id: &'project str,
    pub availability: InteractiveSceneAudioAvailability,
    pub compiled: Option<CompiledSceneAudioClip>,
}

#[derive(Debug)]
pub enum AdapterCompileError {
    Timeline(TimelineError),
    AssetIdentityCollision {
        asset_index: usize,
    },
    CapacityUnavailable,
    MixedTrackAuthority {
        placement_index: usize,
    },
    InconsistentCompiledGraph,
    Graph(GraphError),
    Scene(SceneCompileError),
    SceneAudio {
        binding_index: usize,
        error: CompileError,
    },
}

impl fmt::Display for AdapterCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeline(error) => write!(formatter, "timeline compilation failed: {error}"),
            Self::AssetIdentityCollision { asset_index } => {
                write!(
                    formatter,
                    "asset {asset_index} collides with a resource identifier"
                )
            }
            Self::CapacityUnavailable => {
                formatter.write_str("playback adapter capacity is unavailable")
            }
            Self::MixedTrackAuthority { placement_index } => write!(
                formatter,
                "placement {placement_index} targets a scene-controlled track"
            ),
            Self::InconsistentCompiledGraph => {
                formatter.write_str("compiled playback graph is internally inconsistent")
            }
            Self::Graph(error) => write!(formatter, "playback graph compilation failed: {error}"),
            Self::Scene(error) => write!(formatter, "scene authority compilation failed: {error}"),
            Self::SceneAudio {
                binding_index,
                error,
            } => {
                write!(
                    formatter,
                    "scene audio binding {binding_index} compilation failed: {error}"
                )
            }
        }
    }
}

impl Error for AdapterCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(error) => Some(error),
            Self::Graph(error) => Some(error),
            Self::Scene(error) => Some(error),
            Self::SceneAudio { error, .. } => Some(error),
            Self::AssetIdentityCollision { .. }
            | Self::CapacityUnavailable
            | Self::MixedTrackAuthority { .. }
            | Self::InconsistentCompiledGraph => None,
        }
    }
}

/// Strictly loads caller-supplied playback resource documents without
/// asserting package provenance.
///
/// This does not replace schema, canonical-byte, digest, manifest, or media
/// verification. It is the bounded execution adapter after those gates.
///
/// # Errors
///
/// Returns [`LoadError`] for malformed or non-interoperable JSON, duplicate or
/// unknown fields, exceeded bounds, foreign resources, incompatible version 1
/// domains, hierarchy failures, or identity collisions.
pub fn load(
    input: PlaybackJsonInput<'_>,
    limits: LoaderLimits,
) -> Result<LoadedPlaybackProject, LoadError> {
    validate_loader_inputs(input, limits)?;
    let timeline_document: TimelineDocument = parse_document(
        input.timeline,
        DocumentLocation {
            kind: DocumentKind::Timeline,
            index: 0,
        },
        limits.max_document_bytes,
    )?;
    validate_header(
        &timeline_document.format,
        timeline_document.format_version,
        "hyper-timeline",
        &timeline_document.project_id,
        input.project_id,
        DocumentLocation {
            kind: DocumentKind::Timeline,
            index: 0,
        },
    )?;
    validate_timeline_contract(&timeline_document)?;
    if timeline_document.ticks_per_quarter != VERSION_1_TICKS_PER_QUARTER {
        return domain_error(
            DocumentLocation {
                kind: DocumentKind::Timeline,
                index: 0,
            },
            "timeline resolution must be 1920 ticks per quarter",
        );
    }
    validate_meter_events(&timeline_document.meter_events)?;

    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(input.tracks.len())
        .map_err(|_| capacity_error())?;
    for (index, bytes) in input.tracks.iter().enumerate() {
        tracks.push(load_track(
            bytes,
            index,
            input.project_id,
            limits.max_document_bytes,
            limits.max_track_clip_references,
        )?);
    }

    let mut clips = Vec::new();
    clips
        .try_reserve_exact(input.clips.len())
        .map_err(|_| capacity_error())?;
    for (index, bytes) in input.clips.iter().enumerate() {
        clips.push(load_clip(
            bytes,
            index,
            input.project_id,
            limits.max_document_bytes,
            limits.max_warp_markers,
        )?);
    }

    let scenes = load_scenes(input.scenes, input.project_id, limits)?;

    let arrangement = input
        .arrangement
        .map(|bytes| {
            load_arrangement(
                bytes,
                input.project_id,
                limits.max_document_bytes,
                limits.max_placements,
                limits.max_scene_launches,
            )
        })
        .transpose()?;

    validate_track_hierarchy(&tracks)?;
    let loaded = LoadedPlaybackProject {
        project_id: try_owned(input.project_id).map_err(|_| capacity_error())?,
        sample_rate_hz: input.sample_rate_hz,
        timeline_id: timeline_document.id,
        tempo_events: timeline_document
            .tempo_events
            .into_iter()
            .map(|event| TempoEvent {
                tick: event.tick,
                microseconds_per_quarter: event.microseconds_per_quarter,
            })
            .collect(),
        tracks,
        clips,
        scenes,
        arrangement,
    };
    validate_loaded_identities(&loaded)?;
    Timeline::try_new(loaded.sample_rate_hz, &loaded.tempo_events)
        .map_err(|error| domain_error_without_location(error.to_string()))?;
    Ok(loaded)
}

fn validate_loader_inputs(
    input: PlaybackJsonInput<'_>,
    limits: LoaderLimits,
) -> Result<(), LoadError> {
    if !is_valid_entity_id(input.project_id) {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::InvalidProjectIdentity,
            message: "project identifier is outside the version 1 schema domain".to_owned(),
        });
    }
    if input.sample_rate_hz != VERSION_1_SAMPLE_RATE_HZ {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::InvalidVersionOneDomain,
            message: "version 1 project sample rate must be 48000 Hz".to_owned(),
        });
    }
    enforce_loader_limit(
        CollectionLimit::TrackDocuments,
        input.tracks.len(),
        limits.max_track_documents,
        None,
    )?;
    enforce_loader_limit(
        CollectionLimit::ClipDocuments,
        input.clips.len(),
        limits.max_clip_documents,
        None,
    )?;
    enforce_loader_limit(
        CollectionLimit::SceneDocuments,
        input.scenes.len(),
        limits.max_scene_documents,
        None,
    )
}

fn load_track(
    bytes: &[u8],
    index: usize,
    project_id: &str,
    maximum_bytes: usize,
    maximum_clip_references: usize,
) -> Result<LoadedTrack, LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Track,
        index,
    };
    let document: TrackDocument = parse_document(bytes, location, maximum_bytes)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-track",
        &document.project_id,
        project_id,
        location,
    )?;
    validate_contract_document(
        &document.id,
        Some(&document.name),
        &document.extensions,
        location,
    )?;
    if let Some(parent_id) = &document.parent_track_id {
        validate_entity_reference(parent_id, location)?;
    }
    let mut clip_ids = BTreeSet::new();
    for clip_id in &document.clip_ids {
        validate_entity_reference(clip_id, location)?;
        if !clip_ids.insert(clip_id.as_str()) {
            return domain_error(location, "track clip identifiers must be unique");
        }
    }
    if document.order > 7 {
        return domain_error(location, "track order must be from zero through seven");
    }
    enforce_loader_limit(
        CollectionLimit::TrackClipReferences,
        document.clip_ids.len(),
        maximum_clip_references,
        Some(location),
    )?;
    let kind = match document.kind {
        TrackKindDocument::Audio => TrackKind::Audio,
        TrackKindDocument::Midi => TrackKind::Midi,
        TrackKindDocument::CvGate => TrackKind::CvGate,
        TrackKindDocument::ExternalInput => TrackKind::ExternalInput,
        TrackKindDocument::Group => TrackKind::Group,
        TrackKindDocument::Return => TrackKind::Return,
    };
    if matches!(
        kind,
        TrackKind::Group | TrackKind::Return | TrackKind::ExternalInput
    ) && !document.clip_ids.is_empty()
    {
        return domain_error(location, "this track kind cannot own clips");
    }
    Ok(LoadedTrack {
        id: document.id,
        name: document.name,
        kind,
        order: document.order,
        active: document.active,
        parent_track_id: document.parent_track_id,
        clip_ids: document.clip_ids,
    })
}

fn load_clip(
    bytes: &[u8],
    index: usize,
    project_id: &str,
    maximum_bytes: usize,
    maximum_warp_markers: usize,
) -> Result<LoadedClip, LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Clip,
        index,
    };
    let document: ClipDocument = parse_document(bytes, location, maximum_bytes)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-audio-clip",
        &document.project_id,
        project_id,
        location,
    )?;
    validate_contract_document(
        &document.id,
        Some(&document.name),
        &document.extensions,
        location,
    )?;
    validate_entity_reference(&document.track_id, location)?;
    validate_entity_reference(&document.source.asset_id, location)?;
    enforce_loader_limit(
        CollectionLimit::WarpMarkers,
        document.warp_markers.len(),
        maximum_warp_markers,
        Some(location),
    )?;
    validate_clip_document(&document, location)?;
    let source_start_frame = usize::try_from(document.source.start_frame)
        .map_err(|_| domain_error_value(location, "source start frame exceeds platform range"))?;
    let source_frame_count = usize::try_from(document.source.frame_count)
        .map_err(|_| domain_error_value(location, "source frame count exceeds platform range"))?;
    let mut warp_markers = Vec::new();
    warp_markers
        .try_reserve_exact(document.warp_markers.len())
        .map_err(|_| capacity_error())?;
    for marker in document.warp_markers {
        warp_markers.push(WarpMarker {
            tick: marker.tick,
            source_frame_offset: usize::try_from(marker.source_frame_offset).map_err(|_| {
                domain_error_value(location, "warp source frame exceeds platform range")
            })?,
        });
    }
    Ok(LoadedClip {
        id: document.id,
        name: document.name,
        track_id: document.track_id,
        asset_id: document.source.asset_id,
        enabled: document.enabled,
        duration_ticks: document.duration_ticks,
        source_start_frame,
        source_frame_count,
        loop_enabled: document.loop_spec.enabled,
        loop_range: TickRange {
            start: document.loop_spec.start_tick,
            end: document.loop_spec.end_tick,
        },
        reverse: document.playback.reverse,
        gain_decibels: document.playback.gain_decibels,
        transpose_cents: document.playback.transpose_cents,
        fade_in_ticks: document.fades.in_ticks,
        fade_out_ticks: document.fades.out_ticks,
        warp_markers,
    })
}

fn load_scene(
    bytes: &[u8],
    index: usize,
    project_id: &str,
    maximum_bytes: usize,
    maximum_slots: usize,
) -> Result<LoadedScene, LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Scene,
        index,
    };
    let document: SceneDocument = parse_document(bytes, location, maximum_bytes)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-scene",
        &document.project_id,
        project_id,
        location,
    )?;
    validate_contract_document(
        &document.id,
        Some(&document.name),
        &document.extensions,
        location,
    )?;
    enforce_loader_limit(
        CollectionLimit::SceneSlots,
        document.slots.len(),
        maximum_slots,
        Some(location),
    )?;
    if document.order > 7 {
        return domain_error(location, "scene order must be from zero through seven");
    }
    let launch_quantization = match document.launch_quantization {
        SceneQuantizationDocument::Immediate => LaunchQuantization::Immediate,
        SceneQuantizationDocument::Grid { quantum_ticks } => {
            if quantum_ticks == 0 {
                return domain_error(location, "scene launch quantum must be positive");
            }
            LaunchQuantization::Grid { quantum_ticks }
        }
    };
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(document.slots.len())
        .map_err(|_| capacity_error())?;
    for slot in document.slots {
        let (track_id, action) = match slot {
            SceneSlotDocument::Play { track_id, clip_id } => {
                validate_entity_reference(&track_id, location)?;
                validate_entity_reference(&clip_id, location)?;
                (track_id, LoadedSceneSlotAction::Play { clip_id })
            }
            SceneSlotDocument::Stop { track_id } => {
                validate_entity_reference(&track_id, location)?;
                (track_id, LoadedSceneSlotAction::Stop)
            }
        };
        if slots
            .iter()
            .any(|previous: &LoadedSceneSlot| previous.track_id == track_id)
        {
            return domain_error(location, "scene slots must target distinct tracks");
        }
        slots.push(LoadedSceneSlot { track_id, action });
    }
    Ok(LoadedScene {
        id: document.id,
        name: document.name,
        enabled: document.enabled,
        order: document.order,
        launch_quantization,
        slots,
    })
}

fn load_scenes(
    documents: &[&[u8]],
    project_id: &str,
    limits: LoaderLimits,
) -> Result<Vec<LoadedScene>, LoadError> {
    let mut scenes = Vec::new();
    scenes
        .try_reserve_exact(documents.len())
        .map_err(|_| capacity_error())?;
    for (index, bytes) in documents.iter().enumerate() {
        scenes.push(load_scene(
            bytes,
            index,
            project_id,
            limits.max_document_bytes,
            limits.max_scene_slots,
        )?);
    }
    scenes.sort_unstable_by_key(|scene| scene.order);
    Ok(scenes)
}

fn load_arrangement(
    bytes: &[u8],
    project_id: &str,
    maximum_bytes: usize,
    maximum_placements: usize,
    maximum_scene_launches: usize,
) -> Result<LoadedArrangement, LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Arrangement,
        index: 0,
    };
    let document: ArrangementDocument = parse_document(bytes, location, maximum_bytes)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-arrangement",
        &document.project_id,
        project_id,
        location,
    )?;
    validate_contract_document(
        &document.id,
        Some(&document.name),
        &document.extensions,
        location,
    )?;
    for placement in &document.placements {
        validate_entity_reference(&placement.id, location)?;
        validate_entity_reference(&placement.track_id, location)?;
        validate_entity_reference(&placement.clip_id, location)?;
    }
    for launch in &document.scene_launches {
        validate_entity_reference(&launch.id, location)?;
        validate_entity_reference(&launch.scene_id, location)?;
    }
    enforce_loader_limit(
        CollectionLimit::Placements,
        document.placements.len(),
        maximum_placements,
        Some(location),
    )?;
    enforce_loader_limit(
        CollectionLimit::SceneLaunches,
        document.scene_launches.len(),
        maximum_scene_launches,
        Some(location),
    )?;
    validate_arrangement_document(&document, location)?;
    let placements = document
        .placements
        .into_iter()
        .map(|placement| LoadedPlacement {
            id: placement.id,
            track_id: placement.track_id,
            clip_id: placement.clip_id,
            start_tick: placement.start_tick,
            clip_start_tick: placement.clip_start_tick,
            duration_ticks: placement.duration_ticks,
            enabled: placement.enabled,
        })
        .collect();
    let scene_launches = document
        .scene_launches
        .into_iter()
        .map(|launch| LoadedSceneLaunch {
            id: launch.id,
            tick: launch.tick,
            scene_id: launch.scene_id,
        })
        .collect();
    Ok(LoadedArrangement {
        id: document.id,
        enabled: document.enabled,
        placements,
        scene_launches,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "the strict mixer transaction keeps parsing, graph validation, and typed conversion reviewable"
)]
fn load_mixer_document(
    project: &LoadedPlaybackProject,
    bytes: &[u8],
    assets: &[MediaAssetFact<'_>],
    limits: LoaderLimits,
) -> Result<LoadedMixer, LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Mixer,
        index: 0,
    };
    let document: MixerDocument = parse_document(bytes, location, limits.max_document_bytes)?;
    validate_header(
        &document.format,
        document.format_version,
        "hyper-mixer",
        &document.project_id,
        &project.project_id,
        location,
    )?;
    validate_contract_document(&document.id, None, &document.extensions, location)?;
    enforce_loader_limit(
        CollectionLimit::MixerChannels,
        document.channels.len(),
        limits.max_mixer_channels,
        Some(location),
    )?;
    enforce_loader_limit(
        CollectionLimit::ReturnBuses,
        document.return_buses.len(),
        limits.max_return_buses,
        Some(location),
    )?;
    let send_count = document
        .channels
        .iter()
        .try_fold(0_usize, |total, channel| {
            total.checked_add(channel.sends.len())
        })
        .ok_or_else(capacity_error)?;
    enforce_loader_limit(
        CollectionLimit::MixerSends,
        send_count,
        limits.max_mixer_sends,
        Some(location),
    )?;
    validate_mixer_document(project, assets, &document, location)?;

    let mut channels = Vec::new();
    channels
        .try_reserve_exact(document.channels.len())
        .map_err(|_| capacity_error())?;
    for channel in document.channels {
        let track_kind = project
            .tracks
            .iter()
            .find(|track| track.id == channel.track_id)
            .map(|track| track.kind)
            .ok_or_else(|| domain_error_value(location, "mixer channel track does not exist"))?;
        let gain = LinearGain::from_millionths(channel.gain_millionths)
            .map_err(|error| domain_error_value(location, &error.to_string()))?;
        let pan = Pan::from_millionths(channel.pan_millionths)
            .map_err(|error| domain_error_value(location, &error.to_string()))?;
        let destination = match channel.destination {
            MixerDestinationDocument::Main(_) => MixerDestination::Main,
            MixerDestinationDocument::Track(destination) => {
                MixerDestination::Track(destination.track_id)
            }
        };
        let mut sends = Vec::new();
        sends
            .try_reserve_exact(channel.sends.len())
            .map_err(|_| capacity_error())?;
        for send in channel.sends {
            let gain = LinearGain::from_millionths(send.gain_millionths)
                .map_err(|error| domain_error_value(location, &error.to_string()))?;
            let pan = Pan::from_millionths(send.pan_millionths)
                .map_err(|error| domain_error_value(location, &error.to_string()))?;
            sends.push(LoadedMixerSend {
                id: send.id,
                bus: mixer_bus(send.bus),
                tap: match send.tap {
                    MixerTapDocument::PreFader => MixerTap::PreFader,
                    MixerTapDocument::PostFader => MixerTap::PostFader,
                },
                parameters: ChannelParameters::new(gain, pan, send.enabled),
            });
        }
        channels.push(LoadedMixerChannel {
            track_id: channel.track_id,
            track_kind,
            parameters: ChannelParameters::new(gain, pan, !channel.mute),
            solo: channel.solo,
            destination,
            sends,
        });
    }

    let mut return_buses = Vec::new();
    return_buses
        .try_reserve_exact(document.return_buses.len())
        .map_err(|_| capacity_error())?;
    for return_bus in document.return_buses {
        return_buses.push((
            mixer_bus(return_bus.bus),
            try_owned_load(&return_bus.track_id)?,
        ));
    }
    let main_gain = LinearGain::from_millionths(document.main.gain_millionths)
        .map_err(|error| domain_error_value(location, &error.to_string()))?;
    let cue_gain = LinearGain::from_millionths(document.cue.gain_millionths)
        .map_err(|error| domain_error_value(location, &error.to_string()))?;
    let cue_source = match document.cue.source {
        CueSourceDocument::Main => CueSource::Main,
        CueSourceDocument::Solo => CueSource::Solo,
    };
    Ok(LoadedMixer {
        project_id: document.project_id,
        id: document.id,
        channels,
        return_buses,
        main_gain,
        main_muted: document.main.mute,
        cue_source,
        cue_gain,
        cue_muted: document.cue.mute,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "every mixer graph invariant is checked at the one post-verification boundary"
)]
fn validate_mixer_document(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    document: &MixerDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    validate_asset_contract_inventory(project, assets, location)?;
    if project_contains_identity(project, &document.id)
        || assets.iter().any(|asset| asset.id == document.id)
    {
        return domain_error(
            location,
            "mixer identity is empty or collides with the project",
        );
    }
    validate_gain(document.main.gain_millionths, location)?;
    validate_gain(document.cue.gain_millionths, location)?;

    let mut mixable_tracks: Vec<&LoadedTrack> = Vec::new();
    mixable_tracks
        .try_reserve_exact(project.tracks.len())
        .map_err(|_| capacity_error())?;
    mixable_tracks.extend(project.tracks.iter().filter(|track| {
        matches!(
            track.kind,
            TrackKind::Audio | TrackKind::ExternalInput | TrackKind::Group | TrackKind::Return
        )
    }));
    mixable_tracks.sort_unstable_by_key(|track| track.order);
    if document.channels.len() != mixable_tracks.len() {
        return domain_error(
            location,
            "mixer must contain exactly one channel for every mixable track",
        );
    }
    for (channel, track) in document.channels.iter().zip(&mixable_tracks) {
        if channel.track_id != track.id {
            return domain_error(
                location,
                "mixer channels are outside structural track order",
            );
        }
    }

    let mut return_buses = BTreeSet::new();
    let mut return_tracks = BTreeSet::new();
    for return_bus in &document.return_buses {
        let bus = mixer_bus(return_bus.bus);
        if !return_buses.insert(bus) || !return_tracks.insert(return_bus.track_id.as_str()) {
            return domain_error(location, "return bus names and tracks must be unique");
        }
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == return_bus.track_id);
        if track.is_none_or(|track| track.kind != TrackKind::Return) {
            return domain_error(location, "return bus must reference a return track");
        }
    }

    let mut send_ids = BTreeSet::new();
    for (channel_index, channel) in document.channels.iter().enumerate() {
        validate_entity_reference(&channel.track_id, location)?;
        if channel.sends.len() > MAX_VERSION_1_SENDS_PER_CHANNEL {
            return domain_error(
                location,
                "mixer channel send count exceeds the version 1 schema domain",
            );
        }
        if let MixerDestinationDocument::Track(destination) = &channel.destination {
            validate_entity_reference(&destination.track_id, location)?;
        }
        validate_gain(channel.gain_millionths, location)?;
        validate_pan(channel.pan_millionths, location)?;
        let track = mixable_tracks[channel_index];
        if channel.solo && !matches!(track.kind, TrackKind::Audio | TrackKind::ExternalInput) {
            return domain_error(location, "only source tracks may be soloed");
        }
        if track.kind == TrackKind::Return && !channel.sends.is_empty() {
            return domain_error(location, "return channels cannot contain sends");
        }
        let mut previous_bus = None;
        for send in &channel.sends {
            validate_entity_reference(&send.id, location)?;
            validate_gain(send.gain_millionths, location)?;
            validate_pan(send.pan_millionths, location)?;
            let bus = mixer_bus(send.bus);
            if previous_bus.is_some_and(|previous| previous >= bus) {
                return domain_error(location, "channel sends must be unique and sorted by bus");
            }
            previous_bus = Some(bus);
            if !return_buses.contains(&bus) {
                return domain_error(location, "mixer send references an unconfigured return bus");
            }
            if send.id.is_empty()
                || !send_ids.insert(send.id.as_str())
                || send.id == document.id
                || project_contains_identity(project, &send.id)
                || assets.iter().any(|asset| asset.id == send.id)
            {
                return domain_error(location, "mixer send identity is empty or collides");
            }
        }
    }

    for return_bus in &document.return_buses {
        validate_entity_reference(&return_bus.track_id, location)?;
    }

    for channel in &document.channels {
        if let MixerDestinationDocument::Track(destination) = &channel.destination {
            if destination.track_id == channel.track_id {
                return domain_error(location, "mixer channel cannot route to itself");
            }
            let target = mixable_tracks
                .iter()
                .position(|track| track.id == destination.track_id)
                .ok_or_else(|| {
                    domain_error_value(location, "mixer primary destination does not exist")
                })?;
            if mixable_tracks[target].kind != TrackKind::Group {
                return domain_error(location, "mixer primary destination must be a group track");
            }
        }
    }
    validate_mixer_cycles(document, location)?;
    Ok(())
}

fn validate_asset_contract_inventory(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    location: DocumentLocation,
) -> Result<(), LoadError> {
    let mut identities = BTreeSet::new();
    for asset in assets {
        if !is_valid_entity_id(asset.id)
            || project_contains_identity(project, asset.id)
            || !identities.insert(asset.id)
        {
            return domain_error(
                location,
                "asset identity is invalid, duplicated, or collides with the project",
            );
        }
    }
    Ok(())
}

fn validate_mixer_cycles(
    document: &MixerDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    let mut state = Vec::new();
    state
        .try_reserve_exact(document.channels.len())
        .map_err(|_| capacity_error())?;
    state.resize(document.channels.len(), 0_u8);
    for start in 0..document.channels.len() {
        if state[start] == 0 {
            visit_mixer_channel(document, start, &mut state, location)?;
        }
    }
    Ok(())
}

fn visit_mixer_channel(
    document: &MixerDocument,
    channel_index: usize,
    state: &mut [u8],
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if state[channel_index] == 1 {
        return domain_error(location, "mixer routing contains a cycle");
    }
    if state[channel_index] == 2 {
        return Ok(());
    }
    state[channel_index] = 1;
    if let MixerDestinationDocument::Track(destination) =
        &document.channels[channel_index].destination
        && let Some(target) = document
            .channels
            .iter()
            .position(|channel| channel.track_id == destination.track_id)
    {
        visit_mixer_channel(document, target, state, location)?;
    }
    for send in &document.channels[channel_index].sends {
        if let Some(return_bus) = document
            .return_buses
            .iter()
            .find(|return_bus| return_bus.bus == send.bus)
            && let Some(target) = document
                .channels
                .iter()
                .position(|channel| channel.track_id == return_bus.track_id)
        {
            visit_mixer_channel(document, target, state, location)?;
        }
    }
    state[channel_index] = 2;
    Ok(())
}

fn validate_gain(value: u32, location: DocumentLocation) -> Result<(), LoadError> {
    LinearGain::from_millionths(value)
        .map(|_| ())
        .map_err(|error| domain_error_value(location, &error.to_string()))
}

fn validate_pan(value: i32, location: DocumentLocation) -> Result<(), LoadError> {
    Pan::from_millionths(value)
        .map(|_| ())
        .map_err(|error| domain_error_value(location, &error.to_string()))
}

const fn mixer_bus(bus: MixerBusDocument) -> MixerBus {
    match bus {
        MixerBusDocument::A => MixerBus::A,
        MixerBusDocument::B => MixerBus::B,
    }
}

fn project_contains_identity(project: &LoadedPlaybackProject, id: &str) -> bool {
    project.timeline_id == id
        || project.tracks.iter().any(|track| track.id == id)
        || project.clips.iter().any(|clip| clip.id == id)
        || project.scenes.iter().any(|scene| scene.id == id)
        || project.arrangement.as_ref().is_some_and(|arrangement| {
            arrangement.id == id
                || arrangement
                    .placements
                    .iter()
                    .any(|placement| placement.id == id)
                || arrangement
                    .scene_launches
                    .iter()
                    .any(|launch| launch.id == id)
        })
}

fn try_owned_load(value: &str) -> Result<String, LoadError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| capacity_error())?;
    output.push_str(value);
    Ok(output)
}

fn parse_document<T>(
    bytes: &[u8],
    location: DocumentLocation,
    maximum_bytes: usize,
) -> Result<T, LoadError>
where
    T: for<'de> Deserialize<'de>,
{
    strict::parse(bytes, maximum_bytes).map_err(|error| json_error(location, error))
}

fn json_error(location: DocumentLocation, error: StrictJsonError) -> LoadError {
    LoadError {
        location: Some(location),
        kind: LoadErrorKind::Json {
            rejection: error.kind.into(),
            line: error.line,
            column: error.column,
        },
        message: error.message,
    }
}

fn validate_header(
    actual_format: &str,
    version: u32,
    expected_format: &str,
    resource_project_id: &str,
    project_id: &str,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if actual_format != expected_format {
        return Err(LoadError {
            location: Some(location),
            kind: LoadErrorKind::InvalidFormat,
            message: format!("expected format {expected_format}"),
        });
    }
    if version != 1 {
        return Err(LoadError {
            location: Some(location),
            kind: LoadErrorKind::UnsupportedVersion,
            message: "only format version 1 is supported".to_owned(),
        });
    }
    if resource_project_id != project_id {
        return Err(LoadError {
            location: Some(location),
            kind: LoadErrorKind::ForeignProject,
            message: "resource belongs to another project".to_owned(),
        });
    }
    Ok(())
}

fn validate_contract_document(
    id: &str,
    name: Option<&str>,
    extensions: &ExtensionsDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if !is_valid_entity_id(id)
        || name.is_some_and(|value| {
            let length = value.chars().count();
            !(1..=MAX_VERSION_1_NAME_CHARACTERS).contains(&length)
        })
        || !extensions.0.iter().all(|key| is_valid_extension_key(key))
    {
        return domain_error(location, "document is outside the version 1 schema domain");
    }
    Ok(())
}

fn validate_timeline_contract(document: &TimelineDocument) -> Result<(), LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Timeline,
        index: 0,
    };
    validate_contract_document(&document.id, None, &document.extensions, location)?;
    if !(1..=MAX_VERSION_1_TIMELINE_EVENTS).contains(&document.tempo_events.len())
        || !(1..=MAX_VERSION_1_TIMELINE_EVENTS).contains(&document.meter_events.len())
    {
        return domain_error(
            location,
            "timeline event count is outside the version 1 schema domain",
        );
    }
    Ok(())
}

fn validate_entity_reference(id: &str, location: DocumentLocation) -> Result<(), LoadError> {
    if !is_valid_entity_id(id) {
        return domain_error(
            location,
            "entity reference is outside the version 1 schema domain",
        );
    }
    Ok(())
}

fn validate_meter_events(events: &[MeterEventDocument]) -> Result<(), LoadError> {
    let location = DocumentLocation {
        kind: DocumentKind::Timeline,
        index: 0,
    };
    if events.first().is_none_or(|event| event.tick != 0) {
        return domain_error(location, "meter events must begin at tick zero");
    }
    let denominators = [1_u16, 2, 4, 8, 16, 32, 64, 128, 256];
    let mut previous_tick = None;
    for event in events {
        if event.numerator == 0
            || event.numerator > 255
            || !denominators.contains(&event.denominator)
        {
            return domain_error(location, "meter event is outside the version 1 domain");
        }
        if previous_tick.is_some_and(|tick| event.tick <= tick) {
            return domain_error(location, "meter-event ticks must be strictly increasing");
        }
        previous_tick = Some(event.tick);
    }
    Ok(())
}

fn validate_clip_document(
    document: &ClipDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if document.duration_ticks == 0 || document.source.frame_count == 0 {
        return domain_error(location, "clip duration and source count must be positive");
    }
    if document.source.start_frame > MAX_SAFE_INTEGER - document.source.frame_count {
        return domain_error(
            location,
            "clip source range exceeds the safe-integer domain",
        );
    }
    if document.loop_spec.start_tick >= document.loop_spec.end_tick
        || document.loop_spec.end_tick > document.duration_ticks
    {
        return domain_error(location, "clip loop range is invalid");
    }
    if document.fades.in_ticks
        > document
            .duration_ticks
            .saturating_sub(document.fades.out_ticks)
    {
        return domain_error(location, "combined clip fades exceed clip duration");
    }
    if document.warp_markers.len() < 2 {
        return domain_error(location, "clip needs at least two warp markers");
    }
    let first = &document.warp_markers[0];
    let last = &document.warp_markers[document.warp_markers.len() - 1];
    if first.tick != 0
        || first.source_frame_offset != 0
        || last.tick != document.duration_ticks
        || last.source_frame_offset != document.source.frame_count
    {
        return domain_error(location, "clip warp endpoints do not cover the source view");
    }
    for pair in document.warp_markers.windows(2) {
        if pair[1].tick <= pair[0].tick
            || pair[1].source_frame_offset <= pair[0].source_frame_offset
        {
            return domain_error(location, "clip warp markers must be strictly increasing");
        }
    }
    Ok(())
}

fn validate_arrangement_document(
    document: &ArrangementDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    let mut event_ids = BTreeSet::new();
    for placement in &document.placements {
        if placement.duration_ticks == 0 {
            return domain_error(location, "placement duration must be positive");
        }
        if placement.start_tick > MAX_SAFE_INTEGER - placement.duration_ticks
            || placement.clip_start_tick > MAX_SAFE_INTEGER - placement.duration_ticks
        {
            return domain_error(location, "placement range exceeds the safe-integer domain");
        }
        if !event_ids.insert(placement.id.as_str()) {
            return domain_error(location, "arrangement event identifiers must be unique");
        }
    }
    for (index, pair) in document.placements.windows(2).enumerate() {
        let ordered = pair[0]
            .start_tick
            .cmp(&pair[1].start_tick)
            .then_with(|| pair[0].track_id.cmp(&pair[1].track_id))
            .then_with(|| pair[0].id.cmp(&pair[1].id))
            .is_lt();
        if !ordered {
            return domain_error(
                location,
                &format!("placement {} is outside canonical order", index + 1),
            );
        }
    }
    let mut track_ends = std::collections::BTreeMap::new();
    for placement in &document.placements {
        if track_ends
            .get(placement.track_id.as_str())
            .is_some_and(|end| placement.start_tick < *end)
        {
            return domain_error(location, "placements overlap on one track");
        }
        track_ends.insert(
            placement.track_id.as_str(),
            placement.start_tick + placement.duration_ticks,
        );
    }
    let mut previous_launch_tick = None;
    for launch in &document.scene_launches {
        if !event_ids.insert(launch.id.as_str()) {
            return domain_error(location, "arrangement event identifiers must be unique");
        }
        if previous_launch_tick.is_some_and(|tick| launch.tick <= tick) {
            return domain_error(location, "scene-launch ticks must be strictly increasing");
        }
        previous_launch_tick = Some(launch.tick);
    }
    Ok(())
}

fn validate_track_hierarchy(tracks: &[LoadedTrack]) -> Result<(), LoadError> {
    let mut orders = BTreeSet::new();
    for (index, track) in tracks.iter().enumerate() {
        let location = DocumentLocation {
            kind: DocumentKind::Track,
            index,
        };
        if !orders.insert(track.order) {
            return domain_error(location, "track order must be unique");
        }
        if let Some(parent_id) = &track.parent_track_id {
            let parent = tracks
                .iter()
                .position(|candidate| candidate.id == *parent_id)
                .ok_or_else(|| domain_error_value(location, "parent track does not exist"))?;
            if tracks[parent].kind != TrackKind::Group {
                return domain_error(location, "parent track must be a group");
            }
        }
    }
    for start in 0..tracks.len() {
        let mut visited = BTreeSet::new();
        let mut cursor = Some(start);
        while let Some(index) = cursor {
            if !visited.insert(index) {
                return domain_error(
                    DocumentLocation {
                        kind: DocumentKind::Track,
                        index: start,
                    },
                    "track hierarchy contains a cycle",
                );
            }
            cursor = tracks[index]
                .parent_track_id
                .as_ref()
                .and_then(|parent| tracks.iter().position(|candidate| candidate.id == *parent));
        }
    }
    Ok(())
}

fn validate_loaded_identities(project: &LoadedPlaybackProject) -> Result<(), LoadError> {
    let mut identities = BTreeSet::new();
    insert_identity(&mut identities, &project.timeline_id)?;
    for track in &project.tracks {
        insert_identity(&mut identities, &track.id)?;
    }
    for clip in &project.clips {
        insert_identity(&mut identities, &clip.id)?;
    }
    for scene in &project.scenes {
        insert_identity(&mut identities, &scene.id)?;
    }
    if let Some(arrangement) = &project.arrangement {
        insert_identity(&mut identities, &arrangement.id)?;
        for placement in &arrangement.placements {
            insert_identity(&mut identities, &placement.id)?;
        }
        for launch in &arrangement.scene_launches {
            insert_identity(&mut identities, &launch.id)?;
        }
    }
    Ok(())
}

fn insert_identity<'a>(identities: &mut BTreeSet<&'a str>, id: &'a str) -> Result<(), LoadError> {
    if !is_valid_entity_id(id) {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::InvalidProjectIdentity,
            message: "resource identifier is outside the version 1 schema domain".to_owned(),
        });
    }
    if !identities.insert(id) {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::DuplicateEntityId,
            message: "resource and event identifiers must be globally unique".to_owned(),
        });
    }
    Ok(())
}

fn validate_asset_identities(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
) -> Result<(), AdapterCompileError> {
    let mut identities = BTreeSet::new();
    identities.insert(project.timeline_id.as_str());
    identities.extend(project.tracks.iter().map(|track| track.id.as_str()));
    identities.extend(project.clips.iter().map(|clip| clip.id.as_str()));
    identities.extend(project.scenes.iter().map(|scene| scene.id.as_str()));
    if let Some(arrangement) = &project.arrangement {
        identities.insert(arrangement.id.as_str());
        identities.extend(
            arrangement
                .placements
                .iter()
                .map(|placement| placement.id.as_str()),
        );
        identities.extend(
            arrangement
                .scene_launches
                .iter()
                .map(|launch| launch.id.as_str()),
        );
    }
    for (asset_index, asset) in assets.iter().enumerate() {
        if !is_valid_entity_id(asset.id) || !identities.insert(asset.id) {
            return Err(AdapterCompileError::AssetIdentityCollision { asset_index });
        }
    }
    Ok(())
}

fn build_clip_id_views(tracks: &[LoadedTrack]) -> Result<Vec<Vec<&str>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(tracks.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for track in tracks {
        let mut ids = Vec::new();
        ids.try_reserve_exact(track.clip_ids.len())
            .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
        ids.extend(track.clip_ids.iter().map(String::as_str));
        output.push(ids);
    }
    Ok(output)
}

fn build_track_resources<'a>(
    tracks: &'a [LoadedTrack],
    clip_ids: &'a [Vec<&'a str>],
) -> Result<Vec<TrackResource<'a>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(tracks.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for (index, track) in tracks.iter().enumerate() {
        output.push(TrackResource {
            id: &track.id,
            kind: track.kind,
            active: track.active,
            clip_ids: &clip_ids[index],
        });
    }
    Ok(output)
}

fn build_clip_resources(
    clips: &[LoadedClip],
) -> Result<Vec<ClipResource<'_>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(clips.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for clip in clips {
        output.push(ClipResource {
            id: &clip.id,
            track_id: &clip.track_id,
            asset_id: &clip.asset_id,
            enabled: clip.enabled,
            duration_ticks: clip.duration_ticks,
            source_start_frame: clip.source_start_frame,
            source_frame_count: clip.source_frame_count,
            loop_enabled: clip.loop_enabled,
            loop_range: clip.loop_range,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            transpose_cents: clip.transpose_cents,
            fade_in_ticks: clip.fade_in_ticks,
            fade_out_ticks: clip.fade_out_ticks,
            warp_markers: &clip.warp_markers,
        });
    }
    Ok(output)
}

fn build_asset_resources<'a>(
    assets: &[MediaAssetFact<'a>],
) -> Result<Vec<AssetResource<'a>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(assets.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.extend(assets.iter().map(|asset| AssetResource {
        id: asset.id,
        sample_rate_hz: asset.sample_rate_hz,
        frame_count: asset.frame_count,
    }));
    Ok(output)
}

fn build_placement_resources(
    arrangement: Option<&LoadedArrangement>,
) -> Result<Vec<PlacementResource<'_>>, AdapterCompileError> {
    let placements = arrangement.map_or(&[][..], |value| value.placements.as_slice());
    let mut output = Vec::new();
    output
        .try_reserve_exact(placements.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.extend(placements.iter().map(|placement| PlacementResource {
        id: &placement.id,
        track_id: &placement.track_id,
        clip_id: &placement.clip_id,
        start_tick: placement.start_tick,
        clip_start_tick: placement.clip_start_tick,
        duration_ticks: placement.duration_ticks,
        enabled: placement.enabled,
    }));
    Ok(output)
}

fn compile_loaded_arrangement(
    project: &LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    limits: GraphLimits,
    reject_scene_launches: bool,
    timeline: Timeline<'_>,
) -> Result<OwnedCompiledSchedule, AdapterCompileError> {
    validate_asset_identities(project, assets)?;
    let clip_id_views = build_clip_id_views(&project.tracks)?;
    let track_resources = build_track_resources(&project.tracks, &clip_id_views)?;
    let clip_resources = build_clip_resources(&project.clips)?;
    let asset_resources = build_asset_resources(assets)?;
    let placement_resources = build_placement_resources(project.arrangement.as_ref())?;
    let scene_launch_count = if reject_scene_launches {
        project
            .arrangement
            .as_ref()
            .map_or(0, |arrangement| arrangement.scene_launches.len())
    } else {
        0
    };
    let compiled = compile_arrangement(
        timeline,
        GraphInput {
            tracks: &track_resources,
            clips: &clip_resources,
            assets: &asset_resources,
            arrangement: ArrangementResource {
                enabled: project
                    .arrangement
                    .as_ref()
                    .is_none_or(|arrangement| arrangement.enabled),
                placements: &placement_resources,
                scene_launch_count,
            },
        },
        limits,
    )
    .map_err(AdapterCompileError::Graph)?;
    own_schedule(project, &compiled)
}

fn validate_single_track_authority(
    project: &LoadedPlaybackProject,
) -> Result<(), AdapterCompileError> {
    let Some(arrangement) = &project.arrangement else {
        return Ok(());
    };
    let mut scene_tracks = BTreeSet::new();
    for launch in &arrangement.scene_launches {
        if let Some(scene) = project
            .scenes
            .iter()
            .find(|scene| scene.id == launch.scene_id)
        {
            scene_tracks.extend(scene.slots.iter().map(|slot| slot.track_id.as_str()));
        }
    }
    for (placement_index, placement) in arrangement.placements.iter().enumerate() {
        if scene_tracks.contains(placement.track_id.as_str()) {
            return Err(AdapterCompileError::MixedTrackAuthority { placement_index });
        }
    }
    Ok(())
}

fn build_scene_track_resources(
    tracks: &[LoadedTrack],
) -> Result<Vec<SceneTrackResource<'_>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(tracks.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.extend(tracks.iter().map(|track| SceneTrackResource {
        id: &track.id,
        active: track.active,
    }));
    Ok(output)
}

fn build_scene_clip_resources(
    clips: &[LoadedClip],
) -> Result<Vec<SceneClipResource<'_>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(clips.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.extend(clips.iter().map(|clip| SceneClipResource {
        id: &clip.id,
        track_id: &clip.track_id,
        enabled: clip.enabled,
        duration_ticks: clip.duration_ticks,
        loop_enabled: clip.loop_enabled,
    }));
    Ok(output)
}

fn build_scene_slot_resources(
    scenes: &[LoadedScene],
) -> Result<Vec<Vec<SceneSlotResource<'_>>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(scenes.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for scene in scenes {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(scene.slots.len())
            .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
        slots.extend(scene.slots.iter().map(|slot| SceneSlotResource {
            track_id: &slot.track_id,
            action: match &slot.action {
                LoadedSceneSlotAction::Play { clip_id } => SceneSlotAction::Play { clip_id },
                LoadedSceneSlotAction::Stop => SceneSlotAction::Stop,
            },
        }));
        output.push(slots);
    }
    Ok(output)
}

fn build_scene_resources<'input, 'project>(
    scenes: &'project [LoadedScene],
    slots: &'input [Vec<SceneSlotResource<'project>>],
) -> Result<Vec<SceneResource<'input, 'project>>, AdapterCompileError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(scenes.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for (index, scene) in scenes.iter().enumerate() {
        output.push(SceneResource {
            id: &scene.id,
            enabled: scene.enabled,
            order: scene.order,
            launch_quantization: scene.launch_quantization,
            slots: &slots[index],
        });
    }
    Ok(output)
}

fn build_scene_launch_resources(
    arrangement: Option<&LoadedArrangement>,
) -> Result<Vec<SceneLaunchResource<'_>>, AdapterCompileError> {
    let launches = arrangement.map_or(&[][..], |value| value.scene_launches.as_slice());
    let mut output = Vec::new();
    output
        .try_reserve_exact(launches.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.extend(launches.iter().map(|launch| SceneLaunchResource {
        id: &launch.id,
        tick: launch.tick,
        scene_id: &launch.scene_id,
    }));
    Ok(output)
}

fn compile_scene_audio_bindings<'project>(
    project: &'project LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    timeline: Timeline<'_>,
    authority: &CompiledScenePerformance<'project>,
) -> Result<Vec<CompiledSceneAudioBinding<'project>>, AdapterCompileError> {
    let binding_count = authority.playing_clips().count();
    let mut output = Vec::new();
    output
        .try_reserve_exact(binding_count)
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for (binding_index, (track_id, playing)) in authority.playing_clips().enumerate() {
        let launch_id = playing
            .launch_id()
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let clip = project
            .clips
            .iter()
            .find(|clip| clip.id == playing.clip_id())
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let asset = assets
            .iter()
            .find(|asset| asset.id == clip.asset_id)
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let compiled = compile_scene_audio_clip(
            timeline,
            ClipCompileSpec {
                enabled: playing.enabled(),
                duration_ticks: clip.duration_ticks,
                source_start_frame: clip.source_start_frame,
                source_frame_count: clip.source_frame_count,
                asset_frame_count: asset.frame_count,
                source_sample_rate_hz: asset.sample_rate_hz,
                loop_enabled: clip.loop_enabled,
                loop_range: clip.loop_range,
                reverse: clip.reverse,
                gain_decibels: clip.gain_decibels,
                transpose_cents: clip.transpose_cents,
                fade_in_ticks: clip.fade_in_ticks,
                fade_out_ticks: clip.fade_out_ticks,
                warp_markers: &clip.warp_markers,
            },
            playing.started_at_tick(),
        )
        .map_err(|error| AdapterCompileError::SceneAudio {
            binding_index,
            error,
        })?;
        output.push(CompiledSceneAudioBinding {
            track_id,
            launch_id,
            scene_id: playing.scene_id(),
            clip_id: playing.clip_id(),
            asset_id: &clip.asset_id,
            compiled,
        });
    }
    Ok(output)
}

fn compile_interactive_scene_audio_bindings<'project>(
    project: &'project LoadedPlaybackProject,
    assets: &[MediaAssetFact<'_>],
    timeline: Timeline<'_>,
    authority: &CompiledScenePerformance<'project>,
) -> Result<Vec<CompiledInteractiveSceneAudioBinding<'project>>, AdapterCompileError> {
    let binding_count = authority.interactive_clips().count();
    let mut output = Vec::new();
    output
        .try_reserve_exact(binding_count)
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for (binding_index, (scene_index, slot_index, track_id, clip_id, enabled)) in
        authority.interactive_clips().enumerate()
    {
        let scene_id = authority
            .scene_id(scene_index)
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let clip = project
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let asset = assets
            .iter()
            .find(|asset| asset.id == clip.asset_id)
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let result = compile_interactive_scene_audio_clip(
            timeline,
            ClipCompileSpec {
                enabled,
                duration_ticks: clip.duration_ticks,
                source_start_frame: clip.source_start_frame,
                source_frame_count: clip.source_frame_count,
                asset_frame_count: asset.frame_count,
                source_sample_rate_hz: asset.sample_rate_hz,
                loop_enabled: clip.loop_enabled,
                loop_range: clip.loop_range,
                reverse: clip.reverse,
                gain_decibels: clip.gain_decibels,
                transpose_cents: clip.transpose_cents,
                fade_in_ticks: clip.fade_in_ticks,
                fade_out_ticks: clip.fade_out_ticks,
                warp_markers: &clip.warp_markers,
            },
        );
        let (availability, compiled) = match result {
            Ok(compiled) => (InteractiveSceneAudioAvailability::Ready, Some(compiled)),
            Err(CompileError::ConsolidationRequired(_)) => (
                InteractiveSceneAudioAvailability::ConsolidationRequired,
                None,
            ),
            Err(error) => {
                return Err(AdapterCompileError::SceneAudio {
                    binding_index,
                    error,
                });
            }
        };
        output.push(CompiledInteractiveSceneAudioBinding {
            scene_index,
            slot_index,
            track_id,
            scene_id,
            clip_id,
            asset_id: &clip.asset_id,
            availability,
            compiled,
        });
    }
    Ok(output)
}

fn build_schedule_identity_index<'a>(
    ids: impl ExactSizeIterator<Item = &'a str>,
) -> Result<HashMap<&'a str, usize>, AdapterCompileError> {
    let mut index = HashMap::new();
    index
        .try_reserve(ids.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for (position, id) in ids.enumerate() {
        if index.insert(id, position).is_some() {
            return Err(AdapterCompileError::InconsistentCompiledGraph);
        }
    }
    Ok(index)
}

#[derive(Clone, Copy)]
struct SchedulePlacementMetadata {
    start_tick: u64,
    clip_start_tick: u64,
    duration_ticks: u64,
    enabled: bool,
}

fn build_schedule_placement_metadata_index(
    project: &LoadedPlaybackProject,
) -> Result<HashMap<&str, SchedulePlacementMetadata>, AdapterCompileError> {
    let placements = project
        .arrangement
        .as_ref()
        .map_or(&[][..], |arrangement| arrangement.placements.as_slice());
    let mut index = HashMap::new();
    index
        .try_reserve(placements.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for placement in placements {
        if index
            .insert(
                placement.id.as_str(),
                SchedulePlacementMetadata {
                    start_tick: placement.start_tick,
                    clip_start_tick: placement.clip_start_tick,
                    duration_ticks: placement.duration_ticks,
                    enabled: placement.enabled,
                },
            )
            .is_some()
        {
            return Err(AdapterCompileError::InconsistentCompiledGraph);
        }
    }
    Ok(index)
}

fn own_schedule(
    project: &LoadedPlaybackProject,
    compiled: &CompiledSchedule<'_>,
) -> Result<OwnedCompiledSchedule, AdapterCompileError> {
    let track_indexes =
        build_schedule_identity_index(project.tracks.iter().map(|track| track.id.as_str()))?;
    let clip_indexes =
        build_schedule_identity_index(project.clips.iter().map(|clip| clip.id.as_str()))?;
    let placement_metadata = build_schedule_placement_metadata_index(project)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(compiled.entries().len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for entry in compiled.entries() {
        let track_index = track_indexes
            .get(entry.track_id)
            .copied()
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let clip_index = clip_indexes
            .get(entry.clip_id)
            .copied()
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        let placement = placement_metadata
            .get(entry.placement_id)
            .copied()
            .ok_or(AdapterCompileError::InconsistentCompiledGraph)?;
        entries.push(OwnedScheduleEntry {
            placement_id: try_owned(entry.placement_id)?,
            placement_start_tick: placement.start_tick,
            placement_clip_start_tick: placement.clip_start_tick,
            placement_duration_ticks: placement.duration_ticks,
            placement_enabled: placement.enabled,
            track_id: try_owned(entry.track_id)?,
            track_index,
            clip_id: try_owned(entry.clip_id)?,
            clip_index,
            asset_id: try_owned(entry.asset_id)?,
            compiled: entry.compiled,
        });
    }
    Ok(OwnedCompiledSchedule {
        project_id: try_owned(&project.project_id)?,
        entries,
    })
}

fn try_owned(value: &str) -> Result<String, AdapterCompileError> {
    let mut output = String::new();
    output
        .try_reserve_exact(value.len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    output.push_str(value);
    Ok(output)
}

fn enforce_loader_limit(
    collection: CollectionLimit,
    actual: usize,
    maximum: usize,
    location: Option<DocumentLocation>,
) -> Result<(), LoadError> {
    if actual > maximum {
        return Err(LoadError {
            location,
            kind: LoadErrorKind::LimitExceeded {
                collection,
                actual,
                maximum,
            },
            message: format!("{collection:?} count {actual} exceeds {maximum}"),
        });
    }
    Ok(())
}

fn domain_error<T>(location: DocumentLocation, message: &str) -> Result<T, LoadError> {
    Err(domain_error_value(location, message))
}

fn domain_error_value(location: DocumentLocation, message: &str) -> LoadError {
    LoadError {
        location: Some(location),
        kind: LoadErrorKind::InvalidVersionOneDomain,
        message: message.to_owned(),
    }
}

fn domain_error_without_location(message: String) -> LoadError {
    LoadError {
        location: None,
        kind: LoadErrorKind::InvalidVersionOneDomain,
        message,
    }
}

fn capacity_error() -> LoadError {
    LoadError {
        location: None,
        kind: LoadErrorKind::CapacityUnavailable,
        message: "playback JSON capacity is unavailable".to_owned(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelineDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    ticks_per_quarter: u64,
    tempo_events: Vec<TempoEventDocument>,
    meter_events: Vec<MeterEventDocument>,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TempoEventDocument {
    tick: u64,
    microseconds_per_quarter: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MeterEventDocument {
    tick: u64,
    numerator: u16,
    denominator: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    name: String,
    kind: TrackKindDocument,
    order: u8,
    active: bool,
    parent_track_id: Option<String>,
    clip_ids: Vec<String>,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
enum TrackKindDocument {
    #[serde(rename = "audio")]
    Audio,
    #[serde(rename = "midi")]
    Midi,
    #[serde(rename = "cv-gate")]
    CvGate,
    #[serde(rename = "external-input")]
    ExternalInput,
    #[serde(rename = "group")]
    Group,
    #[serde(rename = "return")]
    Return,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    track_id: String,
    name: String,
    enabled: bool,
    duration_ticks: u64,
    source: ClipSourceDocument,
    #[serde(rename = "loop")]
    loop_spec: ClipLoopDocument,
    playback: ClipPlaybackDocument,
    fades: ClipFadesDocument,
    warp_markers: Vec<WarpMarkerDocument>,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipSourceDocument {
    asset_id: String,
    start_frame: u64,
    frame_count: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipLoopDocument {
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipPlaybackDocument {
    gain_decibels: f64,
    reverse: bool,
    transpose_cents: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipFadesDocument {
    #[serde(rename = "curve")]
    _curve: LinearAmplitude,
    in_ticks: u64,
    out_ticks: u64,
}

#[derive(Deserialize)]
enum LinearAmplitude {
    #[serde(rename = "linear-amplitude")]
    Expected,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WarpMarkerDocument {
    tick: u64,
    source_frame_offset: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    name: String,
    order: u8,
    enabled: bool,
    launch_quantization: SceneQuantizationDocument,
    slots: Vec<SceneSlotDocument>,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case", deny_unknown_fields)]
enum SceneQuantizationDocument {
    Immediate,
    Grid { quantum_ticks: u64 },
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
enum SceneSlotDocument {
    Play { track_id: String, clip_id: String },
    Stop { track_id: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArrangementDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    name: String,
    enabled: bool,
    placements: Vec<PlacementDocument>,
    scene_launches: Vec<SceneLaunchDocument>,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlacementDocument {
    id: String,
    track_id: String,
    clip_id: String,
    start_tick: u64,
    clip_start_tick: u64,
    duration_ticks: u64,
    enabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SceneLaunchDocument {
    id: String,
    tick: u64,
    scene_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MixerDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    channels: Vec<MixerChannelDocument>,
    return_buses: Vec<ReturnBusDocument>,
    main: MixerOutputDocument,
    cue: CueOutputDocument,
    extensions: ExtensionsDocument,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MixerChannelDocument {
    track_id: String,
    gain_millionths: u32,
    pan_millionths: i32,
    mute: bool,
    solo: bool,
    destination: MixerDestinationDocument,
    sends: Vec<MixerSendDocument>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum MixerDestinationDocument {
    Main(MainDestinationDocument),
    Track(TrackDestinationDocument),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MainDestinationDocument {
    #[serde(rename = "kind")]
    _kind: MainDestinationKind,
}

#[derive(Deserialize)]
enum MainDestinationKind {
    #[serde(rename = "main")]
    Main,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrackDestinationDocument {
    #[serde(rename = "kind")]
    _kind: TrackDestinationKind,
    track_id: String,
}

#[derive(Deserialize)]
enum TrackDestinationKind {
    #[serde(rename = "track")]
    Track,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MixerSendDocument {
    id: String,
    bus: MixerBusDocument,
    tap: MixerTapDocument,
    gain_millionths: u32,
    pan_millionths: i32,
    enabled: bool,
}

#[derive(Clone, Copy, Eq, PartialEq, Deserialize)]
enum MixerBusDocument {
    #[serde(rename = "a")]
    A,
    #[serde(rename = "b")]
    B,
}

#[derive(Clone, Copy, Deserialize)]
enum MixerTapDocument {
    #[serde(rename = "pre-fader")]
    PreFader,
    #[serde(rename = "post-fader")]
    PostFader,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReturnBusDocument {
    bus: MixerBusDocument,
    track_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MixerOutputDocument {
    gain_millionths: u32,
    mute: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CueOutputDocument {
    source: CueSourceDocument,
    gain_millionths: u32,
    mute: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
enum CueSourceDocument {
    #[serde(rename = "main")]
    Main,
    #[serde(rename = "solo")]
    Solo,
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334458",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    const TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334456",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122334460"],"extensions":{}
    }"#;
    const CLIP: &[u8] = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334460",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122334457","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    const ARRANGEMENT: &[u8] = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334463",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122334464","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","clip_id":"018f47a2-6b2d-7c11-8a44-001122334460",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    const SCENE: &[u8] = br#"{
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334461",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Verse","order":0,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122334456","action":"play","clip_id":"018f47a2-6b2d-7c11-8a44-001122334460"}],
        "extensions":{}
    }"#;
    const MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334468",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455",
        "channels":[{
            "track_id":"018f47a2-6b2d-7c11-8a44-001122334456","gain_millionths":500000,
            "pan_millionths":-1000000,"mute":false,"solo":true,
            "destination":{"kind":"main"},"sends":[]
        }],
        "return_buses":[],
        "main":{"gain_millionths":1000000,"mute":true},
        "cue":{"source":"solo","gain_millionths":750000,"mute":false},
        "extensions":{}
    }"#;

    fn input<'a>(
        timeline: &'a [u8],
        tracks: &'a [&'a [u8]],
        clips: &'a [&'a [u8]],
        arrangement: Option<&'a [u8]>,
    ) -> PlaybackJsonInput<'a> {
        PlaybackJsonInput {
            project_id: PROJECT_ID,
            sample_rate_hz: 48_000,
            timeline,
            tracks,
            clips,
            scenes: &[],
            arrangement,
        }
    }

    fn input_with_scenes<'a>(
        timeline: &'a [u8],
        tracks: &'a [&'a [u8]],
        clips: &'a [&'a [u8]],
        scenes: &'a [&'a [u8]],
        arrangement: Option<&'a [u8]>,
    ) -> PlaybackJsonInput<'a> {
        PlaybackJsonInput {
            project_id: PROJECT_ID,
            sample_rate_hz: 48_000,
            timeline,
            tracks,
            clips,
            scenes,
            arrangement,
        }
    }

    fn asset_facts() -> [MediaAssetFact<'static>; 1] {
        [MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122334457",
            sample_rate_hz: 48_000,
            frame_count: 100,
        }]
    }

    #[test]
    fn schedule_identity_index_preserves_document_positions() {
        let index = build_schedule_identity_index(["clip-b", "clip-a"].into_iter()).unwrap();
        assert_eq!(index.get("clip-b"), Some(&0));
        assert_eq!(index.get("clip-a"), Some(&1));
        assert_eq!(index.get("missing"), None);
        assert!(matches!(
            build_schedule_identity_index(["duplicate", "duplicate"].into_iter()),
            Err(AdapterCompileError::InconsistentCompiledGraph)
        ));
    }

    #[test]
    fn loads_and_compiles_the_explicit_playback_documents() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let arrangement = String::from_utf8(ARRANGEMENT.to_vec())
            .unwrap()
            .replace("\"start_tick\":0", "\"start_tick\":8")
            .replace(
                "\"duration_ticks\":4,\"enabled\":true",
                "\"duration_ticks\":4,\"enabled\":false",
            );
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(arrangement.as_bytes())),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(loaded.project_id(), PROJECT_ID);
        assert_eq!(loaded.track_count(), 1);
        assert_eq!(
            loaded.track_name("018f47a2-6b2d-7c11-8a44-001122334456"),
            Some("Audio")
        );
        assert_eq!(loaded.track_name("missing"), None);
        assert_eq!(loaded.clip_count(), 1);
        assert_eq!(loaded.clip_name_at(0), Some("Clip"));
        assert_eq!(loaded.clip_name_at(1), None);
        assert_eq!(loaded.placement_count(), 1);
        let schedule = loaded
            .compile(
                &[MediaAssetFact {
                    id: "018f47a2-6b2d-7c11-8a44-001122334457",
                    sample_rate_hz: 48_000,
                    frame_count: 100,
                }],
                GraphLimits::VERSION_1,
            )
            .unwrap();
        assert_eq!(schedule.entries()[0].track_index, 0);
        assert_eq!(schedule.entries()[0].clip_index, 0);
        assert_eq!(schedule.entries()[0].placement_start_tick, 8);
        assert_eq!(schedule.entries()[0].placement_clip_start_tick, 0);
        assert_eq!(schedule.entries()[0].placement_duration_ticks, 4);
        assert!(!schedule.entries()[0].placement_enabled);
        assert_eq!(schedule.project_id(), PROJECT_ID);
        assert_eq!(schedule.entries().len(), 1);
        assert_eq!(
            schedule.entries()[0].asset_id,
            "018f47a2-6b2d-7c11-8a44-001122334457"
        );
    }

    #[test]
    fn loads_resolves_and_compiles_the_flat_main_mixer() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = loaded
            .load_mixer(MIXER, &asset_facts(), LoaderLimits::VERSION_1)
            .unwrap();
        assert_eq!(mixer.id(), "018f47a2-6b2d-7c11-8a44-001122334468");
        assert_eq!(mixer.channel_count(), 1);
        assert_eq!(mixer.cue_source(), CueSource::Solo);
        assert_eq!(
            mixer.cue_gain(),
            LinearGain::from_millionths(750_000).unwrap()
        );
        assert!(!mixer.cue_muted());

        let compiled = mixer.compile_flat_main().unwrap();
        assert_eq!(compiled.project_id(), PROJECT_ID);
        assert_eq!(compiled.master_gain(), LinearGain::SILENCE);
        assert_eq!(
            compiled.slots()[0].track_id(),
            "018f47a2-6b2d-7c11-8a44-001122334456"
        );
        assert_eq!(
            compiled.slots()[0].parameters(),
            ChannelParameters::new(
                LinearGain::from_millionths(500_000).unwrap(),
                Pan::HARD_LEFT,
                true,
            )
        );
    }

    #[test]
    fn serializes_live_mixer_base_state_without_losing_versioned_fields() {
        let mut source: Value = serde_json::from_slice(MIXER).unwrap();
        source["extensions"]["org.example.state"] = serde_json::json!({"opaque":[1, 2, 3]});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let state = MixerBaseState {
            channels: vec![MixerChannelBaseState {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                gain: LinearGain::from_millionths(1_234_567).unwrap(),
                pan: Pan::from_millionths(345_678).unwrap(),
                muted: true,
                solo: false,
                sends: Vec::new(),
            }],
            main_gain: LinearGain::from_millionths(876_543).unwrap(),
            main_muted: false,
        };

        let output = serialize_mixer_base_state(
            &source,
            PROJECT_ID,
            "018f47a2-6b2d-7c11-8a44-001122334468",
            &state,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();

        assert_eq!(value["channels"][0]["gain_millionths"], 1_234_567);
        assert_eq!(value["channels"][0]["pan_millionths"], 345_678);
        assert_eq!(value["channels"][0]["mute"], true);
        assert_eq!(value["channels"][0]["solo"], false);
        assert_eq!(value["main"]["gain_millionths"], 876_543);
        assert_eq!(value["main"]["mute"], false);
        assert_eq!(value["cue"]["source"], "solo");
        assert_eq!(
            value["extensions"]["org.example.state"],
            serde_json::json!({"opaque":[1, 2, 3]})
        );
        assert_eq!(serde_json_canonicalizer::to_vec(&value).unwrap(), output);
    }

    #[test]
    fn serializes_a_validated_audio_track_and_matching_mixer_channel() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut source: Value = serde_json::from_slice(MIXER).unwrap();
        source["extensions"]["org.example.state"] = json!({"opaque": true});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let state = MixerBaseState {
            channels: vec![MixerChannelBaseState {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                gain: LinearGain::from_millionths(1_250_000).unwrap(),
                pan: Pan::from_millionths(125_000).unwrap(),
                muted: true,
                solo: false,
                sends: Vec::new(),
            }],
            main_gain: LinearGain::from_millionths(900_000).unwrap(),
            main_muted: false,
        };
        let source = serialize_mixer_base_state(
            &source,
            PROJECT_ID,
            "018f47a2-6b2d-7c11-8a44-001122334468",
            &state,
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let documents = serialize_audio_track_addition(
            &loaded,
            &asset_facts(),
            &source,
            "018f47a2-6b2d-7c11-8a44-001122334468",
            "018f47a2-6b2d-7c11-8a44-001122334469",
            "Vocal",
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let track: Value = serde_json::from_slice(documents.track()).unwrap();
        let mixer: Value = serde_json::from_slice(documents.mixer()).unwrap();

        assert_eq!(track["name"], "Vocal");
        assert_eq!(track["kind"], "audio");
        assert_eq!(track["order"], 1);
        assert_eq!(track["clip_ids"], json!([]));
        assert_eq!(mixer["channels"].as_array().unwrap().len(), 2);
        assert_eq!(mixer["channels"][0]["gain_millionths"], 1_250_000);
        assert_eq!(mixer["channels"][0]["mute"], true);
        assert_eq!(
            mixer["channels"][1]["track_id"],
            "018f47a2-6b2d-7c11-8a44-001122334469"
        );
        assert_eq!(mixer["channels"][1]["gain_millionths"], 1_000_000);
        assert_eq!(mixer["channels"][1]["pan_millionths"], 0);
        assert_eq!(mixer["main"]["gain_millionths"], 900_000);
        assert_eq!(
            mixer["extensions"]["org.example.state"],
            json!({"opaque": true})
        );
        assert_eq!(
            serde_json_canonicalizer::to_vec(&track).unwrap(),
            documents.track()
        );
        assert_eq!(
            serde_json_canonicalizer::to_vec(&mixer).unwrap(),
            documents.mixer()
        );
    }

    #[test]
    fn serializes_removal_of_only_one_empty_unreferenced_audio_track() {
        let track = serde_json_canonicalizer::to_vec(&json!({
            "format":"hyper-track", "format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334469", "project_id":PROJECT_ID,
            "name":"Vocal", "kind":"audio", "order":0, "active":true,
            "clip_ids":[], "extensions":{"org.example.track":{"kept":true}}
        }))
        .unwrap();
        let tracks: [&[u8]; 1] = [&track];
        let loaded = load(input(TIMELINE, &tracks, &[], None), LoaderLimits::VERSION_1).unwrap();
        let mixer = serde_json_canonicalizer::to_vec(&json!({
            "format":"hyper-mixer", "format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334468", "project_id":PROJECT_ID,
            "channels":[{
                "track_id":"018f47a2-6b2d-7c11-8a44-001122334469",
                "gain_millionths":750_000, "pan_millionths":125_000,
                "mute":true, "solo":false, "destination":{"kind":"main"}, "sends":[]
            }],
            "return_buses":[], "main":{"gain_millionths":900_000,"mute":false},
            "cue":{"source":"solo","gain_millionths":500_000,"mute":true},
            "extensions":{"org.example.mixer":{"kept":true}}
        }))
        .unwrap();

        let reduced = serialize_audio_track_removal(
            &loaded,
            &[],
            &mixer,
            "018f47a2-6b2d-7c11-8a44-001122334468",
            "018f47a2-6b2d-7c11-8a44-001122334469",
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&reduced).unwrap();

        assert_eq!(value["channels"], json!([]));
        assert_eq!(value["main"]["gain_millionths"], 900_000);
        assert_eq!(value["cue"]["mute"], true);
        assert_eq!(value["extensions"]["org.example.mixer"]["kept"], true);
        assert_eq!(serde_json_canonicalizer::to_vec(&value).unwrap(), reduced);
    }

    #[test]
    fn audio_track_removal_reports_structural_references() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let scenes: [&[u8]; 1] = [SCENE];
        let loaded = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let error = serialize_audio_track_removal(
            &loaded,
            &asset_facts(),
            MIXER,
            "018f47a2-6b2d-7c11-8a44-001122334468",
            "018f47a2-6b2d-7c11-8a44-001122334456",
            LoaderLimits::VERSION_1,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            AudioTrackRemovalSerializationError::Blocked(AudioTrackRemovalBlockers {
                clip_references: 1,
                scene_slots: 1,
                arrangement_placements: 1,
                ..
            })
        ));
    }

    #[test]
    fn audio_track_addition_refuses_capacity_and_identity_conflicts() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let capacity = LoaderLimits {
            max_track_documents: 1,
            ..LoaderLimits::VERSION_1
        };

        assert!(matches!(
            serialize_audio_track_addition(
                &loaded,
                &asset_facts(),
                MIXER,
                "018f47a2-6b2d-7c11-8a44-001122334468",
                "018f47a2-6b2d-7c11-8a44-001122334469",
                "Vocal",
                capacity,
            ),
            Err(AudioTrackAdditionSerializationError::TrackCapacity)
        ));
        assert!(matches!(
            serialize_audio_track_addition(
                &loaded,
                &asset_facts(),
                MIXER,
                "018f47a2-6b2d-7c11-8a44-001122334468",
                "018f47a2-6b2d-7c11-8a44-001122334457",
                "Vocal",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackAdditionSerializationError::InvalidTrackIdentity)
        ));
        assert!(matches!(
            serialize_audio_track_addition(
                &loaded,
                &asset_facts(),
                MIXER,
                "018f47a2-6b2d-7c11-8a44-001122334469",
                "018f47a2-6b2d-7c11-8a44-00112233446a",
                "Vocal",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackAdditionSerializationError::MixerIdentityMismatch)
        ));
    }

    #[test]
    fn renames_only_the_selected_canonical_audio_track() {
        let mut source: Value = serde_json::from_slice(TRACK).unwrap();
        source["extensions"]["org.example.track"] = json!({"opaque": [1, 2, 3]});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [&source];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let renamed = serialize_audio_track_rename(
            &loaded,
            &source,
            "018f47a2-6b2d-7c11-8a44-001122334456",
            "Lead Vocal",
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut expected: Value = serde_json::from_slice(&source).unwrap();
        expected["name"] = "Lead Vocal".into();

        assert_eq!(
            renamed,
            serde_json_canonicalizer::to_vec(&expected).unwrap()
        );
        assert_eq!(
            expected["extensions"]["org.example.track"],
            json!({"opaque": [1, 2, 3]})
        );
    }

    #[test]
    fn audio_track_rename_refuses_stale_non_audio_and_invalid_inputs() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        assert!(matches!(
            serialize_audio_track_rename(
                &loaded,
                TRACK,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                "Vocal",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackRenameSerializationError::Document(
                AudioTrackDocumentSerializationError::TrackMissing
            ))
        ));
        assert!(matches!(
            serialize_audio_track_rename(
                &loaded,
                TRACK,
                "018f47a2-6b2d-7c11-8a44-001122334456",
                " Vocal ",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackRenameSerializationError::InvalidTrackName)
        ));

        let mut stale: Value = serde_json::from_slice(TRACK).unwrap();
        stale["active"] = false.into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_audio_track_rename(
                &loaded,
                &stale,
                "018f47a2-6b2d-7c11-8a44-001122334456",
                "Vocal",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackRenameSerializationError::Document(
                AudioTrackDocumentSerializationError::SourceTrackMismatch
            ))
        ));

        let mut non_audio: Value = serde_json::from_slice(TRACK).unwrap();
        non_audio["kind"] = "midi".into();
        non_audio["clip_ids"] = json!([]);
        let non_audio = serde_json_canonicalizer::to_vec(&non_audio).unwrap();
        let non_audio_tracks: [&[u8]; 1] = [&non_audio];
        let no_clips: [&[u8]; 0] = [];
        let non_audio_project = load(
            input(TIMELINE, &non_audio_tracks, &no_clips, None),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            serialize_audio_track_rename(
                &non_audio_project,
                &non_audio,
                "018f47a2-6b2d-7c11-8a44-001122334456",
                "MIDI",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackRenameSerializationError::Document(
                AudioTrackDocumentSerializationError::TrackNotAudio
            ))
        ));
    }

    #[test]
    fn audio_track_rename_enforces_canonical_source_and_unicode_name_boundaries() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        let source: Value = serde_json::from_slice(TRACK).unwrap();
        let canonical = serde_json_canonicalizer::to_vec(&source).unwrap();
        let noncanonical = serde_json::to_vec_pretty(&source).unwrap();

        assert!(matches!(
            serialize_audio_track_rename(
                &loaded,
                &noncanonical,
                track_id,
                "Vocal",
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackRenameSerializationError::Document(
                AudioTrackDocumentSerializationError::NonCanonicalSource
            ))
        ));
        for invalid in [String::new(), "é".repeat(121)] {
            assert!(matches!(
                serialize_audio_track_rename(
                    &loaded,
                    TRACK,
                    track_id,
                    &invalid,
                    LoaderLimits::VERSION_1,
                ),
                Err(AudioTrackRenameSerializationError::InvalidTrackName)
            ));
        }

        let maximum = "é".repeat(120);
        let renamed = serialize_audio_track_rename(
            &loaded,
            &canonical,
            track_id,
            &maximum,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&renamed).unwrap();

        assert_eq!(value["name"].as_str(), Some(maximum.as_str()));
    }

    #[test]
    fn audio_track_activation_changes_only_the_selected_canonical_field() {
        let mut source: Value = serde_json::from_slice(TRACK).unwrap();
        source["extensions"]["org.example.track"] = json!({"opaque": true});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [&source];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";

        let inactive = serialize_audio_track_activation(
            &loaded,
            &source,
            track_id,
            false,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut expected: Value = serde_json::from_slice(&source).unwrap();
        expected["active"] = false.into();

        assert_eq!(
            inactive,
            serde_json_canonicalizer::to_vec(&expected).unwrap()
        );
        assert_eq!(loaded.track_active(track_id), Some(true));
        let mut mismatched: Value = serde_json::from_slice(&source).unwrap();
        mismatched["name"] = "Different source".into();
        let mismatched = serde_json_canonicalizer::to_vec(&mismatched).unwrap();
        assert!(matches!(
            serialize_audio_track_activation(
                &loaded,
                &mismatched,
                track_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioTrackDocumentSerializationError::SourceTrackMismatch)
        ));
        assert_eq!(
            serialize_audio_track_activation(
                &loaded,
                &source,
                track_id,
                true,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_activation_changes_only_the_selected_field() {
        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["extensions"]["org.example.arrangement"] = json!({"opaque": [1, 2, 3]});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        let inactive = serialize_arrangement_placement_activation(
            &loaded,
            &source,
            placement_id,
            false,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut expected: Value = serde_json::from_slice(&source).unwrap();
        expected["placements"][0]["enabled"] = false.into();

        assert_eq!(
            inactive,
            serde_json_canonicalizer::to_vec(&expected).unwrap()
        );
        assert_eq!(
            expected["extensions"]["org.example.arrangement"],
            json!({"opaque": [1, 2, 3]})
        );
        assert_eq!(
            serialize_arrangement_placement_activation(
                &loaded,
                &source,
                placement_id,
                true,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_activation_rejects_missing_stale_and_noncanonical_inputs() {
        let source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        let canonical = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&canonical)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        assert!(matches!(
            serialize_arrangement_placement_activation(
                &loaded,
                &canonical,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementActivationSerializationError::PlacementMissing)
        ));

        let mut stale = source.clone();
        stale["enabled"] = false.into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_arrangement_placement_activation(
                &loaded,
                &stale,
                placement_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementActivationSerializationError::SourceArrangementMismatch)
        ));

        let noncanonical = serde_json::to_vec_pretty(&source).unwrap();
        assert!(matches!(
            serialize_arrangement_placement_activation(
                &loaded,
                &noncanonical,
                placement_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementActivationSerializationError::NonCanonicalSource)
        ));

        let project_without_arrangement = load(
            input(TIMELINE, &tracks, &clips, None),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            serialize_arrangement_placement_activation(
                &project_without_arrangement,
                &canonical,
                placement_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementActivationSerializationError::ArrangementMissing)
        ));
    }

    #[test]
    fn arrangement_placement_move_preserves_identity_and_restores_canonical_order() {
        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["extensions"]["org.example.arrangement"] = json!({"opaque": [1, 2, 3]});
        source["placements"].as_array_mut().unwrap().push(json!({
            "id": "018f47a2-6b2d-7c11-8a44-001122334465",
            "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
            "clip_id": "018f47a2-6b2d-7c11-8a44-001122334460",
            "start_tick": 8,
            "clip_start_tick": 0,
            "duration_ticks": 4,
            "enabled": false
        }));
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        let moved = serialize_arrangement_placement_move(
            &loaded,
            &source,
            placement_id,
            16,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let moved: Value = serde_json::from_slice(&moved).unwrap();

        assert_eq!(
            moved["placements"][0]["id"],
            "018f47a2-6b2d-7c11-8a44-001122334465"
        );
        assert_eq!(moved["placements"][0]["start_tick"], 8);
        assert_eq!(moved["placements"][0]["enabled"], false);
        assert_eq!(moved["placements"][1]["id"], placement_id);
        assert_eq!(moved["placements"][1]["start_tick"], 16);
        assert_eq!(moved["placements"][1]["clip_start_tick"], 0);
        assert_eq!(moved["placements"][1]["duration_ticks"], 4);
        assert_eq!(moved["placements"][1]["enabled"], true);
        assert_eq!(
            moved["extensions"]["org.example.arrangement"],
            json!({"opaque": [1, 2, 3]})
        );
    }

    #[test]
    fn arrangement_placement_move_rejects_overlap_and_range_overflow() {
        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["placements"].as_array_mut().unwrap().push(json!({
            "id": "018f47a2-6b2d-7c11-8a44-001122334465",
            "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
            "clip_id": "018f47a2-6b2d-7c11-8a44-001122334460",
            "start_tick": 8,
            "clip_start_tick": 0,
            "duration_ticks": 4,
            "enabled": true
        }));
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        let overlap = serialize_arrangement_placement_move(
            &loaded,
            &source,
            placement_id,
            6,
            LoaderLimits::VERSION_1,
        )
        .unwrap_err();
        assert!(matches!(
            overlap,
            ArrangementPlacementMoveSerializationError::Source(_)
        ));
        assert!(
            overlap
                .to_string()
                .contains("placements overlap on one track")
        );

        let overflow = serialize_arrangement_placement_move(
            &loaded,
            &source,
            placement_id,
            MAX_SAFE_INTEGER,
            LoaderLimits::VERSION_1,
        )
        .unwrap_err();
        assert!(matches!(
            overflow,
            ArrangementPlacementMoveSerializationError::Source(_)
        ));
        assert!(overflow.to_string().contains("safe-integer domain"));
    }

    #[test]
    fn arrangement_placement_move_noop_preserves_canonical_bytes() {
        let source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        assert_eq!(
            serialize_arrangement_placement_move(
                &loaded,
                &source,
                "018f47a2-6b2d-7c11-8a44-001122334464",
                0,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_duration_preserves_every_other_field() {
        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["extensions"]["org.example.arrangement"] = json!({"opaque": true});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let resized = serialize_arrangement_placement_duration(
            &loaded,
            &source,
            "018f47a2-6b2d-7c11-8a44-001122334464",
            2,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let resized: Value = serde_json::from_slice(&resized).unwrap();
        let placement = &resized["placements"][0];

        assert_eq!(placement["duration_ticks"], 2);
        assert_eq!(placement["start_tick"], 0);
        assert_eq!(placement["clip_start_tick"], 0);
        assert_eq!(placement["enabled"], true);
        assert_eq!(placement["id"], "018f47a2-6b2d-7c11-8a44-001122334464");
        assert_eq!(
            resized["extensions"]["org.example.arrangement"],
            json!({"opaque": true})
        );
    }

    #[test]
    fn arrangement_placement_duration_rejects_zero_source_overrun_and_overlap() {
        let source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        let mut stale: Value = serde_json::from_slice(&source).unwrap();
        stale["enabled"] = false.into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_arrangement_placement_duration(
                &loaded,
                &stale,
                placement_id,
                0,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementDurationSerializationError::SourceArrangementMismatch)
        ));
        assert!(matches!(
            serialize_arrangement_placement_duration(
                &loaded,
                &source,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                0,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementDurationSerializationError::PlacementMissing)
        ));

        assert!(matches!(
            serialize_arrangement_placement_duration(
                &loaded,
                &source,
                placement_id,
                0,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementDurationSerializationError::Source(_))
        ));
        assert!(matches!(
            serialize_arrangement_placement_duration(
                &loaded,
                &source,
                placement_id,
                5,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementDurationSerializationError::Compilation(
                CompileError::InvalidPlacement
            ))
        ));

        let mut looping_clip: Value = serde_json::from_slice(CLIP).unwrap();
        looping_clip["loop"]["enabled"] = true.into();
        let looping_clip = serde_json_canonicalizer::to_vec(&looping_clip).unwrap();
        let mut with_next: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        with_next["placements"].as_array_mut().unwrap().push(json!({
            "id": "018f47a2-6b2d-7c11-8a44-001122334465",
            "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
            "clip_id": "018f47a2-6b2d-7c11-8a44-001122334460",
            "start_tick": 8,
            "clip_start_tick": 0,
            "duration_ticks": 4,
            "enabled": false
        }));
        let with_next = serde_json_canonicalizer::to_vec(&with_next).unwrap();
        let looping_clips: [&[u8]; 1] = [&looping_clip];
        let loaded = load(
            input(TIMELINE, &tracks, &looping_clips, Some(&with_next)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let overlap = serialize_arrangement_placement_duration(
            &loaded,
            &with_next,
            placement_id,
            9,
            LoaderLimits::VERSION_1,
        )
        .unwrap_err();
        assert!(
            overlap
                .to_string()
                .contains("placements overlap on one track")
        );
    }

    #[test]
    fn arrangement_placement_duration_noop_preserves_canonical_bytes() {
        let source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        assert_eq!(
            serialize_arrangement_placement_duration(
                &loaded,
                &source,
                "018f47a2-6b2d-7c11-8a44-001122334464",
                4,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_front_trim_preserves_right_edge_and_is_reversible() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";

        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["extensions"]["org.example.arrangement"] = json!({"opaque": true});
        source["placements"][0]["start_tick"] = 4.into();
        source["placements"][0]["clip_start_tick"] = 2.into();
        source["placements"][0]["duration_ticks"] = 2.into();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let extended = serialize_arrangement_placement_front_trim(
            &loaded,
            &source,
            placement_id,
            2,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let extended_value: Value = serde_json::from_slice(&extended).unwrap();
        let placement = &extended_value["placements"][0];
        assert_eq!(placement["start_tick"], 2);
        assert_eq!(placement["clip_start_tick"], 0);
        assert_eq!(placement["duration_ticks"], 4);
        assert_eq!(placement["id"], placement_id);
        assert_eq!(placement["enabled"], true);
        assert_eq!(
            extended_value["extensions"]["org.example.arrangement"],
            json!({"opaque": true})
        );

        let extended_project = load(
            input(TIMELINE, &tracks, &clips, Some(&extended)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(
            serialize_arrangement_placement_front_trim(
                &extended_project,
                &extended,
                placement_id,
                4,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_front_trim_keeps_loop_offsets_unfolded() {
        let tracks: [&[u8]; 1] = [TRACK];
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";
        let mut looping_clip: Value = serde_json::from_slice(CLIP).unwrap();
        looping_clip["loop"]["enabled"] = true.into();
        let looping_clip = serde_json_canonicalizer::to_vec(&looping_clip).unwrap();
        let clips: [&[u8]; 1] = [&looping_clip];
        let mut source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        source["placements"][0]["duration_ticks"] = 12.into();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let trimmed = serialize_arrangement_placement_front_trim(
            &loaded,
            &source,
            placement_id,
            5,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let trimmed_value: Value = serde_json::from_slice(&trimmed).unwrap();
        let placement = &trimmed_value["placements"][0];
        assert_eq!(placement["start_tick"], 5);
        assert_eq!(placement["clip_start_tick"], 5);
        assert_eq!(placement["duration_ticks"], 7);

        let trimmed_project = load(
            input(TIMELINE, &tracks, &clips, Some(&trimmed)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(
            serialize_arrangement_placement_front_trim(
                &trimmed_project,
                &trimmed,
                placement_id,
                0,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_front_trim_rejects_stale_invalid_and_preserves_noop() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";
        let source: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&source)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let mut stale: Value = serde_json::from_slice(&source).unwrap();
        stale["enabled"] = false.into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_arrangement_placement_front_trim(
                &loaded,
                &stale,
                placement_id,
                4,
                LoaderLimits::VERSION_1,
            ),
            Err(ArrangementPlacementFrontTrimSerializationError::SourceArrangementMismatch)
        ));
        assert!(matches!(
            serialize_arrangement_placement_front_trim(
                &loaded,
                &source,
                placement_id,
                4,
                LoaderLimits::VERSION_1,
            ),
            Err(
                ArrangementPlacementFrontTrimSerializationError::Compilation(
                    CompileError::InvalidPlacement
                )
            )
        ));
        assert_eq!(
            serialize_arrangement_placement_front_trim(
                &loaded,
                &source,
                placement_id,
                0,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn arrangement_placement_front_trim_rejects_extension_before_source_zero() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";
        let mut cannot_extend: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        cannot_extend["placements"][0]["start_tick"] = 4.into();
        cannot_extend["placements"][0]["clip_start_tick"] = 2.into();
        cannot_extend["placements"][0]["duration_ticks"] = 2.into();
        let cannot_extend = serde_json_canonicalizer::to_vec(&cannot_extend).unwrap();
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&cannot_extend)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            serialize_arrangement_placement_front_trim(
                &loaded,
                &cannot_extend,
                placement_id,
                1,
                LoaderLimits::VERSION_1,
            ),
            Err(
                ArrangementPlacementFrontTrimSerializationError::Compilation(
                    CompileError::InvalidPlacement
                )
            )
        ));
    }

    #[test]
    fn arrangement_placement_front_trim_rejects_same_track_overlap() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334464";
        let mut cannot_extend: Value = serde_json::from_slice(ARRANGEMENT).unwrap();
        cannot_extend["placements"][0]["start_tick"] = 4.into();
        cannot_extend["placements"][0]["clip_start_tick"] = 2.into();
        cannot_extend["placements"][0]["duration_ticks"] = 2.into();
        let mut overlap = cannot_extend;
        overlap["placements"].as_array_mut().unwrap().insert(
            0,
            json!({
                "id": "018f47a2-6b2d-7c11-8a44-001122334465",
                "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
                "clip_id": "018f47a2-6b2d-7c11-8a44-001122334460",
                "start_tick": 0,
                "clip_start_tick": 0,
                "duration_ticks": 3,
                "enabled": false
            }),
        );
        let overlap = serde_json_canonicalizer::to_vec(&overlap).unwrap();
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(&overlap)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let error = serialize_arrangement_placement_front_trim(
            &loaded,
            &overlap,
            placement_id,
            2,
            LoaderLimits::VERSION_1,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ArrangementPlacementFrontTrimSerializationError::Source(_)
        ));
        assert!(
            error
                .to_string()
                .contains("placements overlap on one track")
        );
    }

    #[test]
    fn audio_clip_gain_changes_only_the_selected_canonical_field() {
        let mut source: Value = serde_json::from_slice(CLIP).unwrap();
        source["extensions"]["org.example.clip"] = json!({"opaque": [1, 2, 3]});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [&source];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334460";

        let edited =
            serialize_audio_clip_gain(&loaded, &source, clip_id, -12.5, LoaderLimits::VERSION_1)
                .unwrap();
        let mut expected: Value = serde_json::from_slice(&source).unwrap();
        expected["playback"]["gain_decibels"] = (-12.5).into();

        assert_eq!(edited, serde_json_canonicalizer::to_vec(&expected).unwrap());
        assert_eq!(
            expected["extensions"]["org.example.clip"],
            json!({"opaque": [1, 2, 3]})
        );
        assert_eq!(
            serialize_audio_clip_gain(&loaded, &source, clip_id, 0.0, LoaderLimits::VERSION_1,)
                .unwrap(),
            source
        );
    }

    #[test]
    fn audio_clip_activation_changes_only_the_selected_canonical_field() {
        let mut source: Value = serde_json::from_slice(CLIP).unwrap();
        source["extensions"]["org.example.clip"] = json!({"opaque": [1, 2, 3]});
        let source = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [&source];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334460";

        let inactive = serialize_audio_clip_activation(
            &loaded,
            &source,
            clip_id,
            false,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mut expected: Value = serde_json::from_slice(&source).unwrap();
        expected["enabled"] = false.into();

        assert_eq!(
            inactive,
            serde_json_canonicalizer::to_vec(&expected).unwrap()
        );
        assert_eq!(
            expected["extensions"]["org.example.clip"],
            json!({"opaque": [1, 2, 3]})
        );
        assert_eq!(
            serialize_audio_clip_activation(
                &loaded,
                &source,
                clip_id,
                true,
                LoaderLimits::VERSION_1,
            )
            .unwrap(),
            source
        );
    }

    #[test]
    fn audio_clip_activation_rejects_missing_stale_and_noncanonical_inputs() {
        let source: Value = serde_json::from_slice(CLIP).unwrap();
        let canonical = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [&canonical];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334460";

        assert!(matches!(
            serialize_audio_clip_activation(
                &loaded,
                &canonical,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioClipDocumentSerializationError::ClipMissing)
        ));
        let mut stale = source.clone();
        stale["playback"]["gain_decibels"] = (-1.0).into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_audio_clip_activation(
                &loaded,
                &stale,
                clip_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioClipDocumentSerializationError::SourceClipMismatch)
        ));
        let noncanonical = serde_json::to_vec_pretty(&source).unwrap();
        assert!(matches!(
            serialize_audio_clip_activation(
                &loaded,
                &noncanonical,
                clip_id,
                false,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioClipDocumentSerializationError::NonCanonicalSource)
        ));
    }

    #[test]
    fn audio_clip_gain_rejects_missing_stale_noncanonical_and_non_native_values() {
        let source: Value = serde_json::from_slice(CLIP).unwrap();
        let canonical = serde_json_canonicalizer::to_vec(&source).unwrap();
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [&canonical];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334460";

        assert!(matches!(
            serialize_audio_clip_gain(
                &loaded,
                &canonical,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                0.0,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioClipGainSerializationError::Document(
                AudioClipDocumentSerializationError::ClipMissing
            ))
        ));
        let mut stale = source.clone();
        stale["enabled"] = false.into();
        let stale = serde_json_canonicalizer::to_vec(&stale).unwrap();
        assert!(matches!(
            serialize_audio_clip_gain(&loaded, &stale, clip_id, 0.0, LoaderLimits::VERSION_1,),
            Err(AudioClipGainSerializationError::Document(
                AudioClipDocumentSerializationError::SourceClipMismatch
            ))
        ));
        let noncanonical = serde_json::to_vec_pretty(&source).unwrap();
        assert!(matches!(
            serialize_audio_clip_gain(
                &loaded,
                &noncanonical,
                clip_id,
                0.0,
                LoaderLimits::VERSION_1,
            ),
            Err(AudioClipGainSerializationError::Document(
                AudioClipDocumentSerializationError::NonCanonicalSource
            ))
        ));
        for gain in [f64::NAN, f64::INFINITY, 6.1] {
            assert!(matches!(
                serialize_audio_clip_gain(
                    &loaded,
                    &canonical,
                    clip_id,
                    gain,
                    LoaderLimits::VERSION_1,
                ),
                Err(AudioClipGainSerializationError::UnsupportedGain)
            ));
        }
        assert!(
            serialize_audio_clip_gain(
                &loaded,
                &canonical,
                clip_id,
                20.0 * 2.0_f64.log10(),
                LoaderLimits::VERSION_1,
            )
            .is_ok()
        );
    }

    #[test]
    fn mixer_save_rejects_layout_and_noncanonical_source_disagreement() {
        let state = MixerBaseState {
            channels: vec![MixerChannelBaseState {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334499".to_owned(),
                gain: LinearGain::UNITY,
                pan: Pan::CENTER,
                muted: false,
                solo: false,
                sends: Vec::new(),
            }],
            main_gain: LinearGain::UNITY,
            main_muted: false,
        };
        let canonical =
            serde_json_canonicalizer::to_vec(&serde_json::from_slice::<Value>(MIXER).unwrap())
                .unwrap();

        assert!(matches!(
            serialize_mixer_base_state(
                &canonical,
                PROJECT_ID,
                "018f47a2-6b2d-7c11-8a44-001122334468",
                &state,
                LoaderLimits::VERSION_1,
            ),
            Err(MixerStateSerializationError::ChannelIdentityMismatch { channel_index: 0 })
        ));
        assert!(matches!(
            serialize_mixer_base_state(
                MIXER,
                PROJECT_ID,
                "018f47a2-6b2d-7c11-8a44-001122334468",
                &MixerBaseState {
                    channels: vec![MixerChannelBaseState {
                        track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                        gain: LinearGain::UNITY,
                        pan: Pan::CENTER,
                        muted: false,
                        solo: false,
                        sends: Vec::new(),
                    }],
                    main_gain: LinearGain::UNITY,
                    main_muted: false,
                },
                LoaderLimits::VERSION_1,
            ),
            Err(MixerStateSerializationError::NonCanonicalSource)
        ));
    }

    #[test]
    fn mixer_loader_rejects_identity_shape_domain_and_graph_failures() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        for invalid in [
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace(
                    "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334468\"",
                    "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334456\"",
                ),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"gain_millionths\":500000", "\"gain_millionths\":2000001"),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"channels\":[{", "\"unexpected\":true,\"channels\":[{"),
            String::from_utf8(MIXER.to_vec()).unwrap().replace(
                "\"destination\":{\"kind\":\"main\"}",
                "\"destination\":{\"kind\":\"track\",\"track_id\":\"018f47a2-6b2d-7c11-8a44-001122334489\"}",
            ),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"channels\":[{", "\"channels\":[],\"unused\":[{"),
        ] {
            assert!(
                loaded
                    .load_mixer(invalid.as_bytes(), &asset_facts(), LoaderLimits::VERSION_1)
                    .is_err()
            );
        }
    }

    #[test]
    fn flat_main_compiler_rejects_unimplemented_graph_sources() {
        let parameters = ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true);
        let channel = LoadedMixerChannel {
            track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
            track_kind: TrackKind::Audio,
            parameters,
            solo: false,
            destination: MixerDestination::Track("018f47a2-6b2d-7c11-8a44-001122334471".to_owned()),
            sends: Vec::new(),
        };
        let mixer = LoadedMixer {
            project_id: "018f47a2-6b2d-7c11-8a44-001122334455".to_owned(),
            id: "018f47a2-6b2d-7c11-8a44-001122334468".to_owned(),
            channels: vec![channel.clone()],
            return_buses: Vec::new(),
            main_gain: LinearGain::UNITY,
            main_muted: false,
            cue_source: CueSource::Main,
            cue_gain: LinearGain::UNITY,
            cue_muted: false,
        };
        assert_eq!(
            mixer.compile_flat_main(),
            Err(MixerCompileError::UnsupportedPrimaryRouting)
        );

        let mut send_channel = channel.clone();
        send_channel.destination = MixerDestination::Main;
        send_channel.sends.push(LoadedMixerSend {
            id: "send".to_owned(),
            bus: MixerBus::A,
            tap: MixerTap::PostFader,
            parameters,
        });
        let mut mixer = LoadedMixer {
            channels: vec![send_channel],
            ..mixer
        };
        assert_eq!(
            mixer.compile_flat_main(),
            Err(MixerCompileError::UnsupportedSendsOrReturns)
        );

        mixer.channels[0].sends.clear();
        mixer.channels[0].track_kind = TrackKind::ExternalInput;
        assert_eq!(
            mixer.compile_flat_main(),
            Err(MixerCompileError::UnsupportedExternalInput)
        );
        assert_eq!(
            mixer.compile_main_graph(),
            Err(MixerCompileError::UnsupportedExternalInput)
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the complete route fixture keeps identity and graph rejection evidence together"
    )]
    fn mixer_loader_resolves_valid_group_send_and_return_graphs_before_capability_rejection() {
        let group = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334471",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Group","kind":"group","order":1,
            "active":true,"clip_ids":[],"extensions":{}
        }"#;
        let return_track = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334472",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Return","kind":"return","order":2,
            "active":true,"clip_ids":[],"extensions":{}
        }"#;
        let tracks: [&[u8]; 3] = [TRACK, group, return_track];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = br#"{
            "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334468",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455",
            "channels":[
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334456","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"track","track_id":"018f47a2-6b2d-7c11-8a44-001122334471"},
                    "sends":[{
                        "id":"018f47a2-6b2d-7c11-8a44-001122334469","bus":"a","tap":"post-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":true
                    }]
                },
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334471","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                },
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334472","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                }
            ],
            "return_buses":[{"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-001122334472"}],
            "main":{"gain_millionths":1000000,"mute":false},
            "cue":{"source":"main","gain_millionths":1000000,"mute":false},
            "extensions":{}
        }"#;

        for colliding_id in [
            "018f47a2-6b2d-7c11-8a44-001122334457",
            "018f47a2-6b2d-7c11-8a44-001122334464",
        ] {
            let invalid = String::from_utf8(mixer.to_vec())
                .unwrap()
                .replace("018f47a2-6b2d-7c11-8a44-001122334469", colliding_id);
            let error = loaded
                .load_mixer(invalid.as_bytes(), &asset_facts(), LoaderLimits::VERSION_1)
                .unwrap_err();
            assert_eq!(error.kind, LoadErrorKind::InvalidVersionOneDomain);
            assert!(error.message.contains("collides"));
        }

        let mixer = loaded
            .load_mixer(mixer, &asset_facts(), LoaderLimits::VERSION_1)
            .unwrap();
        assert_eq!(
            mixer.compile_flat_main(),
            Err(MixerCompileError::UnsupportedPrimaryRouting)
        );
        let graph = mixer.compile_main_graph().unwrap();
        assert_eq!(graph.project_id(), PROJECT_ID);
        assert_eq!(graph.processing_order(), &[0, 1, 2]);
        assert_eq!(
            graph.slots()[0].destination(),
            CompiledMixerDestination::Slot(1)
        );
        assert_eq!(graph.slots()[0].sends().len(), 1);
        assert_eq!(
            graph.slots()[0].sends()[0].id(),
            "018f47a2-6b2d-7c11-8a44-001122334469"
        );
        assert_eq!(graph.slots()[0].sends()[0].target_slot(), 2);
        assert_eq!(
            graph.slots()[0].sends()[0].tap(),
            CompiledMixerTap::PostFader
        );

        let feedback = br#"{
            "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334487",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455",
            "channels":[
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334456","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                },
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334471","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},
                    "sends":[{
                        "id":"018f47a2-6b2d-7c11-8a44-001122334488","bus":"a","tap":"pre-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":false
                    }]
                },
                {
                    "track_id":"018f47a2-6b2d-7c11-8a44-001122334472","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"track","track_id":"018f47a2-6b2d-7c11-8a44-001122334471"},"sends":[]
                }
            ],
            "return_buses":[{"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-001122334472"}],
            "main":{"gain_millionths":1000000,"mute":false},
            "cue":{"source":"main","gain_millionths":1000000,"mute":false},
            "extensions":{}
        }"#;
        assert!(
            loaded
                .load_mixer(feedback, &asset_facts(), LoaderLimits::VERSION_1)
                .is_err()
        );
    }

    #[test]
    fn automation_loader_resolves_targets_and_matches_exact_tick_vectors() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = loaded
            .load_mixer(MIXER, &asset_facts(), LoaderLimits::VERSION_1)
            .unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334479","project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[
                {
                    "id":"018f47a2-6b2d-7c11-8a44-001122334481",
                    "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","parameter":"gain"},
                    "interpolation":"linear",
                    "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
                },
                {
                    "id":"018f47a2-6b2d-7c11-8a44-001122334482",
                    "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","parameter":"mute"},
                    "interpolation":"hold",
                    "points":[{"tick":0,"value":false},{"tick":4,"value":true}]
                }
            ],
            "extensions":{}
        }"#;
        let automation = loaded
            .load_automation_takes(&mixer, &asset_facts(), &[take], LoaderLimits::VERSION_1)
            .unwrap();
        let lanes = automation.takes()[0].lanes();
        assert_eq!(
            lanes[0].value_at(0),
            Ok(AutomationValue::Integer(1_000_000))
        );
        assert_eq!(lanes[0].value_at(1), Ok(AutomationValue::Integer(666_667)));
        assert_eq!(lanes[0].value_at(2), Ok(AutomationValue::Integer(333_334)));
        assert_eq!(lanes[0].value_at(7), Ok(AutomationValue::Integer(0)));
        assert_eq!(lanes[1].value_at(3), Ok(AutomationValue::Boolean(false)));
        assert_eq!(lanes[1].value_at(4), Ok(AutomationValue::Boolean(true)));
        assert_eq!(
            lanes[0].value_at(MAX_SAFE_INTEGER + 1),
            Err(AutomationEvaluationError::TickOutOfRange)
        );
        assert_eq!(
            lanes[0].target(),
            AutomationTarget::Channel {
                slot: 0,
                parameter: AutomationChannelParameter::Gain,
            }
        );
        assert!(automation.takes()[0].is_active_at(0));
        assert!(automation.takes()[0].is_active_at(7));
        assert!(!automation.takes()[0].is_active_at(8));

        let invalid_boolean = String::from_utf8(take.to_vec())
            .unwrap()
            .replace("\"interpolation\":\"hold\"", "\"interpolation\":\"linear\"");
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &asset_facts(),
                    &[invalid_boolean.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_err()
        );
        assert_automation_schema_domain_rejections(&loaded, &mixer, take);
        let overlapping = String::from_utf8(take.to_vec())
            .unwrap()
            .replace(
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334479\"",
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334484\"",
            )
            .replace(
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334481\"",
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334483\"",
            )
            .replace(
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334482\"",
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334490\"",
            );
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &asset_facts(),
                    &[take, overlapping.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_err()
        );
    }

    fn assert_automation_schema_domain_rejections(
        loaded: &LoadedPlaybackProject,
        mixer: &LoadedMixer,
        take: &[u8],
    ) {
        for invalid in [
            String::from_utf8(take.to_vec()).unwrap().replace(
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334479\"",
                "\"id\":\"take\"",
            ),
            String::from_utf8(take.to_vec()).unwrap().replace(
                "\"id\":\"018f47a2-6b2d-7c11-8a44-001122334481\"",
                "\"id\":\"lane\"",
            ),
            String::from_utf8(take.to_vec())
                .unwrap()
                .replace("\"name\":\"Pass\"", "\"name\":\"\""),
            String::from_utf8(take.to_vec())
                .unwrap()
                .replace("\"extensions\":{}", "\"extensions\":false"),
            String::from_utf8(take.to_vec()).unwrap().replace(
                "\"track_id\":\"018f47a2-6b2d-7c11-8a44-001122334456\"",
                "\"track_id\":\"track\"",
            ),
        ] {
            assert!(
                loaded
                    .load_automation_takes(
                        mixer,
                        &asset_facts(),
                        &[invalid.as_bytes()],
                        LoaderLimits::VERSION_1,
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn automation_compiler_evaluates_exact_sample_starts() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = loaded
            .load_mixer(MIXER, &asset_facts(), LoaderLimits::VERSION_1)
            .unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334479","project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"018f47a2-6b2d-7c11-8a44-001122334480",
                "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let automation = loaded
            .load_automation_takes(&mixer, &asset_facts(), &[take], LoaderLimits::VERSION_1)
            .unwrap();
        let graph = mixer.compile_main_graph().unwrap();
        let compiled = loaded.compile_main_automation(&graph, &automation).unwrap();
        assert_eq!(compiled.project_id(), PROJECT_ID);
        assert_eq!(compiled.target_count(), 1);
        let mut foreign_graph = graph.clone();
        foreign_graph.project_id = "foreign-project".to_owned();
        assert!(matches!(
            loaded.compile_main_automation(&foreign_graph, &automation),
            Err(AutomationCompileError::ProjectMismatch)
        ));
        let mut frame_values = Vec::new();
        compiled
            .visit_frame_values(25, |target, value| {
                frame_values.push((target, value));
            })
            .unwrap();
        assert_eq!(
            frame_values,
            vec![(
                AutomationTarget::Channel {
                    slot: 0,
                    parameter: AutomationChannelParameter::Gain,
                },
                AutomationValue::Integer(666_667),
            )]
        );
        frame_values.clear();
        compiled
            .visit_frame_values(200, |target, value| frame_values.push((target, value)))
            .unwrap();
        assert!(frame_values.is_empty());

        let cue_take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334485","project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Cue pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:06:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"018f47a2-6b2d-7c11-8a44-001122334486",
                "target":{"kind":"mixer-cue","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let cue_automation = loaded
            .load_automation_takes(&mixer, &asset_facts(), &[cue_take], LoaderLimits::VERSION_1)
            .unwrap();
        assert!(matches!(
            loaded.compile_main_automation(&graph, &cue_automation),
            Err(AutomationCompileError::UnsupportedCueTarget)
        ));
    }

    #[test]
    fn automation_loader_enforces_intervals_calendar_and_limits() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = loaded
            .load_mixer(MIXER, &asset_facts(), LoaderLimits::VERSION_1)
            .unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334479","project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"First","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"018f47a2-6b2d-7c11-8a44-001122334481",
                "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let adjacent = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-001122334484","project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Adjacent","enabled":true,
            "captured_at_utc":"2026-07-28T12:06:00.000Z",
            "start_tick":8,"end_tick":16,
            "lanes":[{
                "id":"018f47a2-6b2d-7c11-8a44-001122334483",
                "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","parameter":"gain"},
                "interpolation":"hold","points":[{"tick":8,"value":500000}]
            }],
            "extensions":{}
        }"#;
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &asset_facts(),
                    &[take, adjacent],
                    LoaderLimits::VERSION_1,
                )
                .is_ok()
        );

        let disabled_overlap = String::from_utf8(adjacent.to_vec())
            .unwrap()
            .replace("\"enabled\":true", "\"enabled\":false")
            .replace(
                "\"start_tick\":8,\"end_tick\":16",
                "\"start_tick\":0,\"end_tick\":16",
            );
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &asset_facts(),
                    &[take, disabled_overlap.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_ok()
        );

        let mut point_limit = LoaderLimits::VERSION_1;
        point_limit.max_automation_points = 1;
        assert!(matches!(
            loaded
                .load_automation_takes(&mixer, &asset_facts(), &[take], point_limit)
                .unwrap_err()
                .kind,
            LoadErrorKind::LimitExceeded {
                collection: CollectionLimit::AutomationPoints,
                ..
            }
        ));

        let invalid_calendar = String::from_utf8(take.to_vec())
            .unwrap()
            .replace("2026-07-28T12:05:00.000Z", "2026-02-29T12:05:00.000Z");
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &asset_facts(),
                    &[invalid_calendar.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_err()
        );
    }

    #[test]
    fn rejects_foreign_unknown_and_duplicate_documents() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let foreign_timeline = String::from_utf8(TIMELINE.to_vec()).unwrap().replacen(
            "\"project_id\":\"018f47a2-6b2d-7c11-8a44-001122334455\"",
            "\"project_id\":\"018f47a2-6b2d-7c11-8a44-001122334491\"",
            1,
        );
        assert_eq!(
            load(
                input(
                    foreign_timeline.as_bytes(),
                    &tracks,
                    &clips,
                    Some(ARRANGEMENT),
                ),
                LoaderLimits::VERSION_1,
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::ForeignProject
        );

        let unknown = br#"{
            "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334458",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","ticks_per_quarter":1920,
            "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
            "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
            "unknown":true,"extensions":{}
        }"#;
        assert!(matches!(
            load(
                input(unknown, &tracks, &clips, Some(ARRANGEMENT)),
                LoaderLimits::VERSION_1
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::Json {
                rejection: JsonRejection::TypedDocumentMismatch,
                ..
            }
        ));

        let duplicate = br#"{
            "format":"hyper-timeline","format":"hyper-timeline",
            "format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334458","project_id":"018f47a2-6b2d-7c11-8a44-001122334455",
            "ticks_per_quarter":1920,
            "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
            "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
            "extensions":{}
        }"#;
        assert!(matches!(
            load(
                input(duplicate, &tracks, &clips, Some(ARRANGEMENT)),
                LoaderLimits::VERSION_1
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::Json {
                rejection: JsonRejection::DuplicateObjectName,
                ..
            }
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one transaction covers legacy rejection, atomic publication, strict parsing, and authority conflicts"
    )]
    fn enforces_limits_hierarchy_and_publishes_scene_authority_atomically() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let mut limits = LoaderLimits::VERSION_1;
        limits.max_clip_documents = 0;
        assert!(matches!(
            load(input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)), limits)
                .unwrap_err()
                .kind,
            LoadErrorKind::LimitExceeded {
                collection: CollectionLimit::ClipDocuments,
                ..
            }
        ));

        let bad_parent = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334456",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Audio","kind":"audio","order":0,
            "active":true,"parent_track_id":"018f47a2-6b2d-7c11-8a44-001122334489","clip_ids":["018f47a2-6b2d-7c11-8a44-001122334460"],
            "extensions":{}
        }"#;
        let bad_tracks: [&[u8]; 1] = [bad_parent];
        assert_eq!(
            load(
                input(TIMELINE, &bad_tracks, &clips, Some(ARRANGEMENT)),
                LoaderLimits::VERSION_1
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::InvalidVersionOneDomain
        );

        let scene_arrangement = br#"{
            "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334463",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Main","enabled":true,"placements":[],
            "scene_launches":[{"id":"018f47a2-6b2d-7c11-8a44-001122334465","tick":0,"scene_id":"018f47a2-6b2d-7c11-8a44-001122334461"}],
            "extensions":{}
        }"#;
        let scenes: [&[u8]; 1] = [SCENE];
        let loaded = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(scene_arrangement)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(loaded.scene_count(), 1);
        assert_eq!(loaded.scene_name(0), Some("Verse"));
        assert_eq!(loaded.scene_name(1), None);
        let later_scene = br#"{
            "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334462",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Outro","order":1,"enabled":true,
            "launch_quantization":{"mode":"immediate"},"slots":[],"extensions":{}
        }"#;
        let unordered_scenes: [&[u8]; 2] = [later_scene, SCENE];
        let ordered = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &unordered_scenes, None),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(ordered.scene_name(0), Some("Verse"));
        assert_eq!(ordered.scene_name(1), Some("Outro"));
        assert!(matches!(
            loaded.compile(
                &[MediaAssetFact {
                    id: "018f47a2-6b2d-7c11-8a44-001122334457",
                    sample_rate_hz: 48_000,
                    frame_count: 100
                }],
                GraphLimits::VERSION_1
            ),
            Err(AdapterCompileError::Graph(
                GraphError::UnsupportedSceneLaunches
            ))
        ));
        let graph = loaded
            .compile_playback(
                &[MediaAssetFact {
                    id: "018f47a2-6b2d-7c11-8a44-001122334457",
                    sample_rate_hz: 48_000,
                    frame_count: 100,
                }],
                GraphLimits::VERSION_1,
                SceneLimits::VERSION_1,
            )
            .unwrap();
        assert_eq!(graph.project_id(), PROJECT_ID);
        assert!(graph.arrangement().entries().is_empty());
        assert_eq!(graph.scene_authority().track_count(), 1);
        assert_eq!(graph.scene_audio().len(), 1);
        assert_eq!(graph.interactive_scene_audio().len(), 1);
        assert_eq!(
            graph.interactive_scene_audio()[0].availability,
            InteractiveSceneAudioAvailability::Ready
        );
        assert_eq!(
            graph.scene_audio()[0].launch_id,
            "018f47a2-6b2d-7c11-8a44-001122334465"
        );
        assert_eq!(graph.scene_audio()[0].compiled.launch_frame(), 0);

        let malformed_scene = String::from_utf8(SCENE.to_vec()).unwrap().replace(
            "\"action\":\"play\",\"clip_id\":\"018f47a2-6b2d-7c11-8a44-001122334460\"",
            "\"action\":\"stop\",\"clip_id\":\"018f47a2-6b2d-7c11-8a44-001122334460\"",
        );
        let malformed_scenes: [&[u8]; 1] = [malformed_scene.as_bytes()];
        assert!(matches!(
            load(
                input_with_scenes(
                    TIMELINE,
                    &tracks,
                    &clips,
                    &malformed_scenes,
                    Some(scene_arrangement),
                ),
                LoaderLimits::VERSION_1,
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::Json {
                rejection: JsonRejection::TypedDocumentMismatch,
                ..
            }
        ));

        let mixed_arrangement = br#"{
            "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334463",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Main","enabled":true,
            "placements":[{
                "id":"018f47a2-6b2d-7c11-8a44-001122334464","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","clip_id":"018f47a2-6b2d-7c11-8a44-001122334460",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }],
            "scene_launches":[{"id":"018f47a2-6b2d-7c11-8a44-001122334465","tick":4,"scene_id":"018f47a2-6b2d-7c11-8a44-001122334461"}],
            "extensions":{}
        }"#;
        let mixed = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(mixed_arrangement)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            mixed.compile_playback(
                &[MediaAssetFact {
                    id: "018f47a2-6b2d-7c11-8a44-001122334457",
                    sample_rate_hz: 48_000,
                    frame_count: 100,
                }],
                GraphLimits::VERSION_1,
                SceneLimits::VERSION_1,
            ),
            Err(AdapterCompileError::MixedTrackAuthority { placement_index: 0 })
        ));
    }

    #[test]
    fn combined_scene_graph_rejects_quantization_and_audio_consolidation_atomically() {
        let tracks: [&[u8]; 1] = [TRACK];
        let off_grid_scene = String::from_utf8(SCENE.to_vec()).unwrap().replace(
            "\"launch_quantization\":{\"mode\":\"immediate\"}",
            "\"launch_quantization\":{\"mode\":\"grid\",\"quantum_ticks\":2}",
        );
        let scenes: [&[u8]; 1] = [off_grid_scene.as_bytes()];
        let arrangement = br#"{
            "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334463",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Main","enabled":true,"placements":[],
            "scene_launches":[{"id":"018f47a2-6b2d-7c11-8a44-001122334465","tick":1,"scene_id":"018f47a2-6b2d-7c11-8a44-001122334461"}],
            "extensions":{}
        }"#;
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(arrangement)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let facts = [MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122334457",
            sample_rate_hz: 48_000,
            frame_count: 100,
        }];
        assert!(matches!(
            loaded.compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1,),
            Err(AdapterCompileError::Scene(
                SceneCompileError::LaunchOffGrid { launch_index: 0 }
            ))
        ));

        let transposed_clip = String::from_utf8(CLIP.to_vec())
            .unwrap()
            .replace("\"transpose_cents\":0", "\"transpose_cents\":100");
        let transposed_clips: [&[u8]; 1] = [transposed_clip.as_bytes()];
        let scenes: [&[u8]; 1] = [SCENE];
        let loaded = load(
            input_with_scenes(
                TIMELINE,
                &tracks,
                &transposed_clips,
                &scenes,
                Some(arrangement),
            ),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            loaded.compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1,),
            Err(AdapterCompileError::SceneAudio {
                binding_index: 0,
                error: CompileError::ConsolidationRequired(_),
            })
        ));
    }

    #[test]
    fn rejects_asset_collision_with_non_graph_resource_ids() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert!(matches!(
            loaded.compile(
                &[MediaAssetFact {
                    id: "018f47a2-6b2d-7c11-8a44-001122334458",
                    sample_rate_hz: 48_000,
                    frame_count: 100
                }],
                GraphLimits::VERSION_1
            ),
            Err(AdapterCompileError::AssetIdentityCollision { asset_index: 0 })
        ));
        assert!(matches!(
            loaded.compile(
                &[MediaAssetFact {
                    id: "asset",
                    sample_rate_hz: 48_000,
                    frame_count: 100
                }],
                GraphLimits::VERSION_1
            ),
            Err(AdapterCompileError::AssetIdentityCollision { asset_index: 0 })
        ));
    }

    #[test]
    fn validates_unplaced_clip_structure_during_loading() {
        let tracks: [&[u8]; 1] = [TRACK];
        let invalid_clip = String::from_utf8(CLIP.to_vec()).unwrap().replacen(
            "\"duration_ticks\":4",
            "\"duration_ticks\":0",
            1,
        );
        let clips: [&[u8]; 1] = [invalid_clip.as_bytes()];
        assert_eq!(
            load(
                input(TIMELINE, &tracks, &clips, None),
                LoaderLimits::VERSION_1
            )
            .unwrap_err()
            .kind,
            LoadErrorKind::InvalidVersionOneDomain
        );
    }
}
