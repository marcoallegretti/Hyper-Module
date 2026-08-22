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
    MAX_QUEUED_OUTPUT_METER_SNAPSHOTS, MAX_QUEUED_TRANSPORT_COMMANDS, MixerChannelHandle,
    MixerParameterCommand, MixerParameterCommandSendError, MixerParameterCommandSender,
    MixerParameterResolveError, MixerParameterStateRestoreError, MixerParameterTargetError,
    OutputMeterPublication, OutputMeterReceiver, OutputMeterSender, OutputMeterSnapshot,
    PreparedPlaybackMainRuntime, PreparedPlaybackTransportRuntime, RuntimeCreationError,
    RuntimeProcessError, RuntimeProjectComponent, SamplePeakBallisticsConfig, SceneLaunchState,
    StereoSamplePeakBallistics, TransportCommand, TransportCommandSendError, TransportCycle,
    TransportLoopRange, TransportLoopRangeError, TransportOutputCreationError,
    TransportOutputProcessor, TransportState, loudness_command_channel, output_meter_channel,
    playback_recovery_channel,
};
use hyper_native_device::{
    DeviceChain, NativeCompressorState, NativeDeviceInstance, NativeFilterMode, NativeFilterState,
};

use common::{PlaybackFixture, fixture, graph_fixture, playback_fixture};

const GRAPH_SOURCE_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336421";
const GRAPH_GROUP_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336423";
const GRAPH_RETURN_A_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336424";
const GRAPH_RETURN_B_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336425";
const GRAPH_SEND_A_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336433";
const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336400";
const GRAPH_PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336420";
const ARRANGEMENT_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336440";
const SCENE_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122336441";

fn native_filter_chain(enabled: bool) -> DeviceChain {
    native_filter_chain_with_ids(
        enabled,
        "018f47a2-6b2d-7c11-8a44-001122336470",
        "018f47a2-6b2d-7c11-8a44-001122336471",
    )
}

fn native_filter_chain_with_ids(enabled: bool, chain_id: &str, device_id: &str) -> DeviceChain {
    native_filter_chain_for_track(
        PROJECT_ID,
        ARRANGEMENT_TRACK_ID,
        enabled,
        chain_id,
        device_id,
    )
}

