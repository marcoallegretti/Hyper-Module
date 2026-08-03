mod common;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{
    InterleavedStereoCallback, LinearGain, LoudnessMeasurementCommand, LoudnessMeasurementState,
    MAX_PROCESS_FRAMES, Pan, StereoCallbackError, StereoOutput,
};
use hyper_media_cache::PreparedSceneAvailability;
use hyper_mixer_runtime::{
    EbuLoudnessPresentationConfig, EbuLoudnessScale, FlatMainRuntime, GraphMainRuntime,
    GraphTransportRuntime, LocateState, LoudnessCommandSendError, LoudnessCommandSender,
    LoudnessDisplayUnit, MAX_QUEUED_LOUDNESS_COMMANDS, MAX_QUEUED_MIXER_PARAMETER_COMMANDS,
    MAX_QUEUED_OUTPUT_METER_SNAPSHOTS, MAX_QUEUED_TRANSPORT_COMMANDS, MixerParameterCommand,
    MixerParameterCommandSendError, MixerParameterResolveError, MixerParameterStateRestoreError,
    MixerParameterTargetError, OutputMeterPublication, OutputMeterReceiver, OutputMeterSender,
    OutputMeterSnapshot, PreparedPlaybackMainRuntime, PreparedPlaybackTransportRuntime,
    RuntimeCreationError, RuntimeProcessError, RuntimeProjectComponent, SamplePeakBallisticsConfig,
    StereoSamplePeakBallistics, TransportCommand, TransportCommandSendError, TransportCycle,
    TransportOutputCreationError, TransportOutputProcessor, TransportState,
    loudness_command_channel, output_meter_channel,
};

use common::{fixture, graph_fixture, playback_fixture};

const GRAPH_SOURCE_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336421";
const GRAPH_GROUP_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336423";
const GRAPH_SEND_A_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336433";

fn assert_constant(samples: &[f32], expected: f32) {
    assert!(
        samples
            .iter()
            .all(|sample| (*sample - expected).abs() <= f32::EPSILON)
    );
}

fn assert_samples(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert!(
            (*actual - *expected).abs() <= 1.0e-6,
            "expected {expected}, got {actual}"
        );
    }
}

fn queue_loudness_restart(sender: &mut LoudnessCommandSender) {
    sender
        .try_send(LoudnessMeasurementCommand::SetState(
            LoudnessMeasurementState::Standby,
        ))
        .unwrap();
    sender.try_send(LoudnessMeasurementCommand::Reset).unwrap();
    sender
        .try_send(LoudnessMeasurementCommand::SetState(
            LoudnessMeasurementState::Running,
        ))
        .unwrap();
}

fn assert_started_loudness(snapshot: OutputMeterSnapshot, frame_count: u64) {
    assert_eq!(snapshot.applied_loudness_command_count(), 3);
    assert!(!snapshot.loudness_command_sender_disconnected());
    assert_eq!(
        snapshot.meter().loudness().state(),
        LoudnessMeasurementState::Running
    );
    assert_eq!(
        snapshot.meter().loudness().programme_frame_count(),
        frame_count
    );
}

fn assert_meter_delivery_and_backpressure(
    transport: &mut GraphTransportRuntime<'_, '_, '_>,
    callback: &mut InterleavedStereoCallback,
    scratch: &mut [RenderScratchSlot<'_>],
    interleaved: &mut [f32],
    meter_sender: &mut OutputMeterSender,
    mut meter_receiver: OutputMeterReceiver,
    first_snapshot: OutputMeterSnapshot,
) {
    let first_read = meter_receiver.read_latest();
    assert_eq!(first_read.snapshot(), Some(first_snapshot));
    assert_eq!(first_read.coalesced_snapshot_count(), 0);
    assert_eq!(first_read.dropped_snapshot_count(), 0);
    assert!(!first_read.sender_disconnected());
    assert_eq!(
        first_snapshot.processed_frame_end(),
        u64::try_from(first_snapshot.meter().frame_count()).unwrap()
    );

    for _ in 0..MAX_QUEUED_OUTPUT_METER_SNAPSHOTS {
        let report = transport
            .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
            .unwrap();
        assert_eq!(report.publication(), OutputMeterPublication::Published);
    }
    let full = transport
        .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
        .unwrap();
    assert_eq!(full.publication(), OutputMeterPublication::QueueFull);
    let mut ballistics = StereoSamplePeakBallistics::new(
        SamplePeakBallisticsConfig::try_new(48_000, -90.0, 13.3, 0).unwrap(),
    );
    let ballistic_read = meter_receiver.read_ballistics(&mut ballistics);
    let latest = ballistic_read.latest();
    assert!(latest.snapshot().is_some());
    assert_eq!(
        latest.coalesced_snapshot_count(),
        MAX_QUEUED_OUTPUT_METER_SNAPSHOTS - 1
    );
    assert_eq!(latest.dropped_snapshot_count(), 0);
    assert!(!latest.sender_disconnected());
    assert!(!ballistic_read.ballistics_reset());
    assert_eq!(
        ballistic_read.ballistics().unwrap().processed_frame_end(),
        latest.snapshot().unwrap().processed_frame_end()
    );
    let loudness = latest
        .snapshot()
        .unwrap()
        .present_loudness(EbuLoudnessPresentationConfig::new(
            EbuLoudnessScale::Plus18,
            LoudnessDisplayUnit::RelativeLu,
        ));
    assert_eq!(loudness.config().scale(), EbuLoudnessScale::Plus18);
    assert_eq!(loudness.config().unit(), LoudnessDisplayUnit::RelativeLu);
    assert_eq!(
        loudness.state(),
        latest.snapshot().unwrap().meter().loudness().state()
    );
    assert_eq!(
        loudness.programme_frame_count(),
        latest
            .snapshot()
            .unwrap()
            .meter()
            .loudness()
            .programme_frame_count()
    );
    let frame_count = u64::try_from(full.snapshot().meter().frame_count()).unwrap();
    assert_eq!(
        full.snapshot().processed_frame_end(),
        latest
            .snapshot()
            .unwrap()
            .processed_frame_end()
            .saturating_add(frame_count)
    );

    let recovered = transport
        .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
        .unwrap();
    assert_eq!(recovered.publication(), OutputMeterPublication::Published);
    assert_eq!(recovered.snapshot().dropped_before(), 1);
    assert_eq!(
        recovered.snapshot().processed_frame_end(),
        full.snapshot()
            .processed_frame_end()
            .saturating_add(frame_count)
    );
    let recovery_ballistics = meter_receiver.read_ballistics(&mut ballistics);
    let recovery_read = recovery_ballistics.latest();
    assert_eq!(recovery_read.snapshot(), Some(recovered.snapshot()));
    assert_eq!(recovery_read.dropped_snapshot_count(), 1);
    assert_eq!(
        recovery_ballistics
            .ballistics()
            .unwrap()
            .processed_frame_end(),
        recovered.snapshot().processed_frame_end()
    );
    assert!(!recovery_ballistics.ballistics_reset());

    drop(meter_receiver);
    let disconnected = transport
        .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
        .unwrap();
    assert_eq!(
        disconnected.publication(),
        OutputMeterPublication::ReceiverDisconnected
    );
}

fn render_automated_graph(
    runtime: &mut GraphMainRuntime<'_, '_, '_>,
    block_size: usize,
) -> ([f32; 100], [f32; 100]) {
    let mut scratch_left = [[9.0_f32; 100]; 5];
    let mut scratch_right = [[9.0_f32; 100]; 5];
    let mut output_left = [9.0_f32; 100];
    let mut output_right = [9.0_f32; 100];
    let mut start = 0;
    while start < output_left.len() {
        let end = start.saturating_add(block_size).min(output_left.len());
        let frame_count = end - start;
        let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
        let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
        let mut scratch = [
            RenderScratchSlot::new(&mut left_0[..frame_count], &mut right_0[..frame_count]),
            RenderScratchSlot::new(&mut left_1[..frame_count], &mut right_1[..frame_count]),
            RenderScratchSlot::new(&mut left_2[..frame_count], &mut right_2[..frame_count]),
            RenderScratchSlot::new(&mut left_3[..frame_count], &mut right_3[..frame_count]),
            RenderScratchSlot::new(&mut left_4[..frame_count], &mut right_4[..frame_count]),
        ];
        runtime
            .process_window(
                start,
                &mut scratch,
                StereoOutput {
                    left: &mut output_left[start..end],
                    right: &mut output_right[start..end],
                },
            )
            .unwrap();
        start = end;
    }
    (output_left, output_right)
}

fn process_transport<const FRAMES: usize>(
    transport: &mut GraphTransportRuntime<'_, '_, '_>,
    output_left: &mut [f32; FRAMES],
    output_right: &mut [f32; FRAMES],
) -> Result<TransportCycle, RuntimeProcessError> {
    let mut scratch_left = [[9.0_f32; FRAMES]; 5];
    let mut scratch_right = [[9.0_f32; FRAMES]; 5];
    let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
    let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
    let mut scratch = [
        RenderScratchSlot::new(left_0, right_0),
        RenderScratchSlot::new(left_1, right_1),
        RenderScratchSlot::new(left_2, right_2),
        RenderScratchSlot::new(left_3, right_3),
        RenderScratchSlot::new(left_4, right_4),
    ];
    transport.process_cycle(
        &mut scratch,
        StereoOutput {
            left: output_left,
            right: output_right,
        },
    )
}

#[derive(Clone, Copy, Debug)]
enum LiveMixerCommandKind {
    ChannelGain,
    ChannelPan,
    ChannelMute,
    ChannelSolo,
    SendGain,
    SendPan,
    SendEnabled,
    MainGain,
    MainMute,
}

fn process_one_live_mixer_command(kind: LiveMixerCommandKind) -> (f32, f32) {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 1).unwrap();
    let (mut transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let source = parameter_sender
        .resolve_channel(GRAPH_SOURCE_TRACK_ID)
        .unwrap();
    let send_a = parameter_sender.resolve_send(GRAPH_SEND_A_ID).unwrap();
    let command = match kind {
        LiveMixerCommandKind::ChannelGain => MixerParameterCommand::ChannelGain {
            channel: source,
            gain: LinearGain::SILENCE,
        },
        LiveMixerCommandKind::ChannelPan => MixerParameterCommand::ChannelPan {
            channel: source,
            pan: Pan::CENTER,
        },
        LiveMixerCommandKind::ChannelMute => MixerParameterCommand::ChannelMute {
            channel: source,
            muted: true,
        },
        LiveMixerCommandKind::ChannelSolo => MixerParameterCommand::ChannelSolo {
            channel: source,
            solo: false,
        },
        LiveMixerCommandKind::SendGain => MixerParameterCommand::SendGain {
            send: send_a,
            gain: LinearGain::SILENCE,
        },
        LiveMixerCommandKind::SendPan => MixerParameterCommand::SendPan {
            send: send_a,
            pan: Pan::HARD_LEFT,
        },
        LiveMixerCommandKind::SendEnabled => MixerParameterCommand::SendEnabled {
            send: send_a,
            enabled: false,
        },
        LiveMixerCommandKind::MainGain => MixerParameterCommand::MainGain {
            gain: LinearGain::new(0.5).unwrap(),
        },
        LiveMixerCommandKind::MainMute => MixerParameterCommand::MainMute { muted: true },
    };
    transport_sender
        .try_send(TransportCommand::Locate {
            frame: 25,
            state: LocateState::Rolling,
        })
        .unwrap();
    parameter_sender.try_send(command).unwrap();
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];
    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(cycle.applied_mixer_parameter_command_count(), 1);
    (left[0], right[0])
}

