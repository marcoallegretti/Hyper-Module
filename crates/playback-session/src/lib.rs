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
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::{RecordedSceneExtent, SceneLimits};
use hyper_playback_json::{
    AdapterCompileError, AutomationCompileError, CompiledAutomation, CompiledMixerGraph,
    CompiledPlaybackGraph, LoadError, LoadedPlaybackProject, LoaderLimits, MediaAssetFact,
    MixerCompileError, PlaybackJsonInput, load,
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

#[derive(Debug, Eq, PartialEq)]
pub struct PlaybackArrangementPlacement {
    id: String,
    track_index: usize,
    clip_index: usize,
    start_tick: u64,
    start_frame: usize,
    end_frame: usize,
    enabled: bool,
    active: bool,
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

#[derive(Debug)]
pub struct PlaybackArrangement {
    track_names: Vec<String>,
    clip_names: Vec<String>,
    placements: Vec<PlaybackArrangementPlacement>,
    end_frame: usize,
}

impl PlaybackArrangement {
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
    ArrangementCapacityUnavailable,
    ArrangementMetadataMismatch,
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
            Self::ArrangementCapacityUnavailable => {
                formatter.write_str("playback arrangement capacity is unavailable")
            }
            Self::ArrangementMetadataMismatch => {
                formatter.write_str("loaded project arrangement metadata is inconsistent")
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
            Self::DocumentCountLimit { .. }
            | Self::DocumentReferenceCapacityUnavailable
            | Self::MediaFactCapacityUnavailable
            | Self::ArrangementCapacityUnavailable
            | Self::ArrangementMetadataMismatch => None,
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
            start_frame,
            end_frame: placement_end_frame,
            enabled: entry.placement_enabled,
            active: entry.compiled.clip_spec().enabled,
        });
    }
    Ok(PlaybackArrangement {
        track_names,
        clip_names,
        placements,
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
    Ok(Arc::new(build_playback_arrangement(
        project,
        graph.into_arrangement(),
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
    let automation = if has_automation {
        let loaded = project
            .load_automation_takes(
                &loaded_mixer,
                &facts,
                &automation_documents,
                limits.documents,
            )
            .map_err(PlaybackSessionAssemblyError::Automation)?;
        Some(
            project
                .compile_main_automation(&mixer, &loaded)
                .map_err(PlaybackSessionAssemblyError::AutomationCompile)?,
        )
    } else {
        None
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