fn native_filter_chain_for_track(
    project_id: &str,
    track_id: &str,
    enabled: bool,
    chain_id: &str,
    device_id: &str,
) -> DeviceChain {
    DeviceChain::try_new(
        chain_id.to_owned(),
        project_id.to_owned(),
        track_id.to_owned(),
        vec![
            NativeDeviceInstance::try_new_filter(
                device_id.to_owned(),
                enabled,
                NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_107)
                    .unwrap(),
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

fn native_compressor_chain(enabled: bool) -> DeviceChain {
    native_compressor_chain_with_makeup(enabled, 0)
}

fn native_compressor_chain_with_makeup(enabled: bool, makeup_millidecibels: i32) -> DeviceChain {
    DeviceChain::try_new(
        "018f47a2-6b2d-7c11-8a44-001122336474".to_owned(),
        PROJECT_ID.to_owned(),
        ARRANGEMENT_TRACK_ID.to_owned(),
        vec![NativeDeviceInstance::try_new_compressor(
            "018f47a2-6b2d-7c11-8a44-001122336475".to_owned(),
            enabled,
            NativeCompressorState::try_new(
                -80_000,
                4_000_000,
                100,
                100_000,
                makeup_millidecibels,
            )
            .unwrap(),
        )
        .unwrap()],
    )
    .unwrap()
}

fn native_filter_then_compressor_chain() -> DeviceChain {
    native_filter_chain(true)
        .try_with_appended_compressor(
            "018f47a2-6b2d-7c11-8a44-001122336476".to_owned(),
            true,
            NativeCompressorState::try_new(-36_000, 8_000_000, 100, 100_000, 6_000).unwrap(),
        )
        .unwrap()
}

fn native_compressor_then_filter_chain() -> DeviceChain {
    DeviceChain::try_new(
        "018f47a2-6b2d-7c11-8a44-001122336477".to_owned(),
        PROJECT_ID.to_owned(),
        ARRANGEMENT_TRACK_ID.to_owned(),
        vec![
            NativeDeviceInstance::try_new_compressor(
                "018f47a2-6b2d-7c11-8a44-001122336478".to_owned(),
                true,
                NativeCompressorState::try_new(-36_000, 8_000_000, 100, 100_000, 6_000).unwrap(),
            )
            .unwrap(),
            NativeDeviceInstance::try_new_filter(
                "018f47a2-6b2d-7c11-8a44-001122336479".to_owned(),
                true,
                NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_107)
                    .unwrap(),
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

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

fn assert_started_loudness(snapshot: &OutputMeterSnapshot, frame_count: u64) {
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

fn render_with_device_chain(chain: Option<&DeviceChain>) -> ([f32; 64], [f32; 64]) {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let chains = chain.map_or(&[][..], core::slice::from_ref);
    let mut runtime = PreparedPlaybackMainRuntime::try_new_prepared_playback_with_devices(
        &prepared,
        &fixture.mixer,
        chains,
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
    let mut left = [0.0_f32; 64];
    let mut right = [0.0_f32; 64];
    runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();
    (left, right)
}

#[test]
fn prepared_playback_runs_native_filter_before_the_mixer() {
    let baseline = render_with_device_chain(None);
    let bypassed_chain = native_filter_chain(false);
    let bypassed = render_with_device_chain(Some(&bypassed_chain));
    assert_eq!(baseline.0.map(f32::to_bits), bypassed.0.map(f32::to_bits));
    assert_eq!(baseline.1.map(f32::to_bits), bypassed.1.map(f32::to_bits));

    let filtered_chain = native_filter_chain(true);
    let filtered = render_with_device_chain(Some(&filtered_chain));
    assert!(
        baseline
            .0
            .iter()
            .zip(filtered.0)
            .any(|(baseline, filtered)| baseline.to_bits() != filtered.to_bits())
    );
    assert_eq!(baseline.1.map(f32::to_bits), filtered.1.map(f32::to_bits));
}

#[test]
fn prepared_playback_runs_native_compressor_before_the_mixer() {
    let baseline = render_with_device_chain(None);
    let bypassed_chain = native_compressor_chain(false);
    let bypassed = render_with_device_chain(Some(&bypassed_chain));
    assert_eq!(baseline.0.map(f32::to_bits), bypassed.0.map(f32::to_bits));
    assert_eq!(baseline.1.map(f32::to_bits), bypassed.1.map(f32::to_bits));

    let compressed_chain = native_compressor_chain(true);
    let compressed = render_with_device_chain(Some(&compressed_chain));
    assert!(
        baseline
            .0
            .iter()
            .zip(compressed.0)
            .any(|(baseline, compressed)| baseline.to_bits() != compressed.to_bits())
    );
    assert_eq!(baseline.1.map(f32::to_bits), compressed.1.map(f32::to_bits));

    let makeup = render_with_device_chain(Some(&native_compressor_chain_with_makeup(true, 6_000)));
    assert!(
        compressed
            .0
            .iter()
            .zip(makeup.0)
            .any(|(compressed, makeup)| compressed.to_bits() != makeup.to_bits())
    );
}

#[test]
fn prepared_playback_preserves_native_filter_compressor_processing_order() {
    let filter_then_compressor = native_filter_then_compressor_chain();
    let compressor_then_filter = native_compressor_then_filter_chain();
    assert!(filter_then_compressor.devices()[0].filter_state().is_some());
    assert!(filter_then_compressor.devices()[1].compressor_state().is_some());

    let first = render_with_device_chain(Some(&filter_then_compressor));
    let second = render_with_device_chain(Some(&compressor_then_filter));
    assert!(
        first
            .0
            .iter()
            .zip(second.0)
            .any(|(first, second)| first.to_bits() != second.to_bits())
    );
}

#[test]
fn prepared_playback_refuses_duplicate_device_chains_for_one_track() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let first = native_filter_chain(true);
    let second = native_filter_chain_with_ids(
        true,
        "018f47a2-6b2d-7c11-8a44-001122336472",
        "018f47a2-6b2d-7c11-8a44-001122336473",
    );

    assert!(matches!(
        PreparedPlaybackMainRuntime::try_new_prepared_playback_with_devices(
            &prepared,
            &fixture.mixer,
            &[first, second],
            64,
        ),
        Err(RuntimeCreationError::DuplicateDeviceChainTrack)
    ));
}

#[test]
fn locating_resets_native_filter_history() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let chain = native_filter_chain(true);
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
    let mut first_left = [0.0_f32; 64];
    let mut first_right = [0.0_f32; 64];
    runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut first_left,
                right: &mut first_right,
            },
        )
        .unwrap();

    runtime.seek(0);
    let mut replay_left = [0.0_f32; 64];
    let mut replay_right = [0.0_f32; 64];
    runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut replay_left,
                right: &mut replay_right,
            },
        )
        .unwrap();

    assert_eq!(first_left.map(f32::to_bits), replay_left.map(f32::to_bits));
    assert_eq!(first_right.map(f32::to_bits), replay_right.map(f32::to_bits));
}

fn render_routed_graph_with_device(target: Option<&str>) -> ([f32; 64], [f32; 64]) {
    let fixture = graph_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let chain = target.map(|track_id| {
        native_filter_chain_for_track(
            GRAPH_PROJECT_ID,
            track_id,
            true,
            "018f47a2-6b2d-7c11-8a44-001122336474",
            "018f47a2-6b2d-7c11-8a44-001122336475",
        )
    });
    let chains = chain.as_slice();
    let mut runtime = PreparedPlaybackMainRuntime::try_new_prepared_playback_with_devices(
        &prepared,
        &fixture.mixer,
        chains,
        64,
    )
    .unwrap();
    let mut scratch_left = [[0.0_f32; 64]; 5];
    let mut scratch_right = [[0.0_f32; 64]; 5];
    let mut scratch = scratch_left
        .iter_mut()
        .zip(&mut scratch_right)
        .map(|(left, right)| RenderScratchSlot::new(left, right))
        .collect::<Vec<_>>();
    let mut left = [0.0_f32; 64];
    let mut right = [0.0_f32; 64];
    runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();
    (left, right)
}

#[test]
fn native_filters_run_on_audio_group_and_return_routes_with_pre_and_post_sends() {
    let baseline = render_routed_graph_with_device(None);
    for target in [
        GRAPH_SOURCE_TRACK_ID,
        GRAPH_GROUP_TRACK_ID,
        GRAPH_RETURN_A_TRACK_ID,
        GRAPH_RETURN_B_TRACK_ID,
    ] {
        let filtered = render_routed_graph_with_device(Some(target));
        assert!(
            baseline
                .0
                .iter()
                .chain(&baseline.1)
                .zip(filtered.0.iter().chain(&filtered.1))
                .any(|(baseline, filtered)| baseline.to_bits() != filtered.to_bits()),
            "the device on route {target} must affect its audible routed contribution"
        );
    }
}

#[test]
fn native_filter_processes_arrangement_and_scheduled_scene_sources() {
    let baseline = render_with_device_chain(None);
    let arrangement = native_filter_chain_for_track(
        PROJECT_ID,
        ARRANGEMENT_TRACK_ID,
        true,
        "018f47a2-6b2d-7c11-8a44-001122336476",
        "018f47a2-6b2d-7c11-8a44-001122336477",
    );
    let filtered_arrangement = render_with_device_chain(Some(&arrangement));
    assert_ne!(baseline.0.map(f32::to_bits), filtered_arrangement.0.map(f32::to_bits));
    assert_eq!(baseline.1.map(f32::to_bits), filtered_arrangement.1.map(f32::to_bits));

    let scene = native_filter_chain_for_track(
        PROJECT_ID,
        SCENE_TRACK_ID,
        true,
        "018f47a2-6b2d-7c11-8a44-001122336478",
        "018f47a2-6b2d-7c11-8a44-001122336479",
    );
    let filtered_scene = render_with_device_chain(Some(&scene));
    assert_eq!(baseline.0.map(f32::to_bits), filtered_scene.0.map(f32::to_bits));
    assert_ne!(baseline.1.map(f32::to_bits), filtered_scene.1.map(f32::to_bits));
}

fn assert_meter_delivery_and_backpressure(
    transport: &mut GraphTransportRuntime<'_, '_, '_>,
    callback: &mut InterleavedStereoCallback,
    scratch: &mut [RenderScratchSlot<'_>],
    interleaved: &mut [f32],
    meter_sender: &mut OutputMeterSender,
    mut meter_receiver: OutputMeterReceiver,
    first_snapshot: &OutputMeterSnapshot,
) {
    let first_read = meter_receiver.read_latest();
    assert_eq!(first_read.snapshot().as_ref(), Some(first_snapshot));
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

    assert_terminal_meter_drop_delivery(
        transport,
        callback,
        scratch,
        interleaved,
        meter_sender,
        &mut meter_receiver,
    );
}

fn assert_terminal_meter_drop_delivery(
    transport: &mut GraphTransportRuntime<'_, '_, '_>,
    callback: &mut InterleavedStereoCallback,
    scratch: &mut [RenderScratchSlot<'_>],
    interleaved: &mut [f32],
    meter_sender: &mut OutputMeterSender,
    meter_receiver: &mut OutputMeterReceiver,
) {
    for _ in 0..MAX_QUEUED_OUTPUT_METER_SNAPSHOTS {
        let report = transport
            .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
            .unwrap();
        assert_eq!(report.publication(), OutputMeterPublication::Published);
    }
    for _ in 0..2 {
        let report = transport
            .process_interleaved_cycle(callback, scratch, interleaved, meter_sender)
            .unwrap();
        assert_eq!(report.publication(), OutputMeterPublication::QueueFull);
    }
    let (replacement_sender, replacement_receiver) = output_meter_channel();
    let terminal_sender = std::mem::replace(meter_sender, replacement_sender);
    drop(terminal_sender);

    let terminal = meter_receiver.read_latest();
    assert!(terminal.snapshot().is_some());
    assert_eq!(
        terminal.coalesced_snapshot_count(),
        MAX_QUEUED_OUTPUT_METER_SNAPSHOTS - 1
    );
    assert_eq!(terminal.dropped_snapshot_count(), 2);
    assert!(terminal.sender_disconnected());
    let terminal_already_consumed = meter_receiver.read_latest();
    assert_eq!(terminal_already_consumed.dropped_snapshot_count(), 0);
    assert!(terminal_already_consumed.sender_disconnected());

    drop(replacement_receiver);
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

/// Renders `frames` from `start`, driving the transport by hand so the result
/// is independent of any looping decision the runtime makes.
fn render_linear(fixture: &PlaybackFixture, start: usize, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::Locate {
            frame: start,
            state: LocateState::Rolling,
        })
        .unwrap();
    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = vec![9.0_f32; frames];
    let mut right = vec![9.0_f32; frames];
    transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();
    (left, right)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one linear test proves audio and recovery continuity across a real loop wrap"
)]
fn a_looped_cycle_matches_the_two_linear_cycles_it_replaces() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(20, 50).unwrap(),
        )))
        .unwrap();
    transport
        .apply_command(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    transport
        .apply_command(TransportCommand::Locate {
            frame: 40,
            state: LocateState::Rolling,
        })
        .unwrap();

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = [9.0_f32; 30];
    let mut right = [9.0_f32; 30];
    let cycle = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();

    assert_eq!(cycle.loop_wrap_count(), 1);
    assert_eq!(
        cycle.frame_count(),
        30,
        "the callback still describes the whole block"
    );
    assert_eq!(
        transport.position_frame(),
        40,
        "10 frames to the loop end, then 20 more from the loop start"
    );
    assert_eq!(cycle.position_frame(), transport.position_frame());

    let (head_left, head_right) = render_linear(&fixture, 40, 10);
    let (tail_left, tail_right) = render_linear(&fixture, 20, 20);
    let expected_left: Vec<f32> = head_left.iter().chain(&tail_left).copied().collect();
    let expected_right: Vec<f32> = head_right.iter().chain(&tail_right).copied().collect();
    assert_samples(&left, &expected_left);
    assert_samples(&right, &expected_right);
    assert!(
        expected_left.iter().any(|sample| sample.abs() > 0.0),
        "the fixture actually produces audio across the wrap"
    );

    let (mut recovery_publisher, recovery) = playback_recovery_channel(None, None);
    recovery_publisher.publish_cycle(cycle, None);
    let checkpoint = recovery.snapshot();
    assert_eq!(checkpoint.position_frame(), transport.position_frame());
    assert_eq!(
        checkpoint.loop_range(),
        Some(TransportLoopRange::new(20, 50).unwrap())
    );
    assert!(checkpoint.loop_enabled());

    let replacement =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut replacement = PreparedPlaybackTransportRuntime::new(replacement);
    for command in [
        TransportCommand::SetLoopRange(checkpoint.loop_range()),
        TransportCommand::SetLoopEnabled(checkpoint.loop_enabled()),
        checkpoint.restore_command(),
    ] {
        replacement.apply_command(command).unwrap();
    }
    let mut resumed_arrangement_left = [9.0_f32; 100];
    let mut resumed_arrangement_right = [9.0_f32; 100];
    let mut resumed_scene_left = [9.0_f32; 100];
    let mut resumed_scene_right = [9.0_f32; 100];
    let mut resumed_scratch = [
        RenderScratchSlot::new(
            &mut resumed_arrangement_left,
            &mut resumed_arrangement_right,
        ),
        RenderScratchSlot::new(&mut resumed_scene_left, &mut resumed_scene_right),
    ];
    let mut resumed_left = [9.0_f32; 30];
    let mut resumed_right = [9.0_f32; 30];
    let resumed = replacement
        .process_cycle(
            &mut resumed_scratch,
            StereoOutput {
                left: &mut resumed_left,
                right: &mut resumed_right,
            },
        )
        .unwrap();
    assert_eq!(resumed.start_frame(), 40);
    assert_eq!(resumed.position_frame(), 40);
    assert_eq!(replacement.position_frame(), 40);
    assert_eq!(resumed.loop_wrap_count(), 1);
    assert_samples(&resumed_left, &expected_left);
    assert_samples(&resumed_right, &expected_right);
}

