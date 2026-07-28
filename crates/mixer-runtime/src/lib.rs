#![doc = "Fixed-track arrangement-to-main-mixer execution for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::sync::atomic::{Ordering, fence};

use hyper_arrangement_renderer::{
    ArrangementRenderer, PlaybackRenderError, PlaybackRenderer, PlaybackRendererCreationError,
    RenderScratchSlot, RenderWindowError, RenderedWindow, RendererCreationError,
};
use hyper_audio_engine::{
    CallbackAdapterCreationError, ChannelParameters, ControlledInputBlock, InputBlock,
    InterleavedStereoCallback, LinearGain, MAX_PROCESS_FRAMES, MixerCreationError, Pan,
    ParameterError, ProcessError, StereoBlockMeter, StereoCallbackError, StereoMixer, StereoOutput,
};
use hyper_media_cache::{PreparedPlayback, PreparedSchedule};
use hyper_playback_compiler::FrameTimelineError;
use hyper_playback_json::{
    AutomationChannelParameter, AutomationMainParameter, AutomationSendParameter, AutomationTarget,
    AutomationValue, CompiledAutomation, CompiledMainMixer, CompiledMixerDestination,
    CompiledMixerGraph, CompiledMixerTap, CompiledMixerTrackKind,
};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

pub const MAX_MAIN_MIXER_SLOTS: usize = 8;
pub const MAX_QUEUED_TRANSPORT_COMMANDS: usize = 64;
pub const MAX_QUEUED_OUTPUT_METER_SNAPSHOTS: usize = 64;
const MAX_SENDS_PER_SLOT: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProjectComponent {
    Mixer,
    Automation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCreationError {
    ProjectMismatch { component: RuntimeProjectComponent },
    SlotLimit { actual: usize, maximum: usize },
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    CapacityUnavailable,
    Renderer(RendererCreationError),
    PlaybackRenderer(PlaybackRendererCreationError),
    Mixer(MixerCreationError),
}

impl fmt::Display for RuntimeCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectMismatch { component } => write!(
                formatter,
                "compiled {} belongs to a different project",
                match component {
                    RuntimeProjectComponent::Mixer => "mixer",
                    RuntimeProjectComponent::Automation => "automation",
                }
            ),
            Self::SlotLimit { actual, maximum } => write!(
                formatter,
                "compiled main mixer has {actual} slots; maximum is {maximum}"
            ),
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "maximum callback block has {actual} frames; valid range is 1..={maximum}"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("mixer runtime buffer capacity is unavailable")
            }
            Self::Renderer(source) => write!(formatter, "mixer track indexing failed: {source}"),
            Self::PlaybackRenderer(source) => {
                write!(formatter, "playback track indexing failed: {source}")
            }
            Self::Mixer(source) => write!(formatter, "mixer construction failed: {source}"),
        }
    }
}

impl Error for RuntimeCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Renderer(source) => Some(source),
            Self::PlaybackRenderer(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::ProjectMismatch { .. }
            | Self::SlotLimit { .. }
            | Self::InvalidMaximumBlockFrames { .. }
            | Self::CapacityUnavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProcessError {
    OutputLengthMismatch { left: usize, right: usize },
    BlockLimit { actual: usize, maximum: usize },
    FrameRangeOverflow,
    Automation(FrameTimelineError),
    AutomationValueMismatch,
    Parameter(ParameterError),
    Render(RenderWindowError),
    PlaybackRender(PlaybackRenderError),
    Mixer(ProcessError),
}

impl fmt::Display for RuntimeProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLengthMismatch { left, right } => {
                write!(
                    formatter,
                    "runtime output lengths differ: left={left}, right={right}"
                )
            }
            Self::BlockLimit { actual, maximum } => write!(
                formatter,
                "runtime block has {actual} frames; configured maximum is {maximum}"
            ),
            Self::FrameRangeOverflow => formatter.write_str("runtime frame range overflowed"),
            Self::Automation(source) => write!(formatter, "automation evaluation failed: {source}"),
            Self::AutomationValueMismatch => {
                formatter.write_str("compiled automation value differs from its target domain")
            }
            Self::Parameter(source) => write!(formatter, "automation parameter failed: {source}"),
            Self::Render(source) => write!(formatter, "track rendering failed: {source}"),
            Self::PlaybackRender(source) => {
                write!(formatter, "combined track rendering failed: {source}")
            }
            Self::Mixer(source) => write!(formatter, "main mixing failed: {source}"),
        }
    }
}

impl Error for RuntimeProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Render(source) => Some(source),
            Self::PlaybackRender(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::Automation(source) => Some(source),
            Self::Parameter(source) => Some(source),
            Self::OutputLengthMismatch { .. }
            | Self::BlockLimit { .. }
            | Self::FrameRangeOverflow
            | Self::AutomationValueMismatch => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportState {
    Stopped,
    Rolling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocateState {
    Preserve,
    Stopped,
    Rolling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportCommand {
    Start,
    Stop,
    Locate { frame: usize, state: LocateState },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportCommandSendError {
    Full(TransportCommand),
    Disconnected(TransportCommand),
}

impl TransportCommandSendError {
    #[must_use]
    pub const fn command(self) -> TransportCommand {
        match self {
            Self::Full(command) | Self::Disconnected(command) => command,
        }
    }
}

impl fmt::Display for TransportCommandSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => formatter.write_str("transport command queue is full"),
            Self::Disconnected(_) => {
                formatter.write_str("transport command receiver is disconnected")
            }
        }
    }
}

impl Error for TransportCommandSendError {}

/// Application-thread half of the bounded transport command queue.
#[derive(Debug)]
pub struct TransportCommandSender {
    producer: Producer<TransportCommand>,
}

impl TransportCommandSender {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_TRANSPORT_COMMANDS
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Attempts to enqueue one command without waiting or allocating.
    ///
    /// # Errors
    ///
    /// Returns the original command when the fixed queue is full or when the
    /// receiving transport has already been destroyed.
    pub fn try_send(&mut self, command: TransportCommand) -> Result<(), TransportCommandSendError> {
        if self.is_disconnected() {
            return Err(TransportCommandSendError::Disconnected(command));
        }
        self.producer.push(command).map_err(|error| match error {
            PushError::Full(command) => TransportCommandSendError::Full(command),
        })
    }
}

/// Result of one device-independent transport processing cycle.
///
/// Queue diagnostics describe commands applied at the beginning of this cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportCycle {
    state: TransportState,
    start_frame: usize,
    frame_count: usize,
    active_slot_count: usize,
    applied_command_count: usize,
    command_sender_disconnected: bool,
}

