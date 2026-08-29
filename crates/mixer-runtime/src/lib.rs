#![doc = "Fixed-track arrangement-to-main-mixer execution for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::Arc;

use hyper_arrangement_renderer::{
    ArrangementRenderer, PlaybackRenderError, PlaybackRenderer, PlaybackRendererCreationError,
    RenderScratchSlot, RenderWindowError, RenderedWindow, RendererCreationError,
};
use hyper_audio_engine::{
    CallbackAdapterCreationError, ChannelParameters, ControlledInputBlock, InputBlock,
    InterleavedStereoCallback, LinearGain, LoudnessMeasurementCommand, MAX_PROCESS_FRAMES,
    MixerCreationError, Pan, ParameterError, ProcessError, StereoBlockMeter, StereoCallbackError,
    StereoMixer, StereoOutput,
};
use hyper_media_cache::{PreparedPlayback, PreparedSceneAvailability, PreparedSchedule};
use hyper_native_device::{
    DeviceChain, MAX_DEVICES_PER_CHAIN, NativeDeviceChainProcessor, NativeDeviceCreationError,
    NativeDeviceProcessError, NativeDeviceState, PROJECT_SAMPLE_RATE_HZ,
};
use hyper_playback_compiler::FrameTimelineError;
use hyper_playback_compiler::scene::{SceneRuntimeCheckpoint, SceneRuntimeError};
use hyper_playback_json::{
    AutomationChannelParameter, AutomationMainParameter, AutomationSendParameter, AutomationTarget,
    AutomationValue, CompiledAutomation, CompiledMainMixer, CompiledMixerDestination,
    CompiledMixerGraph, CompiledMixerTap, CompiledMixerTrackKind,
};
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use sha2::{Digest, Sha256};

use crate::metronome::Metronome;

mod automation_capture;
mod loudness_presentation;
mod meter_ballistics;
mod metronome;
mod recovery;
mod scene_capture;
pub use automation_capture::{
    InteractiveMixerParameterDrain, InteractiveMixerParameterObservation,
    InteractiveMixerParameterPublication, InteractiveMixerParameterPublisher,
    InteractiveMixerParameterReceiver, interactive_mixer_parameter_channel,
};
pub use hyper_playback_compiler::scene::{InteractiveSceneLaunch, SceneLaunchState};
pub use loudness_presentation::{
    EBU_TARGET_LOUDNESS_LUFS, EbuLoudnessPresentationConfig, EbuLoudnessScale, LoudnessDisplayUnit,
    LoudnessPresentationSnapshot, LoudnessScaleRegion, MaximumTruePeakDisplay, ScaledLoudnessValue,
};
pub use meter_ballistics::{
    ChannelSamplePeakBallistics, ChannelSamplePeakDisplay, MixerStripSamplePeakBallistics,
    MixerStripSamplePeakBallisticsSnapshot, SamplePeakBallisticsConfig,
    SamplePeakBallisticsConfigError, StereoSamplePeakBallistics,
    StereoSamplePeakBallisticsSnapshot, StereoSamplePeakDisplay,
};
pub use metronome::{
    MetronomeGain, MetronomeGainError, MetronomePlan, MetronomePlanError, MetronomeProcessError,
};
pub use recovery::{
    InitialMetronomeState, PlaybackCycleObservation, PlaybackRecoveryCheckpoint,
    PlaybackRecoveryPublisher, PlaybackRecoverySnapshot, PlaybackRecoveryTransportState,
    playback_recovery_channel, playback_recovery_channel_with_initial_loop,
    playback_recovery_channel_with_initial_loop_and_metronome,
    playback_recovery_channel_with_initial_loop_metronome_and_native_device_state,
};
pub use scene_capture::{
    InteractiveSceneLaunchDrain, InteractiveSceneLaunchObservation,
    InteractiveSceneLaunchPublication, InteractiveSceneLaunchPublisher,
    InteractiveSceneLaunchReceiver, interactive_scene_launch_channel,
};

pub const MAX_MAIN_MIXER_SLOTS: usize = 8;
pub const MAX_MIXER_SENDS_PER_SLOT: usize = 2;
pub const MAX_QUEUED_TRANSPORT_COMMANDS: usize = 64;
pub const MAX_QUEUED_MIXER_PARAMETER_COMMANDS: usize = 64;
pub const MAX_QUEUED_NATIVE_DEVICE_COMMANDS: usize = 64;
pub const MAX_QUEUED_LOUDNESS_COMMANDS: usize = 32;
pub const MAX_QUEUED_OUTPUT_METER_SNAPSHOTS: usize = 64;
/// Bounded, loss-detecting control-thread capture queue for committed scene launches.
pub const MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES: usize = 256;
/// Bounded, loss-detecting control-thread queue for committed live mixer changes.
///
/// This holds four complete worst-case mixer command batches. The capture path
/// treats any excess as an explicit incomplete capture rather than discarding
/// timing evidence silently.
pub const MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS: usize = 256;
const ALL_MAIN_MIXER_SLOT_INDICES: [usize; MAX_MAIN_MIXER_SLOTS] = [0, 1, 2, 3, 4, 5, 6, 7];
const MIXER_PARAMETER_LAYOUT_SIGNATURE_VERSION: &[u8] = b"hyper-mixer-parameter-layout-v1";
const NATIVE_DEVICE_LAYOUT_SIGNATURE_VERSION: &[u8] = b"hyper-native-device-layout-v1";
static NEXT_OUTPUT_METER_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MIXER_PARAMETER_LAYOUT_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_NATIVE_DEVICE_LAYOUT_ID: AtomicU64 = AtomicU64::new(1);

fn next_output_meter_channel_id() -> u64 {
    NEXT_OUTPUT_METER_CHANNEL_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("output meter channel identity space exhausted")
}

fn next_mixer_parameter_layout_id() -> u64 {
    NEXT_MIXER_PARAMETER_LAYOUT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("mixer parameter layout identity space exhausted")
}

fn next_native_device_layout_id() -> u64 {
    NEXT_NATIVE_DEVICE_LAYOUT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("native device layout identity space exhausted")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProjectComponent {
    Mixer,
    Automation,
    DeviceChain,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCreationError {
    ProjectMismatch { component: RuntimeProjectComponent },
    DuplicateDeviceChainTrack,
    SlotLimit { actual: usize, maximum: usize },
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    CapacityUnavailable,
    Renderer(RendererCreationError),
    PlaybackRenderer(PlaybackRendererCreationError),
    Mixer(MixerCreationError),
    NativeDevice(NativeDeviceCreationError),
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
                    RuntimeProjectComponent::DeviceChain => "device chain",
                }
            ),
            Self::DuplicateDeviceChainTrack => {
                formatter.write_str("more than one native device chain targets the same track")
            }
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
            Self::NativeDevice(source) => {
                write!(formatter, "native device construction failed: {source}")
            }
        }
    }
}

impl Error for RuntimeCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Renderer(source) => Some(source),
            Self::PlaybackRenderer(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::NativeDevice(source) => Some(source),
            Self::ProjectMismatch { .. }
            | Self::DuplicateDeviceChainTrack
            | Self::SlotLimit { .. }
            | Self::InvalidMaximumBlockFrames { .. }
            | Self::CapacityUnavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProcessError {
    OutputLengthMismatch {
        left: usize,
        right: usize,
    },
    ChannelTapOutputLengthMismatch {
        output_frames: usize,
        left: usize,
        right: usize,
    },
    ChannelTapUnavailable {
        channel_index: usize,
        channel_count: usize,
    },
    DuplicateChannelTap {
        channel_index: usize,
    },
    PreFaderChannelTapRequiresAudio {
        channel_index: usize,
    },
    BlockLimit {
        actual: usize,
        maximum: usize,
    },
    FrameRangeOverflow,
    Scene(SceneRuntimeError),
    TransportCommand(TransportCommandApplyError),
    Metronome(MetronomeProcessError),
    Automation(FrameTimelineError),
    AutomationValueMismatch,
    Parameter(ParameterError),
    Render(RenderWindowError),
    PlaybackRender(PlaybackRenderError),
    Mixer(ProcessError),
    NativeDevice(NativeDeviceProcessError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChannelTapPosition {
    PostDevicePreFader,
    PostFader,
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
            Self::ChannelTapOutputLengthMismatch {
                output_frames,
                left,
                right,
            } => write!(
                formatter,
                "runtime channel tap lengths differ from output: output={output_frames}, left={left}, right={right}"
            ),
            Self::ChannelTapUnavailable {
                channel_index,
                channel_count,
            } => write!(
                formatter,
                "runtime channel tap {channel_index} is outside {channel_count} compiled channels"
            ),
            Self::DuplicateChannelTap { channel_index } => {
                write!(
                    formatter,
                    "runtime channel tap {channel_index} was requested more than once"
                )
            }
            Self::PreFaderChannelTapRequiresAudio { channel_index } => write!(
                formatter,
                "runtime pre-fader channel tap {channel_index} does not target an audio channel"
            ),
            Self::BlockLimit { actual, maximum } => write!(
                formatter,
                "runtime block has {actual} frames; configured maximum is {maximum}"
            ),
            Self::FrameRangeOverflow => formatter.write_str("runtime frame range overflowed"),
            Self::Scene(source) => write!(formatter, "interactive scene launch failed: {source}"),
            Self::TransportCommand(source) => {
                write!(formatter, "transport command application failed: {source}")
            }
            Self::Metronome(source) => write!(formatter, "metronome rendering failed: {source}"),
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
            Self::NativeDevice(source) => write!(formatter, "native device failed: {source}"),
        }
    }
}

impl Error for RuntimeProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Render(source) => Some(source),
            Self::PlaybackRender(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::NativeDevice(source) => Some(source),
            Self::Automation(source) => Some(source),
            Self::Parameter(source) => Some(source),
            Self::Scene(source) => Some(source),
            Self::TransportCommand(source) => Some(source),
            Self::Metronome(source) => Some(source),
            Self::OutputLengthMismatch { .. }
            | Self::ChannelTapOutputLengthMismatch { .. }
            | Self::ChannelTapUnavailable { .. }
            | Self::DuplicateChannelTap { .. }
            | Self::PreFaderChannelTapRequiresAudio { .. }
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
    LaunchScene { scene_index: usize },
    StopInteractiveScenes,
    SetLoopRange(Option<TransportLoopRange>),
    SetLoopEnabled(bool),
    SetMetronomeEnabled(bool),
    SetMetronomeGain(MetronomeGain),
}

/// A half-open `[start, end)` span the transport repeats while looping.
/// Ardour and Maolan both keep the range separate from whether looping is
/// engaged, so a user can arm a range without the playhead moving.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLoopRange {
    start_frame: usize,
    end_frame: usize,
}

impl TransportLoopRange {
    /// # Errors
    ///
    /// Returns [`TransportLoopRangeError`] when the span is empty or inverted.
    /// An unusable range is refused rather than clamped, so a caller cannot
    /// believe it armed a loop the transport silently ignored.
    pub const fn new(
        start_frame: usize,
        end_frame: usize,
    ) -> Result<Self, TransportLoopRangeError> {
        if end_frame <= start_frame {
            return Err(TransportLoopRangeError::EmptyRange {
                start_frame,
                end_frame,
            });
        }
        Ok(Self {
            start_frame,
            end_frame,
        })
    }

    #[must_use]
    pub const fn start_frame(self) -> usize {
        self.start_frame
    }

    #[must_use]
    pub const fn end_frame(self) -> usize {
        self.end_frame
    }

    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.end_frame - self.start_frame
    }

    #[must_use]
    pub const fn contains(self, frame: usize) -> bool {
        frame >= self.start_frame && frame < self.end_frame
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportLoopRangeError {
    EmptyRange {
        start_frame: usize,
        end_frame: usize,
    },
}

impl fmt::Display for TransportLoopRangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::EmptyRange {
            start_frame,
            end_frame,
        } = self;
        write!(
            formatter,
            "loop range [{start_frame}, {end_frame}) is empty or inverted"
        )
    }
}

impl std::error::Error for TransportLoopRangeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportCommandSendError {
    Full(TransportCommand),
    Disconnected(TransportCommand),
    MissingScene {
        command: TransportCommand,
        available: usize,
    },
    LoopRangeShorterThanBlock {
        command: TransportCommand,
        block_frames: usize,
    },
    SceneUnavailable {
        command: TransportCommand,
        reason: PreparedSceneAvailability,
    },
    MetronomeUnavailable {
        command: TransportCommand,
    },
}

impl TransportCommandSendError {
    #[must_use]
    pub const fn command(self) -> TransportCommand {
        match self {
            Self::Full(command)
            | Self::Disconnected(command)
            | Self::MissingScene { command, .. }
            | Self::LoopRangeShorterThanBlock { command, .. }
            | Self::SceneUnavailable { command, .. }
            | Self::MetronomeUnavailable { command } => command,
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
            Self::MissingScene { available, .. } => write!(
                formatter,
                "scene launch target is missing; {available} scenes are available"
            ),
            Self::LoopRangeShorterThanBlock { block_frames, .. } => write!(
                formatter,
                "loop range is shorter than the {block_frames}-frame callback block"
            ),
            Self::SceneUnavailable { reason, .. } => match reason {
                PreparedSceneAvailability::Disabled => formatter.write_str("scene is disabled"),
                PreparedSceneAvailability::ConsolidationRequired => {
                    formatter.write_str("scene requires consolidation before interactive launch")
                }
                PreparedSceneAvailability::NotPrepared => {
                    formatter.write_str("scene audio is not prepared")
                }
                PreparedSceneAvailability::Ready => {
                    formatter.write_str("scene launch is unexpectedly unavailable")
                }
            },
            Self::MetronomeUnavailable { .. } => {
                formatter.write_str("metronome is unavailable for this transport")
            }
        }
    }
}

impl Error for TransportCommandSendError {}

/// A synchronous transport-command application failure.
///
/// This is distinct from [`TransportCommandSendError`]: direct callers do not
/// cross the bounded command queue, but they receive the same availability
/// refusal rather than an unobservable no-op.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportCommandApplyError {
    Scene(SceneRuntimeError),
    MetronomeUnavailable { command: TransportCommand },
}

impl fmt::Display for TransportCommandApplyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scene(source) => write!(formatter, "scene command failed: {source}"),
            Self::MetronomeUnavailable { .. } => {
                formatter.write_str("metronome is unavailable for this transport")
            }
        }
    }
}

impl Error for TransportCommandApplyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scene(source) => Some(source),
            Self::MetronomeUnavailable { .. } => None,
        }
    }
}

/// Opaque channel target bound to one prepared mixer runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerChannelHandle {
    layout_id: u64,
    slot: usize,
}

/// Opaque send target bound to one prepared mixer runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MixerSendHandle {
    layout_id: u64,
    slot: usize,
    send: usize,
}

/// Opaque native-device target bound to one prepared playback runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDeviceHandle {
    layout_id: u64,
    slot: usize,
    device: usize,
}

impl NativeDeviceHandle {
    #[must_use]
    pub const fn slot(self) -> usize {
        self.slot
    }

    #[must_use]
    pub const fn device(self) -> usize {
        self.device
    }
}

/// One session-only prepared native-device command.
///
/// The target is resolved from a prepared device-chain layout and is accepted
/// only at the next process-cycle boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceCommand {
    SetEnabled {
        device: NativeDeviceHandle,
        enabled: bool,
    },
    SetState {
        device: NativeDeviceHandle,
        state: NativeDeviceState,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceResolveError {
    DeviceNotFound,
}

impl fmt::Display for NativeDeviceResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("native device identity was not found")
    }
}

impl Error for NativeDeviceResolveError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceTargetError {
    ForeignDevice(NativeDeviceHandle),
    DeviceKindMismatch { device: NativeDeviceHandle },
}

impl fmt::Display for NativeDeviceTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignDevice(_) => {
                formatter.write_str("native device handle belongs to another runtime")
            }
            Self::DeviceKindMismatch { .. } => {
                formatter.write_str("native device state does not match the prepared device kind")
            }
        }
    }
}

impl Error for NativeDeviceTargetError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceCommandSendError {
    InvalidTarget {
        command: NativeDeviceCommand,
        source: NativeDeviceTargetError,
    },
    Full(NativeDeviceCommand),
    Disconnected(NativeDeviceCommand),
}

impl NativeDeviceCommandSendError {
    #[must_use]
    pub const fn command(self) -> NativeDeviceCommand {
        match self {
            Self::InvalidTarget { command, .. }
            | Self::Full(command)
            | Self::Disconnected(command) => command,
        }
    }
}

impl fmt::Display for NativeDeviceCommandSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget { source, .. } => {
                write!(formatter, "invalid native device target: {source}")
            }
            Self::Full(_) => formatter.write_str("native device command queue is full"),
            Self::Disconnected(_) => {
                formatter.write_str("native device command receiver is disconnected")
            }
        }
    }
}

impl Error for NativeDeviceCommandSendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTarget { source, .. } => Some(source),
            Self::Full(_) | Self::Disconnected(_) => None,
        }
    }
}