#[test]
fn a_range_shorter_than_the_block_wraps_repeatedly_within_one_callback() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(10, 20).unwrap(),
        )))
        .unwrap();
    transport
        .apply_command(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    transport
        .apply_command(TransportCommand::Locate {
            frame: 10,
            state: LocateState::Rolling,
        })
        .unwrap();

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = [9.0_f32; 35];
    let mut right = [9.0_f32; 35];
    let cycle = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();

    assert_eq!(cycle.loop_wrap_count(), 3, "35 frames over a 10-frame range");
    assert_eq!(transport.position_frame(), 15);
    let (segment_left, _) = render_linear(&fixture, 10, 10);
    for (index, sample) in left.iter().enumerate() {
        let expected = segment_left[index % 10];
        assert!(
            (sample - expected).abs() <= f32::EPSILON,
            "frame {index} repeats the range: {sample} vs {expected}"
        );
    }
}

#[test]
fn a_loop_shorter_than_the_callback_block_is_refused_at_the_sender() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 64)
            .unwrap();
    let (mut sender, _transport) = PreparedPlaybackTransportRuntime::with_command_queue(runtime);

    let short = TransportLoopRange::new(0, 63).unwrap();
    assert!(
        matches!(
            sender.try_send(TransportCommand::SetLoopRange(Some(short))),
            Err(TransportCommandSendError::LoopRangeShorterThanBlock {
                block_frames: 64,
                ..
            })
        ),
        "a range shorter than one callback would force a full graph render per          range of frames, so it is refused rather than armed"
    );

    let exact = TransportLoopRange::new(0, 64).unwrap();
    assert!(
        sender
            .try_send(TransportCommand::SetLoopRange(Some(exact)))
            .is_ok(),
        "a range exactly one block long wraps at most once per callback"
    );
    assert!(
        sender.try_send(TransportCommand::SetLoopRange(None)).is_ok(),
        "and clearing the range is never refused"
    );
}

