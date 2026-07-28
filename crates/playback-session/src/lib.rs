#![doc = "Post-verification playback session assembly for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{
    CacheBuildError, CacheLimits, DecodedMediaCache, DecodedMediaCacheBuilder, PreparePlaybackError,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{
    AdapterCompileError, AutomationCompileError, CompiledAutomation, CompiledMixerGraph, LoadError,
    LoadedPlaybackProject, LoaderLimits, MediaAssetFact, MixerCompileError, PlaybackJsonInput,
    load,
};

/// Canonical resource bytes returned by the package-verification boundary.
///
/// This type performs no filesystem access and does not replace package
/// canonicalization, digest, manifest, or media verification.
#[derive(Debug)]
pub struct VerifiedPlaybackDocuments {
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

/// One package-verified asset decoded outside the real-time path.
#[derive(Debug)]
pub struct VerifiedDecodedAsset {
    asset_id: String,
    audio: DecodedAudio,
}

impl VerifiedDecodedAsset {
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
pub struct VerifiedPlaybackSessionInput {
    pub documents: VerifiedPlaybackDocuments,
    pub decoded_assets: Vec<VerifiedDecodedAsset>,
    pub limits: PlaybackSessionAssemblyLimits,
}

/// Fully owned state required to prepare a live playback runtime.
#[derive(Debug)]
pub struct PreparedPlaybackSession {
    project: LoadedPlaybackProject,
    mixer: CompiledMixerGraph,
    automation: Option<CompiledAutomation>,
    media: DecodedMediaCache,
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
            | Self::MediaFactCapacityUnavailable => None,
        }
    }
}

/// Converts one already package-verified snapshot into owned playback state.
///
/// All resource references, mixer routing, automation targets, decoded-media
/// limits, and project identities are checked before any session is returned.
///
/// # Errors
///
/// Returns [`PlaybackSessionAssemblyError`] without publishing partial state
/// when any bounded loading, compilation, or cache step fails.
pub fn assemble_verified_playback_session(
    input: VerifiedPlaybackSessionInput,
) -> Result<PreparedPlaybackSession, PlaybackSessionAssemblyError> {
    let VerifiedPlaybackSessionInput {
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
    let loaded_mixer = project
        .load_mixer(&documents.mixer, limits.documents)
        .map_err(PlaybackSessionAssemblyError::Mixer)?;
    let mixer = loaded_mixer
        .compile_main_graph()
        .map_err(PlaybackSessionAssemblyError::MixerCompile)?;
    let automation = if has_automation {
        let loaded = project
            .load_automation_takes(&loaded_mixer, &automation_documents, limits.documents)
            .map_err(PlaybackSessionAssemblyError::Automation)?;
        Some(
            project
                .compile_main_automation(&mixer, &loaded)
                .map_err(PlaybackSessionAssemblyError::AutomationCompile)?,
        )
    } else {
        None
    };

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
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(PlaybackSessionAssemblyError::Playback)?;
    let _prepared = media
        .prepare_playback(&graph)
        .map_err(PlaybackSessionAssemblyError::MediaBinding)?;

    Ok(PreparedPlaybackSession {
        project,
        mixer,
        automation,
        media,
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
    const MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"mixer",
        "project_id":"project",
        "channels":[{
            "track_id":"track","gain_millionths":1000000,
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

    fn input(assets: Vec<VerifiedDecodedAsset>) -> VerifiedPlaybackSessionInput {
        VerifiedPlaybackSessionInput {
            documents: VerifiedPlaybackDocuments {
                project_id: "project".to_owned(),
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

    fn asset(id: &str) -> VerifiedDecodedAsset {
        VerifiedDecodedAsset::new(
            id.to_owned(),
            DecodedAudio::from_mono(48_000, vec![0.25; 100]).unwrap(),
        )
    }

    #[test]
    fn verified_documents_and_media_assemble_as_one_owned_session() {
        let session = assemble_verified_playback_session(input(vec![asset("asset")])).unwrap();

        assert_eq!(session.project_id(), "project");
        assert_eq!(session.sample_rate_hz(), 48_000);
        assert_eq!(session.media_asset_count(), 1);
        assert!(session.has_automation());
        let (project, mixer, automation, media) = session.into_parts();
        assert_eq!(project.project_id(), mixer.project_id());
        assert_eq!(
            automation.as_ref().map(CompiledAutomation::project_id),
            Some(project.project_id())
        );
        assert!(media.contains("asset"));
    }

    #[test]
    fn duplicate_decoded_identity_rejects_the_entire_session() {
        let result =
            assemble_verified_playback_session(input(vec![asset("asset"), asset("asset")]));

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
        let mut input = input(vec![asset("asset")]);
        input.documents.mixer = String::from_utf8(MIXER.to_vec())
            .unwrap()
            .replace(r#""project_id":"project""#, r#""project_id":"foreign""#)
            .into_bytes();

        assert!(matches!(
            assemble_verified_playback_session(input),
            Err(PlaybackSessionAssemblyError::Mixer(_))
        ));
    }

    #[test]
    fn document_count_limit_precedes_reference_allocation() {
        let mut input = input(vec![asset("asset")]);
        input.limits.documents.max_track_documents = 0;

        assert!(matches!(
            assemble_verified_playback_session(input),
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
            assemble_verified_playback_session(input(vec![asset("foreign")])),
            Err(PlaybackSessionAssemblyError::Playback(_))
        ));
    }

    #[test]
    fn automation_is_absent_only_when_no_take_was_verified() {
        let mut input = input(vec![asset("asset")]);
        input.documents.automation_takes.clear();

        let session = assemble_verified_playback_session(input).unwrap();

        assert!(!session.has_automation());
    }
}