/// One live mixer-state change resolved to the runtime's fixed slot layout.
///
/// These commands change the base mixer state at a process-cycle boundary.
/// Compiled automation, when present at a sample, has precedence over that
/// base state for the same parameter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MixerParameterCommand {
    ChannelGain {
        channel: MixerChannelHandle,
        gain: LinearGain,
    },
    ChannelPan {
        channel: MixerChannelHandle,
        pan: Pan,
    },
    ChannelMute {
        channel: MixerChannelHandle,
        muted: bool,
    },
    ChannelSolo {
        channel: MixerChannelHandle,
        solo: bool,
    },
    SendGain {
        send: MixerSendHandle,
        gain: LinearGain,
    },
    SendPan {
        send: MixerSendHandle,
        pan: Pan,
    },
    SendEnabled {
        send: MixerSendHandle,
        enabled: bool,
    },
    MainGain {
        gain: LinearGain,
    },
    MainMute {
        muted: bool,
    },
    /// Opaque FIFO barrier allocated by the sole command producer.
    #[doc(hidden)]
    Fence(MixerParameterFence),
}

impl MixerParameterCommand {
    fn automation_change(self) -> Option<MixerParameterChange> {
        let (target, value) = match self {
            Self::ChannelGain { channel, gain } => (
                AutomationTarget::Channel {
                    slot: channel.slot,
                    parameter: AutomationChannelParameter::Gain,
                },
                AutomationValue::Integer(
                    i32::try_from(gain.to_millionths())
                        .expect("the bounded gain persistence domain fits i32"),
                ),
            ),
            Self::ChannelPan { channel, pan } => (
                AutomationTarget::Channel {
                    slot: channel.slot,
                    parameter: AutomationChannelParameter::Pan,
                },
                AutomationValue::Integer(pan.to_millionths()),
            ),
            Self::ChannelMute { channel, muted } => (
                AutomationTarget::Channel {
                    slot: channel.slot,
                    parameter: AutomationChannelParameter::Mute,
                },
                AutomationValue::Boolean(muted),
            ),
            Self::ChannelSolo { channel, solo } => (
                AutomationTarget::Channel {
                    slot: channel.slot,
                    parameter: AutomationChannelParameter::Solo,
                },
                AutomationValue::Boolean(solo),
            ),
            Self::SendGain { send, gain } => (
                AutomationTarget::Send {
                    slot: send.slot,
                    send: send.send,
                    parameter: AutomationSendParameter::Gain,
                },
                AutomationValue::Integer(
                    i32::try_from(gain.to_millionths())
                        .expect("the bounded gain persistence domain fits i32"),
                ),
            ),
            Self::SendPan { send, pan } => (
                AutomationTarget::Send {
                    slot: send.slot,
                    send: send.send,
                    parameter: AutomationSendParameter::Pan,
                },
                AutomationValue::Integer(pan.to_millionths()),
            ),
            Self::SendEnabled { send, enabled } => (
                AutomationTarget::Send {
                    slot: send.slot,
                    send: send.send,
                    parameter: AutomationSendParameter::Enabled,
                },
                AutomationValue::Boolean(enabled),
            ),
            Self::MainGain { gain } => (
                AutomationTarget::Main {
                    parameter: AutomationMainParameter::Gain,
                },
                AutomationValue::Integer(
                    i32::try_from(gain.to_millionths())
                        .expect("the bounded gain persistence domain fits i32"),
                ),
            ),
            Self::MainMute { muted } => (
                AutomationTarget::Main {
                    parameter: AutomationMainParameter::Mute,
                },
                AutomationValue::Boolean(muted),
            ),
            Self::Fence(_) => return None,
        };
        Some(MixerParameterChange { target, value })
    }
}

/// One live mixer parameter resolved to the compiled automation address.
///
/// This is kept private until its successful audio callback reaches the
/// output-owner commit boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MixerParameterChange {
    target: AutomationTarget,
    value: AutomationValue,
}

/// Bounded live parameter changes accepted during one callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MixerParameterChanges {
    entries: [Option<MixerParameterChange>; MAX_QUEUED_MIXER_PARAMETER_COMMANDS],
    count: usize,
    sequence: Option<u64>,
}

impl Default for MixerParameterChanges {
    fn default() -> Self {
        Self {
            entries: [None; MAX_QUEUED_MIXER_PARAMETER_COMMANDS],
            count: 0,
            sequence: None,
        }
    }
}

impl MixerParameterChanges {
    const fn is_empty(&self) -> bool {
        self.count == 0
    }

    const fn len(&self) -> usize {
        self.count
    }

    const fn sequence(&self) -> Option<u64> {
        self.sequence
    }

    fn iter(&self) -> impl Iterator<Item = MixerParameterChange> + '_ {
        self.entries[..self.count].iter().flatten().copied()
    }

    fn push(&mut self, change: MixerParameterChange) {
        debug_assert!(self.count < MAX_QUEUED_MIXER_PARAMETER_COMMANDS);
        self.entries[self.count] = Some(change);
        self.count += 1;
    }

    fn commit(&mut self, sequence: u64) {
        debug_assert!(!self.is_empty());
        debug_assert_eq!(self.sequence, None);
        self.sequence = Some(sequence);
    }
}

/// Opaque mixer barrier carried through the realtime command queue.
///
/// Only [`MixerParameterCommandSender::try_send_fence`] can construct a value,
/// which keeps the monotonically increasing sequence namespace single-owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[doc(hidden)]
pub struct MixerParameterFence {
    sequence: u64,
}

/// Base parameters for one send in a recoverable mixer-state snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerSendParameterState {
    parameters: ChannelParameters,
    enabled: bool,
}

impl Eq for MixerSendParameterState {}

impl MixerSendParameterState {
    #[must_use]
    pub(crate) const fn from_recovery_parts(parameters: ChannelParameters, enabled: bool) -> Self {
        Self {
            parameters,
            enabled,
        }
    }

    /// Builds one send's parameter state for tests. See
    /// [`MixerParameterStateSnapshot::for_test`].
    #[cfg(feature = "test-support")]
    #[must_use]
    pub const fn for_test(parameters: ChannelParameters, enabled: bool) -> Self {
        Self {
            parameters,
            enabled,
        }
    }

    #[must_use]
    pub const fn parameters(self) -> ChannelParameters {
        self.parameters
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

/// Base parameters for one channel in a recoverable mixer-state snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerChannelParameterState {
    parameters: ChannelParameters,
    muted: bool,
    solo: bool,
    sends: [MixerSendParameterState; MAX_MIXER_SENDS_PER_SLOT],
    send_count: usize,
}

impl Eq for MixerChannelParameterState {}

impl MixerChannelParameterState {
    #[must_use]
    pub(crate) const fn from_recovery_parts(
        parameters: ChannelParameters,
        muted: bool,
        solo: bool,
        sends: [MixerSendParameterState; MAX_MIXER_SENDS_PER_SLOT],
        send_count: usize,
    ) -> Option<Self> {
        if send_count <= MAX_MIXER_SENDS_PER_SLOT {
            Some(Self {
                parameters,
                muted,
                solo,
                sends,
                send_count,
            })
        } else {
            None
        }
    }

    /// Builds one channel's parameter state for tests. See
    /// [`MixerParameterStateSnapshot::for_test`].
    ///
    /// # Panics
    ///
    /// Panics when more sends are supplied than a channel slot can hold.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn for_test(
        parameters: ChannelParameters,
        muted: bool,
        solo: bool,
        sends: &[MixerSendParameterState],
    ) -> Self {
        assert!(
            sends.len() <= MAX_MIXER_SENDS_PER_SLOT,
            "a channel holds at most {MAX_MIXER_SENDS_PER_SLOT} sends"
        );
        let mut slots = [MixerSendParameterState::for_test(
            ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
            false,
        ); MAX_MIXER_SENDS_PER_SLOT];
        slots[..sends.len()].copy_from_slice(sends);
        Self {
            parameters,
            muted,
            solo,
            sends: slots,
            send_count: sends.len(),
        }
    }

    #[must_use]
    pub const fn parameters(self) -> ChannelParameters {
        self.parameters
    }

    #[must_use]
    pub const fn muted(self) -> bool {
        self.muted
    }

    #[must_use]
    pub const fn solo(self) -> bool {
        self.solo
    }

    #[must_use]
    pub const fn send_count(self) -> usize {
        self.send_count
    }

    #[must_use]
    pub const fn send(self, index: usize) -> Option<MixerSendParameterState> {
        if index < self.send_count {
            Some(self.sends[index])
        } else {
            None
        }
    }
}

/// Exact live mixer base state bound to one compiled parameter layout.
///
/// The layout signature is derived from persisted project, channel, route, and
/// send identities. It is stable when the same mixer graph is rebuilt but does
/// not make runtime-local command handles transferable between runtimes.
/// Snapshot construction remains private to the runtime so downstream crates
/// cannot forge restorable mixer state.
///
/// ```compile_fail
/// use hyper_mixer_runtime::MixerParameterStateSnapshot;
///
/// let _constructor = MixerParameterStateSnapshot::from_recovery_parts;
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerParameterStateSnapshot {
    layout_signature: [u8; 32],
    channels: [MixerChannelParameterState; MAX_MAIN_MIXER_SLOTS],
    channel_count: usize,
    main_gain: LinearGain,
    main_muted: bool,
}

impl Eq for MixerParameterStateSnapshot {}

impl MixerParameterStateSnapshot {
    #[must_use]
    pub(crate) const fn from_recovery_parts(
        layout_signature: [u8; 32],
        channels: &[MixerChannelParameterState; MAX_MAIN_MIXER_SLOTS],
        channel_count: usize,
        main_gain: LinearGain,
        main_muted: bool,
    ) -> Option<Self> {
        if channel_count <= MAX_MAIN_MIXER_SLOTS {
            Some(Self {
                layout_signature,
                channels: *channels,
                channel_count,
                main_gain,
                main_muted,
            })
        } else {
            None
        }
    }

    #[must_use]
    pub(crate) const fn layout_signature(&self) -> [u8; 32] {
        self.layout_signature
    }

    #[must_use]
    pub const fn channel_count(&self) -> usize {
        self.channel_count
    }

    #[must_use]
    pub const fn channel(&self, index: usize) -> Option<MixerChannelParameterState> {
        if index < self.channel_count {
            Some(self.channels[index])
        } else {
            None
        }
    }

    /// Builds one snapshot for tests in crates that read mixer state but cannot
    /// run a mixer.
    ///
    /// The layout signature is zeroed, so a snapshot built here is not
    /// restorable against any real runtime — it carries parameter values for
    /// readers to inspect and nothing more.
    ///
    /// # Panics
    ///
    /// Panics when more channels are supplied than a mixer can hold.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn for_test(
        channels: &[MixerChannelParameterState],
        main_gain: LinearGain,
        main_muted: bool,
    ) -> Self {
        assert!(
            channels.len() <= MAX_MAIN_MIXER_SLOTS,
            "a mixer holds at most {MAX_MAIN_MIXER_SLOTS} channels"
        );
        let mut slots = [MixerChannelParameterState::for_test(
            ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
            false,
            false,
            &[],
        ); MAX_MAIN_MIXER_SLOTS];
        slots[..channels.len()].copy_from_slice(channels);
        Self {
            layout_signature: [0; 32],
            channels: slots,
            channel_count: channels.len(),
            main_gain,
            main_muted,
        }
    }

    #[must_use]
    pub const fn main_gain(&self) -> LinearGain {
        self.main_gain
    }

    #[must_use]
    pub const fn main_muted(&self) -> bool {
        self.main_muted
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameterStateRestoreError {
    LayoutMismatch,
}

impl fmt::Display for MixerParameterStateRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("mixer parameter snapshot belongs to a different compiled layout")
    }
}

impl Error for MixerParameterStateRestoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameterResolveError {
    ChannelNotFound,
    SendNotFound,
}

impl fmt::Display for MixerParameterResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ChannelNotFound => "mixer channel identity was not found",
            Self::SendNotFound => "mixer send identity was not found",
        })
    }
}

impl Error for MixerParameterResolveError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerParameterTargetError {
    ForeignChannel(MixerChannelHandle),
    ForeignSend(MixerSendHandle),
    SoloUnavailable { channel: MixerChannelHandle },
}

impl fmt::Display for MixerParameterTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignChannel(_) => {
                formatter.write_str("mixer channel handle belongs to another runtime")
            }
            Self::ForeignSend(_) => {
                formatter.write_str("mixer send handle belongs to another runtime")
            }
            Self::SoloUnavailable { .. } => {
                formatter.write_str("mixer channel is not an audio channel")
            }
        }
    }
}

impl Error for MixerParameterTargetError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MixerParameterCommandSendError {
    InvalidTarget {
        command: MixerParameterCommand,
        source: MixerParameterTargetError,
    },
    Full(MixerParameterCommand),
    Disconnected(MixerParameterCommand),
}

impl MixerParameterCommandSendError {
    #[must_use]
    pub const fn command(self) -> MixerParameterCommand {
        match self {
            Self::InvalidTarget { command, .. }
            | Self::Full(command)
            | Self::Disconnected(command) => command,
        }
    }
}

impl fmt::Display for MixerParameterCommandSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTarget { source, .. } => {
                write!(formatter, "invalid mixer target: {source}")
            }
            Self::Full(_) => formatter.write_str("mixer parameter command queue is full"),
            Self::Disconnected(_) => {
                formatter.write_str("mixer parameter command receiver is disconnected")
            }
        }
    }
}

impl Error for MixerParameterCommandSendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidTarget { source, .. } => Some(source),
            Self::Full(_) | Self::Disconnected(_) => None,
        }
    }
}

#[derive(Debug)]
struct MixerParameterLayout {
    id: u64,
    signature: [u8; 32],
    channels: Vec<MixerParameterChannelLayout>,
}

#[derive(Debug)]
struct MixerParameterChannelLayout {
    track_id: String,
    handle: MixerChannelHandle,
    audio: bool,
    sends: Vec<MixerParameterSendLayout>,
}

#[derive(Debug)]
struct MixerParameterSendLayout {
    id: String,
    handle: MixerSendHandle,
}

impl MixerParameterLayout {
    fn resolve_channel_index(
        &self,
        channel_index: usize,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.channels
            .get(channel_index)
            .map(|channel| channel.handle)
            .ok_or(MixerParameterResolveError::ChannelNotFound)
    }

    fn channel_supports_solo(
        &self,
        channel_index: usize,
    ) -> Result<bool, MixerParameterResolveError> {
        self.channels
            .get(channel_index)
            .map(|channel| channel.audio)
            .ok_or(MixerParameterResolveError::ChannelNotFound)
    }

    fn resolve_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.channels
            .iter()
            .find(|channel| channel.track_id == track_id)
            .map(|channel| channel.handle)
            .ok_or(MixerParameterResolveError::ChannelNotFound)
    }

    fn resolve_send(&self, send_id: &str) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.channels
            .iter()
            .flat_map(|channel| &channel.sends)
            .find(|send| send.id == send_id)
            .map(|send| send.handle)
            .ok_or(MixerParameterResolveError::SendNotFound)
    }

    fn resolve_send_address(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.channels
            .get(channel_index)
            .and_then(|channel| channel.sends.get(send_index))
            .map(|send| send.handle)
            .ok_or(MixerParameterResolveError::SendNotFound)
    }

    fn channel_index(
        &self,
        channel: MixerChannelHandle,
    ) -> Result<usize, MixerParameterTargetError> {
        self.validate_channel(channel).map(|_| channel.slot)
    }

    fn send_address(
        &self,
        send: MixerSendHandle,
    ) -> Result<(usize, usize), MixerParameterTargetError> {
        self.validate_send(send).map(|()| (send.slot, send.send))
    }

    fn validate(&self, command: MixerParameterCommand) -> Result<(), MixerParameterTargetError> {
        match command {
            MixerParameterCommand::ChannelGain { channel, .. }
            | MixerParameterCommand::ChannelPan { channel, .. }
            | MixerParameterCommand::ChannelMute { channel, .. } => {
                self.validate_channel(channel).map(|_| ())
            }
            MixerParameterCommand::ChannelSolo { channel, .. } => {
                let resolved = self.validate_channel(channel)?;
                if resolved.audio {
                    Ok(())
                } else {
                    Err(MixerParameterTargetError::SoloUnavailable { channel })
                }
            }
            MixerParameterCommand::SendGain { send, .. }
            | MixerParameterCommand::SendPan { send, .. }
            | MixerParameterCommand::SendEnabled { send, .. } => self.validate_send(send),
            MixerParameterCommand::MainGain { .. } | MixerParameterCommand::MainMute { .. } => {
                Ok(())
            }
            MixerParameterCommand::Fence(_) => Ok(()),
        }
    }

    fn validate_channel(
        &self,
        channel: MixerChannelHandle,
    ) -> Result<&MixerParameterChannelLayout, MixerParameterTargetError> {
        self.channels
            .get(channel.slot)
            .filter(|resolved| self.id == channel.layout_id && resolved.handle == channel)
            .ok_or(MixerParameterTargetError::ForeignChannel(channel))
    }

    fn validate_send(&self, send: MixerSendHandle) -> Result<(), MixerParameterTargetError> {
        self.channels
            .get(send.slot)
            .and_then(|channel| channel.sends.get(send.send))
            .filter(|resolved| self.id == send.layout_id && resolved.handle == send)
            .map(|_| ())
            .ok_or(MixerParameterTargetError::ForeignSend(send))
    }
}

