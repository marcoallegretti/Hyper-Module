#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{
    ClipOutput, ClipRenderPlan, FrameRange, NativeClipSpec, PlacementSpec, TimeMapping,
};

use realtime_allocator::HeapMeasurement;

#[test]
fn clip_block_rendering_performs_zero_heap_allocations() {
    let source = DecodedAudio::from_stereo(48_000, vec![0.25; 128], vec![-0.25; 128]).unwrap();
    let plan = ClipRenderPlan::try_new(
        &source,
        NativeClipSpec {
            enabled: true,
            source: FrameRange { start: 0, end: 128 },
            loop_range: Some(FrameRange { start: 32, end: 96 }),
            reverse: true,
            gain_decibels: -3.0,
            fade_in_frames: 16,
            fade_out_frames: 16,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
    )
    .unwrap();
    let placement = plan
        .place(PlacementSpec {
            clip_start_frame: 0,
            frame_count: 256,
        })
        .unwrap();
    let mut left = [0.0_f32; 128];
    let mut right = [0.0_f32; 128];

    let measurement = HeapMeasurement::start_verified();
    let result = plan.render_block(
        placement,
        64,
        ClipOutput::Stereo {
            left: &mut left,
            right: &mut right,
        },
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
