#![doc = "Post-verification playback session assembly for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::sync::Arc;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{ClipPlanError, FrameRange, validate_native_clip_gain};
use hyper_media_cache::{
    CacheBuildError, CacheLimits, DecodedMediaCache, DecodedMediaCacheBuilder, PreparePlaybackError,
};
use hyper_playback_compiler::scene::{RecordedSceneExtent, SceneLimits};
use hyper_playback_compiler::{FrameTimelineError, graph::GraphLimits};
use hyper_playback_json::{
    AdapterCompileError, AutomationCompileError, AutomationInterpolation, AutomationTarget,
    CompiledAutomation, CompiledMixerGraph, CompiledMixerTap, CompiledPlaybackGraph, LoadError,
    LoadedAutomationSet, LoadedPlaybackProject, LoaderLimits, MediaAssetFact, MixerCompileError,
    PlaybackJsonInput, load,
};

/// Canonical resource bytes supplied to playback-session assembly.
///
/// This carrier performs no filesystem access and encodes no package
/// provenance. Callers must not treat construction as evidence of
/// canonicalization, digest, manifest, or media verification.
#[derive(Debug)]
pub struct PlaybackDocumentBytes {
    pub project_id: String,
    pub sample_rate_hz: u32,
    pub timeline: Vec<u8>,
    pub tracks: Vec<Vec<u8>>,
    pub clips: Vec<Vec<u8>>,
    pub scenes: Vec<Vec<u8>>,
    pub arrangement: Option<Vec<u8>>,
    pub mixer: Vec<u8>,
    pub automation_takes: Vec<Vec<u8>>,
}

/// One decoded asset supplied outside the real-time path.
///
/// This carrier encodes no package or digest provenance.
#[derive(Debug)]
pub struct DecodedPlaybackAsset {
    asset_id: String,
    audio: DecodedAudio,
}

impl DecodedPlaybackAsset {
    #[must_use]
    pub const fn new(asset_id: String, audio: DecodedAudio) -> Self {
        Self { asset_id, audio }
    }

    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }

    #[must_use]
    pub const fn audio(&self) -> &DecodedAudio {
        &self.audio
    }
}

/// Bounded policies applied while converting verified package data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackSessionAssemblyLimits {
    pub documents: LoaderLimits,
    pub media_cache: CacheLimits,
}

#[derive(Debug)]
pub struct PlaybackSessionAssemblyInput {
    pub documents: PlaybackDocumentBytes,
    pub decoded_assets: Vec<DecodedPlaybackAsset>,
    pub limits: PlaybackSessionAssemblyLimits,
}

/// Fully owned state required to prepare a live playback runtime.
#[derive(Debug)]
pub struct PreparedPlaybackSession {
    project: LoadedPlaybackProject,
    mixer: CompiledMixerGraph,
    automation: Option<CompiledAutomation>,
    media: DecodedMediaCache,
    extent: PlaybackExtent,
    arrangement: Arc<PlaybackArrangement>,
    automation_view: Arc<PlaybackAutomation>,
}

/// Immutable placement metadata for one send in the prepared mixer graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackMixerSend<'a> {
    id: &'a str,
    target_name: &'a str,
    tap: PlaybackMixerSendTap,
}

impl<'a> PlaybackMixerSend<'a> {
    #[must_use]
    pub const fn id(self) -> &'a str {
        self.id
    }

    #[must_use]
    pub const fn target_name(self) -> &'a str {
        self.target_name
    }

    #[must_use]
    pub const fn tap(self) -> PlaybackMixerSendTap {
        self.tap
    }
}

/// Fixed tap position retained by a compiled mixer send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackMixerSendTap {
    PreFader,
    PostFader,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackExtent {
    Bounded { end_frame: usize },
    Unbounded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioClipGainValidationError {
    ClipMissing,
    MediaMissing,
    SourceRangeOverflow,
    Native(ClipPlanError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArrangementPlacementGainValidationError {
    PlacementMissing,
    MediaMissing,
    SourceRangeOverflow,
    EffectiveGainOverflow,
    Native(ClipPlanError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioClipSourceSlipValidationError {
    ClipMissing,
    MediaMissing,
    SourceRangeOverflow,
    SourceOutsideMedia,
    NativeGain(ClipPlanError),
}

impl fmt::Display for AudioClipGainValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClipMissing => formatter.write_str("audio clip is missing from the project"),
            Self::MediaMissing => {
                formatter.write_str("audio clip references unavailable decoded media")
            }
            Self::SourceRangeOverflow => {
                formatter.write_str("audio-clip source range arithmetic overflowed")
            }
            Self::Native(source) => {
                write!(formatter, "native audio-clip gain is invalid: {source}")
            }
        }
    }
}

impl Error for AudioClipGainValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Native(source) => Some(source),
            Self::ClipMissing | Self::MediaMissing | Self::SourceRangeOverflow => None,
        }
    }
}

impl fmt::Display for ArrangementPlacementGainValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PlacementMissing => {
                formatter.write_str("arrangement placement is missing from the project")
            }
            Self::MediaMissing => {
                formatter.write_str("arrangement placement references unavailable decoded media")
            }
            Self::SourceRangeOverflow => {
                formatter.write_str("arrangement-placement source range arithmetic overflowed")
            }
            Self::EffectiveGainOverflow => {
                formatter.write_str("combined clip and arrangement-placement gain is not finite")
            }
            Self::Native(source) => {
                write!(
                    formatter,
                    "native arrangement-placement gain is invalid: {source}"
                )
            }
        }
    }
}

impl Error for ArrangementPlacementGainValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Native(source) => Some(source),
            Self::PlacementMissing
            | Self::MediaMissing
            | Self::SourceRangeOverflow
            | Self::EffectiveGainOverflow => None,
        }
    }
}

impl fmt::Display for AudioClipSourceSlipValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClipMissing => formatter.write_str("audio clip is missing from the project"),
            Self::MediaMissing => {
                formatter.write_str("audio clip references unavailable decoded media")
            }
            Self::SourceRangeOverflow => {
                formatter.write_str("audio-clip source range arithmetic overflowed")
            }
            Self::SourceOutsideMedia => {
                formatter.write_str("audio-clip source view exceeds decoded media")
            }
            Self::NativeGain(source) => {
                write!(
                    formatter,
                    "audio-clip source view is unsafe at persisted gain: {source}"
                )
            }
        }
    }
}