#[derive(Debug)]
struct NativeDeviceLayout {
    id: u64,
    signature: [u8; 32],
    slots: Vec<NativeDeviceSlotLayout>,
}

#[derive(Debug)]
struct NativeDeviceSlotLayout {
    track_id: String,
    devices: Vec<NativeDeviceLayoutEntry>,
}

#[derive(Debug)]
struct NativeDeviceLayoutEntry {
    id: String,
    handle: NativeDeviceHandle,
    state: NativeDeviceState,
}

impl NativeDeviceLayout {
    fn resolve_device(
        &self,
        track_id: &str,
        device_id: &str,
    ) -> Result<NativeDeviceHandle, NativeDeviceResolveError> {
        self.slots
            .iter()
            .find(|slot| slot.track_id == track_id)
            .and_then(|slot| slot.devices.iter().find(|device| device.id == device_id))
            .map(|device| device.handle)
            .ok_or(NativeDeviceResolveError::DeviceNotFound)
    }

    fn validate(&self, command: NativeDeviceCommand) -> Result<(), NativeDeviceTargetError> {
        let device = match command {
            NativeDeviceCommand::SetEnabled { device, .. }
            | NativeDeviceCommand::SetState { device, .. } => device,
        };
        let resolved = self
            .slots
            .get(device.slot)
            .and_then(|slot| slot.devices.get(device.device))
            .filter(|resolved| self.id == device.layout_id && resolved.handle == device)
            .ok_or(NativeDeviceTargetError::ForeignDevice(device))?;
        match command {
            NativeDeviceCommand::SetEnabled { .. } => Ok(()),
            NativeDeviceCommand::SetState { state, .. }
                if matches!(
                    (resolved.state, state),
                    (NativeDeviceState::Filter(_), NativeDeviceState::Filter(_))
                        | (
                            NativeDeviceState::Compressor(_),
                            NativeDeviceState::Compressor(_)
                        )
                ) =>
            {
                Ok(())
            }
            NativeDeviceCommand::SetState { .. } => {
                Err(NativeDeviceTargetError::DeviceKindMismatch { device })
            }
        }
    }
}

/// Exact session-only native-device state bound to one prepared layout.
///
/// The snapshot is private to the playback runtime's construction and recovery
/// path, preventing callers from forging state for another prepared graph.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeDeviceStateSnapshot {
    layout_signature: [u8; 32],
    enabled: [[bool; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS],
    states: [[Option<NativeDeviceState>; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS],
    device_counts: [usize; MAX_MAIN_MIXER_SLOTS],
}

impl NativeDeviceStateSnapshot {
    #[must_use]
    pub(crate) fn from_recovery_parts(
        layout_signature: [u8; 32],
        enabled: [[bool; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS],
        states: &[[Option<NativeDeviceState>; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS],
        device_counts: [usize; MAX_MAIN_MIXER_SLOTS],
    ) -> Option<Self> {
        if device_counts
            .iter()
            .all(|count| *count <= MAX_DEVICES_PER_CHAIN)
            && device_counts
                .iter()
                .enumerate()
                .all(|(slot, count)| states[slot][..*count].iter().all(Option::is_some))
        {
            Some(Self {
                layout_signature,
                enabled,
                states: *states,
                device_counts,
            })
        } else {
            None
        }
    }

    #[must_use]
    pub(crate) const fn layout_signature(&self) -> [u8; 32] {
        self.layout_signature
    }

    #[must_use]
    pub const fn device_count(&self, slot: usize) -> Option<usize> {
        if slot < MAX_MAIN_MIXER_SLOTS {
            Some(self.device_counts[slot])
        } else {
            None
        }
    }

    #[must_use]
    pub const fn enabled(&self, slot: usize, device: usize) -> Option<bool> {
        if slot < MAX_MAIN_MIXER_SLOTS && device < self.device_counts[slot] {
            Some(self.enabled[slot][device])
        } else {
            None
        }
    }

    /// Returns one committed prepared-device parameter state.
    #[must_use]
    pub const fn state(&self, slot: usize, device: usize) -> Option<NativeDeviceState> {
        if slot < MAX_MAIN_MIXER_SLOTS && device < self.device_counts[slot] {
            self.states[slot][device]
        } else {
            None
        }
    }
}

/// Application-thread half of the bounded native-device command queue.
#[derive(Debug)]
pub struct NativeDeviceCommandSender {
    producer: Producer<NativeDeviceCommand>,
    layout: NativeDeviceLayout,
}

impl NativeDeviceCommandSender {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_NATIVE_DEVICE_COMMANDS
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Resolves a persisted device identity within its persisted owning track.
    ///
    /// # Errors
    ///
    /// Returns [`NativeDeviceResolveError::DeviceNotFound`] when the prepared
    /// graph has no matching owned device instance.
    pub fn resolve_device(
        &self,
        track_id: &str,
        device_id: &str,
    ) -> Result<NativeDeviceHandle, NativeDeviceResolveError> {
        self.layout.resolve_device(track_id, device_id)
    }

    /// Validates a command against this sender's immutable prepared layout.
    ///
    /// This does not enqueue it.
    ///
    /// # Errors
    ///
    /// Returns [`NativeDeviceTargetError::ForeignDevice`] when the command was
    /// resolved by another prepared runtime.
    pub fn validate_command(
        &self,
        command: NativeDeviceCommand,
    ) -> Result<(), NativeDeviceTargetError> {
        self.layout.validate(command)
    }

    /// Attempts to enqueue one bypass command without waiting or allocating.
    ///
    /// # Errors
    ///
    /// Returns the original command when its target is foreign, the bounded
    /// queue is full, or the playback runtime has been destroyed.
    pub fn try_send(
        &mut self,
        command: NativeDeviceCommand,
    ) -> Result<(), NativeDeviceCommandSendError> {
        self.validate_command(command)
            .map_err(|source| NativeDeviceCommandSendError::InvalidTarget { command, source })?;
        if self.producer.is_abandoned() {
            fence(Ordering::Acquire);
            return Err(NativeDeviceCommandSendError::Disconnected(command));
        }
        self.producer.push(command).map_err(|error| match error {
            PushError::Full(command) => NativeDeviceCommandSendError::Full(command),
        })
    }
}

/// Application-thread half of the bounded live mixer-control queue.
#[derive(Debug)]
pub struct MixerParameterCommandSender {
    producer: Producer<MixerParameterCommand>,
    layout: MixerParameterLayout,
    committed_fence: Arc<AtomicU64>,
    next_fence_sequence: u64,
}

impl MixerParameterCommandSender {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_MIXER_PARAMETER_COMMANDS
    }

    /// Returns the immutable channel count in this sender's compiled layout.
    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.layout.channels.len()
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Returns the greatest fence sequence committed by a successful process
    /// cycle. Acquire ordering pairs with process-thread publication.
    #[must_use]
    pub fn committed_fence(&self) -> u64 {
        self.committed_fence.load(Ordering::Acquire)
    }

    /// Allocates and enqueues the next FIFO fence without waiting or allocating.
    ///
    /// Returns `Ok(None)` only after the nonzero sequence space is exhausted.
    /// Queue rejection does not consume a sequence.
    ///
    /// # Errors
    ///
    /// Returns the opaque fence command when the bounded queue is full or the
    /// receiving runtime has been destroyed.
    pub fn try_send_fence(&mut self) -> Result<Option<u64>, MixerParameterCommandSendError> {
        let sequence = self.next_fence_sequence;
        if sequence == 0 {
            return Ok(None);
        }
        self.try_send(MixerParameterCommand::Fence(MixerParameterFence {
            sequence,
        }))?;
        self.next_fence_sequence = sequence.checked_add(1).unwrap_or(0);
        Ok(Some(sequence))
    }

    /// Resolves one persisted track identity into a handle bound to this runtime.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterResolveError::ChannelNotFound`] when the
    /// prepared mixer has no channel for `track_id`.
    pub fn resolve_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.layout.resolve_channel(track_id)
    }

    /// Resolves one compiled mixer slot into a handle bound to this runtime.
    ///
    /// This numeric address is valid only when it was compiled from the same
    /// immutable mixer graph used to construct this sender.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterResolveError::ChannelNotFound`] when
    /// `channel_index` is outside the prepared mixer layout.
    pub fn resolve_channel_index(
        &self,
        channel_index: usize,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.layout.resolve_channel_index(channel_index)
    }

    /// Reports whether one compiled mixer slot accepts live Solo commands.
    ///
    /// This capability is retained by the same immutable layout that resolves
    /// the channel handle, so clients do not need to duplicate track-kind
    /// policy.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterResolveError::ChannelNotFound`] when
    /// `channel_index` is outside the prepared mixer layout.
    pub fn channel_supports_solo(
        &self,
        channel_index: usize,
    ) -> Result<bool, MixerParameterResolveError> {
        self.layout.channel_supports_solo(channel_index)
    }

    /// Resolves one persisted send identity into a handle bound to this runtime.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterResolveError::SendNotFound`] when the prepared
    /// mixer has no send with `send_id`.
    pub fn resolve_send(
        &self,
        send_id: &str,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.layout.resolve_send(send_id)
    }

    /// Resolves one compiled channel/send address into a handle bound to this
    /// runtime.
    ///
    /// This numeric address is valid only when it was compiled from the same
    /// immutable mixer graph used to construct this sender.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterResolveError::SendNotFound`] when either index
    /// does not identify a prepared send.
    pub fn resolve_send_address(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.layout.resolve_send_address(channel_index, send_index)
    }

    /// Returns the compiled numeric address for an opaque channel handle bound
    /// to this runtime.
    ///
    /// # Errors
    ///
    /// Returns a foreign-channel rejection for a handle from another runtime.
    pub fn channel_index(
        &self,
        channel: MixerChannelHandle,
    ) -> Result<usize, MixerParameterTargetError> {
        self.layout.channel_index(channel)
    }

    /// Returns the compiled numeric address for an opaque send handle bound to
    /// this runtime.
    ///
    /// # Errors
    ///
    /// Returns a foreign-send rejection for a handle from another runtime.
    pub fn send_address(
        &self,
        send: MixerSendHandle,
    ) -> Result<(usize, usize), MixerParameterTargetError> {
        self.layout.send_address(send)
    }

    /// Validates one command against this runtime's opaque target layout
    /// without inspecting queue state or enqueueing it.
    ///
    /// # Errors
    ///
    /// Returns a foreign channel/send or unavailable-solo rejection.
    pub fn validate_command(
        &self,
        command: MixerParameterCommand,
    ) -> Result<(), MixerParameterTargetError> {
        self.layout.validate(command)
    }

    /// Validates and enqueues one base-state change without waiting or allocating.
    ///
    /// # Errors
    ///
    /// Returns the original command for an invalid target, a full queue, or a
    /// destroyed receiving runtime. Target validation precedes queue state.
    pub fn try_send(
        &mut self,
        command: MixerParameterCommand,
    ) -> Result<(), MixerParameterCommandSendError> {
        self.validate_command(command)
            .map_err(|source| MixerParameterCommandSendError::InvalidTarget { command, source })?;
        if self.is_disconnected() {
            return Err(MixerParameterCommandSendError::Disconnected(command));
        }
        self.producer.push(command).map_err(|error| match error {
            PushError::Full(command) => MixerParameterCommandSendError::Full(command),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoudnessCommandSendError {
    Full(LoudnessMeasurementCommand),
    Disconnected(LoudnessMeasurementCommand),
}

impl LoudnessCommandSendError {
    #[must_use]
    pub const fn command(self) -> LoudnessMeasurementCommand {
        match self {
            Self::Full(command) | Self::Disconnected(command) => command,
        }
    }
}

impl fmt::Display for LoudnessCommandSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => formatter.write_str("loudness command queue is full"),
            Self::Disconnected(_) => {
                formatter.write_str("loudness command receiver is disconnected")
            }
        }
    }
}

impl Error for LoudnessCommandSendError {}

/// Application-thread half of the bounded live-loudness control queue.
#[derive(Debug)]
pub struct LoudnessCommandSender {
    producer: Producer<LoudnessMeasurementCommand>,
}

impl LoudnessCommandSender {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_LOUDNESS_COMMANDS
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Attempts to enqueue one run/standby/reset command without waiting or
    /// allocating.
    ///
    /// # Errors
    ///
    /// Returns the original command when the fixed queue is full or its audio
    /// owner has been destroyed.
    pub fn try_send(
        &mut self,
        command: LoudnessMeasurementCommand,
    ) -> Result<(), LoudnessCommandSendError> {
        if self.is_disconnected() {
            return Err(LoudnessCommandSendError::Disconnected(command));
        }
        self.producer.push(command).map_err(|error| match error {
            PushError::Full(command) => LoudnessCommandSendError::Full(command),
        })
    }
}

#[derive(Debug)]
pub struct LoudnessCommandReceiver {
    consumer: Consumer<LoudnessMeasurementCommand>,
}

/// Allocates one fixed live-loudness command queue before callback processing.
#[must_use]
pub fn loudness_command_channel() -> (LoudnessCommandSender, LoudnessCommandReceiver) {
    let (producer, consumer) = RingBuffer::new(MAX_QUEUED_LOUDNESS_COMMANDS);
    (
        LoudnessCommandSender { producer },
        LoudnessCommandReceiver { consumer },
    )
}

#[derive(Clone, Copy, Debug)]
enum QueuedTransportCommand {
    Transport(TransportCommand),
    CaptureStart { sequence: u64 },
}

/// Application-thread half of the bounded transport command queue.
#[derive(Debug)]
pub struct TransportCommandSender {
    producer: Producer<QueuedTransportCommand>,
    max_block_frames: usize,
    scene_count: usize,
    scene_ready_mask: u8,
    scene_disabled_mask: u8,
    scene_consolidation_mask: u8,
    metronome_available: bool,
    next_capture_start_sequence: u64,
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

    /// Allocates and enqueues one capture-specific start request without
    /// waiting or allocating.
    ///
    /// The returned sequence is carried by the same FIFO entry as the start
    /// command. A successful output cycle exposes it through
    /// [`TransportCycle::capture_start_sequence`].
    ///
    /// Returns `Ok(None)` only after the nonzero sequence space is exhausted.
    /// Queue rejection does not consume a sequence.
    ///
    /// # Errors
    ///
    /// Returns a start-command rejection when the bounded queue is full or
    /// the receiving transport has been destroyed.
    pub fn try_send_capture_start(&mut self) -> Result<Option<u64>, TransportCommandSendError> {
        let sequence = self.next_capture_start_sequence;
        if sequence == 0 {
            return Ok(None);
        }
        if self.is_disconnected() {
            return Err(TransportCommandSendError::Disconnected(
                TransportCommand::Start,
            ));
        }
        self.producer
            .push(QueuedTransportCommand::CaptureStart { sequence })
            .map_err(|_| TransportCommandSendError::Full(TransportCommand::Start))?;
        self.next_capture_start_sequence = sequence.checked_add(1).unwrap_or(0);
        Ok(Some(sequence))
    }

    /// Returns the immutable number of validated scene targets in this command domain.
    #[must_use]
    pub const fn scene_count(&self) -> usize {
        self.scene_count
    }

    /// Whether this transport was prepared with a callback-safe metronome plan.
    #[must_use]
    pub const fn metronome_available(&self) -> bool {
        self.metronome_available
    }

    /// Returns the prepared launch status for a scene, or `None` outside the validated domain.
    #[must_use]
    pub fn scene_availability(&self, scene_index: usize) -> Option<PreparedSceneAvailability> {
        if scene_index >= self.scene_count {
            return None;
        }
        let bit = 1_u8 << scene_index;
        Some(if self.scene_ready_mask & bit != 0 {
            PreparedSceneAvailability::Ready
        } else if self.scene_disabled_mask & bit != 0 {
            PreparedSceneAvailability::Disabled
        } else if self.scene_consolidation_mask & bit != 0 {
            PreparedSceneAvailability::ConsolidationRequired
        } else {
            PreparedSceneAvailability::NotPrepared
        })
    }

    /// Validates command-domain targets without enqueueing.
    ///
    /// # Errors
    ///
    /// Returns a missing or unavailable scene rejection.
    pub fn validate_command(
        &self,
        command: TransportCommand,
    ) -> Result<(), TransportCommandSendError> {
        // A range shorter than one callback would force a full graph render and
        // a scene seek for every `range` frames of every block. The bound is the
        // block the runtime was built for, not an invented constant.
        if let TransportCommand::SetLoopRange(Some(range)) = command
            && range.frame_count() < self.max_block_frames
        {
            return Err(TransportCommandSendError::LoopRangeShorterThanBlock {
                command,
                block_frames: self.max_block_frames,
            });
        }
        if let TransportCommand::LaunchScene { scene_index } = command {
            let Some(reason) = self.scene_availability(scene_index) else {
                return Err(TransportCommandSendError::MissingScene {
                    command,
                    available: self.scene_count,
                });
            };
            if reason != PreparedSceneAvailability::Ready {
                return Err(TransportCommandSendError::SceneUnavailable { command, reason });
            }
        }
        if matches!(
            command,
            TransportCommand::SetMetronomeEnabled(_) | TransportCommand::SetMetronomeGain(_)
        ) && !self.metronome_available
        {
            return Err(TransportCommandSendError::MetronomeUnavailable { command });
        }
        Ok(())
    }

    /// Attempts to enqueue one command without waiting or allocating.
    ///
    /// # Errors
    ///
    /// Returns the original command for an invalid/unavailable scene, when the
    /// fixed queue is full, or when the receiving transport was destroyed.
    pub fn try_send(&mut self, command: TransportCommand) -> Result<(), TransportCommandSendError> {
        self.validate_command(command)?;
        if self.is_disconnected() {
            return Err(TransportCommandSendError::Disconnected(command));
        }
        self.producer
            .push(QueuedTransportCommand::Transport(command))
            .map_err(|_| TransportCommandSendError::Full(command))
    }
}

/// Result of one device-independent transport processing cycle.
///
/// Queue diagnostics describe commands applied at the beginning of this cycle.
#[allow(
    clippy::struct_excessive_bools,
    reason = "the cycle is a flat, copyable publication of independent committed transport facts"
)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportCycle {
    state: TransportState,
    start_frame: usize,
    position_frame: usize,
    frame_count: usize,
    loop_wrap_count: usize,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
    metronome_available: bool,
    metronome_enabled: bool,
    metronome_gain: MetronomeGain,
    active_slot_count: usize,
    scene_launch_state: SceneLaunchState,
    applied_command_count: usize,
    capture_start_sequence: u64,
    command_sender_disconnected: bool,
    applied_mixer_parameter_command_count: usize,
    mixer_parameter_command_sender_disconnected: bool,
    applied_native_device_command_count: usize,
    native_device_command_sender_disconnected: bool,
}

impl TransportCycle {
    /// How many times the playhead returned to the loop start during this
    /// callback. Zero for a linear cycle.
    #[must_use]
    pub const fn loop_wrap_count(self) -> usize {
        self.loop_wrap_count
    }

    /// The armed loop range after commands accepted by this cycle.
    #[must_use]
    pub const fn loop_range(self) -> Option<TransportLoopRange> {
        self.loop_range
    }

    /// Whether loop playback is engaged after commands accepted by this cycle.
    #[must_use]
    pub const fn loop_enabled(self) -> bool {
        self.loop_enabled
    }

    /// Whether this cycle's transport has a prepared metronome plan.
    #[must_use]
    pub const fn metronome_available(self) -> bool {
        self.metronome_available
    }

    /// Whether the metronome was enabled after this cycle's FIFO commands.
    #[must_use]
    pub const fn metronome_enabled(self) -> bool {
        self.metronome_enabled
    }

    /// Returns the committed session-only metronome gain when a prepared plan
    /// is available for this transport.
    #[must_use]
    pub const fn metronome_gain(self) -> Option<MetronomeGain> {
        if self.metronome_available {
            Some(self.metronome_gain)
        } else {
            None
        }
    }

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
        self.position_frame
    }

    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    #[must_use]
    pub const fn active_slot_count(self) -> usize {
        self.active_slot_count
    }

    /// Returns the committed interactive scene state after this cycle.
    #[must_use]
    pub const fn scene_launch_state(self) -> SceneLaunchState {
        self.scene_launch_state
    }

    #[must_use]
    /// Transport commands applied at the beginning of this successful cycle.
    pub const fn applied_command_count(self) -> usize {
        self.applied_command_count
    }

    /// The capture-start sequence committed by this successful cycle, or zero
    /// when it did not accept a capture-specific start request.
    #[must_use]
    pub const fn capture_start_sequence(self) -> u64 {
        self.capture_start_sequence
    }

    #[must_use]
    pub const fn command_sender_disconnected(self) -> bool {
        self.command_sender_disconnected
    }

    #[must_use]
    /// Live mixer base-state commands applied at the beginning of this cycle.
    pub const fn applied_mixer_parameter_command_count(self) -> usize {
        self.applied_mixer_parameter_command_count
    }

    #[must_use]
    pub const fn mixer_parameter_command_sender_disconnected(self) -> bool {
        self.mixer_parameter_command_sender_disconnected
    }

    /// Session-only native-device commands accepted at this cycle boundary.
    ///
    /// Multiple state updates for one device can be coalesced before DSP
    /// processing, so this count includes superseded updates accepted from the
    /// bounded command queue.
    #[must_use]
    pub const fn applied_native_device_command_count(self) -> usize {
        self.applied_native_device_command_count
    }

    #[must_use]
    pub const fn native_device_command_sender_disconnected(self) -> bool {
        self.native_device_command_sender_disconnected
    }
}

