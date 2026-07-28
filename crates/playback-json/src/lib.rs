#![doc = "Strict bounded JSON adapter for verified Hyper Module playback resources."]
#![forbid(unsafe_code)]

mod automation;
mod strict;

pub use automation::{
    AutomationChannelParameter, AutomationCompileError, AutomationEvaluationError,
    AutomationInterpolation, AutomationLane, AutomationMainParameter, AutomationPoint,
    AutomationSendParameter, AutomationTake, AutomationTarget, AutomationValue, CompiledAutomation,
    LoadedAutomationSet,
};

use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;

use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
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
    ClipCompileSpec, CompileError, CompiledPlacement, CompiledSceneAudioClip, TempoEvent,
    TickRange, Timeline, TimelineError, WarpMarker, compile_scene_audio_clip,
};
use serde::Deserialize;
use serde::de::IgnoredAny;

use crate::strict::{StrictJsonError, StrictJsonErrorKind};

const VERSION_1_SAMPLE_RATE_HZ: u32 = 48_000;
const VERSION_1_TICKS_PER_QUARTER: u64 = 1_920;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

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

#[derive(Clone, Debug)]
struct LoadedTrack {
    id: String,
    kind: TrackKind,
    order: u8,
    active: bool,
    parent_track_id: Option<String>,
    clip_ids: Vec<String>,
}

#[derive(Clone, Debug)]
struct LoadedClip {
    id: String,
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

#[derive(Clone, Debug)]
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
    enabled: bool,
    order: u8,
    launch_quantization: LaunchQuantization,
    slots: Vec<LoadedSceneSlot>,
}

#[derive(Clone, Debug)]
struct LoadedSceneLaunch {
    id: String,
    tick: u64,
    scene_id: String,
}