#[test]
fn verified_documents_render_through_fixed_main_mixer_slots() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [9.0_f32; 64];
    let mut scratch_right = [9.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [9.0_f32; 64];
    let mut output_right = [9.0_f32; 64];

    let rendered = runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    assert_eq!(runtime.slot_count(), 1);
    assert_eq!(rendered.active_slot_count, 1);
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
fn combined_transport_renders_each_track_authority_and_reconstructs_scene_state_on_locate() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport.apply_command(TransportCommand::Start).unwrap();

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [9.0_f32; 100];
    let mut output_right = [9.0_f32; 100];
    let first = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    let expected_scene: Vec<f32> = (0..100)
        .map(|frame| {
            if (25..75).contains(&frame) {
                fixture.scene_samples[frame - 25]
            } else {
                0.0
            }
        })
        .collect();
    assert_eq!(first.active_slot_count(), 2);
    assert_eq!(transport.position_frame(), 100);
    assert_samples(&output_left, &fixture.arrangement_samples);
    assert_samples(&output_right, &expected_scene);

    transport
        .apply_command(TransportCommand::Locate {
            frame: 25,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut arrangement_left = [9.0_f32; 50];
    let mut arrangement_right = [9.0_f32; 50];
    let mut scene_left = [9.0_f32; 50];
    let mut scene_right = [9.0_f32; 50];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [9.0_f32; 50];
    let mut output_right = [9.0_f32; 50];
    let located = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    assert_eq!(located.start_frame(), 25);
    assert_eq!(located.active_slot_count(), 2);
    assert_eq!(transport.position_frame(), 75);
    assert_samples(&output_left, &fixture.arrangement_samples[25..75]);
    assert_samples(&output_right, &fixture.scene_samples[..50]);
}

#[test]
fn queued_interactive_scene_launch_reaches_the_mixer_and_stop_takes_ownership() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 50)
            .unwrap();
    let (mut sender, mut transport) = PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    assert_eq!(sender.scene_count(), 2);
    assert_eq!(
        sender.scene_availability(0),
        Some(PreparedSceneAvailability::Ready)
    );
    assert_eq!(
        sender.scene_availability(1),
        Some(PreparedSceneAvailability::Ready)
    );
    assert_eq!(sender.scene_availability(2), None);
    sender
        .try_send(TransportCommand::Locate {
            frame: 100,
            state: LocateState::Rolling,
        })
        .unwrap();
    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();

    let mut arrangement_left = [9.0_f32; 50];
    let mut arrangement_right = [9.0_f32; 50];
    let mut scene_left = [9.0_f32; 50];
    let mut scene_right = [9.0_f32; 50];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [9.0_f32; 50];
    let mut output_right = [9.0_f32; 50];
    let launched = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(launched.applied_command_count(), 2);
    assert_samples(&output_left, &[0.0; 50]);
    assert_samples(&output_right, &fixture.scene_samples[..50]);

    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 1 })
        .unwrap();
    transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_samples(&output_left, &[0.0; 50]);
    assert_samples(&output_right, &[0.0; 50]);
}