impl Error for AudioClipSourceSlipValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NativeGain(source) => Some(source),
            Self::ClipMissing
            | Self::MediaMissing
            | Self::SourceRangeOverflow
            | Self::SourceOutsideMedia => None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct PlaybackArrangementPlacement {
    id: String,
    track_index: usize,
    clip_index: usize,
    start_tick: u64,
    clip_start_tick: u64,
    duration_ticks: u64,
    gain_decibels: f64,
    start_frame: usize,
    end_frame: usize,
    enabled: bool,
    active: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub struct PlaybackArrangementSceneLaunch {
    id: String,
    scene_index: usize,
    tick: u64,
    start_frame: usize,
}

impl PlaybackArrangementSceneLaunch {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn scene_index(&self) -> usize {
        self.scene_index
    }

    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    #[must_use]
    pub const fn start_frame(&self) -> usize {
        self.start_frame
    }
}

impl PlaybackArrangementPlacement {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn track_index(&self) -> usize {
        self.track_index
    }

    #[must_use]
    pub const fn clip_index(&self) -> usize {
        self.clip_index
    }

    /// Exact persisted timeline start; unlike the compiled frame position,
    /// this does not require a lossy inverse tempo conversion.
    #[must_use]
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    /// Exact persisted clip-local start. Looped placements retain the
    /// unfolded offset so front-edge edits remain reversible.
    #[must_use]
    pub const fn clip_start_tick(&self) -> u64 {
        self.clip_start_tick
    }

    /// Exact persisted timeline duration; unlike the compiled frame range,
    /// this does not require a lossy inverse tempo conversion.
    #[must_use]
    pub const fn duration_ticks(&self) -> u64 {
        self.duration_ticks
    }

    /// Exact persisted dB offset local to this arrangement placement.
    #[must_use]
    pub const fn gain_decibels(&self) -> f64 {
        self.gain_decibels
    }

    #[must_use]
    pub const fn start_frame(&self) -> usize {
        self.start_frame
    }

    #[must_use]
    pub const fn end_frame(&self) -> usize {
        self.end_frame
    }

    /// Whether this placement is enabled in the persisted arrangement.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Whether the compiled placement contributes audio before mixer control.
    /// This also reflects arrangement, track, and clip activation.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct PlaybackAutomationLane {
    id: String,
    target: AutomationTarget,
    interpolation: AutomationInterpolation,
    point_count: usize,
}

impl PlaybackAutomationLane {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn target(&self) -> AutomationTarget {
        self.target
    }

    #[must_use]
    pub const fn interpolation(&self) -> AutomationInterpolation {
        self.interpolation
    }

    #[must_use]
    pub const fn point_count(&self) -> usize {
        self.point_count
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct PlaybackAutomationTake {
    id: String,
    name: String,
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
    lanes: Vec<PlaybackAutomationLane>,
}

impl PlaybackAutomationTake {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    #[must_use]
    pub const fn end_tick(&self) -> u64 {
        self.end_tick
    }

    #[must_use]
    pub fn lanes(&self) -> &[PlaybackAutomationLane] {
        &self.lanes
    }
}

/// Immutable view of every persisted automation take, including takes whose
/// `enabled` flag keeps them out of the compiled runtime.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct PlaybackAutomation {
    takes: Vec<PlaybackAutomationTake>,
}

impl PlaybackAutomation {
    #[must_use]
    pub fn takes(&self) -> &[PlaybackAutomationTake] {
        &self.takes
    }
}

fn retain_automation(
    loaded: &LoadedAutomationSet,
) -> Result<PlaybackAutomation, PlaybackSessionAssemblyError> {
    let mut takes = Vec::new();
    takes
        .try_reserve_exact(loaded.takes().len())
        .map_err(|_| PlaybackSessionAssemblyError::AutomationViewCapacityUnavailable)?;
    for take in loaded.takes() {
        let mut lanes = Vec::new();
        lanes
            .try_reserve_exact(take.lanes().len())
            .map_err(|_| PlaybackSessionAssemblyError::AutomationViewCapacityUnavailable)?;
        for lane in take.lanes() {
            lanes.push(PlaybackAutomationLane {
                id: lane.id().to_owned(),
                target: lane.target(),
                interpolation: lane.interpolation(),
                point_count: lane.points().len(),
            });
        }
        takes.push(PlaybackAutomationTake {
            id: take.id().to_owned(),
            name: take.name().to_owned(),
            enabled: take.enabled(),
            start_tick: take.start_tick(),
            end_tick: take.end_tick(),
            lanes,
        });
    }
    Ok(PlaybackAutomation { takes })
}

#[derive(Debug)]
pub struct PlaybackArrangement {
    track_names: Vec<String>,
    clip_names: Vec<String>,
    scene_names: Vec<String>,
    placements: Vec<PlaybackArrangementPlacement>,
    scene_launches: Vec<PlaybackArrangementSceneLaunch>,
    loop_range: Option<PlaybackArrangementLoopRange>,
    end_frame: usize,
}

/// The persisted transport loop, carrying both the authored ticks and the
/// frames the runtime needs, converted the same way scene launches are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackArrangementLoopRange {
    start_tick: u64,
    end_tick: u64,
    start_frame: usize,
    end_frame: usize,
    enabled: bool,
}

impl PlaybackArrangementLoopRange {
    #[must_use]
    pub const fn start_tick(self) -> u64 {
        self.start_tick
    }

    #[must_use]
    pub const fn end_tick(self) -> u64 {
        self.end_tick
    }

    #[must_use]
    pub const fn start_frame(self) -> usize {
        self.start_frame
    }

    #[must_use]
    pub const fn end_frame(self) -> usize {
        self.end_frame
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

impl PlaybackArrangement {
    /// Returns an empty, finite arrangement view.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            track_names: Vec::new(),
            clip_names: Vec::new(),
            scene_names: Vec::new(),
            placements: Vec::new(),
            scene_launches: Vec::new(),
            loop_range: None,
            end_frame: 0,
        }
    }

    #[must_use]
    pub fn track_name(&self, index: usize) -> Option<&str> {
        self.track_names.get(index).map(String::as_str)
    }

    #[must_use]
    pub fn clip_name(&self, index: usize) -> Option<&str> {
        self.clip_names.get(index).map(String::as_str)
    }

    #[must_use]
    pub fn placements(&self) -> &[PlaybackArrangementPlacement] {
        &self.placements
    }

    #[must_use]
    pub fn scene_name(&self, index: usize) -> Option<&str> {
        self.scene_names.get(index).map(String::as_str)
    }

    #[must_use]
    pub fn scene_launches(&self) -> &[PlaybackArrangementSceneLaunch] {
        &self.scene_launches
    }

    #[must_use]
    pub const fn loop_range(&self) -> Option<PlaybackArrangementLoopRange> {
        self.loop_range
    }

    #[must_use]
    pub const fn end_frame(&self) -> usize {
        self.end_frame
    }
}

fn playback_extent(
    arrangement_end_frame: usize,
    recorded_scene_extent: RecordedSceneExtent,
) -> PlaybackExtent {
    match recorded_scene_extent {
        RecordedSceneExtent::Empty => PlaybackExtent::Bounded {
            end_frame: arrangement_end_frame,
        },
        RecordedSceneExtent::Finite { end_frame } => PlaybackExtent::Bounded {
            end_frame: arrangement_end_frame.max(end_frame),
        },
        RecordedSceneExtent::Unbounded => PlaybackExtent::Unbounded,
    }
}

impl PreparedPlaybackSession {
    #[must_use]
    pub fn project_id(&self) -> &str {
        self.project.project_id()
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.project.sample_rate_hz()
    }

    #[must_use]
    pub fn media_asset_count(&self) -> usize {
        self.media.asset_count()
    }

    #[must_use]
    pub fn has_automation(&self) -> bool {
        self.automation.is_some()
    }

    #[must_use]
    pub const fn extent(&self) -> PlaybackExtent {
        self.extent
    }

    #[must_use]
    pub fn arrangement(&self) -> &PlaybackArrangement {
        &self.arrangement
    }

    #[must_use]
    pub fn shared_arrangement(&self) -> Arc<PlaybackArrangement> {
        Arc::clone(&self.arrangement)
    }

    #[must_use]
    pub fn automation_takes(&self) -> &PlaybackAutomation {
        &self.automation_view
    }

    /// Returns the fixed channel count in the compiled mixer layout.
    #[must_use]
    pub fn mixer_channel_count(&self) -> usize {
        self.mixer.slots().len()
    }

    /// Returns one compiled mixer channel's stable project track identity.
    #[must_use]
    pub fn mixer_channel_track_id_at(&self, channel_index: usize) -> Option<&str> {
        self.mixer
            .slots()
            .get(channel_index)
            .map(hyper_playback_json::CompiledGraphSlot::track_id)
    }

    /// Returns one compiled mixer channel's retained project track name.
    #[must_use]
    pub fn mixer_channel_name_at(&self, channel_index: usize) -> Option<&str> {
        let track_id = self.mixer.slots().get(channel_index)?.track_id();
        self.project.track_name(track_id)
    }

    /// Returns the fixed send count for one compiled mixer channel.
    #[must_use]
    pub fn mixer_send_count_at(&self, channel_index: usize) -> Option<usize> {
        Some(self.mixer.slots().get(channel_index)?.sends().len())
    }

    /// Returns immutable routing metadata for one compiled mixer send.
    #[must_use]
    pub fn mixer_send_at(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Option<PlaybackMixerSend<'_>> {
        let send = self
            .mixer
            .slots()
            .get(channel_index)?
            .sends()
            .get(send_index)?;
        let target_track_id = self.mixer.slots().get(send.target_slot())?.track_id();
        let target_name = self.project.track_name(target_track_id)?;
        Some(PlaybackMixerSend {
            id: send.id(),
            target_name,
            tap: match send.tap() {
                CompiledMixerTap::PreFader => PlaybackMixerSendTap::PreFader,
                CompiledMixerTap::PostFader => PlaybackMixerSendTap::PostFader,
            },
        })
    }

    #[must_use]
    pub fn shared_automation_takes(&self) -> Arc<PlaybackAutomation> {
        Arc::clone(&self.automation_view)
    }

    /// Validates a candidate constant gain against the exact immutable decoded
    /// source view retained by this prepared project.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the clip/media binding is missing,
    /// its source range overflows, or native `f32` gain would be unsupported or
    /// overflow any selected decoded sample.
    pub fn validate_audio_clip_gain(
        &self,
        clip_id: &str,
        gain_decibels: f64,
    ) -> Result<(), AudioClipGainValidationError> {
        let view = self
            .project
            .audio_clip_gain_validation_view(clip_id)
            .ok_or(AudioClipGainValidationError::ClipMissing)?;
        let source_end = view
            .source_start_frame()
            .checked_add(view.source_frame_count())
            .ok_or(AudioClipGainValidationError::SourceRangeOverflow)?;
        let source = self
            .media
            .decoded_audio(view.asset_id())
            .ok_or(AudioClipGainValidationError::MediaMissing)?;
        validate_native_clip_gain(
            source,
            FrameRange {
                start: view.source_start_frame(),
                end: source_end,
            },
            gain_decibels,
        )
        .map(|_| ())
        .map_err(AudioClipGainValidationError::Native)
    }

    /// Validates a candidate arrangement-placement gain against the exact
    /// immutable decoded source view retained by this prepared project.
    ///
    /// The candidate is combined with the reusable clip gain once, matching
    /// the native clip plan constructed for arrangement playback.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the placement/media binding is
    /// missing, its source range overflows, the combined gain is non-finite,
    /// or native `f32` gain would be unsupported or overflow decoded samples.
    pub fn validate_arrangement_placement_gain(
        &self,
        placement_id: &str,
        gain_decibels: f64,
    ) -> Result<(), ArrangementPlacementGainValidationError> {
        let view = self
            .project
            .arrangement_placement_gain_validation_view(placement_id)
            .ok_or(ArrangementPlacementGainValidationError::PlacementMissing)?;
        let effective_gain_decibels = view.clip_gain_decibels() + gain_decibels;
        if !effective_gain_decibels.is_finite() {
            return Err(ArrangementPlacementGainValidationError::EffectiveGainOverflow);
        }
        let source_end = view
            .source_start_frame()
            .checked_add(view.source_frame_count())
            .ok_or(ArrangementPlacementGainValidationError::SourceRangeOverflow)?;
        let source = self
            .media
            .decoded_audio(view.asset_id())
            .ok_or(ArrangementPlacementGainValidationError::MediaMissing)?;
        validate_native_clip_gain(
            source,
            FrameRange {
                start: view.source_start_frame(),
                end: source_end,
            },
            effective_gain_decibels,
        )
        .map(|_| ())
        .map_err(ArrangementPlacementGainValidationError::Native)
    }

    /// Validates a candidate source-slip start against the exact decoded media
    /// and the clip's persisted native gain.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the clip/media binding is missing,
    /// the retained frame count cannot fit at the requested start, or applying
    /// the persisted gain to the candidate samples would overflow.
    pub fn validate_audio_clip_source_start(
        &self,
        clip_id: &str,
        source_start_frame: usize,
    ) -> Result<(), AudioClipSourceSlipValidationError> {
        let view = self
            .project
            .audio_clip_gain_validation_view(clip_id)
            .ok_or(AudioClipSourceSlipValidationError::ClipMissing)?;
        let source_end = source_start_frame
            .checked_add(view.source_frame_count())
            .ok_or(AudioClipSourceSlipValidationError::SourceRangeOverflow)?;
        let source = self
            .media
            .decoded_audio(view.asset_id())
            .ok_or(AudioClipSourceSlipValidationError::MediaMissing)?;
        if source_end > source.frame_count() {
            return Err(AudioClipSourceSlipValidationError::SourceOutsideMedia);
        }
        validate_native_clip_gain(
            source,
            FrameRange {
                start: source_start_frame,
                end: source_end,
            },
            view.gain_decibels(),
        )
        .map(|_| ())
        .map_err(AudioClipSourceSlipValidationError::NativeGain)
    }

    /// Validates a candidate trimmed source view against the exact decoded
    /// media and the clip's persisted native gain.
    ///
    /// The serializer owns duration, warp, loop, and fade semantics. This
    /// boundary only verifies the candidate immutable-media range that the
    /// serializer cannot prove from manifest metadata alone.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the clip/media binding is missing,
    /// the requested range overflows or exceeds decoded media, or applying the
    /// persisted gain to the candidate samples would overflow.
    pub fn validate_audio_clip_source_view(
        &self,
        clip_id: &str,
        source_start_frame: usize,
        source_frame_count: usize,
    ) -> Result<(), AudioClipSourceSlipValidationError> {
        let view = self
            .project
            .audio_clip_gain_validation_view(clip_id)
            .ok_or(AudioClipSourceSlipValidationError::ClipMissing)?;
        let source_end = source_start_frame
            .checked_add(source_frame_count)
            .ok_or(AudioClipSourceSlipValidationError::SourceRangeOverflow)?;
        let source = self
            .media
            .decoded_audio(view.asset_id())
            .ok_or(AudioClipSourceSlipValidationError::MediaMissing)?;
        if source_end > source.frame_count() {
            return Err(AudioClipSourceSlipValidationError::SourceOutsideMedia);
        }
        validate_native_clip_gain(
            source,
            FrameRange {
                start: source_start_frame,
                end: source_end,
            },
            view.gain_decibels(),
        )
        .map(|_| ())
        .map_err(AudioClipSourceSlipValidationError::NativeGain)
    }

    #[must_use]
    pub fn parts(
        &self,
    ) -> (
        &LoadedPlaybackProject,
        &CompiledMixerGraph,
        Option<&CompiledAutomation>,
        &DecodedMediaCache,
    ) {
        (
            &self.project,
            &self.mixer,
            self.automation.as_ref(),
            &self.media,
        )
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        LoadedPlaybackProject,
        CompiledMixerGraph,
        Option<CompiledAutomation>,
        DecodedMediaCache,
    ) {
        (self.project, self.mixer, self.automation, self.media)
    }
}

#[derive(Debug)]
pub enum PlaybackSessionAssemblyError {
    DocumentCountLimit {
        kind: PlaybackDocumentKind,
        actual: usize,
        maximum: usize,
    },
    DocumentReferenceCapacityUnavailable,
    Project(LoadError),
    Mixer(LoadError),
    MixerCompile(MixerCompileError),
    Automation(LoadError),
    AutomationCompile(AutomationCompileError),
    MediaCache(CacheBuildError),
    MediaFactCapacityUnavailable,
    MediaAsset {
        asset_index: usize,
        source: CacheBuildError,
    },
    Playback(AdapterCompileError),
    MediaBinding(PreparePlaybackError),
    ArrangementTimeline(FrameTimelineError),
    ArrangementCapacityUnavailable,
    ArrangementMetadataMismatch,
    AutomationViewCapacityUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackDocumentKind {
    Track,
    Clip,
    Scene,
    AutomationTake,
}

impl fmt::Display for PlaybackDocumentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Track => "track",
            Self::Clip => "clip",
            Self::Scene => "scene",
            Self::AutomationTake => "automation-take",
        })
    }
}