#[test]
fn an_empty_or_inverted_loop_range_is_refused() {
    assert_eq!(
        TransportLoopRange::new(50, 50),
        Err(TransportLoopRangeError::EmptyRange {
            start_frame: 50,
            end_frame: 50
        })
    );
    assert_eq!(
        TransportLoopRange::new(80, 20),
        Err(TransportLoopRangeError::EmptyRange {
            start_frame: 80,
            end_frame: 20
        })
    );
    let range = TransportLoopRange::new(20, 50).unwrap();
    assert_eq!(range.frame_count(), 30);
    assert!(range.contains(20) && range.contains(49));
    assert!(
        !range.contains(50),
        "the range is half-open, so the end frame belongs to the next pass"
    );
}

#[test]
fn a_playhead_entering_from_before_a_short_range_still_loops() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(40, 48).unwrap(),
        )))
        .unwrap();
    transport
        .apply_command(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    transport
        .apply_command(TransportCommand::Locate {
            frame: 30,
            state: LocateState::Rolling,
        })
        .unwrap();

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = [9.0_f32; 40];
    let mut right = [9.0_f32; 40];
    let cycle = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();

    assert!(
        cycle.loop_wrap_count() > 0,
        "a block that reaches a range shorter than itself must wrap, not step over it"
    );
    assert!(
        transport.position_frame() >= 40 && transport.position_frame() < 48,
        "and the playhead must end inside the range, not past it: {}",
        transport.position_frame()
    );

    let (approach, _) = render_linear(&fixture, 30, 10);
    assert_samples(&left[..10], &approach);
    let (inside, _) = render_linear(&fixture, 40, 8);
    for (index, sample) in left[10..18].iter().enumerate() {
        assert!(
            (sample - inside[index]).abs() <= f32::EPSILON,
            "frame {index} after the range start replays the range"
        );
    }
}