#[test]
fn failed_cycle_restores_the_prior_interactive_scene_state() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 25)
            .unwrap();
    let (mut sender, mut transport) = PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    sender
        .try_send(TransportCommand::Locate {
            frame: 100,
            state: LocateState::Rolling,
        })
        .unwrap();
    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();

    let mut arrangement_left = [0.0_f32; 25];
    let mut arrangement_right = [0.0_f32; 25];
    let mut scene_left = [0.0_f32; 25];
    let mut scene_right = [0.0_f32; 25];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [0.0_f32; 25];
    let mut output_right = [0.0_f32; 25];
    transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 1 })
        .unwrap();
    let mut incomplete_left = [0.0_f32; 25];
    let mut incomplete_right = [0.0_f32; 25];
    let mut incomplete = [RenderScratchSlot::new(
        &mut incomplete_left,
        &mut incomplete_right,
    )];
    assert!(
        transport
            .process_cycle(
                &mut incomplete,
                StereoOutput {
                    left: &mut output_left,
                    right: &mut output_right,
                },
            )
            .is_err()
    );
    assert_eq!(transport.position_frame(), 125);

    transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_samples(&output_right, &fixture.scene_samples[25..50]);
}

#[test]
fn transport_output_processor_reuses_maximum_scratch_for_variable_quanta() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let (mut command_sender, mut transport) =
        PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    let mut arrangement_left = [9.0_f32; 64];
    let mut arrangement_right = [9.0_f32; 64];
    let mut scene_left = [9.0_f32; 64];
    let mut scene_right = [9.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let (mut meter_sender, mut meter_receiver) = output_meter_channel();
    let (mut loudness_sender, loudness_receiver) = loudness_command_channel();
    let mut processor = TransportOutputProcessor::try_new(
        &mut transport,
        &mut scratch,
        &mut meter_sender,
        loudness_receiver,
        48_000,
    )
    .unwrap();

    queue_loudness_restart(&mut loudness_sender);
    command_sender.try_send(TransportCommand::Start).unwrap();
    let mut first = [9.0_f32; 100];
    let first_cycle = processor.process(&mut first).unwrap();
    assert_eq!(first_cycle.snapshot().cycle().start_frame(), 0);
    assert_eq!(first_cycle.snapshot().cycle().frame_count(), 50);
    assert_eq!(first_cycle.snapshot().cycle().active_slot_count(), 2);
    assert_started_loudness(first_cycle.snapshot(), 50);
    for (actual, expected) in first
        .chunks_exact(2)
        .map(|frame| frame[0])
        .zip(&fixture.arrangement_samples[..50])
    {
        assert!((actual - *expected).abs() <= f32::EPSILON);
    }
    assert!(
        first
            .chunks_exact(2)
            .take(25)
            .all(|frame| frame[1].abs() <= f32::EPSILON)
    );
    for (actual, expected) in first
        .chunks_exact(2)
        .skip(25)
        .map(|frame| frame[1])
        .zip(&fixture.scene_samples[..25])
    {
        assert!((actual - *expected).abs() <= f32::EPSILON);
    }
    assert_eq!(
        meter_receiver.read_latest().snapshot(),
        Some(first_cycle.snapshot())
    );

    drop(loudness_sender);
    let mut second = [9.0_f32; 50];
    let second_cycle = processor.process(&mut second).unwrap();
    assert_eq!(second_cycle.snapshot().cycle().start_frame(), 50);
    assert_eq!(second_cycle.snapshot().cycle().frame_count(), 25);
    assert_eq!(second_cycle.snapshot().applied_loudness_command_count(), 0);
    assert!(
        second_cycle
            .snapshot()
            .loudness_command_sender_disconnected()
    );
    for (actual, expected) in second
        .chunks_exact(2)
        .map(|frame| frame[0])
        .zip(&fixture.arrangement_samples[50..75])
    {
        assert!((actual - *expected).abs() <= f32::EPSILON);
    }
    for (actual, expected) in second
        .chunks_exact(2)
        .map(|frame| frame[1])
        .zip(&fixture.scene_samples[25..50])
    {
        assert!((actual - *expected).abs() <= f32::EPSILON);
    }

    command_sender.try_send(TransportCommand::Stop).unwrap();
    let mut stopped = [9.0_f32; 32];
    let stopped_cycle = processor.process(&mut stopped).unwrap();
    assert_eq!(stopped_cycle.snapshot().cycle().start_frame(), 75);
    assert_eq!(
        stopped_cycle.snapshot().cycle().state(),
        TransportState::Stopped
    );
    assert!(stopped.iter().all(|sample| sample.abs() <= f32::EPSILON));
    drop(processor);
    assert_eq!(scratch[0].left().len(), 25);
    assert_eq!(scratch[0].right().len(), 25);
    assert_eq!(scratch[0].left_capacity_frames(), 64);
    assert_eq!(scratch[0].right_capacity_frames(), 64);
}

#[test]
fn final_post_mix_is_included_in_output_metering() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let (_command_sender, mut transport) =
        PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    let mut arrangement_left = [0.0_f32; 64];
    let mut arrangement_right = [0.0_f32; 64];
    let mut scene_left = [0.0_f32; 64];
    let mut scene_right = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let (mut meter_sender, mut meter_receiver) = output_meter_channel();
    let (_loudness_sender, loudness_receiver) = loudness_command_channel();
    let mut processor = TransportOutputProcessor::try_new(
        &mut transport,
        &mut scratch,
        &mut meter_sender,
        loudness_receiver,
        48_000,
    )
    .unwrap();
    let mut output = [9.0_f32; 128];
    let mut cycle = None;
    for _ in 0..300 {
        let processed = processor
            .process_with_post_mix(&mut output, |post| {
                post.left.fill(0.25);
                post.right.fill(-0.5);
            })
            .unwrap();
        assert_eq!(
            meter_receiver.read_latest().snapshot(),
            Some(processed.snapshot())
        );
        cycle = Some(processed);
    }
    let cycle = cycle.unwrap();

    assert!(output.chunks_exact(2).all(|frame| frame == [0.25, -0.5]));
    let meter = cycle.snapshot().meter();
    assert_eq!(meter.left().sample_peak(), Some(0.25));
    assert_eq!(meter.right().sample_peak(), Some(0.5));
    assert!(meter.left().true_peak().is_some_and(|peak| peak >= 0.25));
    assert!(meter.right().true_peak().is_some_and(|peak| peak >= 0.5));
    assert_eq!(meter.loudness().programme_frame_count(), 300_u64 * 64);
    assert!(
        meter
            .loudness()
            .momentary_lufs()
            .is_some_and(f32::is_finite)
    );
    assert!(
        meter
            .loudness()
            .maximum_true_peak_linear()
            .is_some_and(|peak| peak >= 0.5)
    );
}

#[test]
fn transport_output_processor_rejects_incomplete_scratch_before_processing() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    let (mut meter_sender, _meter_receiver) = output_meter_channel();
    let mut no_scratch = [];

    assert_eq!(
        {
            let (_loudness_sender, loudness_receiver) = loudness_command_channel();
            TransportOutputProcessor::try_new(
                &mut transport,
                &mut no_scratch,
                &mut meter_sender,
                loudness_receiver,
                48_000,
            )
            .unwrap_err()
        },
        TransportOutputCreationError::ScratchSlotCountMismatch {
            expected: 2,
            actual: 0
        }
    );

    let mut left_short = [0.0_f32; 63];
    let mut right_full = [0.0_f32; 64];
    let mut left_full = [0.0_f32; 64];
    let mut right_full_2 = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut left_short, &mut right_full),
        RenderScratchSlot::new(&mut left_full, &mut right_full_2),
    ];
    assert_eq!(
        {
            let (_loudness_sender, loudness_receiver) = loudness_command_channel();
            TransportOutputProcessor::try_new(
                &mut transport,
                &mut scratch,
                &mut meter_sender,
                loudness_receiver,
                48_000,
            )
            .unwrap_err()
        },
        TransportOutputCreationError::ScratchCapacity {
            slot: 0,
            required: 64,
            left: 63,
            right: 64
        }
    );
}

