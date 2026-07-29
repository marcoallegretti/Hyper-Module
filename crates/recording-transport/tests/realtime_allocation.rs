#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_recording_transport::{RecordingPushError, try_recording_transport};
use realtime_allocator::HeapMeasurement;

#[test]
fn producer_paths_perform_zero_heap_allocations() {
    let (mut producer, consumer) = try_recording_transport(2).unwrap();
    consumer.register_current_thread_waker().unwrap();
    let accepted = [0.0_f32, 1.0];
    let saturated = [2.0_f32, 3.0, 4.0, 5.0];
    let malformed = [0.0_f32; 3];
    let non_finite = [f32::NAN, 0.0];

    let measurement = HeapMeasurement::start_verified();
    assert_eq!(producer.push(&accepted), Ok(()));
    assert_eq!(
        producer.push(&saturated),
        Err(RecordingPushError::Saturated {
            frames: 2,
            available_frames: 1,
        })
    );
    assert_eq!(
        producer.push(&malformed),
        Err(RecordingPushError::SampleCountNotStereoAligned { samples: 3 })
    );
    assert_eq!(
        producer.push(&non_finite),
        Err(RecordingPushError::NonFiniteInput)
    );
    let heap_operations = measurement.finish();

    drop(consumer);
    assert_eq!(heap_operations, 0);
}