impl fmt::Display for PlaybackSessionAssemblyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentCountLimit {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind} document count is {actual}; maximum is {maximum}"
            ),
            Self::DocumentReferenceCapacityUnavailable => {
                formatter.write_str("playback document reference capacity is unavailable")
            }
            Self::Project(source) => write!(formatter, "playback project loading failed: {source}"),
            Self::Mixer(source) => write!(formatter, "mixer loading failed: {source}"),
            Self::MixerCompile(source) => write!(formatter, "mixer compilation failed: {source}"),
            Self::Automation(source) => write!(formatter, "automation loading failed: {source}"),
            Self::AutomationCompile(source) => {
                write!(formatter, "automation compilation failed: {source}")
            }
            Self::MediaCache(source) => write!(formatter, "media cache creation failed: {source}"),
            Self::MediaFactCapacityUnavailable => {
                formatter.write_str("media fact capacity is unavailable")
            }
            Self::MediaAsset {
                asset_index,
                source,
            } => write!(
                formatter,
                "decoded media asset {asset_index} could not be cached: {source}"
            ),
            Self::Playback(source) => write!(formatter, "playback compilation failed: {source}"),
            Self::MediaBinding(source) => {
                write!(formatter, "playback media binding failed: {source}")
            }
            Self::ArrangementTimeline(source) => {
                write!(
                    formatter,
                    "arrangement timeline conversion failed: {source}"
                )
            }
            Self::ArrangementCapacityUnavailable => {
                formatter.write_str("playback arrangement capacity is unavailable")
            }
            Self::ArrangementMetadataMismatch => {
                formatter.write_str("loaded project arrangement metadata is inconsistent")
            }
            Self::AutomationViewCapacityUnavailable => {
                formatter.write_str("published automation take capacity is unavailable")
            }
        }
    }
}

impl Error for PlaybackSessionAssemblyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Project(source) | Self::Mixer(source) | Self::Automation(source) => Some(source),
            Self::MixerCompile(source) => Some(source),
            Self::AutomationCompile(source) => Some(source),
            Self::MediaCache(source) | Self::MediaAsset { source, .. } => Some(source),
            Self::Playback(source) => Some(source),
            Self::MediaBinding(source) => Some(source),
            Self::ArrangementTimeline(source) => Some(source),
            Self::DocumentCountLimit { .. }
            | Self::DocumentReferenceCapacityUnavailable
            | Self::MediaFactCapacityUnavailable
            | Self::ArrangementCapacityUnavailable
            | Self::ArrangementMetadataMismatch
            | Self::AutomationViewCapacityUnavailable => None,
        }
    }
}

fn try_arrangement_owned(value: &str) -> Result<String, PlaybackSessionAssemblyError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

fn build_playback_arrangement(
    project: &LoadedPlaybackProject,
    schedule: hyper_playback_json::OwnedCompiledSchedule,
    scene_launch_frames: &[usize],
    loop_range: Option<PlaybackArrangementLoopRange>,
) -> Result<PlaybackArrangement, PlaybackSessionAssemblyError> {
    let mut track_names = Vec::new();
    track_names
        .try_reserve_exact(project.track_count())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    for track_index in 0..project.track_count() {
        let name = project
            .track_name_at(track_index)
            .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?;
        track_names.push(try_arrangement_owned(name)?);
    }

    let mut clip_names = Vec::new();
    clip_names
        .try_reserve_exact(project.clip_count())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    for clip_index in 0..project.clip_count() {
        let name = project
            .clip_name_at(clip_index)
            .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?;
        clip_names.push(try_arrangement_owned(name)?);
    }

    let mut scene_names = Vec::new();
    scene_names
        .try_reserve_exact(project.scene_count())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    for scene_index in 0..project.scene_count() {
        let name = project
            .scene_name(scene_index)
            .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?;
        scene_names.push(try_arrangement_owned(name)?);
    }

    let mut scene_launches = Vec::new();
    scene_launches
        .try_reserve_exact(project.scene_launch_count())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    if scene_launch_frames.len() != project.scene_launch_count() {
        return Err(PlaybackSessionAssemblyError::ArrangementMetadataMismatch);
    }
    for (launch_index, start_frame) in scene_launch_frames.iter().copied().enumerate() {
        scene_launches.push(PlaybackArrangementSceneLaunch {
            id: try_arrangement_owned(
                project
                    .scene_launch_id_at(launch_index)
                    .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?,
            )?,
            scene_index: project
                .scene_launch_scene_index_at(launch_index)
                .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?,
            tick: project
                .scene_launch_tick_at(launch_index)
                .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?,
            start_frame,
        });
    }

    let entries = schedule.into_entries();
    let mut placements = Vec::new();
    placements
        .try_reserve_exact(entries.len())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    let mut end_frame = 0;
    for entry in entries {
        let start_frame = entry.compiled.arrangement_start_frame();
        let placement_end_frame = entry.compiled.arrangement_end_frame();
        end_frame = end_frame.max(placement_end_frame);
        placements.push(PlaybackArrangementPlacement {
            id: entry.placement_id,
            track_index: entry.track_index,
            clip_index: entry.clip_index,
            start_tick: entry.placement_start_tick,
            clip_start_tick: entry.placement_clip_start_tick,
            duration_ticks: entry.placement_duration_ticks,
            gain_decibels: entry.placement_gain_decibels,
            start_frame,
            end_frame: placement_end_frame,
            enabled: entry.placement_enabled,
            active: entry.compiled.clip_spec().enabled,
        });
    }
    Ok(PlaybackArrangement {
        track_names,
        clip_names,
        scene_names,
        placements,
        scene_launches,
        loop_range,
        end_frame,
    })
}