#[test]
fn transport_output_processor_is_fail_silent_before_transport_mutation() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let (mut command_sender, mut transport) =
        PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    let mut arrangement_left = [9.0_f32; 64];
    let mut arrangement_right = [9.0_f32; 64];
    let mut scene_left = [9.0_f32; 64];
    let mut scene_right = [9.0_f32; 64];
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
    command_sender.try_send(TransportCommand::Start).unwrap();
    loudness_sender
        .try_send(LoudnessMeasurementCommand::Reset)
        .unwrap();

    let mut malformed = [9.0_f32; 3];
    assert_eq!(
        processor.process(&mut malformed),
        Err(StereoCallbackError::SampleCountNotStereoAligned { samples: 3 })
    );
    assert!(malformed.iter().all(|sample| sample.abs() <= f32::EPSILON));

    let mut first_valid = [0.0_f32; 16];
    let first_cycle = processor.process(&mut first_valid).unwrap();
    assert_eq!(first_cycle.snapshot().cycle().start_frame(), 0);
    assert_eq!(first_cycle.snapshot().cycle().applied_command_count(), 1);
    assert_eq!(first_cycle.snapshot().applied_loudness_command_count(), 1);

    let mut oversized = [9.0_f32; 130];
    assert_eq!(
        processor.process(&mut oversized),
        Err(StereoCallbackError::BlockTooLarge {
            actual: 65,
            maximum: 64
        })
    );
    assert!(oversized.iter().all(|sample| sample.abs() <= f32::EPSILON));

    let mut after_rejection = [0.0_f32; 16];
    let next_cycle = processor.process(&mut after_rejection).unwrap();
    assert_eq!(next_cycle.snapshot().cycle().start_frame(), 8);
}

#[test]
fn prepared_playback_rejects_a_mixer_from_another_project_before_track_mapping() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let unrelated = graph_fixture();

    assert_eq!(
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &unrelated.mixer, 64)
            .unwrap_err(),
        RuntimeCreationError::ProjectMismatch {
            component: RuntimeProjectComponent::Mixer
        }
    );
}

#[test]
fn prepared_playback_rejects_automation_from_another_project() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let unrelated = graph_fixture();

    assert_eq!(
        PreparedPlaybackMainRuntime::try_new_prepared_playback_automated(
            &prepared,
            &fixture.mixer,
            unrelated.automation,
            64,
        )
        .unwrap_err(),
        RuntimeCreationError::ProjectMismatch {
            component: RuntimeProjectComponent::Automation
        }
    );
}

#[test]
fn output_shape_failure_precedes_scratch_or_output_mutation() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [7.0_f32; 64];
    let mut scratch_right = [7.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [8.0_f32; 64];
    let mut output_right = [8.0_f32; 63];

    assert_eq!(
        runtime.process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        ),
        Err(RuntimeProcessError::OutputLengthMismatch {
            left: 64,
            right: 63,
        })
    );
    assert_eq!(scratch[0].left(), &[7.0; 64]);
    assert_eq!(scratch[0].right(), &[7.0; 64]);
    assert!(
        output_left
            .iter()
            .all(|sample| (*sample - 8.0).abs() <= f32::EPSILON)
    );
    assert!(
        output_right
            .iter()
            .all(|sample| (*sample - 8.0).abs() <= f32::EPSILON)
    );
}

#[test]
fn verified_graph_executes_pre_post_returns_groups_solo_and_main() {
    let fixture = graph_fixture();
    assert_eq!(fixture.mixer.processing_order(), &[0, 1, 3, 4, 2]);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 64).unwrap();
    let mut left_0 = [9.0_f32; 64];
    let mut right_0 = [9.0_f32; 64];
    let mut left_1 = [9.0_f32; 64];
    let mut right_1 = [9.0_f32; 64];
    let mut left_2 = [9.0_f32; 64];
    let mut right_2 = [9.0_f32; 64];
    let mut left_3 = [9.0_f32; 64];
    let mut right_3 = [9.0_f32; 64];
    let mut left_4 = [9.0_f32; 64];
    let mut right_4 = [9.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut left_0, &mut right_0),
        RenderScratchSlot::new(&mut left_1, &mut right_1),
        RenderScratchSlot::new(&mut left_2, &mut right_2),
        RenderScratchSlot::new(&mut left_3, &mut right_3),
        RenderScratchSlot::new(&mut left_4, &mut right_4),
    ];
    let mut output_left = [9.0_f32; 64];
    let mut output_right = [9.0_f32; 64];

    let rendered = runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    assert_eq!(runtime.slot_count(), 5);
    assert_eq!(runtime.max_block_frames(), 64);
    assert_eq!(rendered.active_slot_count, 2);
    for ((left, right), source) in output_left
        .iter()
        .zip(&output_right)
        .zip(&fixture.samples[..64])
    {
        assert!((*left - *source * 0.375).abs() <= f32::EPSILON);
        assert!((*right - *source * 0.25).abs() <= f32::EPSILON);
    }
}

#[test]
fn automated_graph_is_sample_exact_and_block_size_independent() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut whole = GraphMainRuntime::try_new_automated(
        &prepared,
        &fixture.mixer,
        fixture.automation.clone(),
        100,
    )
    .unwrap();
    let mut split =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 100)
            .unwrap();
    let (whole_left, whole_right) = render_automated_graph(&mut whole, 100);
    let (split_left, split_right) = render_automated_graph(&mut split, 37);

    for (whole, split) in whole_left.iter().zip(split_left) {
        assert!((*whole - split).abs() <= f32::EPSILON);
    }
    for (whole, split) in whole_right.iter().zip(split_right) {
        assert!((*whole - split).abs() <= f32::EPSILON);
    }
    assert!((whole_left[25] - 0.223_861_44).abs() <= f32::EPSILON);
    assert!((whole_right[25] - 0.176_986_44).abs() <= f32::EPSILON);
    assert!(whole_left[50].abs() <= f32::EPSILON);
    assert!(whole_right[50].abs() <= f32::EPSILON);
    assert!((whole_left[75] - 0.421_875).abs() <= f32::EPSILON);
    assert!((whole_right[75] - 0.187_5).abs() <= f32::EPSILON);
}

