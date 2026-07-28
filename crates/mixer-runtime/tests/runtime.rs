mod common;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{InterleavedStereoCallback, MAX_PROCESS_FRAMES, StereoOutput};
use hyper_mixer_runtime::{
    FlatMainRuntime, GraphMainRuntime, GraphTransportRuntime, LocateState,
    MAX_QUEUED_TRANSPORT_COMMANDS, RuntimeCreationError, RuntimeProcessError, TransportCommand,
    TransportCommandSendError, TransportCycle, TransportState,
};

use common::{fixture, graph_fixture};

fn assert_constant(samples: &[f32], expected: f32) {
    assert!(
        samples
            .iter()
            .all(|sample| (*sample - expected).abs() <= f32::EPSILON)
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

    transport.apply_command(TransportCommand::Start);
    let empty_rolling =
        process_transport(&mut transport, &mut empty_left, &mut empty_right).unwrap();
    assert_eq!(empty_rolling.state(), TransportState::Rolling);
    assert_eq!(transport.position_frame(), 0);

    let rolling = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(rolling.state(), TransportState::Rolling);
    assert_eq!(rolling.start_frame(), 0);
    assert_eq!(rolling.active_slot_count(), 2);
    assert_eq!(transport.position_frame(), 8);
    assert!(left[7] > 0.0);

    let consecutive = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(consecutive.start_frame(), 8);
    assert_eq!(transport.position_frame(), 16);

    transport.apply_command(TransportCommand::Stop);
    left.fill(9.0);
    right.fill(9.0);
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(transport.position_frame(), 16);
    assert_constant(&left, 0.0);
    assert_constant(&right, 0.0);

    transport.apply_command(TransportCommand::Locate {
        frame: 25,
        state: LocateState::Preserve,
    });
    assert_eq!(transport.state(), TransportState::Stopped);
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(transport.position_frame(), 25);

    transport.apply_command(TransportCommand::Locate {
        frame: 25,
        state: LocateState::Rolling,
    });
    let mut seek_left = [9.0_f32; 1];
    let mut seek_right = [9.0_f32; 1];
    let sought = process_transport(&mut transport, &mut seek_left, &mut seek_right).unwrap();
    assert_eq!(sought.start_frame(), 25);
    assert_eq!(transport.position_frame(), 26);
    assert!((seek_left[0] - 0.223_861_44).abs() <= f32::EPSILON);
    assert!((seek_right[0] - 0.176_986_44).abs() <= f32::EPSILON);

    transport.apply_command(TransportCommand::Locate {
        frame: 40,
        state: LocateState::Stopped,
    });
    process_transport(&mut transport, &mut seek_left, &mut seek_right).unwrap();
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 40);
    assert_constant(&seek_left, 0.0);
    assert_constant(&seek_right, 0.0);

    transport.apply_command(TransportCommand::Locate {
        frame: usize::MAX - 1,
        state: LocateState::Rolling,
    });
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
    transport.apply_command(TransportCommand::Locate {
        frame: 12,
        state: LocateState::Rolling,
    });
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
fn interleaved_callback_executes_the_transport_runtime() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 8)
            .unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(runtime);
    let mut callback = InterleavedStereoCallback::try_new(8).unwrap();
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

    let processed = callback
        .process(&mut interleaved, |output| {
            transport.process_cycle(&mut scratch, output)
        })
        .unwrap();
    let (cycle, meter) = processed.into_parts();

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
    assert_eq!(meter.left().non_finite_sample_count(), 0);
    assert_eq!(meter.right().non_finite_sample_count(), 0);
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
fn queued_command_remains_applied_when_later_cycle_work_fails() {
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
    assert_eq!(transport.state(), TransportState::Rolling);
    assert_eq!(transport.position_frame(), usize::MAX - 1);
    assert_constant(&left, 9.0);
    assert_constant(&right, 9.0);
    sender.try_send(TransportCommand::Stop).unwrap();
    let stopped = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(stopped.applied_command_count(), 1);
    assert_eq!(stopped.state(), TransportState::Stopped);
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
