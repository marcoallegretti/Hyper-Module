use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{CacheLimits, DecodedMediaCache};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_json::{
    LoaderLimits, MediaAssetFact, OwnedCompiledSchedule, PlaybackJsonInput, load,
};

pub struct Fixture {
    pub cache: DecodedMediaCache,
    pub schedule: OwnedCompiledSchedule,
    #[allow(
        dead_code,
        reason = "sample vectors are asserted by the functional test binary"
    )]
    pub mono_samples: Vec<f32>,
    #[allow(
        dead_code,
        reason = "sample vectors are asserted by the functional test binary"
    )]
    pub stereo_left: Vec<f32>,
    #[allow(
        dead_code,
        reason = "sample vectors are asserted by the functional test binary"
    )]
    pub stereo_right: Vec<f32>,
    #[allow(
        dead_code,
        reason = "sample vectors are asserted by the functional test binary"
    )]
    pub sequential_samples: Vec<f32>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one explicit verified JSON-to-cache fixture keeps cross-resource identities reviewable"
)]
pub fn fixture() -> Fixture {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track_a = br#"{
        "format":"hyper-track","format_version":1,"id":"track-a",
        "project_id":"project","name":"A","kind":"audio","order":0,
        "active":true,"clip_ids":["clip-a","clip-d"],"extensions":{}
    }"#;
    let track_b = br#"{
        "format":"hyper-track","format_version":1,"id":"track-b",
        "project_id":"project","name":"B","kind":"audio","order":1,
        "active":true,"clip_ids":["clip-b"],"extensions":{}
    }"#;
    let track_c = br#"{
        "format":"hyper-track","format_version":1,"id":"track-c",
        "project_id":"project","name":"C","kind":"audio","order":2,
        "active":true,"clip_ids":["clip-c"],"extensions":{}
    }"#;
    let clip_a = clip_document("clip-a", "track-a", "asset-a");
    let clip_b = clip_document("clip-b", "track-b", "asset-b");
    let clip_c = clip_document("clip-c", "track-c", "asset-c");
    let clip_d = clip_document("clip-d", "track-a", "asset-d");
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[
            {
                "id":"placement-a","track_id":"track-a","clip_id":"clip-a",
                "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"placement-c","track_id":"track-c","clip_id":"clip-c",
                "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":false
            },
            {
                "id":"placement-b","track_id":"track-b","clip_id":"clip-b",
                "start_tick":5,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"placement-d","track_id":"track-a","clip_id":"clip-d",
                "start_tick":9,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }
        ],
        "scene_launches":[],"extensions":{}
    }"#;
    let tracks: [&[u8]; 3] = [track_a, track_b, track_c];
    let clips: [&[u8]; 4] = [&clip_a, &clip_b, &clip_c, &clip_d];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "project",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let facts = [
        MediaAssetFact {
            id: "asset-a",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "asset-b",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "asset-c",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "asset-d",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let schedule = loaded.compile(&facts, GraphLimits::VERSION_1).unwrap();

    let mono_samples: Vec<f32> = (0_u16..100).map(|frame| f32::from(frame) / 100.0).collect();
    let stereo_left: Vec<f32> = (0_u16..100).map(|frame| f32::from(frame) / 200.0).collect();
    let stereo_right: Vec<f32> = stereo_left.iter().map(|sample| -*sample).collect();
    let sequential_samples: Vec<f32> = (0_u16..100)
        .map(|frame| 1.0 - f32::from(frame) / 200.0)
        .collect();
    let mut builder = hyper_media_cache::DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 4,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 2_000,
    })
    .unwrap();
    builder
        .try_insert(
            "asset-a",
            DecodedAudio::from_mono(48_000, mono_samples.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "asset-b",
            DecodedAudio::from_stereo(48_000, stereo_left.clone(), stereo_right.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "asset-c",
            DecodedAudio::from_mono(48_000, vec![1.0; 100]).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "asset-d",
            DecodedAudio::from_mono(48_000, sequential_samples.clone()).unwrap(),
        )
        .unwrap();

    Fixture {
        cache: builder.seal(),
        schedule,
        mono_samples,
        stereo_left,
        stereo_right,
        sequential_samples,
    }
}

fn clip_document(clip_id: &str, track_id: &str, asset_id: &str) -> Vec<u8> {
    format!(
        r#"{{
            "format":"hyper-audio-clip","format_version":1,"id":"{clip_id}",
            "project_id":"project","track_id":"{track_id}","name":"Clip","enabled":true,
            "duration_ticks":4,
            "source":{{"asset_id":"{asset_id}","start_frame":0,"frame_count":100}},
            "loop":{{"enabled":false,"start_tick":0,"end_tick":4}},
            "playback":{{"gain_decibels":0,"reverse":false,"transpose_cents":0}},
            "fades":{{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0}},
            "warp_markers":[
                {{"tick":0,"source_frame_offset":0}},
                {{"tick":4,"source_frame_offset":100}}
            ],
            "extensions":{{}}
        }}"#
    )
    .into_bytes()
}