#[test]
fn transport_owns_stopped_rolling_and_located_callback_windows() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 8)
            .unwrap();
    let mut transport = GraphTransportRuntime::new(runtime);
    let mut left = [9.0_f32; 8];
    let mut right = [9.0_f32; 8];

    let stopped = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(stopped.state(), TransportState::Stopped);
    assert_eq!(stopped.start_frame(), 0);
    assert_eq!(stopped.position_frame(), 0);
    assert_eq!(stopped.frame_count(), 8);
    assert_eq!(stopped.active_slot_count(), 0);
    assert_eq!(transport.position_frame(), 0);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    let mut empty_left = [];
    let mut empty_right = [];
    let empty_stopped =
        process_transport(&mut transport, &mut empty_left, &mut empty_right).unwrap();
    assert_eq!(empty_stopped.frame_count(), 0);
    assert_eq!(transport.position_frame(), 0);

    transport.apply_command(TransportCommand::Start).unwrap();
    let empty_rolling =
        process_transport(&mut transport, &mut empty_left, &mut empty_right).unwrap();
    assert_eq!(empty_rolling.state(), TransportState::Rolling);
    assert_eq!(empty_rolling.position_frame(), 0);
    assert_eq!(transport.position_frame(), 0);

    let rolling = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(rolling.state(), TransportState::Rolling);
    assert_eq!(rolling.start_frame(), 0);
    assert_eq!(rolling.position_frame(), 8);
    assert_eq!(rolling.active_slot_count(), 2);
    assert_eq!(transport.position_frame(), 8);
    assert!(left[7] > 0.0);

    let consecutive = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(consecutive.start_frame(), 8);
    assert_eq!(consecutive.position_frame(), 16);
    assert_eq!(transport.position_frame(), 16);

    transport.apply_command(TransportCommand::Stop).unwrap();
    left.fill(9.0);
    right.fill(9.0);
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(transport.position_frame(), 16);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    transport
        .apply_command(TransportCommand::Locate {
            frame: 25,
            state: LocateState::Preserve,
        })
        .unwrap();
    assert_eq!(transport.state(), TransportState::Stopped);
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(transport.position_frame(), 25);

    transport
        .apply_command(TransportCommand::Locate {
            frame: 25,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut seek_left = [9.0_f32; 1];
    let mut seek_right = [9.0_f32; 1];
    let sought = process_transport(&mut transport, &mut seek_left, &mut seek_right).unwrap();
    assert_eq!(sought.start_frame(), 25);
    assert_eq!(sought.position_frame(), 26);
    assert_eq!(transport.position_frame(), 26);
    assert!((seek_left[0] - 0.223_861_44).abs() <= f32::EPSILON);
    assert!((seek_right[0] - 0.176_986_44).abs() <= f32::EPSILON);

    transport
        .apply_command(TransportCommand::Locate {
            frame: 40,
            state: LocateState::Stopped,
        })
        .unwrap();
    process_transport(&mut transport, &mut seek_left, &mut seek_right).unwrap();
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 40);
    assert_constant(&seek_left, 0.0);
    assert_constant(&seek_right, 0.0);

    transport
        .apply_command(TransportCommand::Locate {
            frame: usize::MAX - 1,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut overflow_left = [9.0_f32; 2];
    let mut overflow_right = [9.0_f32; 2];
    assert_eq!(
        process_transport(&mut transport, &mut overflow_left, &mut overflow_right),
        Err(RuntimeProcessError::FrameRangeOverflow)
    );
    assert_eq!(transport.position_frame(), usize::MAX - 1);
    assert_constant(&overflow_left, 9.0);
    assert_constant(&overflow_right, 9.0);
}

#[test]
fn failed_transport_cycle_preserves_position_and_output() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let mut transport = GraphTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::Locate {
            frame: 12,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut scratch_left = [[7.0_f32; 2]; 5];
    let mut scratch_right = [[7.0_f32; 2]; 5];
    let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
    let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
    let mut scratch = [
        RenderScratchSlot::new(left_0, right_0),
        RenderScratchSlot::new(left_1, right_1),
        RenderScratchSlot::new(left_2, right_2),
        RenderScratchSlot::new(left_3, right_3),
        RenderScratchSlot::new(left_4, right_4),
    ];
    let mut output_left = [8.0_f32; 2];
    let mut output_right = [8.0_f32; 1];

    assert_eq!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        ),
        Err(RuntimeProcessError::OutputLengthMismatch { left: 2, right: 1 })
    );
    assert_eq!(transport.position_frame(), 12);
    assert_constant(&output_left, 8.0);
    assert_constant(&output_right, 8.0);
}

#[test]
fn queued_transport_commands_preserve_fifo_order_across_threads() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 8)
            .unwrap();
    let (sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    let mut sender = std::thread::spawn(move || {
        let mut sender = sender;
        assert_eq!(sender.capacity(), MAX_QUEUED_TRANSPORT_COMMANDS);
        sender
            .try_send(TransportCommand::Locate {
                frame: 25,
                state: LocateState::Rolling,
            })
            .unwrap();
        sender.try_send(TransportCommand::Stop).unwrap();
        sender
            .try_send(TransportCommand::Locate {
                frame: 40,
                state: LocateState::Preserve,
            })
            .unwrap();
        sender.try_send(TransportCommand::Start).unwrap();
        sender
    })
    .join()
    .unwrap();
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();

    assert_eq!(cycle.applied_command_count(), 4);
    assert!(!cycle.command_sender_disconnected());
    assert_eq!(cycle.state(), TransportState::Rolling);
    assert_eq!(cycle.start_frame(), 40);
    assert_eq!(transport.position_frame(), 41);
    assert!(left[0] > 0.0);
    assert!(right[0] > 0.0);
    sender.try_send(TransportCommand::Stop).unwrap();
}

#[test]
fn live_mixer_command_queue_validates_targets_backpressure_and_disconnection() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, mut sender, transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let source = sender.resolve_channel(GRAPH_SOURCE_TRACK_ID).unwrap();
    let group = sender.resolve_channel(GRAPH_GROUP_TRACK_ID).unwrap();
    let send_a = sender.resolve_send(GRAPH_SEND_A_ID).unwrap();
    assert_eq!(
        sender.resolve_channel("missing"),
        Err(MixerParameterResolveError::ChannelNotFound)
    );
    assert_eq!(
        sender.resolve_send("missing"),
        Err(MixerParameterResolveError::SendNotFound)
    );

    let foreign_fixture = graph_fixture();
    let foreign_prepared = foreign_fixture
        .cache
        .prepare_schedule(&foreign_fixture.schedule)
        .unwrap();
    let foreign_runtime =
        GraphMainRuntime::try_new(&foreign_prepared, &foreign_fixture.mixer, 8).unwrap();
    let (_foreign_transport_sender, foreign_sender, foreign_transport) =
        GraphTransportRuntime::with_command_queues(foreign_runtime);
    let foreign_channel = foreign_sender
        .resolve_channel(GRAPH_SOURCE_TRACK_ID)
        .unwrap();
    let foreign_send = foreign_sender.resolve_send(GRAPH_SEND_A_ID).unwrap();
    assert_ne!(source, foreign_channel);
    assert_ne!(send_a, foreign_send);

    let invalid_channel = MixerParameterCommand::ChannelPan {
        channel: foreign_channel,
        pan: Pan::CENTER,
    };
    assert_eq!(
        sender.try_send(invalid_channel),
        Err(MixerParameterCommandSendError::InvalidTarget {
            command: invalid_channel,
            source: MixerParameterTargetError::ForeignChannel(foreign_channel),
        })
    );
    let invalid_send = MixerParameterCommand::SendGain {
        send: foreign_send,
        gain: LinearGain::UNITY,
    };
    assert_eq!(
        sender.try_send(invalid_send),
        Err(MixerParameterCommandSendError::InvalidTarget {
            command: invalid_send,
            source: MixerParameterTargetError::ForeignSend(foreign_send),
        })
    );
    let invalid_solo = MixerParameterCommand::ChannelSolo {
        channel: group,
        solo: true,
    };
    assert_eq!(
        sender.validate_command(invalid_solo),
        Err(MixerParameterTargetError::SoloUnavailable { channel: group })
    );
    assert_eq!(
        sender.try_send(invalid_solo),
        Err(MixerParameterCommandSendError::InvalidTarget {
            command: invalid_solo,
            source: MixerParameterTargetError::SoloUnavailable { channel: group },
        })
    );

    let valid = MixerParameterCommand::MainMute { muted: true };
    assert_eq!(sender.capacity(), MAX_QUEUED_MIXER_PARAMETER_COMMANDS);
    for _ in 0..MAX_QUEUED_MIXER_PARAMETER_COMMANDS {
        sender.try_send(valid).unwrap();
    }
    assert_eq!(
        sender.try_send(valid),
        Err(MixerParameterCommandSendError::Full(valid))
    );
    assert_eq!(
        sender.try_send(invalid_channel),
        Err(MixerParameterCommandSendError::InvalidTarget {
            command: invalid_channel,
            source: MixerParameterTargetError::ForeignChannel(foreign_channel),
        })
    );

    drop(transport);
    assert!(sender.is_disconnected());
    assert_eq!(
        sender.try_send(valid),
        Err(MixerParameterCommandSendError::Disconnected(valid))
    );
    assert_eq!(
        sender.try_send(invalid_channel),
        Err(MixerParameterCommandSendError::InvalidTarget {
            command: invalid_channel,
            source: MixerParameterTargetError::ForeignChannel(foreign_channel),
        })
    );
    drop(foreign_transport);
}

