use hyper_arrangement_renderer::{
    PlaybackRenderer, PlaybackRendererCreationError, RenderScratchSlot,
};
use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{CacheLimits, DecodedMediaCacheBuilder};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{LoaderLimits, MediaAssetFact, PlaybackJsonInput, load};

const EPSILON: f32 = 1.0e-6;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end scenario keeps strict loading, publication, binding, and rendering evidence together"
)]
fn interactive_only_stereo_scene_uses_fixed_playback_topology_and_stops() {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335021",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335022",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335023","018f47a2-6b2d-7c11-8a44-001122335030"],"extensions":{}
    }"#;
    let arrangement_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335030",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","track_id":"018f47a2-6b2d-7c11-8a44-001122335022","name":"Arrangement","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122335031","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335023",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","track_id":"018f47a2-6b2d-7c11-8a44-001122335022","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122335024","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let play_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335025",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","name":"Play","order":0,"enabled":true,
        "launch_quantization":{"mode":"grid","quantum_ticks":1},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122335022","action":"play","clip_id":"018f47a2-6b2d-7c11-8a44-001122335023"}],
        "extensions":{}
    }"#;
    let stop_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335026",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","name":"Stop","order":1,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122335022","action":"stop"}],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335027",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335020","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122335032","track_id":"018f47a2-6b2d-7c11-8a44-001122335022","clip_id":"018f47a2-6b2d-7c11-8a44-001122335030",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 2] = [clip, arrangement_clip];
    let scenes: [&[u8]; 2] = [play_scene, stop_scene];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122335020",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let facts = [
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335024",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335031",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let graph = loaded
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .unwrap();
    assert_eq!(graph.arrangement().entries().len(), 1);
    let source_left: Vec<f32> = (0_u8..100)
        .map(|sample| f32::from(sample) / 100.0)
        .collect();
    let source_right: Vec<f32> = source_left.iter().map(|sample| -*sample).collect();
    let mut mismatch_builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 2,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 1_200,
    })
    .unwrap();
    mismatch_builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335024",
            DecodedAudio::from_stereo(48_000, source_left.clone(), source_right.clone()).unwrap(),
        )
        .unwrap();
    mismatch_builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335031",
            DecodedAudio::from_mono(48_000, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let mismatch_cache = mismatch_builder.seal();
    let mismatch_arrangement = mismatch_cache
        .prepare_schedule(graph.arrangement())
        .unwrap();
    let mismatch_scenes = mismatch_cache.prepare_playback_scenes(&graph).unwrap();
    assert!(matches!(
        PlaybackRenderer::try_new_for_tracks(
            &mismatch_arrangement,
            graph.scene_authority(),
            &mismatch_scenes,
            0,
            &["018f47a2-6b2d-7c11-8a44-001122335022"],
        ),
        Err(PlaybackRendererCreationError::InconsistentTrackChannels {
            track_index: 0,
            arrangement: 1,
            scene: 2,
        })
    ));
    let mut cache_builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 2,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 1_600,
    })
    .unwrap();
    cache_builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335024",
            DecodedAudio::from_stereo(48_000, source_left.clone(), source_right.clone()).unwrap(),
        )
        .unwrap();
    cache_builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335031",
            DecodedAudio::from_stereo(48_000, vec![0.0; 100], vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let cache = cache_builder.seal();
    let arrangement = cache.prepare_schedule(graph.arrangement()).unwrap();
    let prepared = cache.prepare_playback_scenes(&graph).unwrap();
    let mut renderer = PlaybackRenderer::try_new_for_tracks(
        &arrangement,
        graph.scene_authority(),
        &prepared,
        0,
        &["018f47a2-6b2d-7c11-8a44-001122335022"],
    )
    .unwrap();
    let mut left = [9.0_f32; 128];
    let mut right = [9.0_f32; 128];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    renderer.render_track_window(0, 128, &mut scratch).unwrap();
    assert_eq!(scratch[0].channels(), 2);
    assert_samples(scratch[0].left(), &[0.0; 128]);
    assert_samples(scratch[0].right(), &[0.0; 128]);

    renderer.queue_scene_launch(0).unwrap();
    renderer.render_track_window(128, 50, &mut scratch).unwrap();
    assert_eq!(scratch[0].channels(), 2);
    let expected_left: Vec<f32> = [vec![0.0; 22], source_left[..28].to_vec()].concat();
    let expected_right: Vec<f32> = [vec![0.0; 22], source_right[..28].to_vec()].concat();
    assert_samples(scratch[0].left(), &expected_left);
    assert_samples(scratch[0].right(), &expected_right);

    renderer.queue_scene_launch(1).unwrap();
    renderer.render_track_window(178, 10, &mut scratch).unwrap();
    assert_eq!(scratch[0].channels(), 2);
    assert_samples(scratch[0].left(), &[0.0; 10]);
    assert_samples(scratch[0].right(), &[0.0; 10]);
}

fn assert_samples(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "sample {index}: actual={actual}, expected={expected}"
        );
    }
}