impl TransportCycle {
    #[must_use]
    pub const fn state(self) -> TransportState {
        self.state
    }

    #[must_use]
    pub const fn start_frame(self) -> usize {
        self.start_frame
    }

    /// Returns the committed transport position after this successful cycle.
    #[must_use]
    pub const fn position_frame(self) -> usize {
        match self.state {
            TransportState::Stopped => self.start_frame,
            TransportState::Rolling => match self.start_frame.checked_add(self.frame_count) {
                Some(position_frame) => position_frame,
                None => unreachable!(),
            },
        }
    }

    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    #[must_use]
    pub const fn active_slot_count(self) -> usize {
        self.active_slot_count
    }

    #[must_use]
    pub const fn applied_command_count(self) -> usize {
        self.applied_command_count
    }

    #[must_use]
    pub const fn command_sender_disconnected(self) -> bool {
        self.command_sender_disconnected
    }
}

/// Final output meter data associated with one successful transport cycle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputMeterSnapshot {
    cycle: TransportCycle,
    meter: StereoBlockMeter,
    dropped_before: usize,
}

impl OutputMeterSnapshot {
    #[must_use]
    pub const fn cycle(self) -> TransportCycle {
        self.cycle
    }

    #[must_use]
    pub const fn meter(self) -> StereoBlockMeter {
        self.meter
    }

    /// Snapshots rejected by a full queue since the preceding successful
    /// publication.
    #[must_use]
    pub const fn dropped_before(self) -> usize {
        self.dropped_before
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputMeterPublishError {
    Full(OutputMeterSnapshot),
    Disconnected(OutputMeterSnapshot),
}

impl OutputMeterPublishError {
    #[must_use]
    pub const fn snapshot(self) -> OutputMeterSnapshot {
        match self {
            Self::Full(snapshot) | Self::Disconnected(snapshot) => snapshot,
        }
    }
}

impl fmt::Display for OutputMeterPublishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => formatter.write_str("output meter snapshot queue is full"),
            Self::Disconnected(_) => {
                formatter.write_str("output meter snapshot receiver is disconnected")
            }
        }
    }
}

impl Error for OutputMeterPublishError {}

/// Audio-thread half of the bounded output meter snapshot channel.
#[derive(Debug)]
pub struct OutputMeterSender {
    producer: Producer<OutputMeterSnapshot>,
    dropped_snapshot_count: usize,
}

impl OutputMeterSender {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_OUTPUT_METER_SNAPSHOTS
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Attempts to publish one callback snapshot without waiting or allocating.
    ///
    /// # Errors
    ///
    /// Returns the published snapshot on success, including drop telemetry.
    /// Returns the snapshot if the queue is full or its receiver has been
    /// destroyed. A full queue increments a local saturating counter that is
    /// attached to the next successfully published snapshot.
    pub fn try_publish(
        &mut self,
        mut snapshot: OutputMeterSnapshot,
    ) -> Result<OutputMeterSnapshot, OutputMeterPublishError> {
        if self.is_disconnected() {
            return Err(OutputMeterPublishError::Disconnected(snapshot));
        }
        snapshot.dropped_before = self.dropped_snapshot_count;
        match self.producer.push(snapshot) {
            Ok(()) => {
                self.dropped_snapshot_count = 0;
                Ok(snapshot)
            }
            Err(PushError::Full(snapshot)) => {
                self.dropped_snapshot_count = self.dropped_snapshot_count.saturating_add(1);
                Err(OutputMeterPublishError::Full(snapshot))
            }
        }
    }
}

/// Result of draining the bounded output meter channel to its newest snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatestOutputMeterSnapshot {
    snapshot: Option<OutputMeterSnapshot>,
    coalesced_snapshot_count: usize,
    dropped_snapshot_count: usize,
    sender_disconnected: bool,
}

impl LatestOutputMeterSnapshot {
    #[must_use]
    pub const fn snapshot(self) -> Option<OutputMeterSnapshot> {
        self.snapshot
    }

    #[must_use]
    pub const fn coalesced_snapshot_count(self) -> usize {
        self.coalesced_snapshot_count
    }

    #[must_use]
    pub const fn dropped_snapshot_count(self) -> usize {
        self.dropped_snapshot_count
    }

    #[must_use]
    pub const fn sender_disconnected(self) -> bool {
        self.sender_disconnected
    }
}

/// Control-thread half of the bounded output meter snapshot channel.
#[derive(Debug)]
pub struct OutputMeterReceiver {
    consumer: Consumer<OutputMeterSnapshot>,
}

impl OutputMeterReceiver {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_OUTPUT_METER_SNAPSHOTS
    }

    /// Drains available snapshots and returns only the newest one.
    ///
    /// Coalesced snapshots were successfully delivered but superseded during
    /// this read. Dropped snapshots were rejected earlier because the queue
    /// was full. Both counts are explicit and independent.
    #[must_use]
    pub fn read_latest(&mut self) -> LatestOutputMeterSnapshot {
        let mut snapshot = None;
        let mut delivered_snapshot_count = 0_usize;
        let mut dropped_snapshot_count = 0_usize;
        for _ in 0..MAX_QUEUED_OUTPUT_METER_SNAPSHOTS {
            let Ok(next) = self.consumer.pop() else {
                break;
            };
            dropped_snapshot_count = dropped_snapshot_count.saturating_add(next.dropped_before());
            snapshot = Some(next);
            delivered_snapshot_count += 1;
        }
        let sender_disconnected = self.consumer.is_abandoned();
        if sender_disconnected {
            fence(Ordering::Acquire);
        }
        LatestOutputMeterSnapshot {
            snapshot,
            coalesced_snapshot_count: delivered_snapshot_count.saturating_sub(1),
            dropped_snapshot_count,
            sender_disconnected,
        }
    }
}

/// Allocates the fixed output meter queue before callback processing starts.
#[must_use]
pub fn output_meter_channel() -> (OutputMeterSender, OutputMeterReceiver) {
    let (producer, consumer) = RingBuffer::new(MAX_QUEUED_OUTPUT_METER_SNAPSHOTS);
    (
        OutputMeterSender {
            producer,
            dropped_snapshot_count: 0,
        },
        OutputMeterReceiver { consumer },
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMeterPublication {
    Published,
    QueueFull,
    ReceiverDisconnected,
}

/// Result and publication status of one interleaved transport callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InterleavedTransportCycle {
    snapshot: OutputMeterSnapshot,
    publication: OutputMeterPublication,
}

impl InterleavedTransportCycle {
    #[must_use]
    pub const fn snapshot(self) -> OutputMeterSnapshot {
        self.snapshot
    }

    #[must_use]
    pub const fn publication(self) -> OutputMeterPublication {
        self.publication
    }
}

/// A prepared arrangement renderer and mixer whose slot identities cannot
/// diverge after construction.
///
/// Construction performs all indexing and heap allocation. [`Self::process_window`]
/// uses caller-owned scratch and performs no allocation, I/O, locking, logging,
/// or dynamic dispatch.
#[derive(Debug)]
pub struct FlatMainRuntime<'prepared, 'cache, 'schedule> {
    renderer: ArrangementRenderer<'prepared, 'cache, 'schedule>,
    mixer: StereoMixer,
    parameters: [ChannelParameters; MAX_MAIN_MIXER_SLOTS],
    slot_count: usize,
    master_gain: LinearGain,
}

