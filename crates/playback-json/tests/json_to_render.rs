use hyper_audio_source::DecodedAudio;
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_json::{LoaderLimits, MediaAssetFact, PlaybackJsonInput, load};

#[test]
fn strict_documents_resolve_compile_bind_and_render() {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334458",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334456",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122334460"],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334460",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122334457","start_frame":25,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":true,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334463",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122334464","track_id":"018f47a2-6b2d-7c11-8a44-001122334456","clip_id":"018f47a2-6b2d-7c11-8a44-001122334460",
            "start_tick":4,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 1] = [clip];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122334455",
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
    let schedule = loaded
        .compile(
            &[MediaAssetFact {
                id: "018f47a2-6b2d-7c11-8a44-001122334457",
                sample_rate_hz: 48_000,
                frame_count: 125,
            }],
            GraphLimits::VERSION_1,
        )
        .unwrap();

    let source = DecodedAudio::from_mono(
        48_000,
        (0_u16..125)
            .map(|sample| f32::from(sample) / 100.0)
            .collect(),
    )
    .unwrap();
    let entry = &schedule.entries()[0];
    let (plan, placement) = entry.compiled.prepare(&source).unwrap();
    let rendered = plan.render_offline(placement).unwrap();

    assert_eq!(entry.compiled.arrangement_start_frame(), 100);
    let expected: Vec<f32> = source.plane(0).unwrap()[25..125]
        .iter()
        .rev()
        .copied()
        .collect();
    assert_eq!(rendered.plane(0), Some(expected.as_slice()));
}