fn bind_and_retain_arrangement(
    project: &LoadedPlaybackProject,
    media: &DecodedMediaCache,
    graph: CompiledPlaybackGraph<'_>,
) -> Result<Arc<PlaybackArrangement>, PlaybackSessionAssemblyError> {
    let prepared = media
        .prepare_playback(&graph)
        .map_err(PlaybackSessionAssemblyError::MediaBinding)?;
    drop(prepared);
    let mut scene_launch_frames = Vec::new();
    scene_launch_frames
        .try_reserve_exact(project.scene_launch_count())
        .map_err(|_| PlaybackSessionAssemblyError::ArrangementCapacityUnavailable)?;
    for launch_index in 0..project.scene_launch_count() {
        let tick = project
            .scene_launch_tick_at(launch_index)
            .ok_or(PlaybackSessionAssemblyError::ArrangementMetadataMismatch)?;
        scene_launch_frames.push(
            graph
                .scene_authority()
                .tick_to_frame_ceil(tick)
                .map_err(PlaybackSessionAssemblyError::ArrangementTimeline)?,
        );
    }
    let loop_range = project
        .arrangement_loop_range()
        .map(|range| {
            let authority = graph.scene_authority();
            Ok::<_, PlaybackSessionAssemblyError>(PlaybackArrangementLoopRange {
                start_tick: range.start_tick(),
                end_tick: range.end_tick(),
                start_frame: authority
                    .tick_to_frame_ceil(range.start_tick())
                    .map_err(PlaybackSessionAssemblyError::ArrangementTimeline)?,
                end_frame: authority
                    .tick_to_frame_ceil(range.end_tick())
                    .map_err(PlaybackSessionAssemblyError::ArrangementTimeline)?,
                enabled: range.enabled(),
            })
        })
        .transpose()?;
    Ok(Arc::new(build_playback_arrangement(
        project,
        graph.into_arrangement(),
        &scene_launch_frames,
        loop_range,
    )?))
}

