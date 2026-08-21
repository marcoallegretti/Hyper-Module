mod common;
#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use std::sync::Mutex;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{InterleavedStereoCallback, LoudnessMeasurementCommand, StereoOutput};
use hyper_mixer_runtime::{
    EbuLoudnessPresentationConfig, FlatMainRuntime, GraphMainRuntime, GraphTransportRuntime,
    MixerParameterCommand, PreparedPlaybackMainRuntime, PreparedPlaybackTransportRuntime,
    TransportCommand, TransportOutputProcessor, loudness_command_channel, output_meter_channel,
};
use hyper_native_device::{
    DeviceChain, NativeDeviceInstance, NativeFilterMode, NativeFilterState,
};

use common::{fixture, graph_fixture, playback_fixture};
use realtime_allocator::HeapMeasurement;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn native_filter_chain() -> DeviceChain {
    DeviceChain::try_new(
        "018f47a2-6b2d-7c11-8a44-001122336470".to_owned(),
        "018f47a2-6b2d-7c11-8a44-001122336400".to_owned(),
        "018f47a2-6b2d-7c11-8a44-001122336440".to_owned(),
        vec![
            NativeDeviceInstance::try_new_filter(
                "018f47a2-6b2d-7c11-8a44-001122336471".to_owned(),
                true,
                NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_107)
                    .unwrap(),
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

#[test]
fn prepared_arrangement_to_main_mix_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [0.0_f32; 64];
    let mut scratch_right = [0.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];

    let measurement = HeapMeasurement::start_verified();
    let result = runtime.process_window(
        0,
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
    for (actual, source) in output_left.iter().zip(&fixture.samples[..64]) {
        assert!((*actual - *source * 0.25).abs() <= f32::EPSILON);
    }
    assert!(
        output_right
            .iter()
            .all(|sample| sample.abs() <= f32::EPSILON)
    );
}

#[test]
fn prepared_routing_graph_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 64).unwrap();
    let automated =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 64)
            .unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(automated);
    let mut left_0 = [0.0_f32; 64];
    let mut right_0 = [0.0_f32; 64];
    let mut left_1 = [0.0_f32; 64];
    let mut right_1 = [0.0_f32; 64];
    let mut left_2 = [0.0_f32; 64];
    let mut right_2 = [0.0_f32; 64];
    let mut left_3 = [0.0_f32; 64];
    let mut right_3 = [0.0_f32; 64];
    let mut left_4 = [0.0_f32; 64];
    let mut right_4 = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut left_0, &mut right_0),
        RenderScratchSlot::new(&mut left_1, &mut right_1),
        RenderScratchSlot::new(&mut left_2, &mut right_2),
        RenderScratchSlot::new(&mut left_3, &mut right_3),
        RenderScratchSlot::new(&mut left_4, &mut right_4),
    ];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];
    let mut callback = InterleavedStereoCallback::try_new(64, 48_000).unwrap();
    let (mut meter_sender, _meter_receiver) = output_meter_channel();
    let mut interleaved = [0.0_f32; 128];

    let measurement = HeapMeasurement::start_verified();
    let result = runtime.process_window(
        0,
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let start_result = sender.try_send(TransportCommand::Start);
    let automated_result = transport.process_interleaved_cycle(
        &mut callback,
        &mut scratch,
        &mut interleaved,
        &mut meter_sender,
    );
    let stop_result = sender.try_send(TransportCommand::Stop);
    let stopped_result = transport.process_interleaved_cycle(
        &mut callback,
        &mut scratch,
        &mut interleaved,
        &mut meter_sender,
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert!(start_result.is_ok());
    assert!(automated_result.is_ok());
    assert!(stop_result.is_ok());
    assert!(stopped_result.is_ok());
    assert_eq!(heap_operations, 0);
}

#[test]
fn prepared_combined_playback_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = playback_fixture();
    let automation = fixture.automation.clone();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime = PreparedPlaybackMainRuntime::try_new_prepared_playback_automated(
        &prepared,
        &fixture.mixer,
        automation,
        64,
    )
    .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport.apply_command(TransportCommand::Start).unwrap();
    let mut arrangement_left = [0.0_f32; 64];
    let mut arrangement_right = [0.0_f32; 64];
    let mut scene_left = [0.0_f32; 64];
    let mut scene_right = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];

    let measurement = HeapMeasurement::start_verified();
    let result = transport.process_cycle(
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
    for (actual, source) in output_left.iter().zip(&fixture.arrangement_samples[..64]) {
        assert!((*actual - *source).abs() <= f32::EPSILON);
    }
    assert!(output_right[..25].iter().all(|sample| *sample == 0.0));
    for (actual, source) in output_right[25..].iter().zip(&fixture.scene_samples[..39]) {
        assert!((*actual - *source).abs() <= f32::EPSILON);
    }
}