/// Stereo sample-peak data for one compiled mixer strip in a successful
/// transport callback.
///
/// The readings are post-device, post-automation, post-mute, post-fader, and
/// post-pan. They exclude send and downstream-route contributions and are
/// copied before routing, post-fader sends, and main processing. Pre-fader
/// sends have already been dispatched, but they do not alter the measured
/// channel output. These are sample peaks, not true-peak or loudness
/// measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerStripSamplePeak {
    left_sample_peak: Option<f32>,
    right_sample_peak: Option<f32>,
}

impl MixerStripSamplePeak {
    const SILENCE: Self = Self {
        left_sample_peak: Some(0.0),
        right_sample_peak: Some(0.0),
    };

    #[must_use]
    pub const fn left_sample_peak(self) -> Option<f32> {
        self.left_sample_peak
    }

    #[must_use]
    pub const fn right_sample_peak(self) -> Option<f32> {
        self.right_sample_peak
    }

    fn observe(&mut self, left: &[f32], right: &[f32]) {
        observe_strip_sample_peak(&mut self.left_sample_peak, left);
        observe_strip_sample_peak(&mut self.right_sample_peak, right);
    }
}

fn observe_strip_sample_peak(peak: &mut Option<f32>, samples: &[f32]) {
    let Some(mut observed) = *peak else {
        return;
    };
    for &sample in samples {
        if !sample.is_finite() {
            *peak = None;
            return;
        }
        observed = observed.max(sample.abs());
    }
    *peak = Some(observed);
}

/// Fixed-capacity post-fader strip-meter evidence associated with one
/// successful transport callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MixerStripMeterSnapshot {
    slot_count: usize,
    strips: [MixerStripSamplePeak; MAX_MAIN_MIXER_SLOTS],
}

impl MixerStripMeterSnapshot {
    const fn silence(slot_count: usize) -> Self {
        Self {
            slot_count,
            strips: [MixerStripSamplePeak::SILENCE; MAX_MAIN_MIXER_SLOTS],
        }
    }

    #[must_use]
    pub const fn slot_count(self) -> usize {
        self.slot_count
    }

    /// Returns one strip reading only when `slot` belongs to this compiled
    /// callback layout.
    #[must_use]
    pub const fn slot(self, slot: usize) -> Option<MixerStripSamplePeak> {
        if slot < self.slot_count && slot < MAX_MAIN_MIXER_SLOTS {
            Some(self.strips[slot])
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MixerStripMeterAccumulator {
    snapshot: MixerStripMeterSnapshot,
}

impl MixerStripMeterAccumulator {
    fn new(slot_count: usize) -> Self {
        debug_assert!(slot_count <= MAX_MAIN_MIXER_SLOTS);
        Self {
            snapshot: MixerStripMeterSnapshot::silence(slot_count),
        }
    }

    const fn slot_count(&self) -> usize {
        self.snapshot.slot_count
    }

    fn observe(&mut self, slot: usize, left: &[f32], right: &[f32]) {
        debug_assert!(slot < self.snapshot.slot_count);
        if let Some(strip) = self.snapshot.strips.get_mut(slot) {
            strip.observe(left, right);
        }
    }

    const fn finish(self) -> MixerStripMeterSnapshot {
        self.snapshot
    }
}

/// Final output and per-strip meter data associated with one successful
/// transport cycle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputMeterSnapshot {
    cycle: TransportCycle,
    meter: StereoBlockMeter,
    strip_meters: MixerStripMeterSnapshot,
    processed_frame_end: u64,
    dropped_before: usize,
    applied_loudness_command_count: usize,
    loudness_command_sender_disconnected: bool,
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

    /// Returns the fixed post-fader sample peaks for the compiled mixer
    /// layout that produced this cycle.
    #[must_use]
    pub const fn strip_meters(self) -> MixerStripMeterSnapshot {
        self.strip_meters
    }

    /// Converts the persistent loudness measurement into non-visual display
    /// data without changing or consuming the underlying snapshot.
    #[must_use]
    pub fn present_loudness(
        self,
        config: EbuLoudnessPresentationConfig,
    ) -> LoudnessPresentationSnapshot {
        LoudnessPresentationSnapshot::from_measurement(self.meter.loudness(), config)
    }

    /// Successful output frames processed since this meter channel was
    /// created, including frames whose snapshots could not be queued.
    #[must_use]
    pub const fn processed_frame_end(self) -> u64 {
        self.processed_frame_end
    }

    /// Snapshots rejected by a full queue since the preceding successful
    /// publication.
    #[must_use]
    pub const fn dropped_before(self) -> usize {
        self.dropped_before
    }

    /// Loudness commands applied at the beginning of this callback.
    #[must_use]
    pub const fn applied_loudness_command_count(self) -> usize {
        self.applied_loudness_command_count
    }

    #[must_use]
    pub const fn loudness_command_sender_disconnected(self) -> bool {
        self.loudness_command_sender_disconnected
    }
}

/// Builds one valid constant-signal output-meter snapshot for consumer tests.
///
/// This helper is deliberately available only to this crate's tests and to
/// dependants that opt into `test-support`. Production snapshots remain owned
/// by the output processor after a successful callback.
///
/// # Panics
///
/// Panics when the requested callback frame count is outside the supported
/// bound or either supplied sample is non-finite.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn output_meter_snapshot_for_test(
    frame_count: usize,
    left_sample: f32,
    right_sample: f32,
) -> OutputMeterSnapshot {
    assert!(
        (1..=MAX_PROCESS_FRAMES).contains(&frame_count),
        "test meter snapshot frame count must fit one valid callback"
    );
    assert!(
        left_sample.is_finite() && right_sample.is_finite(),
        "test meter snapshot samples must be finite"
    );
    let mut callback = InterleavedStereoCallback::try_new(frame_count, 48_000)
        .expect("a bounded 48 kHz test callback must be constructible");
    let mut interleaved = vec![0.0_f32; frame_count * 2];
    let processed = callback
        .process(&mut interleaved, |output| {
            output.left.fill(left_sample);
            output.right.fill(right_sample);
            Ok::<_, ()>(())
        })
        .expect("a finite constant-signal test callback must succeed");
    let ((), meter) = processed.into_parts();
    OutputMeterSnapshot {
        cycle: TransportCycle {
            state: TransportState::Rolling,
            start_frame: 0,
            position_frame: frame_count,
            frame_count,
            loop_wrap_count: 0,
            loop_range: None,
            loop_enabled: false,
            metronome_available: false,
            metronome_enabled: false,
            metronome_gain: MetronomeGain::UNITY,
            active_slot_count: 1,
            scene_launch_state: SceneLaunchState::default(),
            applied_command_count: 0,
            capture_start_sequence: 0,
            command_sender_disconnected: false,
            applied_mixer_parameter_command_count: 0,
            mixer_parameter_command_sender_disconnected: false,
            applied_native_device_command_count: 0,
            native_device_command_sender_disconnected: false,
        },
        meter,
        strip_meters: MixerStripMeterSnapshot::silence(1),
        processed_frame_end: 0,
        dropped_before: 0,
        applied_loudness_command_count: 0,
        loudness_command_sender_disconnected: false,
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
    processed_frame_end: u64,
    dropped_snapshot_count: usize,
    pending_dropped_snapshot_count: Arc<AtomicUsize>,
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
    /// attached to the next successfully published snapshot. Preallocated
    /// shared evidence preserves an otherwise unpublished pending count if the
    /// producer ends first.
    #[allow(
        clippy::result_large_err,
        reason = "returning the fixed-size snapshot preserves ownership without a real-time allocation"
    )]
    pub fn try_publish(
        &mut self,
        mut snapshot: OutputMeterSnapshot,
    ) -> Result<OutputMeterSnapshot, OutputMeterPublishError> {
        let frame_count = u64::try_from(snapshot.meter().frame_count()).unwrap_or(u64::MAX);
        self.processed_frame_end = self.processed_frame_end.saturating_add(frame_count);
        snapshot.processed_frame_end = self.processed_frame_end;
        if self.is_disconnected() {
            return Err(OutputMeterPublishError::Disconnected(snapshot));
        }
        snapshot.dropped_before = self.dropped_snapshot_count;
        match self.producer.push(snapshot) {
            Ok(()) => {
                if self.dropped_snapshot_count != 0 {
                    self.dropped_snapshot_count = 0;
                    self.pending_dropped_snapshot_count
                        .store(0, Ordering::Release);
                }
                Ok(snapshot)
            }
            Err(PushError::Full(snapshot)) => {
                self.dropped_snapshot_count = self.dropped_snapshot_count.saturating_add(1);
                self.pending_dropped_snapshot_count
                    .store(self.dropped_snapshot_count, Ordering::Release);
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
    /// Returns an empty observation before any output callback has published.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            snapshot: None,
            coalesced_snapshot_count: 0,
            dropped_snapshot_count: 0,
            sender_disconnected: false,
        }
    }

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

impl Default for LatestOutputMeterSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

/// Control-thread half of the bounded output meter snapshot channel.
#[derive(Debug)]
pub struct OutputMeterReceiver {
    consumer: Consumer<OutputMeterSnapshot>,
    channel_id: u64,
    pending_dropped_snapshot_count: Arc<AtomicUsize>,
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
    /// was full, including pending terminal drops when the producer has ended.
    /// Both counts are explicit and independent.
    #[must_use]
    pub fn read_latest(&mut self) -> LatestOutputMeterSnapshot {
        self.drain_with(|_| {})
    }

    /// Drains every available snapshot through one persistent sample-peak
    /// ballistic state and returns the newest raw and ballistic values.
    ///
    /// Delivered snapshots that are coalesced from the raw result still update
    /// the ballistic state in FIFO order. Queue-rejected snapshots cannot
    /// contribute their peak values; their count remains explicit in the raw
    /// result, while the processed-frame clock preserves elapsed falloff time.
    /// Once a display uses this method, it must not consume intervening
    /// snapshots through [`Self::read_latest`].
    #[must_use]
    pub fn read_ballistics(
        &mut self,
        ballistics: &mut StereoSamplePeakBallistics,
    ) -> OutputMeterBallisticsRead {
        self.read_ballistics_with(ballistics, None)
    }

    /// Drains every available snapshot through persistent final-output and
    /// post-fader strip sample-peak display state.
    ///
    /// This has the same FIFO, coalescing, and dropped-snapshot behavior as
    /// [`Self::read_ballistics`]. It is explicit because a consumer that does
    /// not present mixer strips should not allocate or update strip state.
    #[must_use]
    pub fn read_ballistics_with_strips(
        &mut self,
        ballistics: &mut StereoSamplePeakBallistics,
        strip_ballistics: &mut MixerStripSamplePeakBallistics,
    ) -> OutputMeterBallisticsRead {
        self.read_ballistics_with(ballistics, Some(strip_ballistics))
    }

    fn read_ballistics_with(
        &mut self,
        ballistics: &mut StereoSamplePeakBallistics,
        mut strip_ballistics: Option<&mut MixerStripSamplePeakBallistics>,
    ) -> OutputMeterBallisticsRead {
        let mut latest_ballistics = None;
        let mut latest_strip_ballistics = None;
        let mut ballistics_reset = false;
        let channel_id = self.channel_id;
        let latest = self.drain_with(|snapshot| {
            let update = ballistics.update(channel_id, &snapshot);
            ballistics_reset |= update.ballistics_reset();
            latest_ballistics = Some(update.snapshot());
            if let Some(strip_ballistics) = strip_ballistics.as_deref_mut() {
                latest_strip_ballistics = Some(strip_ballistics.update(channel_id, &snapshot));
            }
        });
        OutputMeterBallisticsRead {
            latest,
            ballistics: latest_ballistics,
            strip_ballistics: latest_strip_ballistics,
            ballistics_reset,
        }
    }

    fn drain_with(
        &mut self,
        mut observe: impl FnMut(OutputMeterSnapshot),
    ) -> LatestOutputMeterSnapshot {
        let mut snapshot = None;
        let mut delivered_snapshot_count = 0_usize;
        let mut dropped_snapshot_count = 0_usize;
        for _ in 0..MAX_QUEUED_OUTPUT_METER_SNAPSHOTS {
            let Ok(next) = self.consumer.pop() else {
                break;
            };
            dropped_snapshot_count = dropped_snapshot_count.saturating_add(next.dropped_before());
            observe(next);
            snapshot = Some(next);
            delivered_snapshot_count += 1;
        }
        let sender_disconnected = self.consumer.is_abandoned();
        if sender_disconnected {
            fence(Ordering::Acquire);
            dropped_snapshot_count = dropped_snapshot_count.saturating_add(
                self.pending_dropped_snapshot_count
                    .swap(0, Ordering::AcqRel),
            );
        }
        LatestOutputMeterSnapshot {
            snapshot,
            coalesced_snapshot_count: delivered_snapshot_count.saturating_sub(1),
            dropped_snapshot_count,
            sender_disconnected,
        }
    }
}

/// Result of one bounded receiver drain with sample-peak ballistics applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputMeterBallisticsRead {
    latest: LatestOutputMeterSnapshot,
    ballistics: Option<StereoSamplePeakBallisticsSnapshot>,
    strip_ballistics: Option<MixerStripSamplePeakBallisticsSnapshot>,
    ballistics_reset: bool,
}

impl OutputMeterBallisticsRead {
    #[must_use]
    pub const fn latest(self) -> LatestOutputMeterSnapshot {
        self.latest
    }