/// Converts caller-supplied canonical documents and decoded media into owned
/// playback state.
///
/// All resource references, mixer routing, automation targets, decoded-media
/// limits, and project identities are checked before any session is returned.
/// Package provenance and digests must be established by a separate boundary.
///
/// # Errors
///
/// Returns [`PlaybackSessionAssemblyError`] without publishing partial state
/// when any bounded loading, compilation, or cache step fails.
pub fn assemble_playback_session(
    input: PlaybackSessionAssemblyInput,
) -> Result<PreparedPlaybackSession, PlaybackSessionAssemblyError> {
    let PlaybackSessionAssemblyInput {
        documents,
        decoded_assets,
        limits,
    } = input;
    let track_documents = document_references(
        &documents.tracks,
        limits.documents.max_track_documents,
        PlaybackDocumentKind::Track,
    )?;
    let clip_documents = document_references(
        &documents.clips,
        limits.documents.max_clip_documents,
        PlaybackDocumentKind::Clip,
    )?;
    let scene_documents = document_references(
        &documents.scenes,
        limits.documents.max_scene_documents,
        PlaybackDocumentKind::Scene,
    )?;
    let automation_documents = document_references(
        &documents.automation_takes,
        limits.documents.max_automation_documents,
        PlaybackDocumentKind::AutomationTake,
    )?;
    let has_automation = !automation_documents.is_empty();

    let project = load(
        PlaybackJsonInput {
            project_id: &documents.project_id,
            sample_rate_hz: documents.sample_rate_hz,
            timeline: &documents.timeline,
            tracks: &track_documents,
            clips: &clip_documents,
            scenes: &scene_documents,
            arrangement: documents.arrangement.as_deref(),
        },
        limits.documents,
    )
    .map_err(PlaybackSessionAssemblyError::Project)?;

    let mut media = DecodedMediaCacheBuilder::try_new(limits.media_cache)
        .map_err(PlaybackSessionAssemblyError::MediaCache)?;
    for (asset_index, asset) in decoded_assets.into_iter().enumerate() {
        media
            .try_insert(&asset.asset_id, asset.audio)
            .map_err(|failure| PlaybackSessionAssemblyError::MediaAsset {
                asset_index,
                source: failure.error(),
            })?;
    }

    let media = media.seal();
    let mut facts: Vec<MediaAssetFact<'_>> = Vec::new();
    facts
        .try_reserve_exact(media.asset_count())
        .map_err(|_| PlaybackSessionAssemblyError::MediaFactCapacityUnavailable)?;
    facts.extend(media.media_asset_facts());
    let loaded_mixer = project
        .load_mixer(&documents.mixer, &facts, limits.documents)
        .map_err(PlaybackSessionAssemblyError::Mixer)?;
    let mixer = loaded_mixer
        .compile_main_graph()
        .map_err(PlaybackSessionAssemblyError::MixerCompile)?;
    let (automation, automation_view) = if has_automation {
        let loaded = project
            .load_automation_takes(
                &loaded_mixer,
                &facts,
                &automation_documents,
                limits.documents,
            )
            .map_err(PlaybackSessionAssemblyError::Automation)?;
        let compiled = project
            .compile_main_automation(&mixer, &loaded)
            .map_err(PlaybackSessionAssemblyError::AutomationCompile)?;
        (Some(compiled), retain_automation(&loaded)?)
    } else {
        (None, PlaybackAutomation::default())
    };

    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(PlaybackSessionAssemblyError::Playback)?;
    let extent = playback_extent(
        graph.arrangement().end_frame(),
        graph.scene_authority().recorded_extent(),
    );
    let arrangement = bind_and_retain_arrangement(&project, &media, graph)?;

    Ok(PreparedPlaybackSession {
        project,
        mixer,
        automation,
        media,
        extent,
        arrangement,
        automation_view: Arc::new(automation_view),
    })
}

