use hyper_audio_source::DecodedAudio;
use hyper_playback_compiler::{
    ClipCompileSpec, ClipTimebase, PlacementCompileSpec, TempoEvent, TickRange, Timeline,
    WarpMarker, compile_placement,
};

#[test]
fn compiled_tick_placement_renders_through_the_clip_engine() {
    let tempos = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 1_000_000,
    }];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
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
    let clip = ClipCompileSpec {
        enabled: true,
        timebase: ClipTimebase::Musical,
        duration_ticks: 4,
        source_start_frame: 2,
        source_frame_count: 100,
        asset_frame_count: 102,
        source_sample_rate_hz: 48_000,
        loop_enabled: true,
        loop_range: TickRange { start: 1, end: 4 },
        reverse: false,
        gain_decibels: 0.0,
        transpose_cents: 0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
        warp_markers: &markers,
    };
    let placement = PlacementCompileSpec {
        enabled: true,
        start_tick: 0,
        clip_start_tick: 0,
        duration_ticks: 6,
        gain_decibels: 0.0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
    };
    let compiled = compile_placement(timeline, clip, placement).unwrap();

    let mut samples = vec![-1.0, -1.0];
    samples.extend((0_u16..100).map(f32::from));
    let source = DecodedAudio::from_mono(48_000, samples).unwrap();
    let (plan, placement) = compiled.prepare(&source).unwrap();
    let rendered = plan.render_offline(placement).unwrap();
    let output = rendered.plane(0).unwrap();

    assert_eq!(compiled.arrangement_start_frame(), 0);
    assert_eq!(output.len(), 150);
    assert_eq!(&output[..100], &source.plane(0).unwrap()[2..102]);
    assert_eq!(&output[100..], &source.plane(0).unwrap()[27..77]);

    let unfolded = compile_placement(
        timeline,
        clip,
        PlacementCompileSpec {
            enabled: true,
            start_tick: 0,
            clip_start_tick: 5,
            duration_ticks: 6,
            gain_decibels: 0.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
        },
    )
    .unwrap();
    let (plan, placement) = unfolded.prepare(&source).unwrap();
    let rendered = plan.render_offline(placement).unwrap();
    let output = rendered.plane(0).unwrap();

    assert_eq!(&output[..50], &source.plane(0).unwrap()[52..102]);
    assert_eq!(&output[50..125], &source.plane(0).unwrap()[27..102]);
    assert_eq!(&output[125..], &source.plane(0).unwrap()[27..52]);
}

#[test]
fn sample_locked_placement_preserves_an_arbitrary_recorded_frame_count() {
    let tempos = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 1_000_000,
    }];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
    let markers = [
        WarpMarker {
            tick: 0,
            source_frame_offset: 0,
        },
        WarpMarker {
            tick: 4,
            source_frame_offset: 63,
        },
    ];
    let clip = ClipCompileSpec {
        enabled: true,
        timebase: ClipTimebase::SampleLocked,
        duration_ticks: 4,
        source_start_frame: 0,
        source_frame_count: 63,
        asset_frame_count: 63,
        source_sample_rate_hz: 48_000,
        loop_enabled: false,
        loop_range: TickRange { start: 0, end: 4 },
        reverse: false,
        gain_decibels: 0.0,
        transpose_cents: 0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
        warp_markers: &markers,
    };
    let compiled = compile_placement(
        timeline,
        clip,
        PlacementCompileSpec {
            enabled: true,
            start_tick: 0,
            clip_start_tick: 0,
            duration_ticks: 4,
            gain_decibels: 0.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
        },
    )
    .unwrap();

    assert_eq!(compiled.arrangement_start_frame(), 0);
    assert_eq!(compiled.arrangement_end_frame(), 63);
    assert_eq!(compiled.frame_count(), 63);
}