#[derive(Clone, Debug)]
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
    id: String,
    channels: Vec<LoadedMixerChannel>,
    return_buses: Vec<(MixerBus, String)>,
    main_gain: LinearGain,
    main_muted: bool,
    cue_source: CueSource,
    cue_gain: LinearGain,
    cue_muted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CueSource {
    Main,
    Solo,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledMainMixer {
    slots: Vec<CompiledMixerSlot>,
    master_gain: LinearGain,
}

impl CompiledMainMixer {
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
    slots: Vec<CompiledGraphSlot>,
    processing_order: Vec<usize>,
    master_gain: LinearGain,
    main_gain: LinearGain,
    main_muted: bool,
}

impl CompiledMixerGraph {
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

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledMixerSend {
    target_slot: usize,
    tap: CompiledMixerTap,
    parameters: ChannelParameters,
    enabled: bool,
}

impl CompiledMixerSend {
    #[must_use]
    pub const fn target_slot(self) -> usize {
        self.target_slot
    }

    #[must_use]
    pub const fn tap(self) -> CompiledMixerTap {
        self.tap
    }

    #[must_use]
    pub const fn parameters(self) -> ChannelParameters {
        self.parameters
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
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

    #[must_use]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn clip_count(&self) -> usize {
        self.clips.len()
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn placement_count(&self) -> usize {
        self.arrangement
            .as_ref()
            .map_or(0, |arrangement| arrangement.placements.len())
    }

    /// Loads and resolves one strict version 1 mixer resource against this
    /// project's structural tracks.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] for strict JSON, collection, identity, ordering,
    /// routing, send, return, solo, or parameter-domain failure.
    pub fn load_mixer(&self, bytes: &[u8], limits: LoaderLimits) -> Result<LoadedMixer, LoadError> {
        load_mixer_document(self, bytes, limits)
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
        Ok(CompiledPlaybackGraph {
            arrangement,
            scene_authority,
            scene_audio,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnedScheduleEntry {
    pub placement_id: String,
    pub track_id: String,
    pub clip_id: String,
    pub asset_id: String,
    pub compiled: CompiledPlacement,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OwnedCompiledSchedule {
    entries: Vec<OwnedScheduleEntry>,
}

impl OwnedCompiledSchedule {
    #[must_use]
    pub fn entries(&self) -> &[OwnedScheduleEntry] {
        &self.entries
    }
}

#[derive(Debug)]
pub struct CompiledPlaybackGraph<'project> {
    arrangement: OwnedCompiledSchedule,
    scene_authority: CompiledScenePerformance<'project>,
    scene_audio: Vec<CompiledSceneAudioBinding<'project>>,
}

impl<'project> CompiledPlaybackGraph<'project> {
    #[must_use]
    pub const fn arrangement(&self) -> &OwnedCompiledSchedule {
        &self.arrangement
    }

    #[must_use]
    pub const fn scene_authority(&self) -> &CompiledScenePerformance<'project> {
        &self.scene_authority
    }

    #[must_use]
    pub fn scene_audio(&self) -> &[CompiledSceneAudioBinding<'project>] {
        &self.scene_audio
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

/// Strictly loads already package-verified playback resource documents.
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
    if input.project_id.is_empty() {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::InvalidProjectIdentity,
            message: "project identifier must not be empty".to_owned(),
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
                (track_id, LoadedSceneSlotAction::Play { clip_id })
            }
            SceneSlotDocument::Stop { track_id } => (track_id, LoadedSceneSlotAction::Stop),
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
    validate_mixer_document(project, &document, location)?;

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
    document: &MixerDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if document.id.is_empty() || project_contains_identity(project, &document.id) {
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
            {
                return domain_error(location, "mixer send identity is empty or collides");
            }
        }
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
    if id.is_empty() {
        return Err(LoadError {
            location: None,
            kind: LoadErrorKind::InvalidProjectIdentity,
            message: "resource identifier must not be empty".to_owned(),
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
        if !identities.insert(asset.id) {
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
    own_schedule(&compiled)
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
            launch_id: playing.launch_id(),
            scene_id: playing.scene_id(),
            clip_id: playing.clip_id(),
            asset_id: &clip.asset_id,
            compiled,
        });
    }
    Ok(output)
}

fn own_schedule(
    compiled: &CompiledSchedule<'_>,
) -> Result<OwnedCompiledSchedule, AdapterCompileError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(compiled.entries().len())
        .map_err(|_| AdapterCompileError::CapacityUnavailable)?;
    for entry in compiled.entries() {
        entries.push(OwnedScheduleEntry {
            placement_id: try_owned(entry.placement_id)?,
            track_id: try_owned(entry.track_id)?,
            clip_id: try_owned(entry.clip_id)?,
            asset_id: try_owned(entry.asset_id)?,
            compiled: entry.compiled,
        });
    }
    Ok(OwnedCompiledSchedule { entries })
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
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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
    #[serde(rename = "name")]
    _name: String,
    kind: TrackKindDocument,
    order: u8,
    active: bool,
    parent_track_id: Option<String>,
    clip_ids: Vec<String>,
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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
    #[serde(rename = "name")]
    _name: String,
    enabled: bool,
    duration_ticks: u64,
    source: ClipSourceDocument,
    #[serde(rename = "loop")]
    loop_spec: ClipLoopDocument,
    playback: ClipPlaybackDocument,
    fades: ClipFadesDocument,
    warp_markers: Vec<WarpMarkerDocument>,
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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
    #[serde(rename = "name")]
    _name: String,
    order: u8,
    enabled: bool,
    launch_quantization: SceneQuantizationDocument,
    slots: Vec<SceneSlotDocument>,
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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
    #[serde(rename = "name")]
    _name: String,
    enabled: bool,
    placements: Vec<PlacementDocument>,
    scene_launches: Vec<SceneLaunchDocument>,
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
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

    const PROJECT_ID: &str = "project";
    const TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    const TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"track",
        "project_id":"project","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["clip"],"extensions":{}
    }"#;
    const CLIP: &[u8] = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"clip",
        "project_id":"project","track_id":"track","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"asset","start_frame":0,"frame_count":100},
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
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[{
            "id":"placement","track_id":"track","clip_id":"clip",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    const SCENE: &[u8] = br#"{
        "format":"hyper-scene","format_version":1,"id":"scene",
        "project_id":"project","name":"Verse","order":0,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"track","action":"play","clip_id":"clip"}],
        "extensions":{}
    }"#;
    const MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"mixer",
        "project_id":"project",
        "channels":[{
            "track_id":"track","gain_millionths":500000,
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

    #[test]
    fn loads_and_compiles_the_explicit_playback_documents() {
        let tracks: [&[u8]; 1] = [TRACK];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, Some(ARRANGEMENT)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(loaded.project_id(), PROJECT_ID);
        assert_eq!(loaded.track_count(), 1);
        assert_eq!(loaded.clip_count(), 1);
        assert_eq!(loaded.placement_count(), 1);
        let schedule = loaded
            .compile(
                &[MediaAssetFact {
                    id: "asset",
                    sample_rate_hz: 48_000,
                    frame_count: 100,
                }],
                GraphLimits::VERSION_1,
            )
            .unwrap();
        assert_eq!(schedule.entries().len(), 1);
        assert_eq!(schedule.entries()[0].asset_id, "asset");
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
        let mixer = loaded.load_mixer(MIXER, LoaderLimits::VERSION_1).unwrap();
        assert_eq!(mixer.id(), "mixer");
        assert_eq!(mixer.channel_count(), 1);
        assert_eq!(mixer.cue_source(), CueSource::Solo);
        assert_eq!(
            mixer.cue_gain(),
            LinearGain::from_millionths(750_000).unwrap()
        );
        assert!(!mixer.cue_muted());

        let compiled = mixer.compile_flat_main().unwrap();
        assert_eq!(compiled.master_gain(), LinearGain::SILENCE);
        assert_eq!(compiled.slots()[0].track_id(), "track");
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
                .replace("\"id\":\"mixer\"", "\"id\":\"track\""),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"gain_millionths\":500000", "\"gain_millionths\":2000001"),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"channels\":[{", "\"unexpected\":true,\"channels\":[{"),
            String::from_utf8(MIXER.to_vec()).unwrap().replace(
                "\"destination\":{\"kind\":\"main\"}",
                "\"destination\":{\"kind\":\"track\",\"track_id\":\"missing\"}",
            ),
            String::from_utf8(MIXER.to_vec())
                .unwrap()
                .replace("\"channels\":[{", "\"channels\":[],\"unused\":[{"),
        ] {
            assert!(
                loaded
                    .load_mixer(invalid.as_bytes(), LoaderLimits::VERSION_1)
                    .is_err()
            );
        }
    }

    #[test]
    fn flat_main_compiler_rejects_unimplemented_graph_sources() {
        let parameters = ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true);
        let channel = LoadedMixerChannel {
            track_id: "track".to_owned(),
            track_kind: TrackKind::Audio,
            parameters,
            solo: false,
            destination: MixerDestination::Track("group".to_owned()),
            sends: Vec::new(),
        };
        let mixer = LoadedMixer {
            id: "mixer".to_owned(),
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
    fn mixer_loader_resolves_valid_group_send_and_return_graphs_before_capability_rejection() {
        let group = br#"{
            "format":"hyper-track","format_version":1,"id":"group",
            "project_id":"project","name":"Group","kind":"group","order":1,
            "active":true,"clip_ids":[],"extensions":{}
        }"#;
        let return_track = br#"{
            "format":"hyper-track","format_version":1,"id":"return",
            "project_id":"project","name":"Return","kind":"return","order":2,
            "active":true,"clip_ids":[],"extensions":{}
        }"#;
        let tracks: [&[u8]; 3] = [TRACK, group, return_track];
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input(TIMELINE, &tracks, &clips, None),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = br#"{
            "format":"hyper-mixer","format_version":1,"id":"mixer",
            "project_id":"project",
            "channels":[
                {
                    "track_id":"track","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"track","track_id":"group"},
                    "sends":[{
                        "id":"send-a","bus":"a","tap":"post-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":true
                    }]
                },
                {
                    "track_id":"group","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                },
                {
                    "track_id":"return","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                }
            ],
            "return_buses":[{"bus":"a","track_id":"return"}],
            "main":{"gain_millionths":1000000,"mute":false},
            "cue":{"source":"main","gain_millionths":1000000,"mute":false},
            "extensions":{}
        }"#;

        let mixer = loaded.load_mixer(mixer, LoaderLimits::VERSION_1).unwrap();
        assert_eq!(
            mixer.compile_flat_main(),
            Err(MixerCompileError::UnsupportedPrimaryRouting)
        );
        let graph = mixer.compile_main_graph().unwrap();
        assert_eq!(graph.processing_order(), &[0, 1, 2]);
        assert_eq!(
            graph.slots()[0].destination(),
            CompiledMixerDestination::Slot(1)
        );
        assert_eq!(graph.slots()[0].sends().len(), 1);
        assert_eq!(graph.slots()[0].sends()[0].target_slot(), 2);
        assert_eq!(
            graph.slots()[0].sends()[0].tap(),
            CompiledMixerTap::PostFader
        );

        let feedback = br#"{
            "format":"hyper-mixer","format_version":1,"id":"feedback",
            "project_id":"project",
            "channels":[
                {
                    "track_id":"track","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},"sends":[]
                },
                {
                    "track_id":"group","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"main"},
                    "sends":[{
                        "id":"feedback-send","bus":"a","tap":"pre-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":false
                    }]
                },
                {
                    "track_id":"return","gain_millionths":1000000,
                    "pan_millionths":0,"mute":false,"solo":false,
                    "destination":{"kind":"track","track_id":"group"},"sends":[]
                }
            ],
            "return_buses":[{"bus":"a","track_id":"return"}],
            "main":{"gain_millionths":1000000,"mute":false},
            "cue":{"source":"main","gain_millionths":1000000,"mute":false},
            "extensions":{}
        }"#;
        assert!(
            loaded
                .load_mixer(feedback, LoaderLimits::VERSION_1)
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
        let mixer = loaded.load_mixer(MIXER, LoaderLimits::VERSION_1).unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"take","project_id":"project","name":"Pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[
                {
                    "id":"lane-a",
                    "target":{"kind":"mixer-channel","track_id":"track","parameter":"gain"},
                    "interpolation":"linear",
                    "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
                },
                {
                    "id":"lane-b",
                    "target":{"kind":"mixer-channel","track_id":"track","parameter":"mute"},
                    "interpolation":"hold",
                    "points":[{"tick":0,"value":false},{"tick":4,"value":true}]
                }
            ],
            "extensions":{}
        }"#;
        let automation = loaded
            .load_automation_takes(&mixer, &[take], LoaderLimits::VERSION_1)
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
                    &[invalid_boolean.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_err()
        );
        let overlapping = String::from_utf8(take.to_vec())
            .unwrap()
            .replace("\"id\":\"take\"", "\"id\":\"take-2\"")
            .replace("\"id\":\"lane-a\"", "\"id\":\"lane-c\"")
            .replace("\"id\":\"lane-b\"", "\"id\":\"lane-d\"");
        assert!(
            loaded
                .load_automation_takes(
                    &mixer,
                    &[take, overlapping.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_err()
        );
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
        let mixer = loaded.load_mixer(MIXER, LoaderLimits::VERSION_1).unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"take","project_id":"project","name":"Pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"lane",
                "target":{"kind":"mixer-channel","track_id":"track","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let automation = loaded
            .load_automation_takes(&mixer, &[take], LoaderLimits::VERSION_1)
            .unwrap();
        let graph = mixer.compile_main_graph().unwrap();
        let compiled = loaded.compile_main_automation(&graph, &automation).unwrap();
        assert_eq!(compiled.target_count(), 1);
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
            "id":"cue-take","project_id":"project","name":"Cue pass","enabled":true,
            "captured_at_utc":"2026-07-28T12:06:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"cue-lane",
                "target":{"kind":"mixer-cue","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let cue_automation = loaded
            .load_automation_takes(&mixer, &[cue_take], LoaderLimits::VERSION_1)
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
        let mixer = loaded.load_mixer(MIXER, LoaderLimits::VERSION_1).unwrap();
        let take = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"take","project_id":"project","name":"First","enabled":true,
            "captured_at_utc":"2026-07-28T12:05:00.000Z",
            "start_tick":0,"end_tick":8,
            "lanes":[{
                "id":"lane-a",
                "target":{"kind":"mixer-channel","track_id":"track","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
            }],
            "extensions":{}
        }"#;
        let adjacent = br#"{
            "format":"hyper-automation-take","format_version":1,
            "id":"take-2","project_id":"project","name":"Adjacent","enabled":true,
            "captured_at_utc":"2026-07-28T12:06:00.000Z",
            "start_tick":8,"end_tick":16,
            "lanes":[{
                "id":"lane-c",
                "target":{"kind":"mixer-channel","track_id":"track","parameter":"gain"},
                "interpolation":"hold","points":[{"tick":8,"value":500000}]
            }],
            "extensions":{}
        }"#;
        assert!(
            loaded
                .load_automation_takes(&mixer, &[take, adjacent], LoaderLimits::VERSION_1)
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
                    &[take, disabled_overlap.as_bytes()],
                    LoaderLimits::VERSION_1
                )
                .is_ok()
        );

        let mut point_limit = LoaderLimits::VERSION_1;
        point_limit.max_automation_points = 1;
        assert!(matches!(
            loaded
                .load_automation_takes(&mixer, &[take], point_limit)
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
            "\"project_id\":\"project\"",
            "\"project_id\":\"foreign\"",
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
            "format":"hyper-timeline","format_version":1,"id":"timeline",
            "project_id":"project","ticks_per_quarter":1920,
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
            "format_version":1,"id":"timeline","project_id":"project",
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
            "format":"hyper-track","format_version":1,"id":"track",
            "project_id":"project","name":"Audio","kind":"audio","order":0,
            "active":true,"parent_track_id":"missing","clip_ids":["clip"],
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
            "format":"hyper-arrangement","format_version":1,"id":"arrangement",
            "project_id":"project","name":"Main","enabled":true,"placements":[],
            "scene_launches":[{"id":"launch","tick":0,"scene_id":"scene"}],
            "extensions":{}
        }"#;
        let scenes: [&[u8]; 1] = [SCENE];
        let loaded = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(scene_arrangement)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(loaded.scene_count(), 1);
        assert!(matches!(
            loaded.compile(
                &[MediaAssetFact {
                    id: "asset",
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
                    id: "asset",
                    sample_rate_hz: 48_000,
                    frame_count: 100,
                }],
                GraphLimits::VERSION_1,
                SceneLimits::VERSION_1,
            )
            .unwrap();
        assert!(graph.arrangement().entries().is_empty());
        assert_eq!(graph.scene_authority().track_count(), 1);
        assert_eq!(graph.scene_audio().len(), 1);
        assert_eq!(graph.scene_audio()[0].launch_id, "launch");
        assert_eq!(graph.scene_audio()[0].compiled.launch_frame(), 0);

        let malformed_scene = String::from_utf8(SCENE.to_vec()).unwrap().replace(
            "\"action\":\"play\",\"clip_id\":\"clip\"",
            "\"action\":\"stop\",\"clip_id\":\"clip\"",
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
            "format":"hyper-arrangement","format_version":1,"id":"arrangement",
            "project_id":"project","name":"Main","enabled":true,
            "placements":[{
                "id":"placement","track_id":"track","clip_id":"clip",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }],
            "scene_launches":[{"id":"launch","tick":4,"scene_id":"scene"}],
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
                    id: "asset",
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
            "format":"hyper-arrangement","format_version":1,"id":"arrangement",
            "project_id":"project","name":"Main","enabled":true,"placements":[],
            "scene_launches":[{"id":"launch","tick":1,"scene_id":"scene"}],
            "extensions":{}
        }"#;
        let clips: [&[u8]; 1] = [CLIP];
        let loaded = load(
            input_with_scenes(TIMELINE, &tracks, &clips, &scenes, Some(arrangement)),
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let facts = [MediaAssetFact {
            id: "asset",
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
                    id: "timeline",
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