    #[must_use]
    pub const fn ballistics(self) -> Option<StereoSamplePeakBallisticsSnapshot> {
        self.ballistics
    }

    /// Returns the newest bounded per-strip sample-peak display values from
    /// this drain when a raw snapshot was delivered through
    /// [`OutputMeterReceiver::read_ballistics_with_strips`].
    #[must_use]
    pub const fn strip_ballistics(self) -> Option<MixerStripSamplePeakBallisticsSnapshot> {
        self.strip_ballistics
    }

    /// True when reused state observed a different meter channel or a
    /// regressed frame clock and reset before applying the next snapshot.
    #[must_use]
    pub const fn ballistics_reset(self) -> bool {
        self.ballistics_reset
    }
}

/// Allocates the fixed output meter queue before callback processing starts.
#[must_use]
pub fn output_meter_channel() -> (OutputMeterSender, OutputMeterReceiver) {
    let (producer, consumer) = RingBuffer::new(MAX_QUEUED_OUTPUT_METER_SNAPSHOTS);
    let channel_id = next_output_meter_channel_id();
    let pending_dropped_snapshot_count = Arc::new(AtomicUsize::new(0));
    (
        OutputMeterSender {
            producer,
            processed_frame_end: 0,
            dropped_snapshot_count: 0,
            pending_dropped_snapshot_count: Arc::clone(&pending_dropped_snapshot_count),
        },
        OutputMeterReceiver {
            consumer,
            channel_id,
            pending_dropped_snapshot_count,
        },
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
    mixer_parameter_state: Option<MixerParameterStateSnapshot>,
    mixer_parameter_changes: MixerParameterChanges,
    native_device_state: Option<NativeDeviceStateSnapshot>,
}

impl InterleavedTransportCycle {
    #[must_use]
    pub const fn snapshot(&self) -> OutputMeterSnapshot {
        self.snapshot
    }

    #[must_use]
    pub const fn publication(&self) -> OutputMeterPublication {
        self.publication
    }

    /// Returns the newly committed base state when this callback applied mixer
    /// commands. Callbacks without mixer changes omit the duplicate snapshot.
    #[must_use]
    pub const fn mixer_parameter_state(&self) -> Option<&MixerParameterStateSnapshot> {
        self.mixer_parameter_state.as_ref()
    }

    /// Returns the live mixer changes accepted by this successful callback.
    ///
    /// This internal view remains unavailable outside the runtime crate; the
    /// loss-detecting capture publisher converts it to committed observations
    /// only after the output owner commits the callback.
    const fn committed_mixer_parameter_changes(&self) -> &MixerParameterChanges {
        &self.mixer_parameter_changes
    }

    /// Returns newly committed native-device state when this callback accepts
    /// native-device commands. The snapshot reflects the final state after
    /// same-device updates are coalesced. Callbacks without device changes
    /// omit the duplicate snapshot.
    #[must_use]
    pub const fn native_device_state(&self) -> Option<&NativeDeviceStateSnapshot> {
        self.native_device_state.as_ref()
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
    device: Option<GraphDeviceRuntime>,
}

#[derive(Debug)]
struct GraphDeviceRuntime {
    processor: NativeDeviceChainProcessor,
    output: StereoBuffer,
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
    sends: [[SendOverrides; MAX_MIXER_SENDS_PER_SLOT]; MAX_MAIN_MIXER_SLOTS],
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
    parameter_layout: Option<MixerParameterLayout>,
    parameter_layout_signature: [u8; 32],
    native_device_layout: Option<NativeDeviceLayout>,
    native_device_layout_signature: [u8; 32],
    live_parameters_enabled: bool,
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
    type Checkpoint: Copy;

    fn render_track_window(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RuntimeProcessError>;

    fn seek(&mut self, frame: usize);

    fn checkpoint(&self) -> Self::Checkpoint;

    fn restore(&mut self, checkpoint: Self::Checkpoint);

    fn scene_count(&self) -> usize;

    fn scene_availability(&self, scene_index: usize) -> PreparedSceneAvailability;

    fn scene_launch_state(&self) -> SceneLaunchState;

    fn queue_scene_launch(&mut self, scene_index: usize) -> Result<(), SceneRuntimeError>;

    fn stop_interactive_scenes(&mut self);
}

impl sealed::TrackWindowRenderer for ArrangementRenderer<'_, '_, '_> {}

impl TrackWindowRenderer for ArrangementRenderer<'_, '_, '_> {
    type Checkpoint = ();

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

    fn checkpoint(&self) -> Self::Checkpoint {}

    fn restore(&mut self, (): Self::Checkpoint) {}

    fn scene_count(&self) -> usize {
        0
    }

    fn scene_availability(&self, _scene_index: usize) -> PreparedSceneAvailability {
        PreparedSceneAvailability::NotPrepared
    }

    fn scene_launch_state(&self) -> SceneLaunchState {
        SceneLaunchState::default()
    }

    fn queue_scene_launch(&mut self, scene_index: usize) -> Result<(), SceneRuntimeError> {
        Err(SceneRuntimeError::MissingScene { scene_index })
    }

    fn stop_interactive_scenes(&mut self) {}
}

impl sealed::TrackWindowRenderer for PlaybackRenderer<'_, '_, '_, '_, '_> {}

impl<'resource> TrackWindowRenderer for PlaybackRenderer<'_, '_, '_, '_, 'resource> {
    type Checkpoint = SceneRuntimeCheckpoint<'resource>;

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

    fn checkpoint(&self) -> Self::Checkpoint {
        PlaybackRenderer::checkpoint(self)
    }

    fn restore(&mut self, checkpoint: Self::Checkpoint) {
        PlaybackRenderer::restore(self, checkpoint);
    }

    fn scene_count(&self) -> usize {
        PlaybackRenderer::scene_count(self)
    }

    fn scene_availability(&self, scene_index: usize) -> PreparedSceneAvailability {
        PlaybackRenderer::scene_availability(self, scene_index)
    }

    fn scene_launch_state(&self) -> SceneLaunchState {
        PlaybackRenderer::scene_launch_state(self)
    }

    fn queue_scene_launch(&mut self, scene_index: usize) -> Result<(), SceneRuntimeError> {
        PlaybackRenderer::queue_scene_launch(self, scene_index)
    }

    fn stop_interactive_scenes(&mut self) {
        PlaybackRenderer::stop_interactive_scenes(self);
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
        build_routed_main_runtime(renderer, compiled, automation, &[], max_block_frames)
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

    /// Binds prepared playback and validated Native Device chains to one graph.
    ///
    /// # Errors
    ///
    /// Returns a structural, capacity, mixer, or device construction failure
    /// before callback ownership.
    pub fn try_new_prepared_playback_with_devices(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        device_chains: &[DeviceChain],
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_prepared_playback_inner_with_devices(
            prepared,
            compiled,
            None,
            device_chains,
            max_block_frames,
        )
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

    /// Binds prepared playback, automation, and Native Device chains.
    ///
    /// # Errors
    ///
    /// Returns under the same conditions as
    /// [`Self::try_new_prepared_playback_with_devices`].
    pub fn try_new_prepared_playback_automated_with_devices(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        automation: CompiledAutomation,
        device_chains: &[DeviceChain],
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_prepared_playback_inner_with_devices(
            prepared,
            compiled,
            Some(automation),
            device_chains,
            max_block_frames,
        )
    }

    fn try_new_prepared_playback_inner(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        automation: Option<CompiledAutomation>,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        Self::try_new_prepared_playback_inner_with_devices(
            prepared,
            compiled,
            automation,
            &[],
            max_block_frames,
        )
    }

    fn try_new_prepared_playback_inner_with_devices(
        prepared: &'prepared PreparedPlayback<'cache, 'graph, 'project>,
        compiled: &CompiledMixerGraph,
        automation: Option<CompiledAutomation>,
        device_chains: &[DeviceChain],
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        validate_project_identity(prepared.project_id(), compiled, automation.as_ref())?;
        validate_device_chains(prepared.project_id(), compiled, device_chains)?;
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
        build_routed_main_runtime(
            renderer,
            compiled,
            automation,
            device_chains,
            max_block_frames,
        )
    }
}

fn validate_device_chains(
    project_id: &str,
    compiled: &CompiledMixerGraph,
    device_chains: &[DeviceChain],
) -> Result<(), RuntimeCreationError> {
    for (chain_index, chain) in device_chains.iter().enumerate() {
        if chain.project_id() != project_id
            || !compiled
                .slots()
                .iter()
                .any(|slot| slot.track_id() == chain.track_id())
        {
            return Err(RuntimeCreationError::ProjectMismatch {
                component: RuntimeProjectComponent::DeviceChain,
            });
        }
        if device_chains[..chain_index]
            .iter()
            .any(|other| other.track_id() == chain.track_id())
        {
            return Err(RuntimeCreationError::DuplicateDeviceChainTrack);
        }
    }
    Ok(())
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
    device_chains: &[DeviceChain],
    max_block_frames: usize,
) -> Result<RoutedMainRuntime<R>, RuntimeCreationError> {
    let slot_count = compiled.slots().len();
    let control_frame_count = max_block_frames;
    let parameter_layout = build_mixer_parameter_layout(compiled)?;
    let parameter_layout_signature = parameter_layout.signature;
    let native_device_layout = build_native_device_layout(compiled, device_chains)?;
    let native_device_layout_signature = native_device_layout.signature;
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
        let device = device_chains
            .iter()
            .find(|chain| chain.track_id() == compiled_slot.track_id())
            .map(|chain| {
                Ok(GraphDeviceRuntime {
                    processor: NativeDeviceChainProcessor::try_new(chain, PROJECT_SAMPLE_RATE_HZ)
                        .map_err(RuntimeCreationError::NativeDevice)?,
                    output: StereoBuffer::try_new(max_block_frames)?,
                })
            })
            .transpose()?;
        slots.push(GraphSlotRuntime {
            kind: compiled_slot.kind(),
            destination: compiled_slot.destination(),
            parameters,
            muted: compiled_slot.muted(),
            solo: compiled_slot.solo(),
            controls: try_filled(control_frame_count, parameters)?,
            processor,
            sends,
            device,
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
        parameter_layout: Some(parameter_layout),
        parameter_layout_signature,
        native_device_layout: Some(native_device_layout),
        native_device_layout_signature,
        live_parameters_enabled: false,
        max_block_frames,
    })
}

fn build_mixer_parameter_layout(
    compiled: &CompiledMixerGraph,
) -> Result<MixerParameterLayout, RuntimeCreationError> {
    let layout_id = next_mixer_parameter_layout_id();
    let signature = mixer_parameter_layout_signature(compiled);
    let mut channels = Vec::new();
    channels
        .try_reserve_exact(compiled.slots().len())
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    for (slot, compiled_slot) in compiled.slots().iter().enumerate() {
        let mut sends = Vec::new();
        sends
            .try_reserve_exact(compiled_slot.sends().len())
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        for (send, compiled_send) in compiled_slot.sends().iter().enumerate() {
            sends.push(MixerParameterSendLayout {
                id: try_runtime_identity(compiled_send.id())?,
                handle: MixerSendHandle {
                    layout_id,
                    slot,
                    send,
                },
            });
        }
        channels.push(MixerParameterChannelLayout {
            track_id: try_runtime_identity(compiled_slot.track_id())?,
            handle: MixerChannelHandle { layout_id, slot },
            audio: compiled_slot.kind() == CompiledMixerTrackKind::Audio,
            sends,
        });
    }
    Ok(MixerParameterLayout {
        id: layout_id,
        signature,
        channels,
    })
}

fn build_native_device_layout(
    compiled: &CompiledMixerGraph,
    device_chains: &[DeviceChain],
) -> Result<NativeDeviceLayout, RuntimeCreationError> {
    let layout_id = next_native_device_layout_id();
    let signature = native_device_layout_signature(compiled, device_chains);
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(compiled.slots().len())
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    for (slot, compiled_slot) in compiled.slots().iter().enumerate() {
        let chain = device_chains
            .iter()
            .find(|chain| chain.track_id() == compiled_slot.track_id());
        let devices = if let Some(chain) = chain {
            let mut devices = Vec::new();
            devices
                .try_reserve_exact(chain.devices().len())
                .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
            for (device, instance) in chain.devices().iter().enumerate() {
                devices.push(NativeDeviceLayoutEntry {
                    id: try_runtime_identity(instance.id())?,
                    handle: NativeDeviceHandle {
                        layout_id,
                        slot,
                        device,
                    },
                    state: *instance.state(),
                });
            }
            devices
        } else {
            Vec::new()
        };
        slots.push(NativeDeviceSlotLayout {
            track_id: try_runtime_identity(compiled_slot.track_id())?,
            devices,
        });
    }
    Ok(NativeDeviceLayout {
        id: layout_id,
        signature,
        slots,
    })
}

fn native_device_layout_signature(
    compiled: &CompiledMixerGraph,
    device_chains: &[DeviceChain],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hash_identity(&mut hasher, NATIVE_DEVICE_LAYOUT_SIGNATURE_VERSION);
    hash_identity(&mut hasher, compiled.project_id().as_bytes());
    hasher.update((compiled.slots().len() as u64).to_le_bytes());
    for slot in compiled.slots() {
        hash_identity(&mut hasher, slot.track_id().as_bytes());
        let chain = device_chains
            .iter()
            .find(|chain| chain.track_id() == slot.track_id());
        hasher.update([u8::from(chain.is_some())]);
        if let Some(chain) = chain {
            hash_identity(&mut hasher, chain.id().as_bytes());
            hasher.update((chain.devices().len() as u64).to_le_bytes());
            for device in chain.devices() {
                hash_identity(&mut hasher, device.id().as_bytes());
                hash_identity(&mut hasher, device.device_id().as_bytes());
            }
        }
    }
    hasher.finalize().into()
}

fn mixer_parameter_layout_signature(compiled: &CompiledMixerGraph) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hash_identity(&mut hasher, MIXER_PARAMETER_LAYOUT_SIGNATURE_VERSION);
    hash_identity(&mut hasher, compiled.project_id().as_bytes());
    hasher.update((compiled.slots().len() as u64).to_le_bytes());
    for slot in compiled.slots() {
        hash_identity(&mut hasher, slot.track_id().as_bytes());
        hasher.update([match slot.kind() {
            CompiledMixerTrackKind::Audio => 0,
            CompiledMixerTrackKind::Group => 1,
            CompiledMixerTrackKind::Return => 2,
        }]);
        match slot.destination() {
            CompiledMixerDestination::Main => hasher.update([0]),
            CompiledMixerDestination::Slot(target) => {
                hasher.update([1]);
                hasher.update((target as u64).to_le_bytes());
            }
        }
        hasher.update((slot.sends().len() as u64).to_le_bytes());
        for send in slot.sends() {
            hash_identity(&mut hasher, send.id().as_bytes());
            hasher.update((send.target_slot() as u64).to_le_bytes());
            hasher.update([match send.tap() {
                CompiledMixerTap::PreFader => 0,
                CompiledMixerTap::PostFader => 1,
            }]);
        }
    }
    hasher.finalize().into()
}

fn hash_identity(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn try_runtime_identity(value: &str) -> Result<String, RuntimeCreationError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
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
        self.reset_devices();
    }

    fn renderer_checkpoint(&self) -> R::Checkpoint {
        self.renderer.checkpoint()
    }

    fn restore_renderer(&mut self, checkpoint: R::Checkpoint) {
        self.renderer.restore(checkpoint);
        self.reset_devices();
    }

    fn reset_devices(&mut self) {
        for slot in &mut self.slots {
            if let Some(device) = &mut slot.device {
                device.processor.reset();
            }
        }
    }

    fn scene_count(&self) -> usize {
        self.renderer.scene_count()
    }

    fn scene_availability(&self, scene_index: usize) -> PreparedSceneAvailability {
        self.renderer.scene_availability(scene_index)
    }

    fn scene_launch_state(&self) -> SceneLaunchState {
        self.renderer.scene_launch_state()
    }

    fn queue_scene_launch(&mut self, scene_index: usize) -> Result<(), SceneRuntimeError> {
        self.renderer.queue_scene_launch(scene_index)
    }

    fn stop_interactive_scenes(&mut self) {
        self.renderer.stop_interactive_scenes();
    }

    fn take_parameter_layout(&mut self) -> MixerParameterLayout {
        self.parameter_layout
            .take()
            .expect("a routed runtime owns its parameter identity layout")
    }

    fn take_native_device_layout(&mut self) -> NativeDeviceLayout {
        self.native_device_layout
            .take()
            .expect("a routed runtime owns its native device identity layout")
    }

    const fn controls_enabled(&self) -> bool {
        self.live_parameters_enabled || self.automation.is_some()
    }

    fn enable_live_parameters(&mut self) {
        self.live_parameters_enabled = true;
    }

    fn apply_mixer_parameter_command(&mut self, command: MixerParameterCommand) {
        match command {
            MixerParameterCommand::ChannelGain { channel, gain } => {
                self.slots[channel.slot].parameters.gain = gain;
            }
            MixerParameterCommand::ChannelPan { channel, pan } => {
                self.slots[channel.slot].parameters.pan = pan;
            }
            MixerParameterCommand::ChannelMute { channel, muted } => {
                self.slots[channel.slot].muted = muted;
            }
            MixerParameterCommand::ChannelSolo { channel, solo } => {
                self.slots[channel.slot].solo = solo;
            }
            MixerParameterCommand::SendGain { send, gain } => {
                self.slots[send.slot].sends[send.send].parameters.gain = gain;
            }
            MixerParameterCommand::SendPan { send, pan } => {
                self.slots[send.slot].sends[send.send].parameters.pan = pan;
            }
            MixerParameterCommand::SendEnabled { send, enabled } => {
                self.slots[send.slot].sends[send.send].enabled = enabled;
            }
            MixerParameterCommand::MainGain { gain } => self.main_gain = gain,
            MixerParameterCommand::MainMute { muted } => self.main_muted = muted,
            MixerParameterCommand::Fence(_) => {}
        }
    }

    fn apply_native_device_command(&mut self, command: NativeDeviceCommand) {
        let device = match command {
            NativeDeviceCommand::SetEnabled { device, .. }
            | NativeDeviceCommand::SetState { device, .. } => device,
        };
        let processor = self.slots[device.slot]
            .device
            .as_mut()
            .expect("a validated native device command targets a prepared chain");
        match command {
            NativeDeviceCommand::SetEnabled { enabled, .. } => assert!(
                processor
                    .processor
                    .set_device_enabled(device.device, enabled),
                "a validated native device command targets a prepared device"
            ),
            NativeDeviceCommand::SetState { state, .. } => assert!(
                processor.processor.set_device_state(device.device, state),
                "a validated native device command targets a prepared device kind"
            ),
        }
    }

    fn capture_native_device_state(&self) -> NativeDeviceStateSnapshot {
        let mut enabled = [[false; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS];
        let mut states = [[None; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS];
        let mut device_counts = [0; MAX_MAIN_MIXER_SLOTS];
        for (slot_index, slot) in self.slots.iter().enumerate() {
            let Some(device) = &slot.device else {
                continue;
            };
            device_counts[slot_index] = device.processor.device_count();
            for (device_index, state) in enabled[slot_index]
                .iter_mut()
                .take(device_counts[slot_index])
                .enumerate()
            {
                *state = device
                    .processor
                    .device_enabled(device_index)
                    .expect("a native device count is derived from its processor");
                states[slot_index][device_index] = Some(
                    device
                        .processor
                        .device_state(device_index)
                        .expect("a native device count is derived from its processor"),
                );
            }
        }
        NativeDeviceStateSnapshot::from_recovery_parts(
            self.native_device_layout_signature,
            enabled,
            &states,
            device_counts,
        )
        .expect("prepared native device chains stay within their declared bounds")
    }

    fn restore_native_device_state(
        &mut self,
        state: &NativeDeviceStateSnapshot,
    ) -> Result<(), MixerParameterStateRestoreError> {
        let current = self.capture_native_device_state();
        if state.layout_signature != self.native_device_layout_signature
            || state.device_counts != current.device_counts
        {
            return Err(MixerParameterStateRestoreError::LayoutMismatch);
        }
        self.restore_native_device_state_unchecked(state);
        Ok(())
    }

    fn restore_native_device_state_unchecked(&mut self, state: &NativeDeviceStateSnapshot) {
        for (slot_index, count) in state.device_counts.iter().copied().enumerate() {
            if count == 0 {
                continue;
            }
            let processor = self.slots[slot_index]
                .device
                .as_mut()
                .expect("a matching native device count has a prepared chain");
            for device_index in 0..count {
                debug_assert!(
                    processor
                        .processor
                        .set_device_enabled(device_index, state.enabled[slot_index][device_index]),
                    "a matching native device state addresses a prepared device"
                );
                debug_assert!(
                    processor.processor.set_device_state(
                        device_index,
                        state.states[slot_index][device_index]
                            .expect("a matching native device state retains every device kind"),
                    ),
                    "a matching native device state retains each prepared device kind"
                );
            }
        }
    }

    fn capture_parameter_state(&self) -> MixerParameterStateSnapshot {
        let parameters = ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true);
        let send = MixerSendParameterState {
            parameters,
            enabled: false,
        };
        let channel = MixerChannelParameterState {
            parameters,
            muted: false,
            solo: false,
            sends: [send; MAX_MIXER_SENDS_PER_SLOT],
            send_count: 0,
        };
        let mut state = MixerParameterStateSnapshot {
            layout_signature: self.parameter_layout_signature,
            channels: [channel; MAX_MAIN_MIXER_SLOTS],
            channel_count: self.slots.len(),
            main_gain: self.main_gain,
            main_muted: self.main_muted,
        };
        for (slot_index, slot) in self.slots.iter().enumerate() {
            let channel = &mut state.channels[slot_index];
            channel.parameters = slot.parameters;
            channel.muted = slot.muted;
            channel.solo = slot.solo;
            channel.send_count = slot.sends.len();
            for (send_index, send) in slot.sends.iter().enumerate() {
                channel.sends[send_index] = MixerSendParameterState {
                    parameters: send.parameters,
                    enabled: send.enabled,
                };
            }
        }
        state
    }

    fn restore_parameter_state(
        &mut self,
        state: &MixerParameterStateSnapshot,
    ) -> Result<(), MixerParameterStateRestoreError> {
        if state.layout_signature != self.parameter_layout_signature
            || state.channel_count != self.slots.len()
            || self
                .slots
                .iter()
                .enumerate()
                .any(|(index, slot)| state.channels[index].send_count != slot.sends.len())
        {
            return Err(MixerParameterStateRestoreError::LayoutMismatch);
        }
        self.restore_parameter_state_unchecked(state);
        Ok(())
    }

    fn restore_parameter_state_unchecked(&mut self, state: &MixerParameterStateSnapshot) {
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            let channel = state.channels[slot_index];
            slot.parameters = channel.parameters;
            slot.muted = channel.muted;
            slot.solo = channel.solo;
            for (send_index, send) in slot.sends.iter_mut().enumerate() {
                let recovered_send = channel.sends[send_index];
                send.parameters = recovered_send.parameters;
                send.enabled = recovered_send.enabled;
            }
        }
        self.main_gain = state.main_gain;
        self.main_muted = state.main_muted;
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
        self.process_window_with_channel_tap_sink_at(
            arrangement_start_frame,
            scratch,
            output,
            &[],
            ChannelTapPosition::PostFader,
            |_, _, _| {},
        )
    }

    /// Renders the normal main output while copying one compiled audio channel
    /// after its device and channel controls but before it reaches sends,
    /// groups, returns, or the main processor.
    ///
    /// The tapped channel follows its own automation, mute, fader, and pan.
    /// The copied planes intentionally exclude the master gain/mute and every
    /// contribution that is not the selected channel's direct output.
    ///
    /// # Errors
    ///
    /// Returns the usual processing error, or a classified refusal before any
    /// rendering when the requested channel or output shape is invalid.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the caller transfers its temporary mutable output borrows to the callback-bound render window"
    )]
    pub fn process_window_with_channel_tap(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        channel_index: usize,
        channel_output: StereoOutput<'_>,
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        if output.left.len() == output.right.len()
            && (channel_output.left.len() != output.left.len()
                || channel_output.right.len() != output.left.len())
        {
            return Err(RuntimeProcessError::ChannelTapOutputLengthMismatch {
                output_frames: output.left.len(),
                left: channel_output.left.len(),
                right: channel_output.right.len(),
            });
        }
        self.process_window_with_channel_tap_sink_at(
            arrangement_start_frame,
            scratch,
            output,
            &[channel_index],
            ChannelTapPosition::PostFader,
            |_, left, right| {
                channel_output.left[..left.len()].copy_from_slice(left);
                channel_output.right[..right.len()].copy_from_slice(right);
            },
        )
    }

    /// Renders the normal main output while copying a distinct set of compiled
    /// mixer channels into caller-owned buffers at the channel-output tap.
    ///
    /// An audio-channel tap is taken before its sends and downstream routing;
    /// a group or return tap contains that channel's processed routed input.
    /// Each selected channel includes its own automation, device, mute, fader,
    /// and pan. The set must name each compiled channel at most once.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal before rendering when a channel index or
    /// duplicate selection is invalid.
    pub fn process_window_with_channel_tap_sink(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        channel_indices: &[usize],
        channel_tap: impl FnMut(usize, &[f32], &[f32]),
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        self.process_window_with_channel_tap_sink_at(
            arrangement_start_frame,
            scratch,
            output,
            channel_indices,
            ChannelTapPosition::PostFader,
            channel_tap,
        )
    }

    /// Renders the normal main output while copying selected audio channels
    /// directly after their native device chains and before channel mute,
    /// fader, pan, sends, routing, and main processing.
    ///
    /// A channel without a native device chain is copied from its rendered
    /// clip input, with mono material promoted identically to both planes.
    /// This is the only channel tap that can replace an audio track's source
    /// while retaining its persisted channel controls and routing semantics.
    /// Group and return channels are refused because their inputs already
    /// contain routing contributions from other channels.
    ///
    /// # Errors
    ///
    /// Returns a classified refusal before rendering when a requested channel
    /// is not an audio channel, is unavailable, or is repeated.
    pub fn process_window_with_audio_pre_fader_tap_sink(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        channel_indices: &[usize],
        channel_tap: impl FnMut(usize, &[f32], &[f32]),
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        self.process_window_with_channel_tap_sink_at(
            arrangement_start_frame,
            scratch,
            output,
            channel_indices,
            ChannelTapPosition::PostDevicePreFader,
            channel_tap,
        )
    }

    fn process_window_with_channel_tap_sink_at(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        channel_indices: &[usize],
        tap_position: ChannelTapPosition,
        mut channel_tap: impl FnMut(usize, &[f32], &[f32]),
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
        self.validate_channel_taps(channel_indices, tap_position)?;
        let rendered =
            self.renderer
                .render_track_window(arrangement_start_frame, frame_count, scratch)?;
        if frame_count == 0 {
            return Ok(rendered);
        }
        let result = (|| {
            if self.controls_enabled() {
                self.prepare_controls(arrangement_start_frame, frame_count)?;
            }

            for buffer in &mut self.route_buffers {
                buffer.clear_prefix(frame_count);
            }
            self.main_input.clear_prefix(frame_count);

            for order_index in 0..self.processing_order.len() {
                let slot_index = self.processing_order[order_index];
                self.process_devices(slot_index, frame_count, scratch)?;
                if tap_position == ChannelTapPosition::PostDevicePreFader
                    && channel_indices.contains(&slot_index)
                {
                    self.copy_audio_pre_fader_output(slot_index, frame_count, scratch);
                    channel_tap(
                        slot_index,
                        &self.channel_output.left[..frame_count],
                        &self.channel_output.right[..frame_count],
                    );
                }
                self.process_sends(slot_index, CompiledMixerTap::PreFader, frame_count, scratch)?;
                self.process_channel(slot_index, frame_count, scratch)?;
                if tap_position == ChannelTapPosition::PostFader
                    && channel_indices.contains(&slot_index)
                {
                    channel_tap(
                        slot_index,
                        &self.channel_output.left[..frame_count],
                        &self.channel_output.right[..frame_count],
                    );
                }
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
            if self.controls_enabled() {
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
            self.reset_devices();
        }
        result
    }

    fn validate_channel_taps(
        &self,
        channel_indices: &[usize],
        tap_position: ChannelTapPosition,
    ) -> Result<(), RuntimeProcessError> {
        for (tap_index, &channel_index) in channel_indices.iter().enumerate() {
            let Some(slot) = self.slots.get(channel_index) else {
                return Err(RuntimeProcessError::ChannelTapUnavailable {
                    channel_index,
                    channel_count: self.slots.len(),
                });
            };
            if channel_indices[..tap_index].contains(&channel_index) {
                return Err(RuntimeProcessError::DuplicateChannelTap { channel_index });
            }
            if tap_position == ChannelTapPosition::PostDevicePreFader
                && slot.kind != CompiledMixerTrackKind::Audio
            {
                return Err(RuntimeProcessError::PreFaderChannelTapRequiresAudio { channel_index });
            }
        }
        Ok(())
    }

    fn process_devices(
        &mut self,
        slot_index: usize,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) -> Result<(), RuntimeProcessError> {
        let kind = self.slots[slot_index].kind;
        let (slots, route_buffers) = (&mut self.slots, &self.route_buffers);
        let Some(device) = slots[slot_index].device.as_mut() else {
            return Ok(());
        };
        let input = if kind == CompiledMixerTrackKind::Audio {
            scratch[slot_index].input_block()
        } else {
            InputBlock::Stereo {
                left: &route_buffers[slot_index].left[..frame_count],
                right: &route_buffers[slot_index].right[..frame_count],
            }
        };
        match input {
            InputBlock::Mono(samples) => {
                device.output.left[..frame_count].copy_from_slice(samples);
                device.output.right[..frame_count].copy_from_slice(samples);
            }
            InputBlock::Stereo { left, right } => {
                device.output.left[..frame_count].copy_from_slice(left);
                device.output.right[..frame_count].copy_from_slice(right);
            }
        }
        device
            .processor
            .process_in_place(
                &mut device.output.left[..frame_count],
                &mut device.output.right[..frame_count],
            )
            .map_err(RuntimeProcessError::NativeDevice)
    }

    fn copy_audio_pre_fader_output(
        &mut self,
        slot_index: usize,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) {
        let slot = &self.slots[slot_index];
        if let Some(device) = &slot.device {
            self.channel_output.left[..frame_count]
                .copy_from_slice(&device.output.left[..frame_count]);
            self.channel_output.right[..frame_count]
                .copy_from_slice(&device.output.right[..frame_count]);
            return;
        }
        match scratch[slot_index].input_block() {
            InputBlock::Mono(samples) => {
                self.channel_output.left[..frame_count].copy_from_slice(samples);
                self.channel_output.right[..frame_count].copy_from_slice(samples);
            }
            InputBlock::Stereo { left, right } => {
                self.channel_output.left[..frame_count].copy_from_slice(left);
                self.channel_output.right[..frame_count].copy_from_slice(right);
            }
        }
    }

    fn prepare_controls(
        &mut self,
        arrangement_start_frame: usize,
        frame_count: usize,
    ) -> Result<(), RuntimeProcessError> {
        for frame in 0..frame_count {
            let absolute_frame = arrangement_start_frame
                .checked_add(frame)
                .ok_or(RuntimeProcessError::FrameRangeOverflow)?;
            let mut overrides = FrameOverrides::default();
            if let Some(automation) = self.automation.as_ref() {
                automation
                    .visit_frame_values(absolute_frame, |target, value| {
                        overrides.apply(target, value);
                    })
                    .map_err(RuntimeProcessError::Automation)?;
            }
            if overrides.mismatch {
                return Err(RuntimeProcessError::AutomationValueMismatch);
            }

            let mut muted = [false; MAX_MAIN_MIXER_SLOTS];
            let mut solo = [false; MAX_MAIN_MIXER_SLOTS];
            let mut send_enabled = [[false; MAX_MIXER_SENDS_PER_SLOT]; MAX_MAIN_MIXER_SLOTS];
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
        let controls_enabled = self.controls_enabled();
        for send_index in 0..send_count {
            if self.slots[slot_index].sends[send_index].tap != tap {
                continue;
            }
            let GraphSlotRuntime {
                kind,
                sends,
                device,
                ..
            } = &mut self.slots[slot_index];
            let input = if tap == CompiledMixerTap::PostFader {
                InputBlock::Stereo {
                    left: &self.channel_output.left[..frame_count],
                    right: &self.channel_output.right[..frame_count],
                }
            } else if let Some(device) = device {
                InputBlock::Stereo {
                    left: &device.output.left[..frame_count],
                    right: &device.output.right[..frame_count],
                }
            } else if *kind == CompiledMixerTrackKind::Audio {
                scratch[slot_index].input_block()
            } else {
                InputBlock::Stereo {
                    left: &self.route_buffers[slot_index].left[..frame_count],
                    right: &self.route_buffers[slot_index].right[..frame_count],
                }
            };
            let inputs = [input];
            let send = &mut sends[send_index];
            if controls_enabled {
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
                let parameters = [send.parameters];
                send.processor
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
            let target = send.target_slot;
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
        let controls_enabled = self.controls_enabled();
        let GraphSlotRuntime {
            kind,
            parameters,
            controls,
            processor,
            device,
            ..
        } = &mut self.slots[slot_index];
        let input = if let Some(device) = device {
            InputBlock::Stereo {
                left: &device.output.left[..frame_count],
                right: &device.output.right[..frame_count],
            }
        } else if *kind == CompiledMixerTrackKind::Audio {
            scratch[slot_index].input_block()
        } else {
            InputBlock::Stereo {
                left: &self.route_buffers[slot_index].left[..frame_count],
                right: &self.route_buffers[slot_index].right[..frame_count],
            }
        };
        let inputs = [input];
        if controls_enabled {
            let controlled = [ControlledInputBlock {
                input,
                parameters: &controls[..frame_count],
            }];
            processor
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
            let parameters = [*parameters];
            processor
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
/// Transport commands can be applied synchronously or through an optional
/// bounded cross-thread queue. A second optional queue owns typed live mixer
/// base-state changes. This type does not provide device I/O. Stopped cycles
/// produce silence and retain the current frame; rolling cycles advance only
/// after the complete routed window succeeds.
#[derive(Debug)]
pub struct RoutedTransportRuntime<R> {
    runtime: RoutedMainRuntime<R>,
    state: TransportState,
    position_frame: usize,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
    metronome: Option<Metronome>,
    command_consumer: Option<Consumer<QueuedTransportCommand>>,
    mixer_parameter_command_consumer: Option<Consumer<MixerParameterCommand>>,
    native_device_command_consumer: Option<Consumer<NativeDeviceCommand>>,
    mixer_parameter_fence: Option<Arc<AtomicU64>>,
    committed_mixer_parameter_change_sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LoudnessCycleControl {
    applied_command_count: usize,
    sender_disconnected: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MixerParameterDrain {
    applied_command_count: usize,
    greatest_fence: u64,
    changes: MixerParameterChanges,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TransportCommandDrain {
    applied_command_count: usize,
    capture_start_sequence: u64,
}

#[derive(Debug)]
struct ProcessedTransportCycle {
    cycle: TransportCycle,
    mixer_parameter_changes: MixerParameterChanges,
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
            loop_range: None,
            loop_enabled: false,
            metronome: None,
            command_consumer: None,
            mixer_parameter_command_consumer: None,
            native_device_command_consumer: None,
            mixer_parameter_fence: None,
            committed_mixer_parameter_change_sequence: 0,
        }
    }

    /// Creates a transport and its application-thread command sender.
    ///
    /// Queue storage is allocated once during construction. The sender and
    /// process-cycle receiver are wait-free after construction.
    #[must_use]
    pub fn with_command_queue(runtime: RoutedMainRuntime<R>) -> (TransportCommandSender, Self) {
        let (producer, consumer) = RingBuffer::new(MAX_QUEUED_TRANSPORT_COMMANDS);
        let (scene_count, scene_ready_mask, scene_disabled_mask, scene_consolidation_mask) =
            scene_command_layout(&runtime);
        (
            TransportCommandSender {
                producer,
                max_block_frames: runtime.max_block_frames(),
                scene_count,
                scene_ready_mask,
                scene_disabled_mask,
                scene_consolidation_mask,
                metronome_available: false,
                next_capture_start_sequence: 1,
            },
            Self {
                runtime,
                state: TransportState::Stopped,
                position_frame: 0,
                loop_range: None,
                loop_enabled: false,
                metronome: None,
                command_consumer: Some(consumer),
                mixer_parameter_command_consumer: None,
                native_device_command_consumer: None,
                mixer_parameter_fence: None,
                committed_mixer_parameter_change_sequence: 0,
            },
        )
    }

    /// Creates one transport with a bounded command queue and one prepared
    /// session metronome. The click remains disabled until its explicit
    /// transport command is committed by a process cycle.
    #[must_use]
    pub fn with_command_queue_and_metronome(
        runtime: RoutedMainRuntime<R>,
        plan: MetronomePlan,
    ) -> (TransportCommandSender, Self) {
        let (mut commands, mut transport) = Self::with_command_queue(runtime);
        commands.metronome_available = true;
        transport.metronome = Some(Metronome::new(plan));
        (commands, transport)
    }

    /// Creates one transport with bounded application-thread transport and
    /// live mixer-control queues.
    ///
    /// The mixer sender validates resolved slot/send targets before enqueueing.
    /// Both queues and exact per-sample control storage are fully allocated
    /// before the runtime is returned.
    #[must_use]
    pub fn with_command_queues(
        mut runtime: RoutedMainRuntime<R>,
    ) -> (TransportCommandSender, MixerParameterCommandSender, Self) {
        let layout = runtime.take_parameter_layout();
        runtime.enable_live_parameters();
        let (transport_producer, transport_consumer) =
            RingBuffer::new(MAX_QUEUED_TRANSPORT_COMMANDS);
        let (parameter_producer, parameter_consumer) =
            RingBuffer::new(MAX_QUEUED_MIXER_PARAMETER_COMMANDS);
        let committed_fence = Arc::new(AtomicU64::new(0));
        let (scene_count, scene_ready_mask, scene_disabled_mask, scene_consolidation_mask) =
            scene_command_layout(&runtime);
        (
            TransportCommandSender {
                producer: transport_producer,
                max_block_frames: runtime.max_block_frames(),
                scene_count,
                scene_ready_mask,
                scene_disabled_mask,
                scene_consolidation_mask,
                metronome_available: false,
                next_capture_start_sequence: 1,
            },
            MixerParameterCommandSender {
                producer: parameter_producer,
                layout,
                committed_fence: Arc::clone(&committed_fence),
                next_fence_sequence: 1,
            },
            Self {
                runtime,
                state: TransportState::Stopped,
                position_frame: 0,
                loop_range: None,
                loop_enabled: false,
                metronome: None,
                command_consumer: Some(transport_consumer),
                mixer_parameter_command_consumer: Some(parameter_consumer),
                native_device_command_consumer: None,
                mixer_parameter_fence: Some(committed_fence),
                committed_mixer_parameter_change_sequence: 0,
            },
        )
    }

    /// Creates one transport with bounded transport/mixer queues and a
    /// prepared session metronome. Construction allocates only the command
    /// queues; the metronome plan was already fully prepared by its caller.
    #[must_use]
    pub fn with_command_queues_and_metronome(
        runtime: RoutedMainRuntime<R>,
        plan: MetronomePlan,
    ) -> (TransportCommandSender, MixerParameterCommandSender, Self) {
        let (mut commands, mixer_commands, mut transport) = Self::with_command_queues(runtime);
        commands.metronome_available = true;
        transport.metronome = Some(Metronome::new(plan));
        (commands, mixer_commands, transport)
    }

    /// Creates one transport with bounded transport, mixer, and native-device
    /// queues. Native-device targets are resolved against the same immutable
    /// graph that owns callback processing.
    #[must_use]
    pub fn with_command_queues_and_native_device_commands(
        mut runtime: RoutedMainRuntime<R>,
    ) -> (
        TransportCommandSender,
        MixerParameterCommandSender,
        NativeDeviceCommandSender,
        Self,
    ) {
        let layout = runtime.take_native_device_layout();
        let (transport_commands, mixer_commands, mut transport) =
            Self::with_command_queues(runtime);
        let (producer, consumer) = RingBuffer::new(MAX_QUEUED_NATIVE_DEVICE_COMMANDS);
        transport.native_device_command_consumer = Some(consumer);
        (
            transport_commands,
            mixer_commands,
            NativeDeviceCommandSender { producer, layout },
            transport,
        )
    }

    /// Creates one transport with all live command queues and a prepared
    /// session metronome.
    #[must_use]
    pub fn with_command_queues_native_device_commands_and_metronome(
        runtime: RoutedMainRuntime<R>,
        plan: MetronomePlan,
    ) -> (
        TransportCommandSender,
        MixerParameterCommandSender,
        NativeDeviceCommandSender,
        Self,
    ) {
        let (mut transport_commands, mixer_commands, native_device_commands, mut transport) =
            Self::with_command_queues_and_native_device_commands(runtime);
        transport_commands.metronome_available = true;
        transport.metronome = Some(Metronome::new(plan));
        (
            transport_commands,
            mixer_commands,
            native_device_commands,
            transport,
        )
    }

    #[must_use]
    pub const fn state(&self) -> TransportState {
        self.state
    }

    #[must_use]
    pub const fn loop_range(&self) -> Option<TransportLoopRange> {
        self.loop_range
    }

    #[must_use]
    pub const fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    /// Whether this transport owns a prepared, callback-safe metronome plan.
    #[must_use]
    pub const fn metronome_available(&self) -> bool {
        self.metronome.is_some()
    }

    /// Whether the prepared metronome is enabled after the latest committed
    /// transport command.
    #[must_use]
    pub fn metronome_enabled(&self) -> bool {
        self.metronome.as_ref().is_some_and(Metronome::enabled)
    }

    /// Returns the session-only click gain for a prepared metronome.
    #[must_use]
    pub fn metronome_gain(&self) -> Option<MetronomeGain> {
        self.metronome.as_ref().map(Metronome::gain)
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

    /// Restores a previously committed live mixer base-state snapshot.
    ///
    /// Runtime-local channel and send handles are intentionally unaffected;
    /// callers must resolve fresh handles from the replacement runtime's
    /// [`MixerParameterCommandSender`].
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterStateRestoreError::LayoutMismatch`] before any
    /// state changes when the snapshot was captured from another compiled
    /// mixer layout.
    pub fn restore_mixer_parameter_state(
        &mut self,
        snapshot: &MixerParameterStateSnapshot,
    ) -> Result<(), MixerParameterStateRestoreError> {
        self.runtime.restore_parameter_state(snapshot)
    }

    /// Captures the current live mixer base state without allocation or locks.
    #[must_use]
    pub fn mixer_parameter_state(&self) -> MixerParameterStateSnapshot {
        self.runtime.capture_parameter_state()
    }

    /// Restores one session-only native-device state snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`MixerParameterStateRestoreError::LayoutMismatch`] before any
    /// state changes when the snapshot belongs to another prepared layout.
    pub fn restore_native_device_state(
        &mut self,
        snapshot: &NativeDeviceStateSnapshot,
    ) -> Result<(), MixerParameterStateRestoreError> {
        self.runtime.restore_native_device_state(snapshot)
    }

    /// Captures current session-only native-device state.
    #[must_use]
    pub fn native_device_state(&self) -> NativeDeviceStateSnapshot {
        self.runtime.capture_native_device_state()
    }

    /// Applies one transport command for the next process cycle.
    ///
    /// # Errors
    ///
    /// Returns [`TransportCommandApplyError`] when a synchronous scene command
    /// cannot resolve its launch boundary or the metronome was not prepared.
    pub fn apply_command(
        &mut self,
        command: TransportCommand,
    ) -> Result<(), TransportCommandApplyError> {
        match command {
            TransportCommand::Start => self.state = TransportState::Rolling,
            TransportCommand::Stop => {
                self.state = TransportState::Stopped;
                if let Some(metronome) = &mut self.metronome {
                    metronome.clear_voice();
                }
            }
            TransportCommand::Locate { frame, state } => {
                self.position_frame = frame;
                self.runtime.seek(frame);
                if let Some(metronome) = &mut self.metronome {
                    metronome.clear_voice();
                }
                match state {
                    LocateState::Preserve => {}
                    LocateState::Stopped => self.state = TransportState::Stopped,
                    LocateState::Rolling => self.state = TransportState::Rolling,
                }
            }
            TransportCommand::LaunchScene { scene_index } => {
                self.runtime
                    .queue_scene_launch(scene_index)
                    .map_err(TransportCommandApplyError::Scene)?;
            }
            TransportCommand::StopInteractiveScenes => self.runtime.stop_interactive_scenes(),
            TransportCommand::SetLoopRange(range) => self.loop_range = range,
            TransportCommand::SetLoopEnabled(enabled) => self.loop_enabled = enabled,
            TransportCommand::SetMetronomeEnabled(enabled) => {
                let Some(metronome) = &mut self.metronome else {
                    return Err(TransportCommandApplyError::MetronomeUnavailable { command });
                };
                metronome.set_enabled(enabled);
            }
            TransportCommand::SetMetronomeGain(gain) => {
                let Some(metronome) = &mut self.metronome else {
                    return Err(TransportCommandApplyError::MetronomeUnavailable { command });
                };
                metronome.set_gain(gain);
            }
        }
        Ok(())
    }
    /// Renders one callback, splitting the window every time it reaches the
    /// loop end so the remainder continues from the loop start. A range shorter
    /// than the block wraps more than once, so this repeats until the block is
    /// filled.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] when an underlying window fails or the
    /// playhead would overflow.
    fn render_cycle(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        mut strip_meters: Option<&mut MixerStripMeterAccumulator>,
    ) -> Result<(usize, RenderedWindow, usize), RuntimeProcessError> {
        let StereoOutput { left, right } = output;
        if start_frame.checked_add(frame_count).is_none() {
            return Err(RuntimeProcessError::FrameRangeOverflow);
        }
        let mut position = start_frame;
        let mut written = 0;
        let mut wraps = 0;
        let mut active_slot_count = 0;
        while written < frame_count {
            let remaining = frame_count - written;
            let span = match self.active_loop_range() {
                Some(range) if range.contains(position) => {
                    remaining.min(range.end_frame() - position)
                }
                // Approaching the range from before it: stop at the start so the
                // next sub-window begins inside it. Without this the block runs
                // straight over a range shorter than itself and never wraps.
                Some(range) if position < range.start_frame() => {
                    remaining.min(range.start_frame() - position)
                }
                _ => remaining,
            };
            let window_output = StereoOutput {
                left: &mut left[written..written + span],
                right: &mut right[written..written + span],
            };
            let rendered = if let Some(strip_meters) = &mut strip_meters {
                let slot_count = strip_meters.slot_count();
                self.runtime.process_window_with_channel_tap_sink(
                    position,
                    scratch,
                    window_output,
                    &ALL_MAIN_MIXER_SLOT_INDICES[..slot_count],
                    |slot, left, right| strip_meters.observe(slot, left, right),
                )?
            } else {
                self.runtime
                    .process_window(position, scratch, window_output)?
            };
            if let Some(metronome) = &mut self.metronome {
                metronome
                    .mix_window(
                        position,
                        StereoOutput {
                            left: &mut left[written..written + span],
                            right: &mut right[written..written + span],
                        },
                    )
                    .map_err(RuntimeProcessError::Metronome)?;
            }
            active_slot_count = active_slot_count.max(rendered.active_slot_count);
            written += span;
            position = position
                .checked_add(span)
                .ok_or(RuntimeProcessError::FrameRangeOverflow)?;
            if let Some(range) = self.active_loop_range()
                && position == range.end_frame()
            {
                position = range.start_frame();
                self.runtime.seek(position);
                if let Some(metronome) = &mut self.metronome {
                    metronome.reset_position();
                }
                wraps += 1;
            }
        }
        Ok((position, RenderedWindow { active_slot_count }, wraps))
    }

    /// The range the transport is currently repeating, if looping is engaged.
    const fn active_loop_range(&self) -> Option<TransportLoopRange> {
        if self.loop_enabled {
            self.loop_range
        } else {
            None
        }
    }

    /// Processes the next transport-owned callback window.
    ///
    /// Output shape and block capacity are validated before queued commands are
    /// consumed. A valid cycle applies bounded FIFO batches provisionally
    /// before it captures the cycle's state and start frame. If later work
    /// fails, transport state, renderer position, mixer base parameters, and
    /// native-device state
    /// return to their preceding committed values; consumed commands are not
    /// replayed. Arbitrary mixer processor state is not transactional, so
    /// callers must not continue processing after a mixer failure.
    ///
    /// Stopped cycles clear the output without reading arrangement scratch or
    /// advancing the position. Rolling cycles render from the current absolute
    /// frame and publish the next position only after successful processing.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] for output shape, block capacity, frame
    /// range, rendering, automation, or mixer failures. A failed cycle never
    /// commits queued transport, mixer base state, or native-device
    /// state and never advances the transport position.
    #[allow(
        clippy::too_many_lines,
        reason = "the transactional transport rollback remains auditable when its state fences are co-located"
    )]
    pub fn process_cycle(
        &mut self,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<TransportCycle, RuntimeProcessError> {
        self.process_cycle_with_strip_meters(scratch, output, None)
            .map(|processed| processed.cycle)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the transactional transport rollback remains auditable when its state fences are co-located"
    )]
    fn process_cycle_with_strip_meters(
        &mut self,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
        strip_meters: Option<&mut MixerStripMeterAccumulator>,
    ) -> Result<ProcessedTransportCycle, RuntimeProcessError> {
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
        let committed_state = self.state;
        let committed_position = self.position_frame;
        let committed_loop_range = self.loop_range;
        let committed_loop_enabled = self.loop_enabled;
        let committed_metronome = self.metronome.as_ref().map(Metronome::checkpoint);
        let committed_parameters = self.runtime.capture_parameter_state();
        let committed_native_devices = self.runtime.capture_native_device_state();
        let committed_renderer = self.runtime.renderer_checkpoint();
        let transport_command_drain = match self.drain_commands() {
            Ok(drain) => drain,
            Err(source) => {
                self.state = committed_state;
                self.position_frame = committed_position;
                self.loop_range = committed_loop_range;
                self.loop_enabled = committed_loop_enabled;
                if let (Some(metronome), Some(checkpoint)) =
                    (&mut self.metronome, committed_metronome)
                {
                    metronome.restore(checkpoint);
                }
                self.runtime.restore_renderer(committed_renderer);
                self.runtime
                    .restore_parameter_state_unchecked(&committed_parameters);
                self.runtime
                    .restore_native_device_state_unchecked(&committed_native_devices);
                return Err(match source {
                    TransportCommandApplyError::Scene(source) => RuntimeProcessError::Scene(source),
                    source @ TransportCommandApplyError::MetronomeUnavailable { .. } => {
                        RuntimeProcessError::TransportCommand(source)
                    }
                });
            }
        };
        let applied_command_count = transport_command_drain.applied_command_count;
        let command_sender_disconnected = self.command_sender_disconnected();
        let mixer_parameter_drain = self.drain_mixer_parameter_commands();
        let applied_mixer_parameter_command_count = mixer_parameter_drain.applied_command_count;
        let mixer_parameter_command_sender_disconnected =
            self.mixer_parameter_command_sender_disconnected();
        let applied_native_device_command_count = self.drain_native_device_commands();
        let native_device_command_sender_disconnected =
            self.native_device_command_sender_disconnected();
        let start_frame = self.position_frame;
        if self.state == TransportState::Stopped {
            output.left.fill(0.0);
            output.right.fill(0.0);
            self.commit_mixer_parameter_fence(mixer_parameter_drain.greatest_fence);
            let mixer_parameter_changes =
                self.commit_mixer_parameter_changes(mixer_parameter_drain.changes);
            return Ok(ProcessedTransportCycle {
                cycle: TransportCycle {
                    state: self.state,
                    start_frame,
                    position_frame: start_frame,
                    frame_count,
                    loop_wrap_count: 0,
                    loop_range: self.loop_range,
                    loop_enabled: self.loop_enabled,
                    metronome_available: self.metronome_available(),
                    metronome_enabled: self.metronome_enabled(),
                    metronome_gain: self.metronome_gain().unwrap_or(MetronomeGain::UNITY),
                    active_slot_count: 0,
                    scene_launch_state: self.runtime.scene_launch_state(),
                    applied_command_count,
                    capture_start_sequence: transport_command_drain.capture_start_sequence,
                    command_sender_disconnected,
                    applied_mixer_parameter_command_count,
                    mixer_parameter_command_sender_disconnected,
                    applied_native_device_command_count,
                    native_device_command_sender_disconnected,
                },
                mixer_parameter_changes,
            });
        }
        let rendered = self.render_cycle(start_frame, frame_count, scratch, output, strip_meters);
        let (next_frame, rendered, loop_wrap_count) = match rendered {
            Ok(rendered) => rendered,
            Err(source) => {
                self.state = committed_state;
                self.position_frame = committed_position;
                self.loop_range = committed_loop_range;
                self.loop_enabled = committed_loop_enabled;
                if let (Some(metronome), Some(checkpoint)) =
                    (&mut self.metronome, committed_metronome)
                {
                    metronome.restore(checkpoint);
                }
                self.runtime.restore_renderer(committed_renderer);
                self.runtime
                    .restore_parameter_state_unchecked(&committed_parameters);
                self.runtime
                    .restore_native_device_state_unchecked(&committed_native_devices);
                return Err(source);
            }
        };
        self.position_frame = next_frame;
        self.commit_mixer_parameter_fence(mixer_parameter_drain.greatest_fence);
        let mixer_parameter_changes =
            self.commit_mixer_parameter_changes(mixer_parameter_drain.changes);
        Ok(ProcessedTransportCycle {
            cycle: TransportCycle {
                state: self.state,
                start_frame,
                position_frame: next_frame,
                frame_count,
                loop_wrap_count,
                loop_range: self.loop_range,
                loop_enabled: self.loop_enabled,
                metronome_available: self.metronome_available(),
                metronome_enabled: self.metronome_enabled(),
                metronome_gain: self.metronome_gain().unwrap_or(MetronomeGain::UNITY),
                active_slot_count: rendered.active_slot_count,
                scene_launch_state: self.runtime.scene_launch_state(),
                applied_command_count,
                capture_start_sequence: transport_command_drain.capture_start_sequence,
                command_sender_disconnected,
                applied_mixer_parameter_command_count,
                mixer_parameter_command_sender_disconnected,
                applied_native_device_command_count,
                native_device_command_sender_disconnected,
            },
            mixer_parameter_changes,
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
        self.process_interleaved_cycle_with_loudness_control(
            callback,
            scratch,
            interleaved_output,
            meter_sender,
            LoudnessCycleControl::default(),
            |_| {},
        )
    }

    /// Processes one transport callback and applies an infallible final stereo
    /// mix before validation, metering, loudness, true-peak, and interleaving.
    ///
    /// The final mix is intended for already-bounded realtime sources such as
    /// a cue-monitor bridge. It runs only after the project cycle succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`StereoCallbackError`] under the same conditions as
    /// [`Self::process_interleaved_cycle`]. Non-finite samples introduced by
    /// the final mix are rejected by the callback adapter.
    pub fn process_interleaved_cycle_with_post_mix<F>(
        &mut self,
        callback: &mut InterleavedStereoCallback,
        scratch: &mut [RenderScratchSlot<'_>],
        interleaved_output: &mut [f32],
        meter_sender: &mut OutputMeterSender,
        post_mix: F,
    ) -> Result<InterleavedTransportCycle, StereoCallbackError<RuntimeProcessError>>
    where
        F: FnOnce(StereoOutput<'_>),
    {
        self.process_interleaved_cycle_with_loudness_control(
            callback,
            scratch,
            interleaved_output,
            meter_sender,
            LoudnessCycleControl::default(),
            post_mix,
        )
    }

    fn process_interleaved_cycle_with_loudness_control<F>(
        &mut self,
        callback: &mut InterleavedStereoCallback,
        scratch: &mut [RenderScratchSlot<'_>],
        interleaved_output: &mut [f32],
        meter_sender: &mut OutputMeterSender,
        loudness: LoudnessCycleControl,
        post_mix: F,
    ) -> Result<InterleavedTransportCycle, StereoCallbackError<RuntimeProcessError>>
    where
        F: FnOnce(StereoOutput<'_>),
    {
        let mut strip_meters = MixerStripMeterAccumulator::new(self.runtime.slot_count());
        let processed = callback.process(interleaved_output, |output| {
            let processed = self.process_cycle_with_strip_meters(
                scratch,
                StereoOutput {
                    left: &mut *output.left,
                    right: &mut *output.right,
                },
                Some(&mut strip_meters),
            )?;
            post_mix(output);
            Ok(processed)
        })?;
        let (processed, meter) = processed.into_parts();
        let cycle = processed.cycle;
        let strip_meters = strip_meters.finish();
        let mixer_parameter_state = (cycle.applied_mixer_parameter_command_count() > 0)
            .then(|| self.runtime.capture_parameter_state());
        let native_device_state = (cycle.applied_native_device_command_count() > 0)
            .then(|| self.runtime.capture_native_device_state());
        let snapshot = OutputMeterSnapshot {
            cycle,
            meter,
            strip_meters,
            processed_frame_end: 0,
            dropped_before: 0,
            applied_loudness_command_count: loudness.applied_command_count,
            loudness_command_sender_disconnected: loudness.sender_disconnected,
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
            mixer_parameter_state,
            mixer_parameter_changes: processed.mixer_parameter_changes,
            native_device_state,
        })
    }

    fn drain_commands(&mut self) -> Result<TransportCommandDrain, TransportCommandApplyError> {
        let mut drained = TransportCommandDrain::default();
        for _ in 0..MAX_QUEUED_TRANSPORT_COMMANDS {
            let command = self
                .command_consumer
                .as_mut()
                .and_then(|consumer| consumer.pop().ok());
            let Some(command) = command else {
                break;
            };
            match command {
                QueuedTransportCommand::Transport(command) => self.apply_command(command)?,
                QueuedTransportCommand::CaptureStart { sequence } => {
                    self.apply_command(TransportCommand::Start)?;
                    drained.capture_start_sequence = drained.capture_start_sequence.max(sequence);
                }
            }
            drained.applied_command_count += 1;
        }
        Ok(drained)
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

    fn drain_mixer_parameter_commands(&mut self) -> MixerParameterDrain {
        let mut drained = MixerParameterDrain::default();
        for _ in 0..MAX_QUEUED_MIXER_PARAMETER_COMMANDS {
            let command = self
                .mixer_parameter_command_consumer
                .as_mut()
                .and_then(|consumer| consumer.pop().ok());
            let Some(command) = command else {
                break;
            };
            if let MixerParameterCommand::Fence(fence) = command {
                drained.greatest_fence = drained.greatest_fence.max(fence.sequence);
            }
            if let Some(change) = command.automation_change() {
                drained.changes.push(change);
            }
            self.runtime.apply_mixer_parameter_command(command);
            drained.applied_command_count += 1;
        }
        drained
    }

    fn commit_mixer_parameter_fence(&self, sequence: u64) {
        if sequence != 0
            && let Some(committed) = &self.mixer_parameter_fence
        {
            committed.fetch_max(sequence, Ordering::Release);
        }
    }

    fn commit_mixer_parameter_changes(
        &mut self,
        mut changes: MixerParameterChanges,
    ) -> MixerParameterChanges {
        if !changes.is_empty() {
            self.committed_mixer_parameter_change_sequence = self
                .committed_mixer_parameter_change_sequence
                .checked_add(1)
                .expect("mixer parameter capture sequence space exhausted");
            changes.commit(self.committed_mixer_parameter_change_sequence);
        }
        changes
    }

    fn mixer_parameter_command_sender_disconnected(&self) -> bool {
        let disconnected = self
            .mixer_parameter_command_consumer
            .as_ref()
            .is_some_and(Consumer::is_abandoned);
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    fn drain_native_device_commands(&mut self) -> usize {
        let mut accepted = 0;
        let mut pending_states = [None; MAX_QUEUED_NATIVE_DEVICE_COMMANDS];
        let mut pending_state_count = 0;
        for _ in 0..MAX_QUEUED_NATIVE_DEVICE_COMMANDS {
            let command = self
                .native_device_command_consumer
                .as_mut()
                .and_then(|consumer| consumer.pop().ok());
            let Some(command) = command else {
                break;
            };
            accepted += 1;
            match command {
                NativeDeviceCommand::SetState { device, state } => {
                    if let Some(index) =
                        pending_states[..pending_state_count]
                            .iter()
                            .position(|pending| {
                                pending.is_some_and(|(pending_device, _)| pending_device == device)
                            })
                    {
                        pending_states[index] = Some((device, state));
                    } else {
                        pending_states[pending_state_count] = Some((device, state));
                        pending_state_count += 1;
                    }
                }
                NativeDeviceCommand::SetEnabled { device, enabled } => {
                    if let Some(index) =
                        pending_states[..pending_state_count]
                            .iter()
                            .position(|pending| {
                                pending.is_some_and(|(pending_device, _)| pending_device == device)
                            })
                        && let Some((device, state)) = pending_states[index].take()
                    {
                        self.runtime
                            .apply_native_device_command(NativeDeviceCommand::SetState {
                                device,
                                state,
                            });
                    }
                    self.runtime
                        .apply_native_device_command(NativeDeviceCommand::SetEnabled {
                            device,
                            enabled,
                        });
                }
            }
        }
        for (device, state) in pending_states.into_iter().flatten() {
            self.runtime
                .apply_native_device_command(NativeDeviceCommand::SetState { device, state });
        }
        accepted
    }

    fn native_device_command_sender_disconnected(&self) -> bool {
        let disconnected = self
            .native_device_command_consumer
            .as_ref()
            .is_some_and(Consumer::is_abandoned);
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }
}

fn scene_command_layout<R: TrackWindowRenderer>(
    runtime: &RoutedMainRuntime<R>,
) -> (usize, u8, u8, u8) {
    let scene_count = runtime.scene_count();
    debug_assert!(scene_count <= hyper_playback_compiler::scene::VERSION_1_MAX_SCENES);
    let mut ready = 0_u8;
    let mut disabled = 0_u8;
    let mut consolidation = 0_u8;
    for scene_index in 0..scene_count {
        let target = match runtime.scene_availability(scene_index) {
            PreparedSceneAvailability::Ready => &mut ready,
            PreparedSceneAvailability::Disabled => &mut disabled,
            PreparedSceneAvailability::ConsolidationRequired => &mut consolidation,
            PreparedSceneAvailability::NotPrepared => continue,
        };
        *target |= 1_u8 << scene_index;
    }
    (scene_count, ready, disabled, consolidation)
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
    loudness_commands: LoudnessCommandReceiver,
}

impl<'state, 'buffers, R: TrackWindowRenderer> TransportOutputProcessor<'state, 'buffers, R> {
    /// Binds transport, scratch, interleaving, and meter publication once.
    ///
    /// # Errors
    ///
    /// Returns [`TransportOutputCreationError`] when callback storage cannot be
    /// allocated, the true-peak sample rate is unsupported, or scratch slot
    /// count/capacity cannot serve every callback.
    pub fn try_new(
        transport: &'state mut RoutedTransportRuntime<R>,
        scratch: &'state mut [RenderScratchSlot<'buffers>],
        meter_sender: &'state mut OutputMeterSender,
        loudness_commands: LoudnessCommandReceiver,
        sample_rate_hz: u32,
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
        let callback = InterleavedStereoCallback::try_new(required, sample_rate_hz)
            .map_err(TransportOutputCreationError::Callback)?;
        Ok(Self {
            transport,
            callback,
            scratch,
            meter_sender,
            loudness_commands,
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
        self.process_with_post_mix(interleaved_output, |_| {})
    }

    /// Renders one callback and applies a final planar stereo mix before all
    /// output validation and metering.
    ///
    /// # Errors
    ///
    /// Returns [`StereoCallbackError`] for invalid device-buffer shape,
    /// excessive quantum, transport failure, or non-finite final output.
    pub fn process_with_post_mix<F>(
        &mut self,
        interleaved_output: &mut [f32],
        post_mix: F,
    ) -> Result<InterleavedTransportCycle, StereoCallbackError<RuntimeProcessError>>
    where
        F: FnOnce(StereoOutput<'_>),
    {
        if !interleaved_output.len().is_multiple_of(2)
            || interleaved_output.len() / 2 > self.callback.maximum_frames()
        {
            return self.transport.process_interleaved_cycle_with_post_mix(
                &mut self.callback,
                self.scratch,
                interleaved_output,
                self.meter_sender,
                post_mix,
            );
        }
        let applied_loudness_command_count = self.drain_loudness_commands();
        let loudness_command_sender_disconnected = self.loudness_command_sender_disconnected();
        self.transport
            .process_interleaved_cycle_with_loudness_control(
                &mut self.callback,
                self.scratch,
                interleaved_output,
                self.meter_sender,
                LoudnessCycleControl {
                    applied_command_count: applied_loudness_command_count,
                    sender_disconnected: loudness_command_sender_disconnected,
                },
                post_mix,
            )
    }

    fn drain_loudness_commands(&mut self) -> usize {
        let mut applied = 0;
        for _ in 0..MAX_QUEUED_LOUDNESS_COMMANDS {
            let Ok(command) = self.loudness_commands.consumer.pop() else {
                break;
            };
            self.callback.apply_loudness_command(command);
            applied += 1;
        }
        applied
    }

    fn loudness_command_sender_disconnected(&self) -> bool {
        let disconnected = self.loudness_commands.consumer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
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