#[test]
fn live_mixer_targets_resolve_from_compiled_numeric_addresses() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, sender, _transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let source = sender.resolve_channel(GRAPH_SOURCE_TRACK_ID).unwrap();
    let send_a = sender.resolve_send(GRAPH_SEND_A_ID).unwrap();

    assert_eq!(sender.resolve_channel_index(0), Ok(source));
    assert_eq!(sender.resolve_send_address(0, 0), Ok(send_a));
    assert_eq!(sender.channel_index(source), Ok(0));
    assert_eq!(sender.send_address(send_a), Ok((0, 0)));
    assert_eq!(
        sender.resolve_channel_index(usize::MAX),
        Err(MixerParameterResolveError::ChannelNotFound)
    );
    assert_eq!(
        sender.resolve_send_address(0, usize::MAX),
        Err(MixerParameterResolveError::SendNotFound)
    );
    assert_eq!(
        sender.resolve_send_address(usize::MAX, 0),
        Err(MixerParameterResolveError::SendNotFound)
    );
}

#[test]
fn live_mixer_commands_apply_while_stopped_and_without_a_block_ramp() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let mut left = [9.0_f32; 8];
    let mut right = [9.0_f32; 8];

    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    let stopped = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(stopped.applied_mixer_parameter_command_count(), 1);
    assert!(!stopped.mixer_parameter_command_sender_disconnected());
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    transport_sender.try_send(TransportCommand::Start).unwrap();
    let muted = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(muted.applied_command_count(), 1);
    assert_eq!(muted.applied_mixer_parameter_command_count(), 0);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: false })
        .unwrap();
    let audible = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(audible.applied_mixer_parameter_command_count(), 2);
    for ((left, right), source) in left.iter().zip(right).zip(&fixture.samples[8..16]) {
        assert!((*left - source * 0.375).abs() <= f32::EPSILON);
        assert!((right - source * 0.25).abs() <= f32::EPSILON);
    }
}

#[test]
fn committed_mixer_snapshot_restores_exact_base_state_to_the_same_layout() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let source = parameter_sender
        .resolve_channel(GRAPH_SOURCE_TRACK_ID)
        .unwrap();
    let send_a = parameter_sender.resolve_send(GRAPH_SEND_A_ID).unwrap();
    let gain = LinearGain::new(0.75).unwrap();
    let pan = Pan::new(0.25).unwrap();
    let send_gain = LinearGain::new(0.625).unwrap();
    let send_pan = Pan::new(-0.375).unwrap();
    let main_gain = LinearGain::new(0.5).unwrap();
    for command in [
        MixerParameterCommand::ChannelGain {
            channel: source,
            gain,
        },
        MixerParameterCommand::ChannelPan {
            channel: source,
            pan,
        },
        MixerParameterCommand::ChannelMute {
            channel: source,
            muted: true,
        },
        MixerParameterCommand::ChannelSolo {
            channel: source,
            solo: true,
        },
        MixerParameterCommand::SendGain {
            send: send_a,
            gain: send_gain,
        },
        MixerParameterCommand::SendPan {
            send: send_a,
            pan: send_pan,
        },
        MixerParameterCommand::SendEnabled {
            send: send_a,
            enabled: false,
        },
        MixerParameterCommand::MainGain { gain: main_gain },
        MixerParameterCommand::MainMute { muted: true },
    ] {
        parameter_sender.try_send(command).unwrap();
    }
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(cycle.applied_mixer_parameter_command_count(), 9);
    let snapshot = transport.mixer_parameter_state();
    assert_eq!(snapshot.channel_count(), 5);
    let source_state = snapshot.channel(0).unwrap();
    assert_eq!(source_state.parameters().gain, gain);
    assert_eq!(source_state.parameters().pan, pan);
    assert!(source_state.muted());
    assert!(source_state.solo());
    assert_eq!(source_state.send_count(), 2);
    let send_state = source_state.send(0).unwrap();
    assert_eq!(send_state.parameters().gain, send_gain);
    assert_eq!(send_state.parameters().pan, send_pan);
    assert!(!send_state.enabled());
    assert_eq!(source_state.send(2), None);
    assert_eq!(snapshot.main_gain(), main_gain);
    assert!(snapshot.main_muted());
    assert_eq!(
        process_transport(&mut transport, &mut left, &mut right)
            .unwrap()
            .applied_mixer_parameter_command_count(),
        0
    );

    let replacement_runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_replacement_transport_sender, mut replacement_parameter_sender, mut replacement) =
        GraphTransportRuntime::with_command_queues(replacement_runtime);
    let replacement_source = replacement_parameter_sender
        .resolve_channel(GRAPH_SOURCE_TRACK_ID)
        .unwrap();
    assert_ne!(source, replacement_source);
    replacement
        .restore_mixer_parameter_state(&snapshot)
        .unwrap();
    replacement_parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: false })
        .unwrap();
    process_transport(&mut replacement, &mut left, &mut right).unwrap();
    let restored = replacement.mixer_parameter_state();
    assert_eq!(restored.channel(0), snapshot.channel(0));
    assert_eq!(restored.main_gain(), main_gain);
    assert!(!restored.main_muted());
}

#[test]
fn interleaved_cycle_carries_mixer_state_only_when_controls_changed() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 1).unwrap();
    let (_transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let mut callback = InterleavedStereoCallback::try_new(1, 48_000).unwrap();
    let mut scratch_left = [[0.0_f32; 1]; 5];
    let mut scratch_right = [[0.0_f32; 1]; 5];
    let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
    let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
    let mut scratch = [
        RenderScratchSlot::new(left_0, right_0),
        RenderScratchSlot::new(left_1, right_1),
        RenderScratchSlot::new(left_2, right_2),
        RenderScratchSlot::new(left_3, right_3),
        RenderScratchSlot::new(left_4, right_4),
    ];
    let (mut meter_sender, _meter_receiver) = output_meter_channel();
    let mut interleaved = [0.0_f32; 2];
    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();

    let changed = transport
        .process_interleaved_cycle(
            &mut callback,
            &mut scratch,
            &mut interleaved,
            &mut meter_sender,
        )
        .unwrap();
    assert!(changed.mixer_parameter_state().unwrap().main_muted());

    let unchanged = transport
        .process_interleaved_cycle(
            &mut callback,
            &mut scratch,
            &mut interleaved,
            &mut meter_sender,
        )
        .unwrap();
    assert_eq!(unchanged.mixer_parameter_state(), None);
}