#[test]
fn a_range_that_is_armed_but_not_enabled_does_nothing() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(20, 50).unwrap(),
        )))
        .unwrap();
    transport
        .apply_command(TransportCommand::Locate {
            frame: 40,
            state: LocateState::Rolling,
        })
        .unwrap();

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = [9.0_f32; 30];
    let mut right = [9.0_f32; 30];
    let cycle = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();

    assert_eq!(
        cycle.loop_wrap_count(),
        0,
        "a range alone does not loop; the enabled flag is what engages it"
    );
    assert_eq!(transport.position_frame(), 70, "playback ran straight past 50");
    let (expected_left, expected_right) = render_linear(&fixture, 40, 30);
    assert_samples(&left, &expected_left);
    assert_samples(&right, &expected_right);
}

#[test]
fn arming_a_range_does_not_move_the_playhead_and_disabling_leaves_it_where_it_is() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 100)
            .unwrap();
    let mut transport = PreparedPlaybackTransportRuntime::new(runtime);
    transport
        .apply_command(TransportCommand::Locate {
            frame: 70,
            state: LocateState::Rolling,
        })
        .unwrap();
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(10, 20).unwrap(),
        )))
        .unwrap();
    transport
        .apply_command(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    assert_eq!(
        transport.position_frame(),
        70,
        "engaging a loop the playhead is outside of does not yank it"
    );

    let mut arrangement_left = [9.0_f32; 100];
    let mut arrangement_right = [9.0_f32; 100];
    let mut scene_left = [9.0_f32; 100];
    let mut scene_right = [9.0_f32; 100];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut left = [9.0_f32; 10];
    let mut right = [9.0_f32; 10];
    let outside = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();
    assert_eq!(outside.loop_wrap_count(), 0);
    assert_eq!(
        transport.position_frame(),
        80,
        "and playback outside the range stays linear"
    );

    transport
        .apply_command(TransportCommand::SetLoopEnabled(false))
        .unwrap();
    assert_eq!(
        transport.position_frame(),
        80,
        "disabling the loop leaves the playhead alone"
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

fn assert_scene_observation(
    observation: hyper_mixer_runtime::PlaybackCycleObservation,
    queued: Option<usize>,
    queued_frame: Option<usize>,
    last_launched: Option<usize>,
) {
    assert_eq!(observation.queued_scene_index(), queued);
    assert_eq!(observation.queued_scene_launch_frame(), queued_frame);
    assert_eq!(observation.last_launched_scene_index(), last_launched);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real transport fixture keeps command-boundary, recovery, and rollback evidence together"
)]
fn stop_interactive_scenes_cancels_pending_launches_without_stopping_transport() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 50)
            .unwrap();
    let (mut sender, mut mixer_sender, mut transport) =
        PreparedPlaybackTransportRuntime::with_command_queues(runtime);
    let (mut recovery_publisher, recovery) = playback_recovery_channel(None, None);
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
            state: LocateState::Stopped,
        })
        .unwrap();
    let loop_range = TransportLoopRange::new(100, 1_000).unwrap();
    sender
        .try_send(TransportCommand::SetLoopRange(Some(loop_range)))
        .unwrap();
    sender
        .try_send(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();
    mixer_sender
        .try_send(MixerParameterCommand::MainGain {
            gain: LinearGain::from_millionths(750_000).unwrap(),
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
    let queued = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(queued.applied_command_count(), 4);
    assert_eq!(queued.applied_mixer_parameter_command_count(), 1);
    assert_eq!(queued.scene_launch_state().queued_scene_index(), Some(0));
    assert_eq!(queued.scene_launch_state().queued_launch_frame(), Some(100));
    assert_eq!(
        queued.scene_launch_state().last_launched_scene_index(),
        None
    );
    let committed_mixer = transport.mixer_parameter_state();
    assert_eq!(
        committed_mixer.main_gain(),
        LinearGain::from_millionths(750_000).unwrap()
    );
    recovery_publisher.publish_cycle(queued, Some(&committed_mixer));
    assert_scene_observation(recovery.observe_cycle(), Some(0), Some(100), None);
    assert_samples(&output_left, &[0.0; 50]);
    assert_samples(&output_right, &[0.0; 50]);

    sender.try_send(TransportCommand::Start).unwrap();
    let launched = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(launched.applied_command_count(), 1);
    assert_eq!(launched.scene_launch_state().queued_scene_index(), None);
    assert_eq!(
        launched.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );
    recovery_publisher.publish_cycle(launched, None);
    assert_scene_observation(recovery.observe_cycle(), None, None, Some(0));
    assert_samples(&output_left, &[0.0; 50]);
    let expected_launched_scene: Vec<f32> = fixture.scene_samples[..50]
        .iter()
        .map(|sample| sample * 0.75)
        .collect();
    assert_samples(&output_right, &expected_launched_scene);

    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();
    sender
        .try_send(TransportCommand::StopInteractiveScenes)
        .unwrap();
    let stopped = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(stopped.applied_command_count(), 2);
    assert_eq!(stopped.state(), TransportState::Rolling);
    assert_eq!(stopped.start_frame(), 150);
    assert_eq!(stopped.position_frame(), 200);
    assert_eq!(stopped.loop_range(), Some(loop_range));
    assert!(stopped.loop_enabled());
    assert_eq!(transport.mixer_parameter_state(), committed_mixer);
    assert_eq!(stopped.scene_launch_state().queued_scene_index(), None);
    assert_eq!(
        stopped.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );
    recovery_publisher.publish_cycle(stopped, None);
    assert_scene_observation(recovery.observe_cycle(), None, None, Some(0));
    let recovered = recovery.snapshot();
    assert_eq!(recovered.loop_range(), Some(loop_range));
    assert!(recovered.loop_enabled());
    assert_eq!(recovered.mixer_parameter_state(), Some(committed_mixer));
    assert_samples(&output_left, &[0.0; 50]);
    assert_samples(&output_right, &[0.0; 50]);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one real transport fixture keeps pending rollback and recovery evidence together"
)]
fn failed_cycle_restores_the_prior_interactive_scene_state() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 25)
            .unwrap();
    let (mut sender, mut transport) = PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    let (mut recovery_publisher, recovery) = playback_recovery_channel(None, None);
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
    let launched = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(launched.state(), TransportState::Rolling);
    assert_eq!(launched.position_frame(), 125);
    assert_eq!(launched.scene_launch_state().queued_scene_index(), None);
    assert_eq!(
        launched.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );
    assert_samples(&output_right, &fixture.scene_samples[..25]);

    sender.try_send(TransportCommand::Stop).unwrap();
    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();
    let pending = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(pending.state(), TransportState::Stopped);
    assert_eq!(pending.position_frame(), 125);
    assert_eq!(pending.scene_launch_state().queued_scene_index(), Some(0));
    assert_eq!(
        pending.scene_launch_state().queued_launch_frame(),
        Some(125)
    );
    assert_eq!(
        pending.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );

    sender.try_send(TransportCommand::Start).unwrap();
    sender
        .try_send(TransportCommand::StopInteractiveScenes)
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
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 125);

    let retry = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(retry.state(), TransportState::Stopped);
    assert_eq!(retry.position_frame(), 125);
    assert_eq!(retry.scene_launch_state().queued_scene_index(), Some(0));
    assert_eq!(
        retry.scene_launch_state().queued_launch_frame(),
        Some(125)
    );
    assert_eq!(
        retry.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );
    recovery_publisher.publish_cycle(retry, None);
    assert_scene_observation(recovery.observe_cycle(), Some(0), Some(125), Some(0));

    sender.try_send(TransportCommand::Start).unwrap();
    let resumed = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(resumed.state(), TransportState::Rolling);
    assert_eq!(resumed.start_frame(), 125);
    assert_eq!(resumed.position_frame(), 150);
    assert_eq!(resumed.scene_launch_state().queued_scene_index(), None);
    assert_eq!(
        resumed.scene_launch_state().last_launched_scene_index(),
        Some(0)
    );
    assert_samples(&output_right, &fixture.scene_samples[..25]);
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
    assert_started_loudness(&first_cycle.snapshot(), 50);
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

