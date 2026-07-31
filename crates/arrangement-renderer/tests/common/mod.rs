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
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335001",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track_a = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335002",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","name":"A","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335005","018f47a2-6b2d-7c11-8a44-001122335008"],"extensions":{}
    }"#;
    let track_b = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335003",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","name":"B","kind":"audio","order":1,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335006"],"extensions":{}
    }"#;
    let track_c = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335004",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","name":"C","kind":"audio","order":2,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335007"],"extensions":{}
    }"#;
    let clip_a = clip_document(
        "018f47a2-6b2d-7c11-8a44-001122335005",
        "018f47a2-6b2d-7c11-8a44-001122335002",
        "018f47a2-6b2d-7c11-8a44-001122335009",
    );
    let clip_b = clip_document(
        "018f47a2-6b2d-7c11-8a44-001122335006",
        "018f47a2-6b2d-7c11-8a44-001122335003",
        "018f47a2-6b2d-7c11-8a44-001122335010",
    );
    let clip_c = clip_document(
        "018f47a2-6b2d-7c11-8a44-001122335007",
        "018f47a2-6b2d-7c11-8a44-001122335004",
        "018f47a2-6b2d-7c11-8a44-001122335011",
    );
    let clip_d = clip_document(
        "018f47a2-6b2d-7c11-8a44-001122335008",
        "018f47a2-6b2d-7c11-8a44-001122335002",
        "018f47a2-6b2d-7c11-8a44-001122335012",
    );
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335013",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","name":"Main","enabled":true,
        "placements":[
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122335014","track_id":"018f47a2-6b2d-7c11-8a44-001122335002","clip_id":"018f47a2-6b2d-7c11-8a44-001122335005",
                "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122335016","track_id":"018f47a2-6b2d-7c11-8a44-001122335004","clip_id":"018f47a2-6b2d-7c11-8a44-001122335007",
                "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":false
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122335015","track_id":"018f47a2-6b2d-7c11-8a44-001122335003","clip_id":"018f47a2-6b2d-7c11-8a44-001122335006",
                "start_tick":5,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122335017","track_id":"018f47a2-6b2d-7c11-8a44-001122335002","clip_id":"018f47a2-6b2d-7c11-8a44-001122335008",
                "start_tick":9,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }
        ],
        "scene_launches":[],"extensions":{}
    }"#;
    let tracks: [&[u8]; 3] = [track_a, track_b, track_c];
    let clips: [&[u8]; 4] = [&clip_a, &clip_b, &clip_c, &clip_d];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122335000",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            scenes: &[],
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let facts = [
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335009",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335010",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335011",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122335012",
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
            "018f47a2-6b2d-7c11-8a44-001122335009",
            DecodedAudio::from_mono(48_000, mono_samples.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335010",
            DecodedAudio::from_stereo(48_000, stereo_left.clone(), stereo_right.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335011",
            DecodedAudio::from_mono(48_000, vec![1.0; 100]).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122335012",
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
            "project_id":"018f47a2-6b2d-7c11-8a44-001122335000","track_id":"{track_id}","name":"Clip","enabled":true,
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