#[test]
fn mixer_snapshot_layout_mismatch_is_rejected_without_partial_restoration() {
    let source_fixture = graph_fixture();
    let source_prepared = source_fixture
        .cache
        .prepare_schedule(&source_fixture.schedule)
        .unwrap();
    let source_runtime =
        GraphMainRuntime::try_new(&source_prepared, &source_fixture.mixer, 1).unwrap();
    let (_source_transport_sender, mut source_parameter_sender, mut source_transport) =
        GraphTransportRuntime::with_command_queues(source_runtime);
    source_parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    let mut source_left = [0.0_f32; 1];
    let mut source_right = [0.0_f32; 1];
    process_transport(&mut source_transport, &mut source_left, &mut source_right).unwrap();
    let foreign = source_transport.mixer_parameter_state();

    let target_fixture = playback_fixture();
    let target_graph = target_fixture.compile_playback();
    let target_prepared = target_fixture
        .cache
        .prepare_playback(&target_graph)
        .unwrap();
    let target_runtime = PreparedPlaybackMainRuntime::try_new_prepared_playback(
        &target_prepared,
        &target_fixture.mixer,
        1,
    )
    .unwrap();
    let (_target_transport_sender, mut target_parameter_sender, mut target_transport) =
        PreparedPlaybackTransportRuntime::with_command_queues(target_runtime);
    let mut target_left = [0.0_f32; 1];
    let mut target_right = [0.0_f32; 1];
    target_parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: false })
        .unwrap();
    target_transport
        .process_cycle(
            &mut [],
            StereoOutput {
                left: &mut target_left,
                right: &mut target_right,
            },
        )
        .unwrap();
    let baseline = target_transport.mixer_parameter_state();

    assert_eq!(
        target_transport.restore_mixer_parameter_state(&foreign),
        Err(MixerParameterStateRestoreError::LayoutMismatch)
    );
    target_parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: false })
        .unwrap();
    target_transport
        .process_cycle(
            &mut [],
            StereoOutput {
                left: &mut target_left,
                right: &mut target_right,
            },
        )
        .unwrap();
    let after_rejection = target_transport.mixer_parameter_state();
    assert_eq!(after_rejection, baseline);
}

#[test]
fn every_live_mixer_parameter_domain_changes_the_resolved_graph() {
    let commands = [
        LiveMixerCommandKind::ChannelGain,
        LiveMixerCommandKind::ChannelPan,
        LiveMixerCommandKind::ChannelMute,
        LiveMixerCommandKind::ChannelSolo,
        LiveMixerCommandKind::SendGain,
        LiveMixerCommandKind::SendPan,
        LiveMixerCommandKind::SendEnabled,
        LiveMixerCommandKind::MainGain,
        LiveMixerCommandKind::MainMute,
    ];
    let baseline = (0.25 * 0.375, 0.25 * 0.25);

    for command in commands {
        let actual = process_one_live_mixer_command(command);
        assert!(
            (actual.0 - baseline.0).abs() > f32::EPSILON
                || (actual.1 - baseline.1).abs() > f32::EPSILON,
            "{command:?} did not change the resolved graph output"
        );
    }
}

#[test]
fn compiled_automation_precedes_live_base_parameters_per_sample() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 25)
            .unwrap();
    let (mut transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    transport_sender
        .try_send(TransportCommand::Locate {
            frame: 25,
            state: LocateState::Rolling,
        })
        .unwrap();
    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    let mut left = [9.0_f32; 25];
    let mut right = [9.0_f32; 25];

    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(cycle.start_frame(), 25);
    assert_eq!(cycle.applied_mixer_parameter_command_count(), 1);
    assert!(left.iter().any(|sample| sample.abs() > f32::EPSILON));
    assert!(right.iter().any(|sample| sample.abs() > f32::EPSILON));

    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: false })
        .unwrap();
    let muted_by_automation = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(muted_by_automation.start_frame(), 50);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    let automation_released_mute =
        process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(automation_released_mute.start_frame(), 75);
    assert!(left.iter().any(|sample| sample.abs() > f32::EPSILON));
    assert!(right.iter().any(|sample| sample.abs() > f32::EPSILON));
}

#[test]
fn interleaved_callback_executes_the_transport_runtime() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 8)
            .unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    let mut callback = InterleavedStereoCallback::try_new(8, 48_000).unwrap();
    let (mut meter_sender, mut meter_receiver) = output_meter_channel();
    sender.try_send(TransportCommand::Start).unwrap();
    let mut scratch_left = [[9.0_f32; 2]; 5];
    let mut scratch_right = [[9.0_f32; 2]; 5];
    let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
    let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
    let mut scratch = [
        RenderScratchSlot::new(left_0, right_0),
        RenderScratchSlot::new(left_1, right_1),
        RenderScratchSlot::new(left_2, right_2),
        RenderScratchSlot::new(left_3, right_3),
        RenderScratchSlot::new(left_4, right_4),
    ];
    let mut interleaved = [9.0_f32; 4];
    let mut invalid_interleaved = [9.0_f32; 3];

    assert!(matches!(
        transport.process_interleaved_cycle(
            &mut callback,
            &mut scratch,
            &mut invalid_interleaved,
            &mut meter_sender,
        ),
        Err(StereoCallbackError::SampleCountNotStereoAligned { samples: 3 })
    ));
    assert_constant(&invalid_interleaved, 0.0);
    assert_eq!(meter_receiver.read_latest().snapshot(), None);

    let reported = transport
        .process_interleaved_cycle(
            &mut callback,
            &mut scratch,
            &mut interleaved,
            &mut meter_sender,
        )
        .unwrap();
    assert_eq!(reported.mixer_parameter_state(), None);
    let snapshot = reported.snapshot();
    let cycle = snapshot.cycle();
    let meter = snapshot.meter();

    assert_eq!(reported.publication(), OutputMeterPublication::Published);
    assert_eq!(cycle.applied_command_count(), 1);
    assert_eq!(cycle.state(), TransportState::Rolling);
    assert_eq!(cycle.start_frame(), 0);
    assert_eq!(cycle.frame_count(), 2);
    assert_eq!(transport.position_frame(), 2);
    assert!(
        interleaved
            .iter()
            .all(|sample| (*sample - 9.0).abs() > f32::EPSILON)
    );
    assert!(interleaved.iter().all(|sample| sample.is_finite()));
    assert!(interleaved.iter().any(|sample| sample.abs() > f32::EPSILON));
    let expected_left_peak = interleaved
        .chunks_exact(2)
        .map(|frame| frame[0].abs())
        .fold(0.0_f32, f32::max);
    let expected_right_peak = interleaved
        .chunks_exact(2)
        .map(|frame| frame[1].abs())
        .fold(0.0_f32, f32::max);
    assert_eq!(meter.frame_count(), 2);
    assert_eq!(meter.left().sample_peak(), Some(expected_left_peak));
    assert_eq!(meter.right().sample_peak(), Some(expected_right_peak));
    assert!(meter.left().true_peak().unwrap() >= expected_left_peak);
    assert!(meter.right().true_peak().unwrap() >= expected_right_peak);
    assert_eq!(meter.left().non_finite_sample_count(), 0);
    assert_eq!(meter.right().non_finite_sample_count(), 0);

    assert_meter_delivery_and_backpressure(
        &mut transport,
        &mut callback,
        &mut scratch,
        &mut interleaved,
        &mut meter_sender,
        meter_receiver,
        snapshot,
    );
}

