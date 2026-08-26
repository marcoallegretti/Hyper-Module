use hyper_audio_source::DecodedAudio;
use hyper_playback_compiler::ClipTimebase;
use hyper_playback_compiler::TempoEvent;
use hyper_playback_compiler::TickRange;
use hyper_playback_compiler::Timeline;
use hyper_playback_compiler::WarpMarker;
use hyper_playback_compiler::graph::{
    ArrangementResource, AssetResource, ClipResource, GraphInput, GraphLimits, PlacementResource,
    TrackKind, TrackResource, compile_arrangement,
};

#[test]
fn resolved_graph_entry_prepares_and_renders_its_declared_asset() {
    let tempos = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 1_000_000,
    }];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
    let track_clip_ids = ["clip"];
    let tracks = [TrackResource {
        id: "track",
        kind: TrackKind::Audio,
        active: true,
        clip_ids: &track_clip_ids,
    }];
    let assets = [AssetResource {
        id: "asset",
        sample_rate_hz: 48_000,
        frame_count: 100,
    }];
    let markers = [
        WarpMarker {
            tick: 0,
            source_frame_offset: 0,
        },
        WarpMarker {
            tick: 4,
            source_frame_offset: 100,
        },
    ];
    let clips = [ClipResource {
        id: "clip",
        track_id: "track",
        asset_id: "asset",
        enabled: true,
        timebase: ClipTimebase::Musical,
        duration_ticks: 4,
        source_start_frame: 0,
        source_frame_count: 100,
        loop_enabled: false,
        loop_range: TickRange { start: 0, end: 4 },
        reverse: false,
        gain_decibels: 0.0,
        transpose_cents: 0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
        warp_markers: &markers,
    }];
    let placements = [PlacementResource {
        id: "placement",
        track_id: "track",
        clip_id: "clip",
        start_tick: 4,
        clip_start_tick: 0,
        duration_ticks: 4,
        enabled: true,
    }];
    let schedule = compile_arrangement(
        timeline,
        GraphInput {
            tracks: &tracks,
            clips: &clips,
            assets: &assets,
            arrangement: ArrangementResource {
                enabled: true,
                placements: &placements,
                scene_launch_count: 0,
            },
        },
        GraphLimits::VERSION_1,
    )
    .unwrap();

    let source = DecodedAudio::from_mono(
        48_000,
        (0_u16..100)
            .map(|sample| f32::from(sample) / 100.0)
            .collect(),
    )
    .unwrap();
    let entry = schedule.entries()[0];
    let (plan, placement) = entry.compiled.prepare(&source).unwrap();
    let output = plan.render_offline(placement).unwrap();

    assert_eq!(entry.compiled.arrangement_start_frame(), 100);
    assert_eq!(output.plane(0), source.plane(0));
}