fn document_references(
    documents: &[Vec<u8>],
    maximum: usize,
    kind: PlaybackDocumentKind,
) -> Result<Vec<&[u8]>, PlaybackSessionAssemblyError> {
    if documents.len() > maximum {
        return Err(PlaybackSessionAssemblyError::DocumentCountLimit {
            kind,
            actual: documents.len(),
            maximum,
        });
    }
    let mut references = Vec::new();
    references
        .try_reserve_exact(documents.len())
        .map_err(|_| PlaybackSessionAssemblyError::DocumentReferenceCapacityUnavailable)?;
    references.extend(documents.iter().map(Vec::as_slice));
    Ok(references)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337501",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    const TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337502",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122337503"],"extensions":{}
    }"#;
    const CLIP: &[u8] = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337503",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337504","start_frame":0,"frame_count":100},
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
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    const MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
        "channels":[{
            "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":1000000,
            "pan_millionths":0,"mute":false,"solo":false,
            "destination":{"kind":"main"},"sends":[]
        }],
        "return_buses":[],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    const RETURN_TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337510",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Return A","kind":"return","order":1,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    const SEND_MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
        "channels":[
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":1000000,
                "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},
                "sends":[{
                    "id":"018f47a2-6b2d-7c11-8a44-001122337511","bus":"a","tap":"pre-fader",
                    "gain_millionths":750000,"pan_millionths":-250000,"enabled":true
                }]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122337510","gain_millionths":1000000,
                "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]
            }
        ],
        "return_buses":[{"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-001122337510"}],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    const AUTOMATION: &[u8] = br#"{
        "format":"hyper-automation-take","format_version":1,
        "id":"018f47a2-6b2d-7c11-8a44-001122337508","project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Pass","enabled":true,
        "captured_at_utc":"2026-07-28T12:05:00.000Z",
        "start_tick":0,"end_tick":8,
        "lanes":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122337509",
            "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","parameter":"gain"},
            "interpolation":"linear",
            "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
        }],
        "extensions":{}
    }"#;

    fn input(assets: Vec<DecodedPlaybackAsset>) -> PlaybackSessionAssemblyInput {
        PlaybackSessionAssemblyInput {
            documents: PlaybackDocumentBytes {
                project_id: "018f47a2-6b2d-7c11-8a44-001122337500".to_owned(),
                sample_rate_hz: 48_000,
                timeline: TIMELINE.to_vec(),
                tracks: vec![TRACK.to_vec()],
                clips: vec![CLIP.to_vec()],
                scenes: Vec::new(),
                arrangement: Some(ARRANGEMENT.to_vec()),
                mixer: MIXER.to_vec(),
                automation_takes: vec![AUTOMATION.to_vec()],
            },
            decoded_assets: assets,
            limits: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: 2,
                    max_frames_per_asset: 100,
                    max_decoded_sample_bytes: 800,
                },
            },
        }
    }

    fn asset(id: &str) -> DecodedPlaybackAsset {
        DecodedPlaybackAsset::new(
            id.to_owned(),
            DecodedAudio::from_mono(48_000, vec![0.25; 100]).unwrap(),
        )
    }

    #[test]
    fn canonical_documents_and_media_assemble_as_one_owned_session() {
        let session =
            assemble_playback_session(input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]))
                .unwrap();

        assert_eq!(session.project_id(), "018f47a2-6b2d-7c11-8a44-001122337500");
        assert_eq!(session.sample_rate_hz(), 48_000);
        assert_eq!(session.media_asset_count(), 1);
        assert!(session.has_automation());
        assert_eq!(session.mixer_channel_count(), 1);
        assert_eq!(session.mixer_channel_name_at(0), Some("Audio"));
        assert_eq!(session.mixer_channel_name_at(1), None);
        assert_eq!(session.mixer_send_count_at(0), Some(0));
        assert_eq!(session.mixer_send_count_at(1), None);
        let takes = session.automation_takes().takes();
        assert_eq!(takes.len(), 1);
        assert_eq!(takes[0].id(), "018f47a2-6b2d-7c11-8a44-001122337508");
        assert_eq!(takes[0].name(), "Pass");
        assert!(takes[0].enabled());
        assert_eq!((takes[0].start_tick(), takes[0].end_tick()), (0, 8));
        assert_eq!(
            takes[0].lanes()[0],
            PlaybackAutomationLane {
                id: "018f47a2-6b2d-7c11-8a44-001122337509".to_owned(),
                target: AutomationTarget::Channel {
                    slot: 0,
                    parameter: hyper_playback_json::AutomationChannelParameter::Gain,
                },
                interpolation: AutomationInterpolation::Linear,
                point_count: 2,
            }
        );
        assert_eq!(session.extent(), PlaybackExtent::Bounded { end_frame: 100 });
        let arrangement = session.arrangement();
        assert_eq!(arrangement.track_name(0), Some("Audio"));
        assert_eq!(arrangement.clip_name(0), Some("Clip"));
        assert_eq!(arrangement.end_frame(), 100);
        assert_eq!(arrangement.placements().len(), 1);
        assert_eq!(
            arrangement.placements()[0],
            PlaybackArrangementPlacement {
                id: "018f47a2-6b2d-7c11-8a44-001122337506".to_owned(),
                track_index: 0,
                clip_index: 0,
                start_tick: 0,
                clip_start_tick: 0,
                duration_ticks: 4,
                gain_decibels: 0.0,
                start_frame: 0,
                end_frame: 100,
                enabled: true,
                active: true,
            }
        );
        let first_owner = session.shared_arrangement();
        let second_owner = session.shared_arrangement();
        assert!(Arc::ptr_eq(&first_owner, &second_owner));
        let (project, mixer, automation, media) = session.parts();
        assert_eq!(project.project_id(), mixer.project_id());
        assert_eq!(
            automation.map(CompiledAutomation::project_id),
            Some(project.project_id())
        );
        assert!(media.contains("018f47a2-6b2d-7c11-8a44-001122337504"));
        let (project, mixer, automation, media) = session.into_parts();
        assert_eq!(project.project_id(), mixer.project_id());
        assert_eq!(
            automation.as_ref().map(CompiledAutomation::project_id),
            Some(project.project_id())
        );
        assert!(media.contains("018f47a2-6b2d-7c11-8a44-001122337504"));
    }

    #[test]
    fn prepared_send_metadata_uses_compiled_order_and_target_authority() {
        let mut input = input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]);
        input.documents.tracks.push(RETURN_TRACK.to_vec());
        input.documents.mixer = SEND_MIXER.to_vec();
        let session = assemble_playback_session(input).unwrap();

        assert_eq!(session.mixer_channel_count(), 2);
        assert_eq!(session.mixer_send_count_at(0), Some(1));
        assert_eq!(session.mixer_send_count_at(1), Some(0));
        assert_eq!(session.mixer_send_count_at(2), None);
        let send = session.mixer_send_at(0, 0).unwrap();
        assert_eq!(send.id(), "018f47a2-6b2d-7c11-8a44-001122337511");
        assert_eq!(send.target_name(), "Return A");
        assert_eq!(send.tap(), PlaybackMixerSendTap::PreFader);
        assert_eq!(session.mixer_send_at(0, 1), None);
        assert_eq!(session.mixer_send_at(2, 0), None);
    }

    #[test]
    fn candidate_clip_gain_is_validated_against_exact_decoded_samples() {
        let loud = DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, vec![f32::MAX; 100]).unwrap(),
        );
        let session = assemble_playback_session(input(vec![loud])).unwrap();

        assert!(matches!(
            session.validate_audio_clip_gain(
                "018f47a2-6b2d-7c11-8a44-001122337503",
                20.0 * 2.0_f64.log10(),
            ),
            Err(AudioClipGainValidationError::Native(
                ClipPlanError::SampleGainOverflow
            ))
        ));
        assert!(
            session
                .validate_audio_clip_gain("018f47a2-6b2d-7c11-8a44-001122337503", -6.0,)
                .is_ok()
        );
    }

    #[test]
    fn candidate_arrangement_placement_gain_is_validated_against_exact_samples() {
        let loud = DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, vec![f32::MAX; 100]).unwrap(),
        );
        let session = assemble_playback_session(input(vec![loud])).unwrap();
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122337506";

        assert!(matches!(
            session.validate_arrangement_placement_gain(placement_id, 20.0 * 2.0_f64.log10(),),
            Err(ArrangementPlacementGainValidationError::Native(
                ClipPlanError::SampleGainOverflow
            ))
        ));
        assert!(
            session
                .validate_arrangement_placement_gain(placement_id, -6.0)
                .is_ok()
        );
        assert_eq!(
            session
                .validate_arrangement_placement_gain("018f47a2-6b2d-7c11-8a44-001122337599", 0.0,),
            Err(ArrangementPlacementGainValidationError::PlacementMissing)
        );
    }

    #[test]
    fn candidate_source_slip_is_validated_against_media_and_persisted_gain() {
        let mut samples = vec![0.0; 100];
        samples.extend([f32::MAX; 100]);
        let loud_tail = DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, samples).unwrap(),
        );
        let mut input = input(vec![loud_tail]);
        input.documents.clips[0] = String::from_utf8(input.documents.clips[0].clone())
            .unwrap()
            .replace(
                r#""gain_decibels":0"#,
                r#""gain_decibels":6.020599913279624"#,
            )
            .into_bytes();
        input.limits.media_cache.max_frames_per_asset = 200;
        input.limits.media_cache.max_decoded_sample_bytes = 1_600;
        let session = assemble_playback_session(input).unwrap();

        assert!(
            session
                .validate_audio_clip_source_start("018f47a2-6b2d-7c11-8a44-001122337503", 0,)
                .is_ok()
        );
        assert!(matches!(
            session.validate_audio_clip_source_start("018f47a2-6b2d-7c11-8a44-001122337503", 100,),
            Err(AudioClipSourceSlipValidationError::NativeGain(
                ClipPlanError::SampleGainOverflow
            ))
        ));
        assert_eq!(
            session.validate_audio_clip_source_start("018f47a2-6b2d-7c11-8a44-001122337503", 101,),
            Err(AudioClipSourceSlipValidationError::SourceOutsideMedia)
        );
        assert!(matches!(
            session.validate_audio_clip_source_view(
                "018f47a2-6b2d-7c11-8a44-001122337503",
                100,
                50,
            ),
            Err(AudioClipSourceSlipValidationError::NativeGain(
                ClipPlanError::SampleGainOverflow
            ))
        ));
        assert_eq!(
            session.validate_audio_clip_source_view(
                "018f47a2-6b2d-7c11-8a44-001122337503",
                151,
                50,
            ),
            Err(AudioClipSourceSlipValidationError::SourceOutsideMedia)
        );
    }

    #[test]
    fn playback_extent_combines_arrangement_and_recorded_scene_authority() {
        assert_eq!(
            playback_extent(100, RecordedSceneExtent::Empty),
            PlaybackExtent::Bounded { end_frame: 100 }
        );
        assert_eq!(
            playback_extent(100, RecordedSceneExtent::Finite { end_frame: 75 }),
            PlaybackExtent::Bounded { end_frame: 100 }
        );
        assert_eq!(
            playback_extent(100, RecordedSceneExtent::Finite { end_frame: 150 }),
            PlaybackExtent::Bounded { end_frame: 150 }
        );
        assert_eq!(
            playback_extent(100, RecordedSceneExtent::Unbounded),
            PlaybackExtent::Unbounded
        );
    }

    #[test]
    fn arrangement_navigation_separates_persisted_and_effective_activation() {
        for (clip_enabled, placement_enabled) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let mut input = input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]);
            if !clip_enabled {
                input.documents.clips[0] = String::from_utf8(input.documents.clips[0].clone())
                    .unwrap()
                    .replace(r#""enabled":true"#, r#""enabled":false"#)
                    .into_bytes();
            }
            if !placement_enabled {
                input.documents.arrangement = input.documents.arrangement.map(|arrangement| {
                    String::from_utf8(arrangement)
                        .unwrap()
                        .replace(
                            r#""duration_ticks":4,"enabled":true"#,
                            r#""duration_ticks":4,"enabled":false"#,
                        )
                        .into_bytes()
                });
            }

            let session = assemble_playback_session(input).unwrap();
            let placements = session.arrangement().placements();
            assert_eq!(placements.len(), 1);
            assert_eq!(placements[0].enabled(), placement_enabled);
            assert_eq!(placements[0].active(), clip_enabled && placement_enabled);
            assert_eq!(session.arrangement().end_frame(), 100);
        }
    }

    #[test]
    fn duplicate_decoded_identity_rejects_the_entire_session() {
        let result = assemble_playback_session(input(vec![
            asset("018f47a2-6b2d-7c11-8a44-001122337504"),
            asset("018f47a2-6b2d-7c11-8a44-001122337504"),
        ]));

        assert!(matches!(
            result,
            Err(PlaybackSessionAssemblyError::MediaAsset {
                asset_index: 1,
                source: CacheBuildError::DuplicateAssetId,
            })
        ));
    }

    #[test]
    fn foreign_mixer_is_rejected_before_media_publication() {
        let mut input = input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]);
        input.documents.mixer = String::from_utf8(MIXER.to_vec())
            .unwrap()
            .replace(
                r#""project_id":"018f47a2-6b2d-7c11-8a44-001122337500""#,
                r#""project_id":"018f47a2-6b2d-7c11-8a44-001122337579""#,
            )
            .into_bytes();

        assert!(matches!(
            assemble_playback_session(input),
            Err(PlaybackSessionAssemblyError::Mixer(_))
        ));
    }

    #[test]
    fn document_count_limit_precedes_reference_allocation() {
        let mut input = input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]);
        input.limits.documents.max_track_documents = 0;

        assert!(matches!(
            assemble_playback_session(input),
            Err(PlaybackSessionAssemblyError::DocumentCountLimit {
                kind: PlaybackDocumentKind::Track,
                actual: 1,
                maximum: 0,
            })
        ));
    }

    #[test]
    fn missing_decoded_binding_rejects_the_entire_session() {
        assert!(matches!(
            assemble_playback_session(input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337579")])),
            Err(PlaybackSessionAssemblyError::Playback(_))
        ));
    }

    #[test]
    fn automation_is_absent_only_when_no_take_was_supplied() {
        let mut input = input(vec![asset("018f47a2-6b2d-7c11-8a44-001122337504")]);
        input.documents.automation_takes.clear();

        let session = assemble_playback_session(input).unwrap();

        assert!(!session.has_automation());
    }
}