impl<'prepared, 'cache, 'schedule> FlatMainRuntime<'prepared, 'cache, 'schedule> {
    /// Binds schedule tracks to compiled mixer slots in one immutable order.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] if the compiled mixer exceeds the
    /// version-1 slot bound, track indexing fails, or mixer state cannot be
    /// allocated.
    pub fn try_new(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMainMixer,
    ) -> Result<Self, RuntimeCreationError> {
        if schedule.project_id() != compiled.project_id() {
            return Err(RuntimeCreationError::ProjectMismatch {
                component: RuntimeProjectComponent::Mixer,
            });
        }
        let slot_count = compiled.slots().len();
        if slot_count > MAX_MAIN_MIXER_SLOTS {
            return Err(RuntimeCreationError::SlotLimit {
                actual: slot_count,
                maximum: MAX_MAIN_MIXER_SLOTS,
            });
        }

        let mut track_ids = [""; MAX_MAIN_MIXER_SLOTS];
        let mut parameters =
            [ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, false); MAX_MAIN_MIXER_SLOTS];
        for (index, slot) in compiled.slots().iter().enumerate() {
            track_ids[index] = slot.track_id();
            parameters[index] = slot.parameters();
        }
        let renderer = ArrangementRenderer::try_new_for_tracks(schedule, &track_ids[..slot_count])
            .map_err(RuntimeCreationError::Renderer)?;
        let mixer = StereoMixer::try_new(&parameters[..slot_count], compiled.master_gain())
            .map_err(RuntimeCreationError::Mixer)?;

        Ok(Self {
            renderer,
            mixer,
            parameters,
            slot_count,
            master_gain: compiled.master_gain(),
        })
    }

    #[must_use]
    pub const fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// Renders and mixes one half-open arrangement frame window.
    ///
    /// The output length defines the callback frame count. Exactly one scratch
    /// slot per configured mixer input is required.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] before output is touched when output
    /// planes differ, or when track rendering or mixer processing rejects the
    /// supplied block.
    pub fn process_window(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(RuntimeProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        let rendered = self
            .renderer
            .render_track_window(arrangement_start_frame, frame_count, scratch)
            .map_err(RuntimeProcessError::Render)?;

        let empty: &[f32] = &[];
        let mut inputs = [InputBlock::Mono(empty); MAX_MAIN_MIXER_SLOTS];
        for (input, slot) in inputs[..self.slot_count].iter_mut().zip(scratch.iter()) {
            *input = slot.input_block();
        }
        self.mixer
            .process(
                &inputs[..self.slot_count],
                &self.parameters[..self.slot_count],
                self.master_gain,
                output,
            )
            .map_err(RuntimeProcessError::Mixer)?;
        Ok(rendered)
    }
}

