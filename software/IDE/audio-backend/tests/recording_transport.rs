use hyper_desktop_audio::{CaptureCallback, CaptureCallbackStatus};
use hyper_recording_transport::{
    RecordingDrain, RecordingTransportSnapshot, try_recording_transport,
};

#[test]
fn capture_callback_publishes_complete_blocks_without_partial_overflow() {
    let (mut producer, mut consumer) = try_recording_transport(2).unwrap();
    let counters = producer.counters();
    let mut callback =
        CaptureCallback::try_new(move |input: &[f32]| producer.push(input), 2).unwrap();

    assert_eq!(
        callback.process(&[0.0, 1.0]),
        CaptureCallbackStatus::Captured
    );
    assert_eq!(
        callback.process(&[2.0, 3.0, 4.0, 5.0]),
        CaptureCallbackStatus::ProcessorFailed
    );
    drop(callback);

    let mut output = [9.0_f32; 4];
    assert_eq!(
        consumer.drain(&mut output).unwrap(),
        RecordingDrain {
            frames: 1,
            producer_closed: true,
        }
    );
    assert_eq!(
        output.map(f32::to_bits),
        [0.0_f32, 1.0, 9.0, 9.0].map(f32::to_bits)
    );
    assert_eq!(
        counters.snapshot(),
        RecordingTransportSnapshot {
            push_attempt_count: 2,
            accepted_block_count: 1,
            accepted_frame_count: 1,
            rejected_block_count: 1,
            rejected_frame_count: 2,
            drained_frame_count: 1,
        }
    );
}
