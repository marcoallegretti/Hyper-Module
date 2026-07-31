use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_json::{
    LoaderLimits, MediaAssetFact, OwnedCompiledSchedule, PlaybackJsonInput, load,
};

pub fn one_clip_schedule() -> OwnedCompiledSchedule {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335031",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335030","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335032",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335030","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122335033"],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335033",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335030","track_id":"018f47a2-6b2d-7c11-8a44-001122335032","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122335034","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122335035",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122335030","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122335036","track_id":"018f47a2-6b2d-7c11-8a44-001122335032","clip_id":"018f47a2-6b2d-7c11-8a44-001122335033",
            "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 1] = [clip];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122335030",
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
    loaded
        .compile(
            &[MediaAssetFact {
                id: "018f47a2-6b2d-7c11-8a44-001122335034",
                sample_rate_hz: 48_000,
                frame_count: 100,
            }],
            GraphLimits::VERSION_1,
        )
        .unwrap()
}