fn assert_channel_solo_capabilities(
    sender: &MixerParameterCommandSender,
    audio: MixerChannelHandle,
    group: MixerChannelHandle,
) {
    assert_eq!(
        sender.channel_supports_solo(sender.channel_index(audio).unwrap()),
        Ok(true)
    );
    assert_eq!(
        sender.channel_supports_solo(sender.channel_index(group).unwrap()),
        Ok(false)
    );
    assert_eq!(
        sender.channel_supports_solo(usize::MAX),
        Err(MixerParameterResolveError::ChannelNotFound)
    );
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
    assert_channel_solo_capabilities(&sender, source, group);
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
fn live_mixer_fence_sequences_are_allocated_by_the_sole_sender() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, mut sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);

    assert_eq!(sender.try_send_fence().unwrap(), Some(1));
    assert_eq!(sender.try_send_fence().unwrap(), Some(2));
    let capacity = sender.capacity();
    for _ in 2..capacity {
        sender
            .try_send(MixerParameterCommand::MainMute { muted: false })
            .unwrap();
    }
    assert!(matches!(
        sender.try_send_fence(),
        Err(MixerParameterCommandSendError::Full(
            MixerParameterCommand::Fence(_)
        ))
    ));

    let mut left = [0.0_f32; 1];
    let mut right = [0.0_f32; 1];
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(sender.try_send_fence().unwrap(), Some(3));
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
fn mixer_fence_commits_after_every_earlier_command_in_a_successful_cycle() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 8).unwrap();
    let (_transport_sender, mut parameter_sender, mut transport) =
        GraphTransportRuntime::with_command_queues(runtime);
    let mut left = [9.0_f32; 1];
    let mut right = [9.0_f32; 1];

    parameter_sender
        .try_send(MixerParameterCommand::MainMute { muted: true })
        .unwrap();
    assert_eq!(parameter_sender.try_send_fence().unwrap(), Some(1));
    assert_eq!(parameter_sender.committed_fence(), 0);

    let cycle = process_transport(&mut transport, &mut left, &mut right).unwrap();

    assert_eq!(cycle.applied_mixer_parameter_command_count(), 2);
    assert!(transport.mixer_parameter_state().main_muted());
    assert_eq!(parameter_sender.committed_fence(), 1);

    assert_eq!(parameter_sender.try_send_fence().unwrap(), Some(2));
    process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(parameter_sender.committed_fence(), 2);
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
        &snapshot,
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
    let committed_range = TransportLoopRange::new(0, 8).unwrap();
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(committed_range)))
        .unwrap();
    sender
        .try_send(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(8, 16).unwrap(),
        )))
        .unwrap();
    sender
        .try_send(TransportCommand::SetLoopEnabled(true))
        .unwrap();
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

    let recovered = process_transport(&mut transport, &mut left, &mut right).unwrap();
    assert_eq!(recovered.loop_range(), Some(committed_range));
    assert!(!recovered.loop_enabled());
}

