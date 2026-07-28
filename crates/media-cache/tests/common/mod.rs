use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_json::{
    LoaderLimits, MediaAssetFact, OwnedCompiledSchedule, PlaybackJsonInput, load,
};

pub fn one_clip_schedule() -> OwnedCompiledSchedule {
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
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[{
            "id":"placement","track_id":"track","clip_id":"clip",
            "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 1] = [clip];
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
    loaded
        .compile(
            &[MediaAssetFact {
                id: "asset",
                sample_rate_hz: 48_000,
                frame_count: 100,
            }],
            GraphLimits::VERSION_1,
        )
        .unwrap()
}