#[test]
fn loudness_command_queue_reports_saturation_and_disconnection() {
    let (mut sender, receiver) = loudness_command_channel();
    for _ in 0..MAX_QUEUED_LOUDNESS_COMMANDS {
        sender.try_send(LoudnessMeasurementCommand::Reset).unwrap();
    }
    let command = LoudnessMeasurementCommand::SetState(LoudnessMeasurementState::Standby);
    let full = sender.try_send(command).unwrap_err();
    assert_eq!(full, LoudnessCommandSendError::Full(command));
    assert_eq!(full.command(), command);

    drop(receiver);
    assert!(sender.is_disconnected());
    let disconnected = sender.try_send(command).unwrap_err();
    assert_eq!(
        disconnected,
        LoudnessCommandSendError::Disconnected(command)
    );
    assert_eq!(disconnected.command(), command);
}

#[test]
fn transport_command_queue_reports_saturation_and_disconnection() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    for _ in 0..MAX_QUEUED_TRANSPORT_COMMANDS {
        sender.try_send(TransportCommand::Stop).unwrap();
    }
    let full = sender.try_send(TransportCommand::Start).unwrap_err();
    assert_eq!(
        full,
        TransportCommandSendError::Full(TransportCommand::Start)
    );
    assert_eq!(full.command(), TransportCommand::Start);
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let drained = process_transport(&mut transport, &mut left, &mut right).unwrap();

    assert_eq!(
        drained.applied_command_count(),
        MAX_QUEUED_TRANSPORT_COMMANDS
    );
    assert_eq!(drained.state(), TransportState::Stopped);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);
    sender.try_send(TransportCommand::Start).unwrap();
    let started = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(started.applied_command_count(), 1);
    assert_eq!(started.state(), TransportState::Rolling);

    drop(transport);
    assert!(sender.is_disconnected());
    assert_eq!(
        sender.try_send(TransportCommand::Stop),
        Err(TransportCommandSendError::Disconnected(
            TransportCommand::Stop
        ))
    );
}

#[test]
fn transport_reports_abandoned_sender_after_applying_pending_commands() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    sender.try_send(TransportCommand::Start).unwrap();
    drop(sender);
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();

    assert_eq!(cycle.applied_command_count(), 1);
    assert!(cycle.command_sender_disconnected());
    assert_eq!(cycle.state(), TransportState::Rolling);
}

#[test]
fn meter_receiver_reports_an_abandoned_sender_without_a_snapshot() {
    let (meter_sender, mut meter_receiver) = output_meter_channel();
    assert_eq!(meter_sender.capacity(), MAX_QUEUED_OUTPUT_METER_SNAPSHOTS);
    assert_eq!(meter_receiver.capacity(), MAX_QUEUED_OUTPUT_METER_SNAPSHOTS);
    drop(meter_sender);

    let latest = meter_receiver.read_latest();

    assert_eq!(latest.snapshot(), None);
    assert_eq!(latest.coalesced_snapshot_count(), 0);
    assert_eq!(latest.dropped_snapshot_count(), 0);
    assert!(latest.sender_disconnected());
}

#[test]
fn invalid_cycle_does_not_consume_queued_commands() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    sender.try_send(TransportCommand::Start).unwrap();
    let mut scratch = [];
    let mut invalid_left = [8.0_f32; 2];
    let mut invalid_right = [8.0_f32; 1];

    assert_eq!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut invalid_left,
                right: &mut invalid_right,
            },
        ),
        Err(RuntimeProcessError::OutputLengthMismatch { left: 2, right: 1 })
    );
    assert_eq!(transport.state(), TransportState::Stopped);
    let mut oversized_left = [8.0_f32; 9];
    let mut oversized_right = [8.0_f32; 9];
    assert_eq!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut oversized_left,
                right: &mut oversized_right,
            },
        ),
        Err(RuntimeProcessError::BlockLimit {
            actual: 9,
            maximum: 8,
        })
    );
    assert_eq!(transport.state(), TransportState::Stopped);
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];
    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(cycle.applied_command_count(), 1);
    assert_eq!(cycle.state(), TransportState::Rolling);
}

#[test]
fn invalid_cycle_does_not_consume_live_mixer_commands() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    let mut scratch = [];
    let mut invalid_left = [8.0_f32; 2];
    let mut invalid_right = [8.0_f32; 1];

    assert_eq!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut invalid_left,
                right: &mut invalid_right,
            },
        ),
        Err(RuntimeProcessError::OutputLengthMismatch { left: 2, right: 1 })
    );
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];
    let stopped = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(stopped.applied_mixer_parameter_command_count(), 1);
}

#[test]
fn queued_command_is_rolled_back_when_later_cycle_work_fails() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    sender
        .try_send(TransportCommand::Locate {
            frame: usize::MAX - 1,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut scratch = [];
    let mut left = [9.0_f32; 2];
    let mut right = [9.0_f32; 2];

    assert_eq!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        ),
        Err(RuntimeProcessError::FrameRangeOverflow)
    );
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 0);
    assert_constant(&left, 9.0);
    assert_constant(&right, 9.0);
    sender.try_send(TransportCommand::Start).unwrap();
    let restarted = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(restarted.applied_command_count(), 1);
    assert_eq!(restarted.state(), TransportState::Rolling);
    assert_eq!(restarted.start_frame(), 0);
}

#[test]
fn queued_start_is_rolled_back_when_track_rendering_fails() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    sender.try_send(TransportCommand::Start).unwrap();
    let mut scratch = [];
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let result = transport.process_cycle(
        &mut scratch,
        StereoOutput {
            left: &mut left,
            right: &mut right,
        },
    );

    assert!(matches!(result, Err(RuntimeProcessError::Render(_))));
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 0);
    assert_constant(&left, 9.0);
    assert_constant(&right, 9.0);
}

#[test]
fn live_mixer_state_is_rolled_back_when_later_cycle_work_fails() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (mut transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    transport_sender.try_send(TransportCommand::Start).unwrap();
    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    let mut scratch = [];
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    let failed = transport.process_cycle(
        &mut scratch,
        StereoOutput {
            left: &mut left,
            right: &mut right,
        },
    );
    assert!(matches!(failed, Err(RuntimeProcessError::Render(_))));
    assert_eq!(transport.state(), TransportState::Stopped);

    transport_sender.try_send(TransportCommand::Start).unwrap();
    let recovered = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(recovered.applied_mixer_parameter_command_count(), 0);
    assert!(left[0].abs() <= f32::EPSILON);
    assert!(right[0].abs() <= f32::EPSILON);

    let mut next_left = [9.0_f32; 1];
    let mut next_right = [9.0_f32; 1];
    process_transport(&mut transport, &mut next_left, &mut next_right).unwrap();
    assert!(next_left[0] > 0.0);
    assert!(next_right[0] > 0.0);
}

#[test]
fn graph_runtime_rejects_invalid_callback_capacity_before_allocation() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();

    assert!(matches!(
        GraphMainRuntime::try_new(&prepared, &fixture.mixer, 0),
        Err(RuntimeCreationError::InvalidMaximumBlockFrames {
            actual: 0,
            maximum: MAX_PROCESS_FRAMES,
        })
    ));
    assert!(matches!(
        GraphMainRuntime::try_new(&prepared, &fixture.mixer, MAX_PROCESS_FRAMES + 1),
        Err(RuntimeCreationError::InvalidMaximumBlockFrames {
            actual,
            maximum: MAX_PROCESS_FRAMES,
        }) if actual == MAX_PROCESS_FRAMES + 1
    ));
}
