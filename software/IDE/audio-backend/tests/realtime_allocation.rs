#[path = "../../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_desktop_audio::{
    CaptureCallback, CaptureCallbackStatus, OutputCallback, OutputCallbackStatus,
};
use hyper_recording_transport::try_recording_transport;
use realtime_allocator::HeapMeasurement;

#[test]
fn callback_processing_performs_zero_heap_allocations() {
    let mut invocation = 0_u8;
    let mut callback = OutputCallback::try_new(
        move |output: &mut [f32]| -> Result<(), u8> {
            invocation = invocation.saturating_add(1);
            output.fill(0.25);
            if invocation == 1 { Ok(()) } else { Err(7) }
        },
        128,
    )
    .unwrap();
    let mut rendered = [0.0_f32; 256];
    let mut failed = [1.0_f32; 256];
    let mut invalid = [1.0_f32; 3];
    let mut oversized = [1.0_f32; 258];

    let measurement = HeapMeasurement::start_verified();
    let rendered_status = callback.process(&mut rendered);
    let failed_status = callback.process(&mut failed);
    let invalid_status = callback.process(&mut invalid);
    let oversized_status = callback.process(&mut oversized);
    let heap_operations = measurement.finish();

    assert_eq!(rendered_status, OutputCallbackStatus::Rendered);
    assert_eq!(failed_status, OutputCallbackStatus::ProcessorFailed);
    assert_eq!(
        invalid_status,
        OutputCallbackStatus::SampleCountNotStereoAligned { samples: 3 }
    );
    assert_eq!(
        oversized_status,
        OutputCallbackStatus::BlockTooLarge {
            frames: 129,
            maximum: 128
        }
    );
    assert_eq!(heap_operations, 0);
}

#[test]
fn capture_callback_processing_performs_zero_heap_allocations() {
    let mut invocation = 0_u8;
    let mut callback = CaptureCallback::try_new(
        move |_: &[f32]| -> Result<(), u8> {
            invocation = invocation.saturating_add(1);
            if invocation == 1 { Ok(()) } else { Err(7) }
        },
        128,
    )
    .unwrap();
    let captured = [0.25_f32; 256];
    let failed = [0.5_f32; 256];
    let invalid = [0.0_f32; 3];
    let oversized = [0.0_f32; 258];
    let non_finite = [f32::NAN, 0.0];

    let measurement = HeapMeasurement::start_verified();
    let captured_status = callback.process(&captured);
    let failed_status = callback.process(&failed);
    let invalid_status = callback.process(&invalid);
    let oversized_status = callback.process(&oversized);
    let non_finite_status = callback.process(&non_finite);
    let heap_operations = measurement.finish();

    assert_eq!(captured_status, CaptureCallbackStatus::Captured);
    assert_eq!(failed_status, CaptureCallbackStatus::ProcessorFailed);
    assert_eq!(
        invalid_status,
        CaptureCallbackStatus::SampleCountNotStereoAligned { samples: 3 }
    );
    assert_eq!(
        oversized_status,
        CaptureCallbackStatus::BlockTooLarge {
            frames: 129,
            maximum: 128,
        }
    );
    assert_eq!(non_finite_status, CaptureCallbackStatus::NonFiniteInput);
    assert_eq!(heap_operations, 0);
}

#[test]
fn capture_to_recording_transport_performs_zero_heap_allocations() {
    let (mut producer, _consumer) = try_recording_transport(2).unwrap();
    let mut callback =
        CaptureCallback::try_new(move |input: &[f32]| producer.push(input), 2).unwrap();
    let accepted = [0.25_f32, 0.5];
    let saturated = [0.5_f32; 4];

    let measurement = HeapMeasurement::start_verified();
    let captured_status = callback.process(&accepted);
    let saturated_status = callback.process(&saturated);
    let heap_operations = measurement.finish();

    assert_eq!(captured_status, CaptureCallbackStatus::Captured);
    assert_eq!(saturated_status, CaptureCallbackStatus::ProcessorFailed);
    assert_eq!(callback.counters().snapshot().processor_failure_count, 1);
    assert_eq!(callback.counters().snapshot().dropped_frame_count, 2);
    assert_eq!(heap_operations, 0);
}
