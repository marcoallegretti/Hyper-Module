use hyper_arrangement_renderer::{RenderScratchSlot, SceneRenderer};
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
fn strict_scene_json_compiles_binds_and_renders_sample_exact_audio() {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"track",
        "project_id":"project","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["clip"],"extensions":{}
    }"#;
    let clip = br#"{
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
    let play_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"play-scene",
        "project_id":"project","name":"Play","order":0,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"track","action":"play","clip_id":"clip"}],
        "extensions":{}
    }"#;
    let stop_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"stop-scene",
        "project_id":"project","name":"Stop","order":1,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"track","action":"stop"}],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,"placements":[],
        "scene_launches":[
            {"id":"play-launch","tick":1,"scene_id":"play-scene"},
            {"id":"stop-launch","tick":3,"scene_id":"stop-scene"}
        ],
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 1] = [clip];
    let scenes: [&[u8]; 2] = [play_scene, stop_scene];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "project",
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
    let facts = [MediaAssetFact {
        id: "asset",
        sample_rate_hz: 48_000,
        frame_count: 100,
    }];
    let graph = loaded
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .unwrap();
    assert!(graph.arrangement().entries().is_empty());
    let source: Vec<f32> = (0_u8..100)
        .map(|sample| f32::from(sample) / 100.0)
        .collect();
    let mut cache_builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 400,
    })
    .unwrap();
    cache_builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, source.clone()).unwrap(),
        )
        .unwrap();
    let cache = cache_builder.seal();
    let prepared = cache.prepare_playback_scenes(&graph).unwrap();
    let mut renderer = SceneRenderer::try_new(graph.scene_authority(), &prepared, 0).unwrap();
    let mut left = [9.0_f32; 128];
    let mut right = [9.0_f32; 128];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    renderer.render_window(0, 128, &mut scratch).unwrap();

    let expected: Vec<f32> = (0..128)
        .map(|frame| {
            if (25..75).contains(&frame) {
                source[frame - 25]
            } else {
                0.0
            }
        })
        .collect();
    assert_samples(scratch[0].left(), &expected);
    assert_samples(scratch[0].right(), &[0.0; 128]);
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
