#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_audio_engine::{
    ChannelParameters, ControlledInputBlock, InputBlock, InterleavedStereoCallback, LinearGain,
    Pan, StereoMixer, StereoOutput,
};

use realtime_allocator::{HeapMeasurement, assert_allocator_positive_control};

#[test]
fn processing_performs_zero_heap_allocations() {
    let parameters = [
        ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
        ChannelParameters::new(LinearGain::UNITY, Pan::HARD_RIGHT, true),
    ];
    let mut mixer = StereoMixer::try_new(&parameters, LinearGain::UNITY).unwrap();
    let mono = [0.25_f32; 128];
    let stereo_left = [0.5_f32; 128];
    let stereo_right = [-0.5_f32; 128];
    let inputs = [
        InputBlock::Mono(&mono),
        InputBlock::Stereo {
            left: &stereo_left,
            right: &stereo_right,
        },
    ];
    let controlled_parameters = [parameters[0]; 128];
    let controlled_master = [LinearGain::UNITY; 128];
    let controlled_inputs = [ControlledInputBlock {
        input: inputs[0],
        parameters: &controlled_parameters,
    }];
    let mut controlled_mixer = StereoMixer::try_new(&parameters[..1], LinearGain::UNITY).unwrap();
    let mut output_left = [0.0_f32; 128];
    let mut output_right = [0.0_f32; 128];
    let mut callback = InterleavedStereoCallback::try_new(128).unwrap();
    let mut interleaved = [0.0_f32; 256];
    let mut failed_interleaved = [1.0_f32; 256];
    let mut invalid_interleaved = [1.0_f32; 3];

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
    let result = mixer.process(
        &inputs,
        &parameters,
        LinearGain::UNITY,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let controlled_result = controlled_mixer.process_controlled(
        &controlled_inputs,
        &controlled_master,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let callback_result = callback.process(&mut interleaved, |output| {
        output.left.fill(0.25);
        output.right.fill(-0.25);
        Ok::<_, u8>(())
    });
    let callback_failure = callback.process(&mut failed_interleaved, |output| {
        output.left.fill(0.5);
        output.right.fill(-0.5);
        Err::<(), _>(7_u8)
    });
    let callback_validation = callback.process(&mut invalid_interleaved, |_| Ok::<_, u8>(()));
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert!(controlled_result.is_ok());
    assert!(callback_result.is_ok());
    assert!(callback_failure.is_err());
    assert!(callback_validation.is_err());
    assert_eq!(heap_operations, 0);
}