#[derive(Debug)]
struct StereoBuffer {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl StereoBuffer {
    fn try_new(frame_count: usize) -> Result<Self, RuntimeCreationError> {
        let mut left = Vec::new();
        left.try_reserve_exact(frame_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        left.resize(frame_count, 0.0);
        let mut right = Vec::new();
        right
            .try_reserve_exact(frame_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        right.resize(frame_count, 0.0);
        Ok(Self { left, right })
    }

    fn clear_prefix(&mut self, frame_count: usize) {
        self.left[..frame_count].fill(0.0);
        self.right[..frame_count].fill(0.0);
    }
}

fn try_filled<T: Copy>(count: usize, value: T) -> Result<Vec<T>, RuntimeCreationError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(count)
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    output.resize(count, value);
    Ok(output)
}

#[derive(Debug)]
struct GraphSendRuntime {
    target_slot: usize,
    tap: CompiledMixerTap,
    parameters: ChannelParameters,
    enabled: bool,
    controls: Vec<ChannelParameters>,
    processor: StereoMixer,
}

#[derive(Debug)]
struct GraphSlotRuntime {
    kind: CompiledMixerTrackKind,
    destination: CompiledMixerDestination,
    parameters: ChannelParameters,
    muted: bool,
    solo: bool,
    controls: Vec<ChannelParameters>,
    processor: StereoMixer,
    sends: Vec<GraphSendRuntime>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ChannelOverrides {
    gain: Option<i32>,
    pan: Option<i32>,
    mute: Option<bool>,
    solo: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct SendOverrides {
    gain: Option<i32>,
    pan: Option<i32>,
    enabled: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct MainOverrides {
    gain: Option<i32>,
    mute: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct FrameOverrides {
    channels: [ChannelOverrides; MAX_MAIN_MIXER_SLOTS],
    sends: [[SendOverrides; MAX_SENDS_PER_SLOT]; MAX_MAIN_MIXER_SLOTS],
    main: MainOverrides,
    mismatch: bool,
}

impl FrameOverrides {
    fn channel_mut(&mut self, slot: usize) -> Option<&mut ChannelOverrides> {
        let Some(channel) = self.channels.get_mut(slot) else {
            self.mismatch = true;
            return None;
        };
        Some(channel)
    }

    fn send_mut(&mut self, slot: usize, send: usize) -> Option<&mut SendOverrides> {
        let Some(sends) = self.sends.get_mut(slot) else {
            self.mismatch = true;
            return None;
        };
        let Some(send) = sends.get_mut(send) else {
            self.mismatch = true;
            return None;
        };
        Some(send)
    }

    fn apply(&mut self, target: AutomationTarget, value: AutomationValue) {
        match (target, value) {
            (
                AutomationTarget::Channel {
                    slot,
                    parameter: AutomationChannelParameter::Gain,
                },
                AutomationValue::Integer(value),
            ) => {
                if let Some(channel) = self.channel_mut(slot) {
                    channel.gain = Some(value);
                }
            }
            (
                AutomationTarget::Channel {
                    slot,
                    parameter: AutomationChannelParameter::Pan,
                },
                AutomationValue::Integer(value),
            ) => {
                if let Some(channel) = self.channel_mut(slot) {
                    channel.pan = Some(value);
                }
            }
            (
                AutomationTarget::Channel {
                    slot,
                    parameter: AutomationChannelParameter::Mute,
                },
                AutomationValue::Boolean(value),
            ) => {
                if let Some(channel) = self.channel_mut(slot) {
                    channel.mute = Some(value);
                }
            }
            (
                AutomationTarget::Channel {
                    slot,
                    parameter: AutomationChannelParameter::Solo,
                },
                AutomationValue::Boolean(value),
            ) => {
                if let Some(channel) = self.channel_mut(slot) {
                    channel.solo = Some(value);
                }
            }
            (
                AutomationTarget::Send {
                    slot,
                    send,
                    parameter: AutomationSendParameter::Gain,
                },
                AutomationValue::Integer(value),
            ) => {
                if let Some(send) = self.send_mut(slot, send) {
                    send.gain = Some(value);
                }
            }
            (
                AutomationTarget::Send {
                    slot,
                    send,
                    parameter: AutomationSendParameter::Pan,
                },
                AutomationValue::Integer(value),
            ) => {
                if let Some(send) = self.send_mut(slot, send) {
                    send.pan = Some(value);
                }
            }
            (
                AutomationTarget::Send {
                    slot,
                    send,
                    parameter: AutomationSendParameter::Enabled,
                },
                AutomationValue::Boolean(value),
            ) => {
                if let Some(send) = self.send_mut(slot, send) {
                    send.enabled = Some(value);
                }
            }
            (
                AutomationTarget::Main {
                    parameter: AutomationMainParameter::Gain,
                },
                AutomationValue::Integer(value),
            ) => self.main.gain = Some(value),
            (
                AutomationTarget::Main {
                    parameter: AutomationMainParameter::Mute,
                },
                AutomationValue::Boolean(value),
            ) => self.main.mute = Some(value),
            _ => self.mismatch = true,
        }
    }
}

/// A bounded primary/send/return graph whose route order and buffers are
/// completely compiled before callback execution.
#[derive(Debug)]
pub struct RoutedMainRuntime<R> {
    renderer: R,
    slots: Vec<GraphSlotRuntime>,
    processing_order: Vec<usize>,
    route_buffers: Vec<StereoBuffer>,
    channel_output: StereoBuffer,
    send_output: StereoBuffer,
    main_input: StereoBuffer,
    main_processor: StereoMixer,
    main_parameters: [ChannelParameters; 1],
    master_gain: LinearGain,
    main_gain: LinearGain,
    main_muted: bool,
    main_input_controls: Vec<ChannelParameters>,
    unity_master_controls: Vec<LinearGain>,
    master_controls: Vec<LinearGain>,
    automation: Option<CompiledAutomation>,
    max_block_frames: usize,
}

pub type GraphMainRuntime<'prepared, 'cache, 'schedule> =
    RoutedMainRuntime<ArrangementRenderer<'prepared, 'cache, 'schedule>>;

pub type PreparedPlaybackMainRuntime<'prepared, 'cache, 'graph, 'project> =
    RoutedMainRuntime<PlaybackRenderer<'prepared, 'graph, 'cache, 'graph, 'project>>;

mod sealed {
    pub trait TrackWindowRenderer {}
}

#[doc(hidden)]
pub trait TrackWindowRenderer: sealed::TrackWindowRenderer {
    fn render_track_window(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RuntimeProcessError>;

    fn seek(&mut self, frame: usize);
}

impl sealed::TrackWindowRenderer for ArrangementRenderer<'_, '_, '_> {}

impl TrackWindowRenderer for ArrangementRenderer<'_, '_, '_> {
    fn render_track_window(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        ArrangementRenderer::render_track_window(self, start_frame, frame_count, scratch)
            .map_err(RuntimeProcessError::Render)
    }

    fn seek(&mut self, _frame: usize) {}
}

impl sealed::TrackWindowRenderer for PlaybackRenderer<'_, '_, '_, '_, '_> {}

impl TrackWindowRenderer for PlaybackRenderer<'_, '_, '_, '_, '_> {
    fn render_track_window(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        PlaybackRenderer::render_track_window(self, start_frame, frame_count, scratch)
            .map_err(RuntimeProcessError::PlaybackRender)
    }

    fn seek(&mut self, frame: usize) {
        PlaybackRenderer::seek(self, frame);
    }
}

impl<'prepared, 'cache, 'schedule>
    RoutedMainRuntime<ArrangementRenderer<'prepared, 'cache, 'schedule>>
{
    /// Binds a prepared schedule to a fully resolved mixer graph.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] for a slot or block bound violation,
    /// unavailable buffer capacity, track-index construction failure, or mixer
    /// state construction failure.
    pub fn try_new(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMixerGraph,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_inner(schedule, compiled, None, max_block_frames)
    }

    /// Binds a prepared schedule, mixer graph, and compiled sample automation.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] for the same structural, capacity,
    /// renderer, or mixer failures as [`Self::try_new`].
    pub fn try_new_automated(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMixerGraph,
        automation: CompiledAutomation,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_inner(schedule, compiled, Some(automation), max_block_frames)
    }

    fn try_new_inner(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMixerGraph,
        automation: Option<CompiledAutomation>,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        validate_project_identity(schedule.project_id(), compiled, automation.as_ref())?;
        validate_routed_runtime_bounds(compiled, max_block_frames)?;
        let slot_count = compiled.slots().len();
        let mut track_ids = [""; MAX_MAIN_MIXER_SLOTS];
        for (index, slot) in compiled.slots().iter().enumerate() {
            track_ids[index] = slot.track_id();
        }
        let renderer = ArrangementRenderer::try_new_for_tracks(schedule, &track_ids[..slot_count])
            .map_err(RuntimeCreationError::Renderer)?;
        build_routed_main_runtime(renderer, compiled, automation, max_block_frames)
    }
}

impl<'prepared, 'cache, 'graph, 'project>
    RoutedMainRuntime<PlaybackRenderer<'prepared, 'graph, 'cache, 'graph, 'project>>
{
    /// Binds one atomically prepared playback graph to a routed mixer.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] for bounds, capacity, renderer, or
    /// mixer construction failures.
    pub fn try_new_prepared_playback(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_prepared_playback_inner(prepared, compiled, None, max_block_frames)
    }

    /// Binds prepared playback and compiled sample automation to one graph.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] under the same conditions as
    /// [`Self::try_new_prepared_playback`].
    pub fn try_new_prepared_playback_automated(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        automation: CompiledAutomation,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_prepared_playback_inner(
            prepared,
            compiled,
            Some(automation),
            max_block_frames,
        )
    }

    fn try_new_prepared_playback_inner(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        automation: Option<CompiledAutomation>,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        validate_project_identity(prepared.project_id(), compiled, automation.as_ref())?;
        validate_routed_runtime_bounds(compiled, max_block_frames)?;
        let slot_count = compiled.slots().len();
        let mut track_ids = [""; MAX_MAIN_MIXER_SLOTS];
        for (index, slot) in compiled.slots().iter().enumerate() {
            track_ids[index] = slot.track_id();
        }
        let renderer = PlaybackRenderer::try_new_for_tracks(
            prepared.schedule(),
            prepared.authority(),
            prepared.scene_audio(),
            0,
            &track_ids[..slot_count],
        )
        .map_err(RuntimeCreationError::PlaybackRenderer)?;
        build_routed_main_runtime(renderer, compiled, automation, max_block_frames)
    }
}

fn validate_project_identity(
    project_id: &str,
    compiled: &CompiledMixerGraph,
    automation: Option<&CompiledAutomation>,
) -> Result<(), RuntimeCreationError> {
    if project_id != compiled.project_id() {
        return Err(RuntimeCreationError::ProjectMismatch {
            component: RuntimeProjectComponent::Mixer,
        });
    }
    if automation.is_some_and(|automation| automation.project_id() != project_id) {
        return Err(RuntimeCreationError::ProjectMismatch {
            component: RuntimeProjectComponent::Automation,
        });
    }
    Ok(())
}

fn validate_routed_runtime_bounds(
    compiled: &CompiledMixerGraph,
    max_block_frames: usize,
) -> Result<(), RuntimeCreationError> {
    let slot_count = compiled.slots().len();
    if slot_count > MAX_MAIN_MIXER_SLOTS {
        return Err(RuntimeCreationError::SlotLimit {
            actual: slot_count,
            maximum: MAX_MAIN_MIXER_SLOTS,
        });
    }
    if max_block_frames == 0 || max_block_frames > MAX_PROCESS_FRAMES {
        return Err(RuntimeCreationError::InvalidMaximumBlockFrames {
            actual: max_block_frames,
            maximum: MAX_PROCESS_FRAMES,
        });
    }
    Ok(())
}

fn build_routed_main_runtime<R>(
    renderer: R,
    compiled: &CompiledMixerGraph,
    automation: Option<CompiledAutomation>,
    max_block_frames: usize,
) -> Result<RoutedMainRuntime<R>, RuntimeCreationError> {
    let slot_count = compiled.slots().len();
    let control_frame_count = if automation.is_some() {
        max_block_frames
    } else {
        0
    };
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(slot_count)
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    for compiled_slot in compiled.slots() {
        let parameters = compiled_slot.parameters();
        let processor = StereoMixer::try_new(&[parameters], LinearGain::UNITY)
            .map_err(RuntimeCreationError::Mixer)?;
        let mut sends = Vec::new();
        sends
            .try_reserve_exact(compiled_slot.sends().len())
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        for compiled_send in compiled_slot.sends() {
            let parameters = compiled_send.parameters();
            sends.push(GraphSendRuntime {
                target_slot: compiled_send.target_slot(),
                tap: compiled_send.tap(),
                parameters,
                enabled: compiled_send.enabled(),
                controls: try_filled(control_frame_count, parameters)?,
                processor: StereoMixer::try_new(&[parameters], LinearGain::UNITY)
                    .map_err(RuntimeCreationError::Mixer)?,
            });
        }
        slots.push(GraphSlotRuntime {
            kind: compiled_slot.kind(),
            destination: compiled_slot.destination(),
            parameters,
            muted: compiled_slot.muted(),
            solo: compiled_slot.solo(),
            controls: try_filled(control_frame_count, parameters)?,
            processor,
            sends,
        });
    }
    let mut processing_order = Vec::new();
    processing_order
        .try_reserve_exact(compiled.processing_order().len())
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    processing_order.extend_from_slice(compiled.processing_order());
    let mut route_buffers = Vec::new();
    route_buffers
        .try_reserve_exact(slot_count)
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    for _ in 0..slot_count {
        route_buffers.push(StereoBuffer::try_new(max_block_frames)?);
    }
    let main_parameters = [ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true)];
    let main_processor = StereoMixer::try_new(&main_parameters, compiled.master_gain())
        .map_err(RuntimeCreationError::Mixer)?;
    Ok(RoutedMainRuntime {
        renderer,
        slots,
        processing_order,
        route_buffers,
        channel_output: StereoBuffer::try_new(max_block_frames)?,
        send_output: StereoBuffer::try_new(max_block_frames)?,
        main_input: StereoBuffer::try_new(max_block_frames)?,
        main_processor,
        main_parameters,
        master_gain: compiled.master_gain(),
        main_gain: compiled.main_gain(),
        main_muted: compiled.main_muted(),
        main_input_controls: try_filled(control_frame_count, main_parameters[0])?,
        unity_master_controls: try_filled(control_frame_count, LinearGain::UNITY)?,
        master_controls: try_filled(control_frame_count, compiled.master_gain())?,
        automation,
        max_block_frames,
    })
}

impl<R: TrackWindowRenderer> RoutedMainRuntime<R> {
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub const fn max_block_frames(&self) -> usize {
        self.max_block_frames
    }

    pub fn seek(&mut self, frame: usize) {
        self.renderer.seek(frame);
    }

    /// Renders and executes one bounded graph window into the main output.
    ///
    /// Primary routes, pre/post-fader sends, returns, groups, channel controls,
    /// solo eligibility, and master gain use precompiled state. Callback work
    /// performs no allocation, I/O, locking, logging, or dynamic dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] for output or configured block bounds,
    /// arrangement rendering failure, or an unexpected mixer invariant
    /// failure.
    pub fn process_window(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(RuntimeProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        if frame_count > self.max_block_frames {
            return Err(RuntimeProcessError::BlockLimit {
                actual: frame_count,
                maximum: self.max_block_frames,
            });
        }
        let rendered =
            self.renderer
                .render_track_window(arrangement_start_frame, frame_count, scratch)?;
        if frame_count == 0 {
            return Ok(rendered);
        }
        let result = (|| {
            if self.automation.is_some() {
                self.prepare_automation_controls(arrangement_start_frame, frame_count)?;
            }

            for buffer in &mut self.route_buffers {
                buffer.clear_prefix(frame_count);
            }
            self.main_input.clear_prefix(frame_count);

            for order_index in 0..self.processing_order.len() {
                let slot_index = self.processing_order[order_index];
                self.process_sends(slot_index, CompiledMixerTap::PreFader, frame_count, scratch)?;
                self.process_channel(slot_index, frame_count, scratch)?;
                self.route_channel_output(slot_index, frame_count);
                self.process_sends(
                    slot_index,
                    CompiledMixerTap::PostFader,
                    frame_count,
                    scratch,
                )?;
            }

            let main_input = [InputBlock::Stereo {
                left: &self.main_input.left[..frame_count],
                right: &self.main_input.right[..frame_count],
            }];
            if self.automation.is_some() {
                let controlled = [ControlledInputBlock {
                    input: main_input[0],
                    parameters: &self.main_input_controls[..frame_count],
                }];
                self.main_processor
                    .process_controlled(&controlled, &self.master_controls[..frame_count], output)
                    .map_err(RuntimeProcessError::Mixer)?;
            } else {
                self.main_processor
                    .process(&main_input, &self.main_parameters, self.master_gain, output)
                    .map_err(RuntimeProcessError::Mixer)?;
            }
            Ok(rendered)
        })();
        if result.is_err() {
            self.renderer.seek(arrangement_start_frame);
        }
        result
    }

    fn prepare_automation_controls(
        &mut self,
        arrangement_start_frame: usize,
        frame_count: usize,
    ) -> Result<(), RuntimeProcessError> {
        for frame in 0..frame_count {
            let absolute_frame = arrangement_start_frame
                .checked_add(frame)
                .ok_or(RuntimeProcessError::FrameRangeOverflow)?;
            let mut overrides = FrameOverrides::default();
            self.automation
                .as_ref()
                .ok_or(RuntimeProcessError::AutomationValueMismatch)?
                .visit_frame_values(absolute_frame, |target, value| {
                    overrides.apply(target, value);
                })
                .map_err(RuntimeProcessError::Automation)?;
            if overrides.mismatch {
                return Err(RuntimeProcessError::AutomationValueMismatch);
            }

            let mut muted = [false; MAX_MAIN_MIXER_SLOTS];
            let mut solo = [false; MAX_MAIN_MIXER_SLOTS];
            let mut send_enabled = [[false; MAX_SENDS_PER_SLOT]; MAX_MAIN_MIXER_SLOTS];
            for (slot_index, slot) in self.slots.iter_mut().enumerate() {
                let channel = overrides.channels[slot_index];
                let gain = automated_gain(channel.gain, slot.parameters.gain)?;
                let pan = automated_pan(channel.pan, slot.parameters.pan)?;
                muted[slot_index] = channel.mute.unwrap_or(slot.muted);
                solo[slot_index] = channel.solo.unwrap_or(slot.solo);
                slot.controls[frame] = ChannelParameters::new(gain, pan, !muted[slot_index]);
                for (send_index, send) in slot.sends.iter_mut().enumerate() {
                    let send_override = overrides.sends[slot_index][send_index];
                    let gain = automated_gain(send_override.gain, send.parameters.gain)?;
                    let pan = automated_pan(send_override.pan, send.parameters.pan)?;
                    let enabled = send_override.enabled.unwrap_or(send.enabled);
                    send_enabled[slot_index][send_index] = enabled;
                    send.controls[frame] = ChannelParameters::new(gain, pan, enabled);
                }
            }

            let any_solo =
                self.slots.iter().enumerate().any(|(slot, channel)| {
                    channel.kind == CompiledMixerTrackKind::Audio && solo[slot]
                });
            let mut eligible = [!any_solo; MAX_MAIN_MIXER_SLOTS];
            if any_solo {
                for (slot, channel) in self.slots.iter().enumerate() {
                    eligible[slot] = channel.kind == CompiledMixerTrackKind::Audio && solo[slot];
                }
                for &slot_index in &self.processing_order {
                    if !eligible[slot_index] {
                        continue;
                    }
                    if let CompiledMixerDestination::Slot(target) =
                        self.slots[slot_index].destination
                    {
                        eligible[target] = true;
                    }
                    for (send_index, send) in self.slots[slot_index].sends.iter().enumerate() {
                        if send_enabled[slot_index][send_index] {
                            eligible[send.target_slot] = true;
                        }
                    }
                }
            }
            for (slot_index, slot) in self.slots.iter_mut().enumerate() {
                slot.controls[frame].audible &= eligible[slot_index];
                for send in &mut slot.sends {
                    send.controls[frame].audible &= !muted[slot_index] && eligible[slot_index];
                }
            }

            let main_gain = automated_gain(overrides.main.gain, self.main_gain)?;
            let main_muted = overrides.main.mute.unwrap_or(self.main_muted);
            self.master_controls[frame] = if main_muted {
                LinearGain::SILENCE
            } else {
                main_gain
            };
        }
        Ok(())
    }

    fn process_sends(
        &mut self,
        slot_index: usize,
        tap: CompiledMixerTap,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) -> Result<(), RuntimeProcessError> {
        let send_count = self.slots[slot_index].sends.len();
        for send_index in 0..send_count {
            if self.slots[slot_index].sends[send_index].tap != tap {
                continue;
            }
            let input = if tap == CompiledMixerTap::PostFader {
                InputBlock::Stereo {
                    left: &self.channel_output.left[..frame_count],
                    right: &self.channel_output.right[..frame_count],
                }
            } else if self.slots[slot_index].kind == CompiledMixerTrackKind::Audio {
                scratch[slot_index].input_block()
            } else {
                InputBlock::Stereo {
                    left: &self.route_buffers[slot_index].left[..frame_count],
                    right: &self.route_buffers[slot_index].right[..frame_count],
                }
            };
            let inputs = [input];
            if self.automation.is_some() {
                let send = &mut self.slots[slot_index].sends[send_index];
                let controlled = [ControlledInputBlock {
                    input,
                    parameters: &send.controls[..frame_count],
                }];
                send.processor
                    .process_controlled(
                        &controlled,
                        &self.unity_master_controls[..frame_count],
                        StereoOutput {
                            left: &mut self.send_output.left[..frame_count],
                            right: &mut self.send_output.right[..frame_count],
                        },
                    )
                    .map_err(RuntimeProcessError::Mixer)?;
            } else {
                let parameters = [self.slots[slot_index].sends[send_index].parameters];
                self.slots[slot_index].sends[send_index]
                    .processor
                    .process(
                        &inputs,
                        &parameters,
                        LinearGain::UNITY,
                        StereoOutput {
                            left: &mut self.send_output.left[..frame_count],
                            right: &mut self.send_output.right[..frame_count],
                        },
                    )
                    .map_err(RuntimeProcessError::Mixer)?;
            }
            let target = self.slots[slot_index].sends[send_index].target_slot;
            accumulate(
                &mut self.route_buffers[target],
                &self.send_output,
                frame_count,
            );
        }
        Ok(())
    }

    fn process_channel(
        &mut self,
        slot_index: usize,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) -> Result<(), RuntimeProcessError> {
        let input = if self.slots[slot_index].kind == CompiledMixerTrackKind::Audio {
            scratch[slot_index].input_block()
        } else {
            InputBlock::Stereo {
                left: &self.route_buffers[slot_index].left[..frame_count],
                right: &self.route_buffers[slot_index].right[..frame_count],
            }
        };
        let inputs = [input];
        if self.automation.is_some() {
            let slot = &mut self.slots[slot_index];
            let controlled = [ControlledInputBlock {
                input,
                parameters: &slot.controls[..frame_count],
            }];
            slot.processor
                .process_controlled(
                    &controlled,
                    &self.unity_master_controls[..frame_count],
                    StereoOutput {
                        left: &mut self.channel_output.left[..frame_count],
                        right: &mut self.channel_output.right[..frame_count],
                    },
                )
                .map_err(RuntimeProcessError::Mixer)
        } else {
            let parameters = [self.slots[slot_index].parameters];
            self.slots[slot_index]
                .processor
                .process(
                    &inputs,
                    &parameters,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut self.channel_output.left[..frame_count],
                        right: &mut self.channel_output.right[..frame_count],
                    },
                )
                .map_err(RuntimeProcessError::Mixer)
        }
    }

    fn route_channel_output(&mut self, slot_index: usize, frame_count: usize) {
        match self.slots[slot_index].destination {
            CompiledMixerDestination::Main => {
                accumulate(&mut self.main_input, &self.channel_output, frame_count);
            }
            CompiledMixerDestination::Slot(target) => {
                accumulate(
                    &mut self.route_buffers[target],
                    &self.channel_output,
                    frame_count,
                );
            }
        }
    }
}

/// Single-owner transport state for the routed main-output engine.
///
/// Commands can be applied synchronously or through the optional bounded
/// cross-thread queue. This type does not provide device I/O. Stopped cycles
/// produce silence and retain the current frame; rolling cycles advance only
/// after the complete routed window succeeds.
#[derive(Debug)]
pub struct RoutedTransportRuntime<R> {
    runtime: RoutedMainRuntime<R>,
    state: TransportState,
    position_frame: usize,
    command_consumer: Option<Consumer<TransportCommand>>,
}

pub type GraphTransportRuntime<'prepared, 'cache, 'schedule> =
    RoutedTransportRuntime<ArrangementRenderer<'prepared, 'cache, 'schedule>>;

pub type PreparedPlaybackTransportRuntime<'prepared, 'cache, 'graph, 'project> =
    RoutedTransportRuntime<PlaybackRenderer<'prepared, 'graph, 'cache, 'graph, 'project>>;

impl<R: TrackWindowRenderer> RoutedTransportRuntime<R> {
    #[must_use]
    pub const fn new(runtime: RoutedMainRuntime<R>) -> Self {
        Self {
            runtime,
            state: TransportState::Stopped,
            position_frame: 0,
            command_consumer: None,
        }
    }

    /// Creates a transport and its application-thread command sender.
    ///
    /// Queue storage is allocated once during construction. The sender and
    /// process-cycle receiver are wait-free after construction.
    #[must_use]
    pub fn with_command_queue(runtime: RoutedMainRuntime<R>) -> (TransportCommandSender, Self) {
        let (producer, consumer) = RingBuffer::new(MAX_QUEUED_TRANSPORT_COMMANDS);
        (
            TransportCommandSender { producer },
            Self {
                runtime,
                state: TransportState::Stopped,
                position_frame: 0,
                command_consumer: Some(consumer),
            },
        )
    }

    #[must_use]
    pub const fn state(&self) -> TransportState {
        self.state
    }

    #[must_use]
    pub const fn position_frame(&self) -> usize {
        self.position_frame
    }

    #[must_use]
    pub const fn max_block_frames(&self) -> usize {
        self.runtime.max_block_frames()
    }

    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.runtime.slot_count()
    }

    /// Applies one transport command for the next process cycle.
    pub fn apply_command(&mut self, command: TransportCommand) {
        match command {
            TransportCommand::Start => self.state = TransportState::Rolling,
            TransportCommand::Stop => self.state = TransportState::Stopped,
            TransportCommand::Locate { frame, state } => {
                self.position_frame = frame;
                self.runtime.seek(frame);
                match state {
                    LocateState::Preserve => {}
                    LocateState::Stopped => self.state = TransportState::Stopped,
                    LocateState::Rolling => self.state = TransportState::Rolling,
                }
            }
        }
    }

    /// Processes the next transport-owned callback window.
    ///
    /// Output shape and block capacity are validated before queued commands are
    /// consumed. A valid cycle applies a bounded FIFO batch before it captures
    /// the cycle's state and start frame.
    ///
    /// Stopped cycles clear the output without reading arrangement scratch or
    /// advancing the position. Rolling cycles render from the current absolute
    /// frame and publish the next position only after successful processing.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] for output shape, block capacity, frame
    /// range, rendering, automation, or mixer failures. A failed cycle never
    /// advances the transport position.
    pub fn process_cycle(
        &mut self,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<TransportCycle, RuntimeProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(RuntimeProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        if frame_count > self.runtime.max_block_frames() {
            return Err(RuntimeProcessError::BlockLimit {
                actual: frame_count,
                maximum: self.runtime.max_block_frames(),
            });
        }
        let applied_command_count = self.drain_commands();
        let command_sender_disconnected = self.command_sender_disconnected();
        let start_frame = self.position_frame;
        if self.state == TransportState::Stopped {
            output.left.fill(0.0);
            output.right.fill(0.0);
            return Ok(TransportCycle {
                state: self.state,
                start_frame,
                frame_count,
                active_slot_count: 0,
                applied_command_count,
                command_sender_disconnected,
            });
        }
        let next_frame = start_frame
            .checked_add(frame_count)
            .ok_or(RuntimeProcessError::FrameRangeOverflow)?;
        let rendered = self.runtime.process_window(start_frame, scratch, output)?;
        self.position_frame = next_frame;
        Ok(TransportCycle {
            state: self.state,
            start_frame,
            frame_count,
            active_slot_count: rendered.active_slot_count,
            applied_command_count,
            command_sender_disconnected,
        })
    }

    /// Processes, interleaves, meters, and publishes one transport callback.
    ///
    /// Audio success is independent from meter-channel availability. Queue
    /// saturation or receiver disconnection is returned as publication status
    /// while the successfully processed audio and snapshot remain available.
    ///
    /// # Errors
    ///
    /// Returns [`StereoCallbackError`] for invalid interleaved output shape,
    /// callback capacity, or an underlying transport processing failure. Every
    /// such failure follows the callback adapter's fail-silent policy and
    /// publishes no meter snapshot.
    pub fn process_interleaved_cycle(
        &mut self,
        callback: &mut InterleavedStereoCallback,
        scratch: &mut [RenderScratchSlot<'_>],
        interleaved_output: &mut [f32],
        meter_sender: &mut OutputMeterSender,
    ) -> Result<InterleavedTransportCycle, StereoCallbackError<RuntimeProcessError>> {
        let processed = callback.process(interleaved_output, |output| {
            self.process_cycle(scratch, output)
        })?;
        let (cycle, meter) = processed.into_parts();
        let snapshot = OutputMeterSnapshot {
            cycle,
            meter,
            dropped_before: 0,
        };
        let (snapshot, publication) = match meter_sender.try_publish(snapshot) {
            Ok(snapshot) => (snapshot, OutputMeterPublication::Published),
            Err(OutputMeterPublishError::Full(snapshot)) => {
                (snapshot, OutputMeterPublication::QueueFull)
            }
            Err(OutputMeterPublishError::Disconnected(_)) => {
                (snapshot, OutputMeterPublication::ReceiverDisconnected)
            }
        };
        Ok(InterleavedTransportCycle {
            snapshot,
            publication,
        })
    }

    fn drain_commands(&mut self) -> usize {
        let mut applied = 0;
        for _ in 0..MAX_QUEUED_TRANSPORT_COMMANDS {
            let command = self
                .command_consumer
                .as_mut()
                .and_then(|consumer| consumer.pop().ok());
            let Some(command) = command else {
                break;
            };
            self.apply_command(command);
            applied += 1;
        }
        applied
    }

    fn command_sender_disconnected(&self) -> bool {
        let disconnected = self
            .command_consumer
            .as_ref()
            .is_some_and(Consumer::is_abandoned);
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportOutputCreationError {
    Callback(CallbackAdapterCreationError),
    ScratchSlotCountMismatch {
        expected: usize,
        actual: usize,
    },
    ScratchCapacity {
        slot: usize,
        required: usize,
        left: usize,
        right: usize,
    },
}

impl fmt::Display for TransportOutputCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Callback(source) => write!(formatter, "callback construction failed: {source}"),
            Self::ScratchSlotCountMismatch { expected, actual } => write!(
                formatter,
                "transport output requires {expected} scratch slots; {actual} were provided"
            ),
            Self::ScratchCapacity {
                slot,
                required,
                left,
                right,
            } => write!(
                formatter,
                "transport output scratch slot {slot} requires {required} frames; left={left}, right={right}"
            ),
        }
    }
}

impl Error for TransportOutputCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Callback(source) => Some(source),
            Self::ScratchSlotCountMismatch { .. } | Self::ScratchCapacity { .. } => None,
        }
    }
}

/// Device-independent owner of one transport's interleaved output path.
///
/// Construction preallocates planar callback storage and verifies that every
/// render scratch slot can serve the transport's maximum block. Processing
/// reuses those capacities for variable callback quanta and publishes meters
/// through the existing bounded channel.
#[derive(Debug)]
pub struct TransportOutputProcessor<'state, 'buffers, R> {
    transport: &'state mut RoutedTransportRuntime<R>,
    callback: InterleavedStereoCallback,
    scratch: &'state mut [RenderScratchSlot<'buffers>],
    meter_sender: &'state mut OutputMeterSender,
}