#[test]
fn loop_commands_are_rolled_back_when_a_later_command_fails() {
    let fixture = playback_fixture();
    let graph = fixture.compile_playback();
    let prepared = fixture.cache.prepare_playback(&graph).unwrap();
    let runtime =
        PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, &fixture.mixer, 25)
            .unwrap();
    let (mut sender, mut transport) = PreparedPlaybackTransportRuntime::with_command_queue(runtime);
    let committed_range = TransportLoopRange::new(0, 50).unwrap();
    transport
        .apply_command(TransportCommand::SetLoopRange(Some(committed_range)))
        .unwrap();
    let committed_mixer = transport.mixer_parameter_state();
    sender
        .try_send(TransportCommand::SetLoopRange(Some(
            TransportLoopRange::new(50, 100).unwrap(),
        )))
        .unwrap();
    sender
        .try_send(TransportCommand::SetLoopEnabled(true))
        .unwrap();
    sender
        .try_send(TransportCommand::Locate {
            frame: usize::MAX - 1,
            state: LocateState::Rolling,
        })
        .unwrap();
    sender
        .try_send(TransportCommand::LaunchScene { scene_index: 0 })
        .unwrap();
    let mut arrangement_left = [9.0_f32; 25];
    let mut arrangement_right = [9.0_f32; 25];
    let mut scene_left = [9.0_f32; 25];
    let mut scene_right = [9.0_f32; 25];
    let mut scratch = [
        RenderScratchSlot::new(&mut arrangement_left, &mut arrangement_right),
        RenderScratchSlot::new(&mut scene_left, &mut scene_right),
    ];
    let mut output_left = [9.0_f32; 25];
    let mut output_right = [9.0_f32; 25];

    assert!(matches!(
        transport.process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        ),
        Err(RuntimeProcessError::Scene(_))
    ));
    assert_eq!(transport.state(), TransportState::Stopped);
    assert_eq!(transport.position_frame(), 0);
    assert_eq!(transport.loop_range(), Some(committed_range));
    assert!(!transport.loop_enabled());
    assert_eq!(transport.mixer_parameter_state(), committed_mixer);
    assert_constant(&output_left, 9.0);
    assert_constant(&output_right, 9.0);

    let recovered = transport
        .process_cycle(
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();
    assert_eq!(recovered.applied_command_count(), 0);
    assert_eq!(recovered.loop_range(), Some(committed_range));
    assert!(!recovered.loop_enabled());
    assert_eq!(recovered.scene_launch_state(), SceneLaunchState::default());
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
    assert_eq!(parameter_sender.try_send_fence().unwrap(), Some(1));
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
    assert_eq!(parameter_sender.committed_fence(), 0);

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