#[test]
fn native_filter_playback_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let chain = native_filter_chain();
    let mut runtime = PreparedPlaybackMainRuntime::try_new_prepared_playback_with_devices(
        &prepared,
        &fixture.mixer,
        core::slice::from_ref(&chain),
        64,
    )
    .unwrap();
    let mut arrangement_left = [0.0_f32; 64];
    let mut arrangement_right = [0.0_f32; 64];
    let mut scene_left = [0.0_f32; 64];
    let mut scene_right = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];

    let measurement = HeapMeasurement::start_verified();
    let result = runtime.process_window(
        0,
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
    assert!(output_left.iter().any(|sample| *sample != 0.0));
}

#[test]
fn transport_output_processor_performs_zero_heap_allocations_for_variable_quanta() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let (mut command_sender, mut mixer_parameter_sender, mut transport) =
        PreparedPlaybackTransportRuntime::with_command_queues(runtime);
    let mut arrangement_left = [0.0_f32; 64];
    let mut arrangement_right = [0.0_f32; 64];
    let mut scene_left = [0.0_f32; 64];
    let mut scene_right = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let (mut meter_sender, _meter_receiver) = output_meter_channel();
    let (mut loudness_sender, loudness_receiver) = loudness_command_channel();
    let mut processor = TransportOutputProcessor::try_new(
        &mut transport,
        &mut scratch,
        &mut meter_sender,
        loudness_receiver,
        48_000,
    )
    .unwrap();
    let mut first = [0.0_f32; 100];
    let mut second = [0.0_f32; 50];

    let measurement = HeapMeasurement::start_verified();
    let start_result = command_sender.try_send(TransportCommand::Start);
    let scene_result = command_sender.try_send(TransportCommand::LaunchScene { scene_index: 0 });
    let channel_result =
        mixer_parameter_sender.resolve_channel("018f47a2-6b2d-7c11-8a44-001122336440");
    let parameter_result = channel_result.map(|channel| {
        mixer_parameter_sender.try_send(MixerParameterCommand::ChannelMute {
            channel,
            muted: false,
        })
    });
    let loudness_result = loudness_sender.try_send(LoudnessMeasurementCommand::Reset);
    let first_result = processor.process(&mut first);
    let loudness_presentation = first_result.as_ref().ok().map(|cycle| {
        cycle
            .snapshot()
            .present_loudness(EbuLoudnessPresentationConfig::default())
    });
    let second_result = processor.process_with_post_mix(&mut second, |post| {
        for sample in post.left.iter_mut().chain(post.right.iter_mut()) {
            *sample += 0.125;
        }
    });
    let heap_operations = measurement.finish();

    assert!(start_result.is_ok());
    assert!(scene_result.is_ok());
    assert!(parameter_result.is_ok_and(|result| result.is_ok()));
    assert!(loudness_result.is_ok());
    assert!(first_result.is_ok());
    assert!(
        first_result
            .as_ref()
            .is_ok_and(|cycle| cycle.mixer_parameter_state().is_some())
    );
    assert!(loudness_presentation.is_some());
    assert!(
        second_result
            .as_ref()
            .is_ok_and(|cycle| cycle.mixer_parameter_state().is_none())
    );
    assert_eq!(heap_operations, 0);
}