impl<'state, 'buffers, R: TrackWindowRenderer> TransportOutputProcessor<'state, 'buffers, R> {
    /// Binds transport, scratch, interleaving, and meter publication once.
    ///
    /// # Errors
    ///
    /// Returns [`TransportOutputCreationError`] when callback storage cannot be
    /// allocated or scratch slot count/capacity cannot serve every callback.
    pub fn try_new(
        transport: &'state mut RoutedTransportRuntime<R>,
        scratch: &'state mut [RenderScratchSlot<'buffers>],
        meter_sender: &'state mut OutputMeterSender,
    ) -> Result<Self, TransportOutputCreationError> {
        let expected_slots = transport.slot_count();
        if scratch.len() != expected_slots {
            return Err(TransportOutputCreationError::ScratchSlotCountMismatch {
                expected: expected_slots,
                actual: scratch.len(),
            });
        }
        let required = transport.max_block_frames();
        for (slot, scratch_slot) in scratch.iter().enumerate() {
            let left = scratch_slot.left_capacity_frames();
            let right = scratch_slot.right_capacity_frames();
            if left < required || right < required {
                return Err(TransportOutputCreationError::ScratchCapacity {
                    slot,
                    required,
                    left,
                    right,
                });
            }
        }
        let callback = InterleavedStereoCallback::try_new(required)
            .map_err(TransportOutputCreationError::Callback)?;
        Ok(Self {
            transport,
            callback,
            scratch,
            meter_sender,
        })
    }

    #[must_use]
    pub fn maximum_block_frames(&self) -> usize {
        self.callback.maximum_frames()
    }

    /// Processes one variable-size interleaved stereo callback.
    ///
    /// # Errors
    ///
    /// Returns [`StereoCallbackError`] for invalid device-buffer shape,
    /// excessive quantum, or an underlying transport failure. Every failure
    /// replaces the full device buffer with silence.
    pub fn process(
        &mut self,
        interleaved_output: &mut [f32],
    ) -> Result<InterleavedTransportCycle, StereoCallbackError<RuntimeProcessError>> {
        self.transport.process_interleaved_cycle(
            &mut self.callback,
            self.scratch,
            interleaved_output,
            self.meter_sender,
        )
    }
}

fn automated_gain(value: Option<i32>, base: LinearGain) -> Result<LinearGain, RuntimeProcessError> {
    let Some(value) = value else {
        return Ok(base);
    };
    let value = u32::try_from(value).map_err(|_| RuntimeProcessError::AutomationValueMismatch)?;
    LinearGain::from_millionths(value).map_err(RuntimeProcessError::Parameter)
}

fn automated_pan(value: Option<i32>, base: Pan) -> Result<Pan, RuntimeProcessError> {
    value.map_or(Ok(base), |value| {
        Pan::from_millionths(value).map_err(RuntimeProcessError::Parameter)
    })
}

fn accumulate(destination: &mut StereoBuffer, source: &StereoBuffer, frame_count: usize) {
    for frame in 0..frame_count {
        destination.left[frame] += source.left[frame];
        destination.right[frame] += source.right[frame];
    }
}
