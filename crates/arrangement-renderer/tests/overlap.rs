#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use std::sync::Mutex;

use hyper_arrangement_renderer::{ArrangementRenderer, RenderScratchSlot};
use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{CacheLimits, DecodedMediaCache, DecodedMediaCacheBuilder};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_json::{
    LoaderLimits, MediaAssetFact, OwnedCompiledSchedule, PlaybackJsonInput, load,
};

use realtime_allocator::HeapMeasurement;

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339000";
const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339001";

static REALTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

struct OverlapFixture {
    cache: DecodedMediaCache,
    schedule: OwnedCompiledSchedule,
}

#[derive(Clone, Copy)]
enum FixtureAudioLayout {
    Mono,
    Stereo,
}

#[test]
fn fixed_track_renderer_sums_a_bounded_overlap_without_losing_a_longer_placement() {
    let fixture = overlap_fixture(FixtureAudioLayout::Mono);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let arrangement_renderer =
        ArrangementRenderer::try_new_for_tracks(&prepared, &[TRACK_ID]).unwrap();
    let mut left = [9.0_f32; 600];
    let mut right = [9.0_f32; 600];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    let window = arrangement_renderer
        .render_track_window(0, 600, &mut scratch)
        .unwrap();

    assert_eq!(window.active_slot_count, 1);
    assert_eq!(scratch[0].entry_index(), Some(0));
    assert_eq!(scratch[0].channels(), 1);
    assert_eq!(&scratch[0].left()[..50], &[1.0; 50]);
    assert_eq!(&scratch[0].left()[50..150], &[1.25; 100]);
    assert_eq!(&scratch[0].left()[150..250], &[1.0; 100]);
    assert_eq!(&scratch[0].left()[250..350], &[1.5; 100]);
    assert_eq!(&scratch[0].left()[350..], &[1.0; 250]);
    assert_eq!(scratch[0].right(), &[0.0; 600]);

    let window = arrangement_renderer
        .render_track_window(250, 100, &mut scratch)
        .unwrap();

    assert_eq!(window.active_slot_count, 1);
    assert_eq!(scratch[0].entry_index(), Some(0));
    assert_eq!(scratch[0].left(), &[1.5; 100]);
    assert_eq!(scratch[0].right(), &[0.0; 100]);
}

#[test]
fn general_renderer_keeps_each_active_overlap_in_its_own_schedule_slot() {
    let fixture = overlap_fixture(FixtureAudioLayout::Mono);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let arrangement_renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut first_left = [9.0_f32; 100];
    let mut first_right = [9.0_f32; 100];
    let mut second_left = [9.0_f32; 100];
    let mut second_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut first_left, &mut first_right),
        RenderScratchSlot::new(&mut second_left, &mut second_right),
    ];

    let window = arrangement_renderer
        .render_window(50, 100, &mut scratch)
        .unwrap();

    assert_eq!(window.active_slot_count, 2);
    assert_eq!(scratch[0].entry_index(), Some(0));
    assert_eq!(scratch[0].left(), &[1.0; 100]);
    assert_eq!(scratch[0].right(), &[0.0; 100]);
    assert_eq!(scratch[1].entry_index(), Some(1));
    assert_eq!(scratch[1].left(), &[0.25; 100]);
    assert_eq!(scratch[1].right(), &[0.0; 100]);
}

#[test]
fn fixed_track_renderer_sums_a_bounded_stereo_overlap() {
    let fixture = overlap_fixture(FixtureAudioLayout::Stereo);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let arrangement_renderer =
        ArrangementRenderer::try_new_for_tracks(&prepared, &[TRACK_ID]).unwrap();
    let mut left = [9.0_f32; 100];
    let mut right = [9.0_f32; 100];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    let window = arrangement_renderer
        .render_track_window(50, 100, &mut scratch)
        .unwrap();

    assert_eq!(window.active_slot_count, 1);
    assert_eq!(scratch[0].entry_index(), Some(0));
    assert_eq!(scratch[0].channels(), 2);
    assert_eq!(scratch[0].left(), &[1.25; 100]);
    assert_eq!(scratch[0].right(), &[-1.25; 100]);
}

#[test]
fn fixed_track_two_voice_overlap_rendering_performs_zero_heap_allocations() {
    let _guard = REALTIME_TEST_LOCK.lock().unwrap();
    let fixture = overlap_fixture(FixtureAudioLayout::Mono);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let arrangement_renderer =
        ArrangementRenderer::try_new_for_tracks(&prepared, &[TRACK_ID]).unwrap();
    let mut left = [0.0_f32; 100];
    let mut right = [0.0_f32; 100];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    let measurement = HeapMeasurement::start_verified();
    let result = arrangement_renderer.render_track_window(50, 100, &mut scratch);
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}

