use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::StereoOutput;
use hyper_audio_source::DecodedAudio;
use hyper_desktop_audio::{OfflineExportError, OfflineExportSpec, render_project_offline};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{RoutedMainRuntime, RoutedTransportRuntime, TransportCommand};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::{
    DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyInput,
    PlaybackSessionAssemblyLimits, PreparedPlaybackSession, assemble_playback_session,
};

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337500";
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
        "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":500000,
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
    "captured_at_utc":"2026-08-16T12:05:00.000Z",
    "start_tick":0,"end_tick":8,
    "lanes":[{
        "id":"018f47a2-6b2d-7c11-8a44-001122337509",
        "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","parameter":"gain"},
        "interpolation":"linear",
        "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
    }],
    "extensions":{}
}"#;

fn session(arrangement: Option<&[u8]>, automation: Vec<Vec<u8>>) -> PreparedPlaybackSession {
    assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: PlaybackDocumentBytes {
            project_id: PROJECT_ID.to_owned(),
            sample_rate_hz: 48_000,
            timeline: TIMELINE.to_vec(),
            tracks: vec![TRACK.to_vec()],
            clips: vec![CLIP.to_vec()],
            scenes: Vec::new(),
            arrangement: arrangement.map(<[u8]>::to_vec),
            mixer: MIXER.to_vec(),
            automation_takes: automation,
        },
        decoded_assets: vec![DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, (0..100_i16).map(|n| f32::from(n) / 200.0).collect()).unwrap(),
        )],
        limits: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 2,
                max_frames_per_asset: 100,
                max_decoded_sample_bytes: 800,
            },
        },
    })
    .unwrap()
}

/// Drives the same runtime the desktop output uses, one block at a time, so the
/// comparison is against playback rather than against a second renderer.
fn render_through_playback(session: &PreparedPlaybackSession, block_frames: usize) -> Vec<(f32, f32)> {
    let (project, mixer, automation, media) = session.parts();
    let facts: Vec<_> = media.media_asset_facts().collect();
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .unwrap();
    let prepared = media.prepare_playback(&graph).unwrap();
    let runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated(
            &prepared,
            mixer,
            automation.clone(),
            block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, block_frames),
    }
    .unwrap();
    let slot_count = mixer.slots().len();
    let mut left_planes = vec![vec![0.0_f32; block_frames]; slot_count];
    let mut right_planes = vec![vec![0.0_f32; block_frames]; slot_count];
    let mut scratch: Vec<RenderScratchSlot<'_>> = left_planes
        .iter_mut()
        .zip(&mut right_planes)
        .map(|(left, right)| RenderScratchSlot::new(left, right))
        .collect();
    let (mut sender, mut transport) = RoutedTransportRuntime::with_command_queue(runtime);
    sender.try_send(TransportCommand::Start).unwrap();
    let mut block_left = vec![0.0_f32; block_frames];
    let mut block_right = vec![0.0_f32; block_frames];
    let mut frames = Vec::new();
    for _ in 0..8 {
        transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut block_left,
                    right: &mut block_right,
                },
            )
            .unwrap();
        frames.extend(block_left.iter().copied().zip(block_right.iter().copied()));
    }
    frames
}

#[test]
fn offline_export_matches_the_playback_runtime_sample_for_sample() {
    let session = session(Some(ARRANGEMENT), vec![AUTOMATION.to_vec()]);
    let rendered = render_project_offline(&session, OfflineExportSpec { block_frames: 32 }).unwrap();

    assert_eq!(rendered.sample_rate_hz(), 48_000);
    assert_eq!(rendered.frame_count(), 100);

    let expected = render_through_playback(&session, 32);
    let left = rendered.plane(0).unwrap();
    let right = rendered.plane(1).unwrap();
    for (index, (expected_left, expected_right)) in expected.iter().take(100).enumerate() {
        assert!(
            (left[index] - expected_left).abs() < f32::EPSILON,
            "left frame {index}: export {} playback {expected_left}",
            left[index]
        );
        assert!(
            (right[index] - expected_right).abs() < f32::EPSILON,
            "right frame {index}: export {} playback {expected_right}",
            right[index]
        );
    }
    assert!(
        left.iter().any(|sample| sample.abs() > 0.0),
        "the export is not silent"
    );
}

#[test]
fn offline_export_is_independent_of_the_block_size() {
    let session = session(Some(ARRANGEMENT), vec![AUTOMATION.to_vec()]);
    let coarse = render_project_offline(&session, OfflineExportSpec { block_frames: 64 }).unwrap();
    let fine = render_project_offline(&session, OfflineExportSpec { block_frames: 16 }).unwrap();
    assert_eq!(coarse.frame_count(), fine.frame_count());
    for plane in 0..2 {
        assert_eq!(
            coarse.plane(plane).unwrap(),
            fine.plane(plane).unwrap(),
            "plane {plane} differs between block sizes"
        );
    }
}

#[test]
fn offline_export_refuses_a_project_that_renders_nothing() {
    let session = session(None, Vec::new());
    assert!(
        matches!(
            session.extent(),
            hyper_playback_session::PlaybackExtent::Bounded { end_frame: 0 }
        ),
        "a project with no arrangement is bounded at zero rather than unbounded"
    );
    assert!(matches!(
        render_project_offline(&session, OfflineExportSpec { block_frames: 32 }),
        Err(OfflineExportError::EmptyExtent)
    ));
}

#[test]
fn offline_export_rejects_an_unusable_block_size() {
    let session = session(Some(ARRANGEMENT), Vec::new());
    for block_frames in [0, hyper_audio_engine::MAX_PROCESS_FRAMES + 1] {
        assert!(matches!(
            render_project_offline(&session, OfflineExportSpec { block_frames }),
            Err(OfflineExportError::InvalidBlockFrames)
        ));
    }
}
