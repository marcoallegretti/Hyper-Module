use hyper_clip_engine::ConsolidationReason;
use hyper_playback_compiler::{
    ClipCompileSpec, CompileError, TempoEvent, TickRange, Timeline, WarpMarker,
    compile_scene_audio_clip,
};

#[test]
fn open_scene_loop_rejects_a_later_incompatible_tempo() {
    let tempos = [
        TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_000,
        },
        TempoEvent {
            tick: 8,
            microseconds_per_quarter: 400_000,
        },
    ];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
    let markers = [
        WarpMarker {
            tick: 0,
            source_frame_offset: 0,
        },
        WarpMarker {
            tick: 2,
            source_frame_offset: 25,
        },
    ];
    let clip = ClipCompileSpec {
        enabled: true,
        duration_ticks: 2,
        source_start_frame: 0,
        source_frame_count: 25,
        asset_frame_count: 25,
        source_sample_rate_hz: 48_000,
        loop_enabled: true,
        loop_range: TickRange { start: 0, end: 2 },
        reverse: false,
        gain_decibels: 0.0,
        transpose_cents: 0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
        warp_markers: &markers,
    };

    assert_eq!(
        compile_scene_audio_clip(timeline, clip, 1),
        Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch
        ))
    );
}