#[allow(
    clippy::too_many_lines,
    reason = "the explicit fixture keeps the two-voice documents, schedule, and decoded media reviewable together"
)]
fn overlap_fixture(layout: FixtureAudioLayout) -> OverlapFixture {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339010",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122339000","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339001",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122339000","name":"Layered audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122339020","018f47a2-6b2d-7c11-8a44-001122339021","018f47a2-6b2d-7c11-8a44-001122339022"],"extensions":{}
    }"#;
    let clips = [
        clip_document(
            "018f47a2-6b2d-7c11-8a44-001122339020",
            "018f47a2-6b2d-7c11-8a44-001122339030",
            24,
            600,
        ),
        clip_document(
            "018f47a2-6b2d-7c11-8a44-001122339021",
            "018f47a2-6b2d-7c11-8a44-001122339031",
            4,
            100,
        ),
        clip_document(
            "018f47a2-6b2d-7c11-8a44-001122339022",
            "018f47a2-6b2d-7c11-8a44-001122339032",
            4,
            100,
        ),
    ];
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339040",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122339000","name":"Layered","enabled":true,
        "placements":[
            {"id":"018f47a2-6b2d-7c11-8a44-001122339050","track_id":"018f47a2-6b2d-7c11-8a44-001122339001","clip_id":"018f47a2-6b2d-7c11-8a44-001122339020","start_tick":0,"clip_start_tick":0,"duration_ticks":24,"enabled":true},
            {"id":"018f47a2-6b2d-7c11-8a44-001122339051","track_id":"018f47a2-6b2d-7c11-8a44-001122339001","clip_id":"018f47a2-6b2d-7c11-8a44-001122339021","start_tick":2,"clip_start_tick":0,"duration_ticks":4,"enabled":true},
            {"id":"018f47a2-6b2d-7c11-8a44-001122339052","track_id":"018f47a2-6b2d-7c11-8a44-001122339001","clip_id":"018f47a2-6b2d-7c11-8a44-001122339022","start_tick":10,"clip_start_tick":0,"duration_ticks":4,"enabled":true}
        ],"scene_launches":[],"extensions":{}
    }"#;
    let tracks = [track.as_slice()];
    let clip_bytes = [
        clips[0].as_slice(),
        clips[1].as_slice(),
        clips[2].as_slice(),
    ];
    let loaded = load(
        PlaybackJsonInput {
            project_id: PROJECT_ID,
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clip_bytes,
            scenes: &[],
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let facts = [
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122339030",
            sample_rate_hz: 48_000,
            frame_count: 600,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122339031",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122339032",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let schedule = loaded.compile(&facts, GraphLimits::VERSION_1).unwrap();
    let mut cache = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 3,
        max_frames_per_asset: 600,
        max_decoded_sample_bytes: match layout {
            FixtureAudioLayout::Mono => 3_200,
            FixtureAudioLayout::Stereo => 6_400,
        },
    })
    .unwrap();
    cache
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122339030",
            decoded_audio(layout, vec![1.0; 600]),
        )
        .unwrap();
    cache
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122339031",
            decoded_audio(layout, vec![0.25; 100]),
        )
        .unwrap();
    cache
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122339032",
            decoded_audio(layout, vec![0.5; 100]),
        )
        .unwrap();
    OverlapFixture {
        cache: cache.seal(),
        schedule,
    }
}

fn decoded_audio(layout: FixtureAudioLayout, left: Vec<f32>) -> DecodedAudio {
    match layout {
        FixtureAudioLayout::Mono => DecodedAudio::from_mono(48_000, left).unwrap(),
        FixtureAudioLayout::Stereo => {
            let right = left.iter().map(|sample| -*sample).collect();
            DecodedAudio::from_stereo(48_000, left, right).unwrap()
        }
    }
}

fn clip_document(
    clip_id: &str,
    asset_id: &str,
    duration_ticks: u64,
    frame_count: usize,
) -> Vec<u8> {
    format!(
        r#"{{
            "format":"hyper-audio-clip","format_version":1,"id":"{clip_id}",
            "project_id":"{PROJECT_ID}","track_id":"{TRACK_ID}","name":"Clip","enabled":true,
            "duration_ticks":{duration_ticks},
            "source":{{"asset_id":"{asset_id}","start_frame":0,"frame_count":{frame_count}}},
            "loop":{{"enabled":false,"start_tick":0,"end_tick":{duration_ticks}}},
            "playback":{{"gain_decibels":0,"reverse":false,"transpose_cents":0}},
            "fades":{{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0}},
            "warp_markers":[{{"tick":0,"source_frame_offset":0}},{{"tick":{duration_ticks},"source_frame_offset":{frame_count}}}],
            "extensions":{{}}
        }}"#
    )
    .into_bytes()
}
