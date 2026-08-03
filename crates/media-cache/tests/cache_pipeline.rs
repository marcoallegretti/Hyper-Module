mod common;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::ClipOutput;
use hyper_media_cache::{
    CacheLimits, DecodedMediaCacheBuilder, PreparePlaybackError, PrepareSceneAudioError,
    PrepareScheduleError,
};
use hyper_playback_compiler::PrepareError;
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{
    LoadedPlaybackProject, LoaderLimits, MediaAssetFact, PlaybackJsonInput, load,
};

use common::one_clip_schedule;

fn builder() -> DecodedMediaCacheBuilder {
    DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 4,
        max_frames_per_asset: 1_000,
        max_decoded_sample_bytes: 32_000,
    })
    .unwrap()
}

#[allow(
    clippy::too_many_lines,
    reason = "strict source documents make both playback authorities explicit"
)]
fn playback_project() -> LoadedPlaybackProject {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335051",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let arrangement_track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335052",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","name":"Arrangement","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335054"],"extensions":{}
    }"#;
    let scene_track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335053",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","name":"Scene","kind":"audio","order":1,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335055"],"extensions":{}
    }"#;
    let arrangement_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335054",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","track_id":"018f47a2-6b2d-7c11-8a44-001122335052","name":"Arrangement Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122335056","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let scene_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335055",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","track_id":"018f47a2-6b2d-7c11-8a44-001122335053","name":"Scene Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122335057","start_frame":0,"frame_count":100},
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
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335058",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","name":"Play","order":0,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122335053","action":"play","clip_id":"018f47a2-6b2d-7c11-8a44-001122335055"}],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335059",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335050","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122335060","track_id":"018f47a2-6b2d-7c11-8a44-001122335052","clip_id":"018f47a2-6b2d-7c11-8a44-001122335054",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[{"id":"018f47a2-6b2d-7c11-8a44-001122335061","tick":1,"scene_id":"018f47a2-6b2d-7c11-8a44-001122335058"}],
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 2] = [arrangement_track, scene_track];
    let clips: [&[u8]; 2] = [arrangement_clip, scene_clip];
    let scenes: [&[u8]; 1] = [play_scene];
    load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122335050",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap()
}

#[test]
fn cache_binds_compiled_identity_and_renders_prepared_audio() {
    let schedule = one_clip_schedule();
    let source_samples: Vec<f32> = (0_u16..100)
        .map(|sample| f32::from(sample) / 100.0)
        .collect();
    let source = DecodedAudio::from_mono(48_000, source_samples.clone()).unwrap();
    let mut builder = builder();
    builder
        .try_insert("018f47a2-6b2d-7c11-8a44-001122335034", source)
        .unwrap();
    let cache = builder.seal();

    let prepared = cache.prepare_schedule(&schedule).unwrap();
    assert_eq!(
        prepared.project_id(),
        "018f47a2-6b2d-7c11-8a44-001122335030"
    );
    let entry = &prepared.entries()[0];
    let mut output = [0.0_f32; 100];
    entry
        .render_block(0, ClipOutput::Mono(&mut output))
        .unwrap();

    assert_eq!(
        entry.schedule_entry().placement_id,
        "018f47a2-6b2d-7c11-8a44-001122335036"
    );
    assert_eq!(
        entry.schedule_entry().compiled.arrangement_start_frame(),
        100
    );
    assert_eq!(output.as_slice(), source_samples.as_slice());
}

#[test]
fn missing_asset_rejects_the_whole_schedule() {
    let schedule = one_clip_schedule();
    let cache = builder().seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::MissingAsset { schedule_index: 0 }
    );
}

#[test]
fn decoded_rate_mismatch_is_rejected_during_binding() {
    let schedule = one_clip_schedule();
    let mut builder = builder();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335034",
            DecodedAudio::from_mono(44_100, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let cache = builder.seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::InvalidAsset {
            schedule_index: 0,
            source: PrepareError::SourceSampleRateMismatch {
                expected: 48_000,
                actual: 44_100,
            },
        }
    );
}

#[test]
fn decoded_frame_count_mismatch_is_rejected_during_binding() {
    let schedule = one_clip_schedule();
    let mut builder = builder();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335034",
            DecodedAudio::from_mono(48_000, vec![0.0; 101]).unwrap(),
        )
        .unwrap();
    let cache = builder.seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::InvalidAsset {
            schedule_index: 0,
            source: PrepareError::SourceFrameCountMismatch {
                expected: 100,
                actual: 101,
            },
        }
    );
}

#[test]
fn complete_playback_preparation_rejects_each_missing_authority_atomically() {
    let project = playback_project();
    let facts = [
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335056",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335057",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .unwrap();

    let empty = builder().seal();
    assert_eq!(
        empty.prepare_playback(&graph).unwrap_err(),
        PreparePlaybackError::Schedule(PrepareScheduleError::MissingAsset { schedule_index: 0 })
    );

    let mut arrangement_only = builder();
    arrangement_only
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335056",
            DecodedAudio::from_mono(48_000, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let arrangement_only = arrangement_only.seal();
    assert_eq!(
        arrangement_only.prepare_playback(&graph).unwrap_err(),
        PreparePlaybackError::SceneAudio(PrepareSceneAudioError::MissingAsset { binding_index: 0 })
    );

    let mut complete = builder();
    complete
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335056",
            DecodedAudio::from_mono(48_000, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    complete
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335057",
            DecodedAudio::from_mono(48_000, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let complete = complete.seal();
    let prepared = complete.prepare_playback(&graph).unwrap();
    assert_eq!(
        prepared.project_id(),
        "018f47a2-6b2d-7c11-8a44-001122335050"
    );
    assert_eq!(prepared.schedule().entries().len(), 1);
    assert_eq!(prepared.scene_audio().entries().len(), 2);
    assert_eq!(
        prepared.scene_audio().scene_availability(0),
        hyper_media_cache::PreparedSceneAvailability::Ready
    );
    assert_eq!(prepared.authority().track_count(), 2);
}
