#[path = "../../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_desktop_audio::{
    CaptureCallback, CaptureCallbackStatus, OutputCallback, OutputCallbackStatus,
};
use realtime_allocator::{HeapMeasurement, assert_allocator_positive_control};

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

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
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

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
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
