use super::audio_preview_session::DesktopAudioPreviewProcessor;
use super::monitoring_session::{
    MonitoringConsumer, MonitoringCounters, MonitoringProducer, try_monitoring_transport,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::pipewire_output::PipeWireThreadRunner;
use super::recording_session::{
    MonitoredRecordingProducer, RecordingCoreStartError, RecordingSessionCore, start_recording_core,
};
#[cfg(target_os = "windows")]
use super::wasapi_output::WasapiThreadRunner;
use super::{
    CheckpointedOutputProcessor, DesktopControlMappingRuntime, DesktopControlSink,
    DesktopControlSinkError, DesktopDirectControlCommand, DesktopDirectControlError,
    DesktopInputStartError, DesktopInputStopError, DesktopMonitoringConfig,
    DesktopMonitoringControl, DesktopMonitoringControlError, DesktopOutputConfig,
    InputMeterReceiver, MonitoringSnapshot, OutputStreamCounters, OutputStreamStatus,
    PendingNativeShutdown, PendingNativeShutdownWait, PlaybackRecoveryCheckpoint,
    PlaybackRecoverySnapshot,
};
use super::{
    DesktopFinalizedRecording, DesktopPublishedRecording, DesktopRecordingSessionCommitError,
    DesktopRecordingSessionConfig, DesktopRecordingSessionConfigError,
    DesktopRecordingSessionStopError, DesktopRecordingWriterSettlement,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{PipeWireInputConfig as NativeInputConfig, PipeWireInputStream as NativeInputStream};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{PipeWireOutputStream, PipeWireStartError, PipeWireStartupStage, PipeWireStopError};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiOutputStream, WasapiStartError, WasapiStartupStage,
    WasapiStopError,
};
#[cfg(target_os = "windows")]
use super::{WasapiInputConfig as NativeInputConfig, WasapiInputStream as NativeInputStream};
use core::error::Error;
use core::fmt;
use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::StereoOutput;
use hyper_control_runtime::{ControlRuntime, ControlRuntimeBuildErrorReason};
use hyper_mapping_compiler::{CompiledControlMappingTable, ControlMappingCompileError};
use hyper_media_cache::{DecodedMediaCache, PreparePlaybackError};
use hyper_mixer_runtime::{
    InitialMetronomeState, InteractiveMixerParameterReceiver, InteractiveSceneLaunchReceiver,
    LoudnessCommandSender, MetronomePlan, MetronomePlanError, MixerChannelHandle,
    MixerParameterCommand, MixerParameterCommandSender, MixerParameterResolveError,
    MixerParameterStateRestoreError, MixerSendHandle, NativeDeviceCommand,
    NativeDeviceCommandSender, NativeDeviceHandle, NativeDeviceResolveError, OutputMeterReceiver,
    RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError, TransportCommand,
    TransportCommandSendError, TransportCommandSender, TransportLoopRange, TransportLoopRangeError,
    TransportOutputCreationError, TransportOutputProcessor, interactive_mixer_parameter_channel,
    interactive_scene_launch_channel, loudness_command_channel, output_meter_channel,
    playback_recovery_channel_with_initial_loop_metronome_and_native_device_state,
};
use hyper_native_device::DeviceChain;
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, LoadedPlaybackProject, MediaAssetFact};
use hyper_playback_session::{PlaybackMixerSendTap, PreparedPlaybackSession};
use hyper_project_loader::{
    PreparedProjectPlayback, ProjectPlaybackError, ProjectPlaybackLimits, prepare_project_playback,
};
use hyper_recording_metadata::{
    FinalizedRecordingMetadata, RecordingMetadataBuildError, RecordingMetadataContext,
};
use hyper_recording_transport::RecordingTransportSnapshot;
use hyper_recording_worker::{RecordingWorkerCompletion, RecordingWorkerStartError};
use hyper_storage_engine::{
    ProjectTransactionRecoveryError, ProjectTransactionRecoveryReport, recover_project_transactions,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

#[cfg(test)]
use hyper_mixer_runtime::playback_recovery_channel;

pub type DesktopPlaybackSessionInput = PreparedPlaybackSession;

/// Immutable prepared routing facts and live handle for one mixer send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopMixerSend<'a> {
    id: &'a str,
    target_name: &'a str,
    tap: DesktopMixerSendTap,
    handle: MixerSendHandle,
}

/// Immutable prepared facts and a generation-bound handle for one native
/// device instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopNativeDevice<'a> {
    track_name: &'a str,
    device_id: &'a str,
    kind_id: &'a str,
    handle: NativeDeviceHandle,
}

impl<'a> DesktopNativeDevice<'a> {
    #[must_use]
    pub const fn track_name(self) -> &'a str {
        self.track_name
    }

    #[must_use]
    pub const fn device_id(self) -> &'a str {
        self.device_id
    }

    #[must_use]
    pub const fn kind_id(self) -> &'a str {
        self.kind_id
    }

    #[must_use]
    pub const fn handle(self) -> NativeDeviceHandle {
        self.handle
    }
}

impl<'a> DesktopMixerSend<'a> {
    #[must_use]
    pub const fn id(self) -> &'a str {
        self.id
    }

    #[must_use]
    pub const fn target_name(self) -> &'a str {
        self.target_name
    }

    #[must_use]
    pub const fn tap(self) -> DesktopMixerSendTap {
        self.tap
    }

    #[must_use]
    pub const fn handle(self) -> MixerSendHandle {
        self.handle
    }
}

/// Fixed tap position of a prepared desktop mixer send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopMixerSendTap {
    PreFader,
    PostFader,
}

trait DesktopPlaybackSource: fmt::Debug + Send + Sync {
    fn session(&self) -> &PreparedPlaybackSession;

    fn project(&self) -> Option<&PreparedProjectPlayback>;
}

impl DesktopPlaybackSource for PreparedPlaybackSession {
    fn session(&self) -> &PreparedPlaybackSession {
        self
    }

    fn project(&self) -> Option<&PreparedProjectPlayback> {
        None
    }
}

impl DesktopPlaybackSource for PreparedProjectPlayback {
    fn session(&self) -> &PreparedPlaybackSession {
        PreparedProjectPlayback::session(self)
    }

    fn project(&self) -> Option<&PreparedProjectPlayback> {
        Some(self)
    }
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeDesktopOutputStartError = PipeWireStartError;
#[cfg(target_os = "windows")]
type NativeDesktopOutputStartError = WasapiStartError;

#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeDesktopOutputStopError = PipeWireStopError;
#[cfg(target_os = "windows")]
type NativeDesktopOutputStopError = WasapiStopError;

#[derive(Debug)]
pub enum DesktopOutputStartError {
    ThreadSpawn(std::io::Error),
    ThreadPanicked,
    StartupTimeout,
    CancellationPending {
        shutdown: Box<PendingNativeShutdown>,
    },
    ThreadTerminated,
    Backend {
        source: Box<dyn Error + Send + Sync>,
    },
}

impl fmt::Display for DesktopOutputStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(_) => formatter.write_str("desktop output thread could not start"),
            Self::ThreadPanicked => formatter.write_str("desktop output thread panicked"),
            Self::StartupTimeout => formatter.write_str("desktop output startup timed out"),
            Self::CancellationPending { .. } => {
                formatter.write_str("desktop output startup cancellation is still pending")
            }
            Self::ThreadTerminated => {
                formatter.write_str("desktop output thread terminated during startup")
            }
            Self::Backend { source } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopOutputStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(source) => Some(source),
            Self::Backend { source } => Some(source.as_ref()),
            Self::ThreadPanicked
            | Self::StartupTimeout
            | Self::CancellationPending { .. }
            | Self::ThreadTerminated => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopOutputStopError {
    RuntimeFailed,
    ThreadPanicked,
    ShutdownPending {
        shutdown: Box<PendingNativeShutdown>,
    },
}

impl fmt::Display for DesktopOutputStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeFailed => "desktop output failed before shutdown completed",
            Self::ThreadPanicked => "desktop output thread panicked during shutdown",
            Self::ShutdownPending { .. } => "desktop output shutdown is still pending",
        })
    }
}

impl Error for DesktopOutputStopError {}

impl DesktopOutputStopError {
    /// Returns the sole native-thread owner when shutdown has not completed.
    #[must_use]
    pub fn into_pending_shutdown(self) -> Option<PendingNativeShutdown> {
        match self {
            Self::ShutdownPending { shutdown } => Some(*shutdown),
            Self::RuntimeFailed | Self::ThreadPanicked => None,
        }
    }
}

impl From<NativeDesktopOutputStartError> for DesktopOutputStartError {
    fn from(source: NativeDesktopOutputStartError) -> Self {
        match source {
            NativeDesktopOutputStartError::ThreadSpawn(source) => Self::ThreadSpawn(source),
            NativeDesktopOutputStartError::ThreadPanicked => Self::ThreadPanicked,
            NativeDesktopOutputStartError::StartupTimeout => Self::StartupTimeout,
            NativeDesktopOutputStartError::CancellationPending { shutdown } => {
                Self::CancellationPending { shutdown }
            }
            NativeDesktopOutputStartError::ThreadTerminated => Self::ThreadTerminated,
            source @ (NativeDesktopOutputStartError::Backend { .. }
            | NativeDesktopOutputStartError::BackendSource { .. }) => Self::Backend {
                source: Box::new(source),
            },
        }
    }
}

impl From<NativeDesktopOutputStopError> for DesktopOutputStopError {
    fn from(source: NativeDesktopOutputStopError) -> Self {
        match source {
            NativeDesktopOutputStopError::RuntimeFailed => Self::RuntimeFailed,
            NativeDesktopOutputStopError::ThreadPanicked => Self::ThreadPanicked,
            NativeDesktopOutputStopError::ShutdownPending { shutdown } => {
                Self::ShutdownPending { shutdown }
            }
        }
    }
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
type DesktopOutputStream = PipeWireOutputStream;
#[cfg(target_os = "windows")]
type DesktopOutputStream = WasapiOutputStream;

#[cfg(all(target_os = "linux", target_endian = "little"))]
type DesktopThreadRunner = PipeWireThreadRunner;
#[cfg(target_os = "windows")]
type DesktopThreadRunner = WasapiThreadRunner;

#[derive(Debug)]
pub enum DesktopPlaybackSessionBuildError {
    SampleRateMismatch { project: u32, output: u32 },
    AssetFactCapacityUnavailable,
    Playback(AdapterCompileError),
    Media(PreparePlaybackError),
    Runtime(RuntimeCreationError),
    Metronome(MetronomePlanError),
    PersistedLoopRange(TransportLoopRangeError),
    PersistedLoopCommand(TransportCommandSendError),
    MixerRecovery(MixerParameterStateRestoreError),
    ControlMappingCopy(ControlMappingCompileError),
    ControlMapping(DesktopControlMappingBuildError),
    ControlMappingOwnerCapacityUnavailable,
    ScratchCapacityUnavailable,
    Output(TransportOutputCreationError),
    MonitoringScratchCapacityUnavailable,
    AudioPreview(super::DesktopAudioPreviewBuildError),
}

#[derive(Debug)]
pub enum DesktopControlMappingBuildError {
    CapacityUnavailable,
    Target {
        binding_index: usize,
        source: DesktopControlSinkError,
    },
}

impl fmt::Display for DesktopControlMappingBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("control mapping runtime capacity is unavailable")
            }
            Self::Target {
                binding_index,
                source,
            } => write!(
                formatter,
                "control mapping binding {binding_index} could not bind: {source}"
            ),
        }
    }
}

impl Error for DesktopControlMappingBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Target { source, .. } => Some(source),
            Self::CapacityUnavailable => None,
        }
    }
}

impl fmt::Display for DesktopPlaybackSessionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleRateMismatch { project, output } => write!(
                formatter,
                "project sample rate is {project} Hz; output is configured for {output} Hz"
            ),
            Self::AssetFactCapacityUnavailable => {
                formatter.write_str("media fact capacity is unavailable")
            }
            Self::Playback(source) => write!(formatter, "playback compilation failed: {source}"),
            Self::Media(source) => write!(formatter, "media preparation failed: {source}"),
            Self::Runtime(source) => write!(formatter, "runtime construction failed: {source}"),
            Self::Metronome(source) => write!(formatter, "metronome preparation failed: {source}"),
            Self::PersistedLoopRange(source) => {
                write!(formatter, "persisted loop range is invalid: {source}")
            }
            Self::PersistedLoopCommand(source) => {
                write!(formatter, "persisted loop cannot be armed: {source}")
            }
            Self::MixerRecovery(source) => {
                write!(formatter, "mixer recovery failed: {source}")
            }
            Self::ControlMappingCopy(source) => {
                write!(formatter, "control mapping copy failed: {source}")
            }
            Self::ControlMapping(source) => {
                write!(formatter, "control mapping runtime failed: {source}")
            }
            Self::ControlMappingOwnerCapacityUnavailable => {
                formatter.write_str("control mapping owner capacity is unavailable")
            }
            Self::ScratchCapacityUnavailable => {
                formatter.write_str("render scratch capacity is unavailable")
            }
            Self::Output(source) => {
                write!(formatter, "transport output construction failed: {source}")
            }
            Self::MonitoringScratchCapacityUnavailable => {
                formatter.write_str("playback monitoring scratch capacity is unavailable")
            }
            Self::AudioPreview(source) => {
                write!(formatter, "audio preview preparation failed: {source}")
            }
        }
    }
}

impl Error for DesktopPlaybackSessionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Playback(source) => Some(source),
            Self::Media(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Metronome(source) => Some(source),
            Self::PersistedLoopRange(source) => Some(source),
            Self::PersistedLoopCommand(source) => Some(source),
            Self::MixerRecovery(source) => Some(source),
            Self::ControlMappingCopy(source) => Some(source),
            Self::ControlMapping(source) => Some(source),
            Self::Output(source) => Some(source),
            Self::AudioPreview(source) => Some(source),
            Self::SampleRateMismatch { .. }
            | Self::AssetFactCapacityUnavailable
            | Self::ControlMappingOwnerCapacityUnavailable
            | Self::ScratchCapacityUnavailable
            | Self::MonitoringScratchCapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackSessionStartError {
    Build(DesktopPlaybackSessionBuildError),
    Output(DesktopOutputStartError),
    OutputCleanup {
        source: Box<DesktopPlaybackSessionStartError>,
        cleanup: DesktopOutputStopError,
    },
    PublicationUnavailable,
    MonitoringCapacityUnavailable,
    MonitoringControl(DesktopMonitoringControlError),
    Input(DesktopInputStartError),
    Monitored {
        source: Box<DesktopPlaybackSessionStartError>,
        input_cleanup: Option<DesktopInputStopError>,
    },
}

impl fmt::Display for DesktopPlaybackSessionStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build(source) => source.fmt(formatter),
            Self::Output(source) => source.fmt(formatter),
            Self::OutputCleanup { source, cleanup } => {
                write!(formatter, "{source}; output cleanup also failed: {cleanup}")
            }
            Self::PublicationUnavailable => {
                formatter.write_str("playback controls were unavailable after output startup")
            }
            Self::MonitoringCapacityUnavailable => {
                formatter.write_str("playback monitoring capacity is unavailable")
            }
            Self::MonitoringControl(source) => source.fmt(formatter),
            Self::Input(source) => write!(formatter, "playback monitoring input failed: {source}"),
            Self::Monitored {
                source,
                input_cleanup: None,
            } => source.fmt(formatter),
            Self::Monitored {
                source,
                input_cleanup: Some(cleanup),
            } => write!(formatter, "{source}; input cleanup also failed: {cleanup}"),
        }
    }
}

impl Error for DesktopPlaybackSessionStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Build(source) => Some(source),
            Self::Output(source) => Some(source),
            Self::OutputCleanup { source, .. } | Self::Monitored { source, .. } => {
                Some(source.as_ref())
            }
            Self::MonitoringControl(source) => Some(source),
            Self::Input(source) => Some(source),
            Self::PublicationUnavailable | Self::MonitoringCapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub struct DesktopPlaybackSessionStopError {
    pub input: Option<DesktopInputStopError>,
    pub output: Option<DesktopOutputStopError>,
}

impl fmt::Display for DesktopPlaybackSessionStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.input, &self.output) {
            (Some(input), Some(output)) => write!(
                formatter,
                "playback monitoring input shutdown failed: {input}; output shutdown failed: {output}"
            ),
            (Some(input), None) => write!(
                formatter,
                "playback monitoring input shutdown failed: {input}"
            ),
            (None, Some(output)) => output.fmt(formatter),
            (None, None) => formatter.write_str("playback native shutdown failed"),
        }
    }
}

impl Error for DesktopPlaybackSessionStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.input
            .as_ref()
            .map(|source| source as &(dyn Error + 'static))
            .or_else(|| {
                self.output
                    .as_ref()
                    .map(|source| source as &(dyn Error + 'static))
            })
    }
}

/// Failure to load and start one verified project on a desktop output.
#[derive(Debug)]
pub enum DesktopProjectOpenError {
    Recovery(ProjectTransactionRecoveryError),
    Load {
        source: Box<ProjectPlaybackError>,
        recovery: ProjectTransactionRecoveryReport,
    },
    Start {
        source: Box<DesktopPlaybackSessionStartError>,
        recovery: ProjectTransactionRecoveryReport,
    },
}

impl DesktopProjectOpenError {
    /// Returns every recovery outcome completed before project open failed.
    #[must_use]
    pub const fn recovery_report(&self) -> &ProjectTransactionRecoveryReport {
        match self {
            Self::Recovery(source) => source.completed(),
            Self::Load { recovery, .. } | Self::Start { recovery, .. } => recovery,
        }
    }
}

impl fmt::Display for DesktopProjectOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recovery(source) => write!(formatter, "project recovery failed: {source}"),
            Self::Load { source, .. } => write!(formatter, "project loading failed: {source}"),
            Self::Start { source, .. } => {
                write!(formatter, "desktop playback startup failed: {source}")
            }
        }
    }
}

impl Error for DesktopProjectOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Recovery(source) => Some(source),
            Self::Load { source, .. } => Some(source.as_ref()),
            Self::Start { source, .. } => Some(source.as_ref()),
        }
    }
}

#[derive(Debug)]
struct SessionHandles {
    controls: SessionControlHandles,
    loudness_commands: LoudnessCommandSender,
    meters: OutputMeterReceiver,
    mixer_parameter_observations: InteractiveMixerParameterReceiver,
    scene_launches: InteractiveSceneLaunchReceiver,
    recovery: PlaybackRecoveryCheckpoint,
    preview: super::DesktopAudioPreviewControl,
}

/// The session's sole control owner.
///
/// Every project has one, with or without a control-mapping document: it owns
/// the live mixer shadow state and any panel bindings, and a panel is
/// discovered hardware rather than something a project declares.
#[derive(Debug)]
struct SessionControlHandles(Vec<DesktopControlMappingRuntime>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Desktop-facing launch status for one validated project scene.
pub enum DesktopSceneAvailability {
    /// The scene has all required prepared audio and accepts launch commands.
    Ready,
    /// The scene document is explicitly disabled.
    Disabled,
    /// The scene requires offline consolidation before realtime playback.
    ConsolidationRequired,
    /// Required scene audio is not prepared in the active session.
    NotPrepared,
}

impl From<hyper_media_cache::PreparedSceneAvailability> for DesktopSceneAvailability {
    fn from(source: hyper_media_cache::PreparedSceneAvailability) -> Self {
        match source {
            hyper_media_cache::PreparedSceneAvailability::Ready => Self::Ready,
            hyper_media_cache::PreparedSceneAvailability::Disabled => Self::Disabled,
            hyper_media_cache::PreparedSceneAvailability::ConsolidationRequired => {
                Self::ConsolidationRequired
            }
            hyper_media_cache::PreparedSceneAvailability::NotPrepared => Self::NotPrepared,
        }
    }
}

impl SessionControlHandles {
    fn runtime(&self) -> &DesktopControlMappingRuntime {
        self.0
            .first()
            .expect("a control owner contains exactly one runtime")
    }

    fn runtime_mut(&mut self) -> &mut DesktopControlMappingRuntime {
        self.0
            .first_mut()
            .expect("a control owner contains exactly one runtime")
    }

    fn transport(&self) -> &TransportCommandSender {
        self.runtime().sink().transport_sender()
    }

    fn mixer(&self) -> &MixerParameterCommandSender {
        self.runtime().sink().mixer_sender()
    }

    fn native_devices(&self) -> Option<&NativeDeviceCommandSender> {
        self.runtime().sink().native_device_sender()
    }

    fn mapping_statuses(&self) -> &[hyper_control_runtime::ControlBindingStatus] {
        self.runtime().statuses()
    }

    fn dispatch_mapping_event(
        &mut self,
        event: hyper_control_runtime::ControlEvent,
        context: hyper_control_runtime::ControlContext,
    ) -> Result<
        hyper_control_runtime::ControlDispatchOutcome,
        hyper_control_runtime::ControlDispatchError<DesktopControlSinkError>,
    > {
        self.runtime_mut().dispatch(event, context)
    }

    fn try_send_transport(
        &mut self,
        command: TransportCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.runtime_mut()
            .try_send_direct(DesktopDirectControlCommand::Transport(command))
    }

    fn try_start_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError> {
        let _ = self.runtime_mut().sink_mut().take_capture_start_sequence();
        self.runtime_mut()
            .try_send_direct(DesktopDirectControlCommand::CaptureStart)?;
        Ok(self
            .runtime_mut()
            .sink_mut()
            .take_capture_start_sequence()
            .expect("a successful capture-start direct command records its sequence"))
    }

    fn resolve_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.runtime().sink().resolve_channel(track_id)
    }

    fn resolve_channel_index(
        &self,
        channel_index: usize,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.mixer().resolve_channel_index(channel_index)
    }

    fn resolve_send(&self, send_id: &str) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.runtime().sink().resolve_send(send_id)
    }

    fn resolve_send_address(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.mixer().resolve_send_address(channel_index, send_index)
    }

    fn try_send_mixer(
        &mut self,
        command: MixerParameterCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.runtime_mut()
            .try_send_direct(DesktopDirectControlCommand::Mixer(command))
    }

    fn resolve_native_device(
        &self,
        track_id: &str,
        device_id: &str,
    ) -> Result<NativeDeviceHandle, NativeDeviceResolveError> {
        self.native_devices()
            .ok_or(NativeDeviceResolveError::DeviceNotFound)?
            .resolve_device(track_id, device_id)
    }

    fn try_send_native_device(
        &mut self,
        command: NativeDeviceCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.runtime_mut()
            .try_send_direct(DesktopDirectControlCommand::NativeDevice(command))
    }

    fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.runtime_mut().sink_mut().try_send_mixer_fence()
    }

    fn try_send_native_device_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.runtime_mut().sink_mut().try_send_native_device_fence()
    }

    fn try_send_transport_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.runtime_mut().sink_mut().try_send_transport_fence()
    }

    fn committed_mixer_fence(&self) -> u64 {
        self.mixer().committed_fence()
    }
}

#[derive(Debug)]
enum SessionPublication {
    Prepared(SessionHandles),
    Failed(DesktopPlaybackSessionBuildError),
}

/// One live native desktop output with its bounded application-thread controls.
#[derive(Debug)]
pub struct DesktopPlaybackSession {
    output: DesktopOutputStream,
    controls: SessionControlHandles,
    loudness_commands: LoudnessCommandSender,
    meters: Option<OutputMeterReceiver>,
    mixer_parameter_observations: Option<InteractiveMixerParameterReceiver>,
    scene_launches: Option<InteractiveSceneLaunchReceiver>,
    recovery: PlaybackRecoveryCheckpoint,
    preview: super::DesktopAudioPreviewControl,
    config: DesktopOutputConfig,
    source: Arc<dyn DesktopPlaybackSource>,
    monitoring: Option<PlaybackMonitoringSession>,
    project_open_recovery: Option<ProjectTransactionRecoveryReport>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioClipPreviewSource {
    source_index: usize,
    view_start_frame: usize,
    view_frame_count: usize,
}

impl AudioClipPreviewSource {
    fn full_range(self) -> Result<(usize, usize), super::DesktopAudioPreviewCommandError> {
        selected_audio_clip_preview_range(
            self.view_start_frame,
            self.view_frame_count,
            self.view_start_frame,
            self.view_frame_count,
        )
    }

    fn selected_range(
        self,
        source_start_frame: usize,
        source_frame_count: usize,
    ) -> Result<(usize, usize), super::DesktopAudioPreviewCommandError> {
        selected_audio_clip_preview_range(
            self.view_start_frame,
            self.view_frame_count,
            source_start_frame,
            source_frame_count,
        )
    }
}

fn selected_audio_clip_preview_range(
    committed_start_frame: usize,
    committed_frame_count: usize,
    requested_start_frame: usize,
    requested_frame_count: usize,
) -> Result<(usize, usize), super::DesktopAudioPreviewCommandError> {
    if committed_frame_count == 0 || requested_frame_count == 0 {
        return Err(super::DesktopAudioPreviewCommandError::SourceRangeUnavailable);
    }
    let committed_end = committed_start_frame
        .checked_add(committed_frame_count)
        .ok_or(super::DesktopAudioPreviewCommandError::SourceRangeUnavailable)?;
    let requested_end = requested_start_frame
        .checked_add(requested_frame_count)
        .ok_or(super::DesktopAudioPreviewCommandError::SourceRangeUnavailable)?;
    if requested_start_frame < committed_start_frame || requested_end > committed_end {
        return Err(super::DesktopAudioPreviewCommandError::SourceRangeUnavailable);
    }
    let relative_start_frame = requested_start_frame
        .checked_sub(committed_start_frame)
        .ok_or(super::DesktopAudioPreviewCommandError::SourceRangeUnavailable)?;
    Ok((relative_start_frame, requested_frame_count))
}

fn resolve_audio_clip_preview_source(
    project: &PreparedProjectPlayback,
    clip_id: &str,
) -> Result<AudioClipPreviewSource, super::DesktopAudioPreviewCommandError> {
    let loaded = project.session().parts().0;
    resolve_audio_clip_preview_source_from_loaded(
        loaded,
        project.recording_metadata().len(),
        clip_id,
    )
}

fn resolve_audio_clip_preview_source_from_loaded(
    loaded: &LoadedPlaybackProject,
    recording_count: usize,
    clip_id: &str,
) -> Result<AudioClipPreviewSource, super::DesktopAudioPreviewCommandError> {
    let clip_index = (0..loaded.clip_count())
        .find(|&index| loaded.clip_id_at(index) == Some(clip_id))
        .ok_or(super::DesktopAudioPreviewCommandError::ClipUnavailable)?;
    let source_start_frame = loaded
        .clip_source_start_frame_at(clip_index)
        .ok_or(super::DesktopAudioPreviewCommandError::ClipUnavailable)?;
    let source_frame_count = loaded
        .clip_source_frame_count_at(clip_index)
        .ok_or(super::DesktopAudioPreviewCommandError::ClipUnavailable)?;
    let source_index = recording_count
        .checked_add(clip_index)
        .ok_or(super::DesktopAudioPreviewCommandError::SequenceExhausted)?;
    Ok(AudioClipPreviewSource {
        source_index,
        view_start_frame: source_start_frame,
        view_frame_count: source_frame_count,
    })
}

#[derive(Debug)]
struct PlaybackMonitoringSession {
    input: NativeInputStream,
    config: DesktopMonitoringConfig,
    counters: MonitoringCounters,
    control: DesktopMonitoringControl,
    meter: Option<InputMeterReceiver>,
}

/// The monitor configuration and control state that must be established before
/// either native stream is allowed to start.
///
/// This stays private to the playback owner because it is restart authority,
/// not a separately addressable session configuration surface.
#[derive(Clone, Debug, PartialEq)]
struct PlaybackMonitoringStartup {
    config: DesktopMonitoringConfig,
    gain_linear: f32,
    muted: bool,
}

impl PlaybackMonitoringStartup {
    fn new(config: DesktopMonitoringConfig, gain_linear: f32, muted: bool) -> Self {
        Self {
            config,
            gain_linear,
            muted,
        }
    }

    fn with_output(
        mut self,
        output: DesktopOutputConfig,
    ) -> Result<Self, super::DesktopMonitoringConfigError> {
        self.config = self.config.with_output(output)?;
        Ok(self)
    }
}

fn playback_monitoring_recovery_startup(
    config: &DesktopMonitoringConfig,
    control: &DesktopMonitoringControl,
) -> PlaybackMonitoringStartup {
    PlaybackMonitoringStartup::new(config.clone(), control.gain_linear(), control.muted())
}

fn prepare_playback_monitoring_transport(
    monitoring: &PlaybackMonitoringStartup,
) -> Result<
    (
        MonitoringProducer,
        MonitoringConsumer,
        MonitoringCounters,
        DesktopMonitoringControl,
        InputMeterReceiver,
    ),
    DesktopPlaybackSessionStartError,
> {
    let (producer, consumer, counters, control, meter) =
        try_monitoring_transport(&monitoring.config)
            .map_err(|_| DesktopPlaybackSessionStartError::MonitoringCapacityUnavailable)?;
    control
        .set_gain_linear(monitoring.gain_linear)
        .map_err(DesktopPlaybackSessionStartError::MonitoringControl)?;
    control.set_muted(monitoring.muted);
    Ok((producer, consumer, counters, control, meter))
}

fn recover_and_prepare_project(
    root: &Path,
    limits: ProjectPlaybackLimits,
) -> Result<(PreparedProjectPlayback, ProjectTransactionRecoveryReport), DesktopProjectOpenError> {
    let recovery = recover_project_transactions(root).map_err(DesktopProjectOpenError::Recovery)?;
    let project = match prepare_project_playback(root, limits) {
        Ok(project) => project,
        Err(source) => {
            return Err(DesktopProjectOpenError::Load {
                source: Box::new(source),
                recovery,
            });
        }
    };
    Ok((project, recovery))
}

struct PlaybackMonitoringMixer {
    consumer: MonitoringConsumer,
    interleaved: Vec<f32>,
    enabled: bool,
}

impl PlaybackMonitoringMixer {
    fn try_new(
        consumer: MonitoringConsumer,
        maximum_block_frames: usize,
    ) -> Result<Self, DesktopPlaybackSessionBuildError> {
        let sample_capacity = maximum_block_frames
            .checked_mul(2)
            .ok_or(DesktopPlaybackSessionBuildError::MonitoringScratchCapacityUnavailable)?;
        let mut interleaved = Vec::new();
        interleaved
            .try_reserve_exact(sample_capacity)
            .map_err(|_| DesktopPlaybackSessionBuildError::MonitoringScratchCapacityUnavailable)?;
        interleaved.resize(sample_capacity, 0.0);
        Ok(Self {
            consumer,
            interleaved,
            enabled: true,
        })
    }

    fn mix(&mut self, output: StereoOutput<'_>) {
        if !self.enabled {
            return;
        }
        if output
            .left
            .iter()
            .chain(output.right.iter())
            .any(|sample| !sample.is_finite())
        {
            return;
        }
        let frames = output.left.len();
        let samples = frames * 2;
        if self
            .consumer
            .process(&mut self.interleaved[..samples])
            .is_err()
        {
            self.enabled = false;
            return;
        }
        if !mix_monitoring_interleaved(output, &self.interleaved[..samples]) {
            self.consumer.record_processor_failure();
            self.enabled = false;
        }
    }
}

fn mix_monitoring_interleaved(output: StereoOutput<'_>, monitoring: &[f32]) -> bool {
    let StereoOutput { left, right } = output;
    debug_assert_eq!(left.len(), right.len());
    debug_assert_eq!(monitoring.len(), left.len() * 2);
    let valid = left
        .iter()
        .zip(right.iter())
        .zip(monitoring.chunks_exact(2))
        .all(|((left, right), monitor)| {
            (*left + monitor[0]).is_finite() && (*right + monitor[1]).is_finite()
        });
    if !valid {
        return false;
    }
    for (frame, monitor) in monitoring.chunks_exact(2).enumerate() {
        left[frame] += monitor[0];
        right[frame] += monitor[1];
    }
    true
}

struct PreviewingTransportOutputProcessor<'state, 'buffers, 'media, R> {
    playback: TransportOutputProcessor<'state, 'buffers, R>,
    monitoring: Option<PlaybackMonitoringMixer>,
    preview: DesktopAudioPreviewProcessor<'media>,
}

impl<R: hyper_mixer_runtime::TrackWindowRenderer> CheckpointedOutputProcessor
    for PreviewingTransportOutputProcessor<'_, '_, '_, R>
{
    type Error = hyper_audio_engine::StereoCallbackError<hyper_mixer_runtime::RuntimeProcessError>;

    fn maximum_block_frames(&self) -> usize {
        self.playback.maximum_block_frames()
    }

    fn process_checkpointed(
        &mut self,
        output: &mut [f32],
    ) -> Result<hyper_mixer_runtime::InterleavedTransportCycle, Self::Error> {
        let Self {
            playback,
            monitoring,
            preview,
        } = self;
        playback.process_with_post_mix(output, |output| {
            let mut output = output;
            if !preview.process(&mut output)
                && let Some(monitoring) = monitoring
            {
                monitoring.mix(output);
            }
        })
    }
}

/// Restart authority retained after a desktop output has been fully torn down.
///
/// Decoded media remains shared and immutable. Each restart rebuilds only the
/// compiled callback/runtime state and restores the final committed checkpoint
/// before the replacement stream is published.
#[derive(Debug)]
pub struct DesktopPlaybackRecovery {
    config: DesktopOutputConfig,
    monitoring: Option<PlaybackMonitoringStartup>,
    source: Option<Arc<dyn DesktopPlaybackSource>>,
    checkpoint: Box<PlaybackRecoverySnapshot>,
}

#[derive(Debug)]
pub struct DesktopPlaybackRecoveryStopError {
    pub input: Option<DesktopInputStopError>,
    pub output: Option<DesktopOutputStopError>,
    recovery: Box<DesktopPlaybackRecovery>,
}

impl DesktopPlaybackRecoveryStopError {
    #[cfg(test)]
    fn from_stop(source: DesktopOutputStopError, recovery: DesktopPlaybackRecovery) -> Self {
        Self {
            input: None,
            output: Some(source),
            recovery: Box::new(recovery),
        }
    }

    fn has_pending_shutdown(&self) -> bool {
        matches!(
            self.input,
            Some(DesktopInputStopError::ShutdownPending { .. })
        ) || matches!(
            self.output,
            Some(DesktopOutputStopError::ShutdownPending { .. })
        )
    }

    fn primary_error(&self) -> &(dyn Error + 'static) {
        if let Some(source) = &self.input {
            source
        } else if let Some(source) = &self.output {
            source
        } else {
            unreachable!("playback recovery stop error requires a native failure")
        }
    }

    /// Returns restart authority only when the backend thread is known to have
    /// terminated.
    /// Returns the fixed number of validated scenes owned by this session.
    #[must_use]
    pub fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        if self.has_pending_shutdown() {
            None
        } else {
            Some(&self.recovery)
        }
    }

    /// Returns restart authority only when the backend thread is known to have
    /// terminated.
    /// Consumes a joined stop error and returns its restart authority.
    ///
    /// # Errors
    ///
    /// Returns the unchanged error while native shutdown remains pending.
    pub fn into_recovery(self) -> Result<DesktopPlaybackRecovery, Self> {
        if self.has_pending_shutdown() {
            Err(self)
        } else {
            Ok(*self.recovery)
        }
    }

    /// Returns one scene's fixed prepared launch status.
    #[must_use]
    pub fn is_shutdown_pending(&self) -> bool {
        self.has_pending_shutdown()
    }

    /// Returns the sole pending native owner together with restart authority.
    ///
    /// # Errors
    ///
    /// Returns the unchanged error when native teardown already completed.
    pub fn into_pending(self) -> Result<DesktopPlaybackRecoveryPendingStop, Self> {
        if !self.has_pending_shutdown() {
            return Err(self);
        }
        let (input, input_failure) = match self.input {
            Some(DesktopInputStopError::ShutdownPending { shutdown }) => (Some(*shutdown), None),
            Some(DesktopInputStopError::RuntimeFailed) => {
                (None, Some(NativeStopFailure::RuntimeFailed))
            }
            Some(DesktopInputStopError::ThreadPanicked) => {
                (None, Some(NativeStopFailure::ThreadPanicked))
            }
            None => (None, None),
        };
        let (output, output_failure) = match self.output {
            Some(DesktopOutputStopError::ShutdownPending { shutdown }) => (Some(*shutdown), None),
            Some(DesktopOutputStopError::RuntimeFailed) => {
                (None, Some(NativeStopFailure::RuntimeFailed))
            }
            Some(DesktopOutputStopError::ThreadPanicked) => {
                (None, Some(NativeStopFailure::ThreadPanicked))
            }
            None => (None, None),
        };
        Ok(DesktopPlaybackRecoveryPendingStop {
            input,
            output,
            input_failure,
            output_failure,
            recovery: *self.recovery,
        })
    }
}

impl fmt::Display for DesktopPlaybackRecoveryStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.input, &self.output) {
            (Some(input), Some(output)) => write!(
                formatter,
                "playback monitoring input shutdown failed: {input}; output shutdown failed: {output}"
            ),
            (Some(input), None) => write!(
                formatter,
                "playback monitoring input shutdown failed: {input}"
            ),
            (None, Some(output)) => output.fmt(formatter),
            (None, None) => formatter.write_str("playback native shutdown failed"),
        }
    }
}

impl Error for DesktopPlaybackRecoveryStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.primary_error())
    }
}

/// The sole playback restart authority produced when a recording-capable
/// playback owner stops.
///
/// A native shutdown failure remains attached to this value. Callers must not
/// create a replacement playback owner until [`Self::into_recovery`] succeeds.
#[derive(Debug)]
pub enum DesktopPlaybackRecordingRecovery {
    Ready(DesktopPlaybackRecovery),
    NativeShutdown(Box<DesktopPlaybackRecoveryStopError>),
}

impl DesktopPlaybackRecordingRecovery {
    /// Returns recovery only after every native owner has terminated.
    ///
    /// # Errors
    ///
    /// Returns the original shutdown evidence while a native owner is still
    /// pending.
    pub fn into_recovery(
        self,
    ) -> Result<DesktopPlaybackRecovery, DesktopPlaybackRecoveryStopError> {
        match self {
            Self::Ready(recovery) => Ok(recovery),
            Self::NativeShutdown(source) => source.into_recovery(),
        }
    }

    /// Returns the completed recovery authority when native teardown was
    /// clean, or when a terminal (non-pending) native failure occurred.
    #[must_use]
    pub fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        match self {
            Self::Ready(recovery) => Some(recovery),
            Self::NativeShutdown(source) => source.recovery(),
        }
    }

    /// Returns native shutdown evidence, including any pending owner.
    #[must_use]
    pub fn native_shutdown_error(&self) -> Option<&DesktopPlaybackRecoveryStopError> {
        match self {
            Self::Ready(_) => None,
            Self::NativeShutdown(source) => Some(source),
        }
    }

    /// Returns whether recording settlement observed terminal output failure.
    /// Pending output teardown is not terminal failure evidence.
    #[must_use]
    pub fn output_shutdown_failed(&self) -> bool {
        self.native_shutdown_error().is_some_and(|source| {
            matches!(
                source.output,
                Some(
                    DesktopOutputStopError::RuntimeFailed | DesktopOutputStopError::ThreadPanicked
                )
            )
        })
    }
}

/// A settled recording result paired with the only authority capable of
/// restarting the playback stream that carried it.
#[derive(Debug)]
pub struct DesktopPlaybackRecordingOutcome<T> {
    recording: T,
    recovery: DesktopPlaybackRecordingRecovery,
}

impl<T> DesktopPlaybackRecordingOutcome<T> {
    #[must_use]
    pub const fn recording(&self) -> &T {
        &self.recording
    }

    #[must_use]
    pub const fn recovery(&self) -> &DesktopPlaybackRecordingRecovery {
        &self.recovery
    }

    #[must_use]
    pub fn into_parts(self) -> (T, DesktopPlaybackRecordingRecovery) {
        (self.recording, self.recovery)
    }
}

pub type DesktopPlaybackPublishedRecording =
    DesktopPlaybackRecordingOutcome<DesktopPublishedRecording>;
pub type DesktopPlaybackFinalizedRecording =
    DesktopPlaybackRecordingOutcome<DesktopFinalizedRecording>;
pub type DesktopPlaybackRecordingAbort = DesktopPlaybackRecordingOutcome<RecordingWorkerCompletion>;

/// Failure to replace stopped monitored playback with its recording-capable
/// counterpart. Every variant after writer startup retains both the journal
/// settlement and the restart authority for the stopped playback owner.
#[derive(Debug)]
pub enum DesktopPlaybackRecordingStartError {
    OutputNotPinned {
        recovery: Box<DesktopPlaybackRecovery>,
    },
    PlaybackUnavailable {
        recovery: Box<DesktopPlaybackRecovery>,
    },
    MonitoringUnavailable {
        recovery: Box<DesktopPlaybackRecovery>,
    },
    InputMismatch {
        recovery: Box<DesktopPlaybackRecovery>,
    },
    Monitoring {
        source: DesktopPlaybackSessionStartError,
        recovery: Box<DesktopPlaybackRecovery>,
    },
    RecordingConfig {
        source: DesktopRecordingSessionConfigError,
        recovery: Box<DesktopPlaybackRecovery>,
    },
    RecordingTransport {
        source: hyper_recording_transport::RecordingTransportBuildError,
        recovery: Box<DesktopPlaybackRecovery>,
    },
    RecordingWriter {
        source: RecordingWorkerStartError,
        recovery: Box<DesktopPlaybackRecovery>,
    },
    Input {
        journal_path: std::path::PathBuf,
        source: DesktopInputStartError,
        writer: Box<DesktopRecordingWriterSettlement>,
        recovery: Box<DesktopPlaybackRecovery>,
    },
    Playback {
        journal_path: std::path::PathBuf,
        source: Box<DesktopPlaybackSessionStartError>,
        input_cleanup: Option<DesktopInputStopError>,
        writer: Box<DesktopRecordingWriterSettlement>,
        recovery: Box<DesktopPlaybackRecovery>,
    },
}

impl DesktopPlaybackRecordingStartError {
    /// Returns the playback recovery authority retained by this failed
    /// transition.
    #[must_use]
    pub fn recovery(&self) -> &DesktopPlaybackRecovery {
        match self {
            Self::OutputNotPinned { recovery }
            | Self::PlaybackUnavailable { recovery }
            | Self::MonitoringUnavailable { recovery }
            | Self::InputMismatch { recovery }
            | Self::Monitoring { recovery, .. }
            | Self::RecordingConfig { recovery, .. }
            | Self::RecordingTransport { recovery, .. }
            | Self::RecordingWriter { recovery, .. }
            | Self::Input { recovery, .. }
            | Self::Playback { recovery, .. } => recovery,
        }
    }

    /// Retains the complete failed-start transaction until native threads and
    /// the recording writer have settled. Playback cannot restart before then.
    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecordingStartRecovery {
        let input_failed = matches!(
            &self,
            Self::Playback {
                input_cleanup: Some(DesktopInputStopError::RuntimeFailed),
                ..
            }
        );
        let input_panicked = matches!(
            &self,
            Self::Input {
                source: DesktopInputStartError::ThreadPanicked,
                ..
            } | Self::Playback {
                input_cleanup: Some(DesktopInputStopError::ThreadPanicked),
                ..
            }
        );
        let output_failed = matches!(
            &self,
            Self::Playback {
                source,
                ..
            } if matches!(source.as_ref(), DesktopPlaybackSessionStartError::OutputCleanup {
                cleanup: DesktopOutputStopError::RuntimeFailed, ..
            })
        );
        let output_panicked = matches!(
            &self,
            Self::Playback {
                source,
                ..
            } if matches!(source.as_ref(),
                DesktopPlaybackSessionStartError::OutputCleanup {
                    cleanup: DesktopOutputStopError::ThreadPanicked, ..
                } | DesktopPlaybackSessionStartError::Output(
                    DesktopOutputStartError::ThreadPanicked
                )
            )
        );
        DesktopPlaybackRecordingStartRecovery {
            error: self,
            input: if input_panicked {
                DesktopPlaybackRecordingNativeSettlement::Panicked
            } else if input_failed {
                DesktopPlaybackRecordingNativeSettlement::Failed
            } else {
                DesktopPlaybackRecordingNativeSettlement::Clean
            },
            output: if output_panicked {
                DesktopPlaybackRecordingNativeSettlement::Panicked
            } else if output_failed {
                DesktopPlaybackRecordingNativeSettlement::Failed
            } else {
                DesktopPlaybackRecordingNativeSettlement::Clean
            },
        }
    }

    /// Returns writer settlement when native startup began after the recording
    /// worker acquired its journal.
    #[must_use]
    pub fn writer_settlement(&self) -> Option<&DesktopRecordingWriterSettlement> {
        match self {
            Self::Input { writer, .. } | Self::Playback { writer, .. } => Some(writer),
            Self::OutputNotPinned { .. }
            | Self::PlaybackUnavailable { .. }
            | Self::MonitoringUnavailable { .. }
            | Self::InputMismatch { .. }
            | Self::Monitoring { .. }
            | Self::RecordingConfig { .. }
            | Self::RecordingTransport { .. }
            | Self::RecordingWriter { .. } => None,
        }
    }
}

/// Sole restart authority after a failed recording-capable playback start.
/// Native teardown and writer settlement must both finish before it releases
/// the stopped playback owner.
#[derive(Debug)]
pub struct DesktopPlaybackRecordingStartRecovery {
    error: DesktopPlaybackRecordingStartError,
    input: DesktopPlaybackRecordingNativeSettlement,
    output: DesktopPlaybackRecordingNativeSettlement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopPlaybackRecordingNativeSettlement {
    Clean,
    Failed,
    Panicked,
}

#[derive(Debug)]
pub struct DesktopPlaybackRecordingStartSettlement {
    pub recovery: DesktopPlaybackRecovery,
    pub writer: Option<DesktopRecordingWriterSettlement>,
    pub journal_path: Option<std::path::PathBuf>,
    pub input: DesktopPlaybackRecordingNativeSettlement,
    pub output: DesktopPlaybackRecordingNativeSettlement,
}

#[derive(Debug)]
pub enum DesktopPlaybackRecordingStartRecoveryWait {
    Settled(DesktopPlaybackRecordingStartSettlement),
    TimedOut(DesktopPlaybackRecordingStartRecovery),
}

fn observe_recording_native_shutdown(
    shutdown: &mut PendingNativeShutdown,
    timeout: std::time::Duration,
    settlement: &mut DesktopPlaybackRecordingNativeSettlement,
) -> bool {
    match shutdown.wait(timeout) {
        PendingNativeShutdownWait::Completed => true,
        PendingNativeShutdownWait::Failed => {
            if *settlement != DesktopPlaybackRecordingNativeSettlement::Panicked {
                *settlement = DesktopPlaybackRecordingNativeSettlement::Failed;
            }
            true
        }
        PendingNativeShutdownWait::Panicked => {
            *settlement = DesktopPlaybackRecordingNativeSettlement::Panicked;
            true
        }
        PendingNativeShutdownWait::TimedOut => false,
    }
}

impl DesktopPlaybackRecordingStartRecovery {
    #[must_use]
    pub fn journal_path(&self) -> Option<&std::path::Path> {
        match &self.error {
            DesktopPlaybackRecordingStartError::Input { journal_path, .. }
            | DesktopPlaybackRecordingStartError::Playback { journal_path, .. } => {
                Some(journal_path)
            }
            _ => None,
        }
    }

    /// Waits within one shared deadline for the writer and every native owner.
    /// Timeout returns this same exclusive restart authority for later polling.
    #[must_use]
    pub fn wait(
        mut self,
        timeout: std::time::Duration,
    ) -> DesktopPlaybackRecordingStartRecoveryWait {
        let started = std::time::Instant::now();
        if let Some(writer) = (match &mut self.error {
            DesktopPlaybackRecordingStartError::Input { writer, .. }
            | DesktopPlaybackRecordingStartError::Playback { writer, .. } => Some(writer),
            _ => None,
        }) && let DesktopRecordingWriterSettlement::Pending(pending) = writer.as_mut()
        {
            match pending.wait(timeout.saturating_sub(started.elapsed())) {
                Ok(completion) => {
                    **writer = DesktopRecordingWriterSettlement::Completed(Box::new(completion));
                }
                Err(hyper_recording_worker::RecordingWorkerWaitError::TimedOut) => {
                    return DesktopPlaybackRecordingStartRecoveryWait::TimedOut(self);
                }
                Err(error) => **writer = DesktopRecordingWriterSettlement::Failed(error),
            }
        }
        let pending_input = match &mut self.error {
            DesktopPlaybackRecordingStartError::Input {
                source: DesktopInputStartError::CancellationPending { shutdown },
                ..
            }
            | DesktopPlaybackRecordingStartError::Playback {
                input_cleanup: Some(DesktopInputStopError::ShutdownPending { shutdown }),
                ..
            } => Some(shutdown.as_mut()),
            _ => None,
        };
        if let Some(shutdown) = pending_input
            && !observe_recording_native_shutdown(
                shutdown,
                timeout.saturating_sub(started.elapsed()),
                &mut self.input,
            )
        {
            return DesktopPlaybackRecordingStartRecoveryWait::TimedOut(self);
        }
        let pending_output = match &mut self.error {
            DesktopPlaybackRecordingStartError::Playback { source, .. } => match source.as_mut() {
                DesktopPlaybackSessionStartError::Output(
                    DesktopOutputStartError::CancellationPending { shutdown },
                )
                | DesktopPlaybackSessionStartError::OutputCleanup {
                    cleanup: DesktopOutputStopError::ShutdownPending { shutdown },
                    ..
                } => Some(shutdown.as_mut()),
                _ => None,
            },
            _ => None,
        };
        if let Some(shutdown) = pending_output
            && !observe_recording_native_shutdown(
                shutdown,
                timeout.saturating_sub(started.elapsed()),
                &mut self.output,
            )
        {
            return DesktopPlaybackRecordingStartRecoveryWait::TimedOut(self);
        }
        let (recovery, writer, journal_path) = match self.error {
            DesktopPlaybackRecordingStartError::OutputNotPinned { recovery }
            | DesktopPlaybackRecordingStartError::PlaybackUnavailable { recovery }
            | DesktopPlaybackRecordingStartError::MonitoringUnavailable { recovery }
            | DesktopPlaybackRecordingStartError::InputMismatch { recovery }
            | DesktopPlaybackRecordingStartError::Monitoring { recovery, .. }
            | DesktopPlaybackRecordingStartError::RecordingConfig { recovery, .. }
            | DesktopPlaybackRecordingStartError::RecordingTransport { recovery, .. }
            | DesktopPlaybackRecordingStartError::RecordingWriter { recovery, .. } => {
                (*recovery, None, None)
            }
            DesktopPlaybackRecordingStartError::Input {
                recovery,
                writer,
                journal_path,
                ..
            }
            | DesktopPlaybackRecordingStartError::Playback {
                recovery,
                writer,
                journal_path,
                ..
            } => (*recovery, Some(*writer), Some(journal_path)),
        };
        DesktopPlaybackRecordingStartRecoveryWait::Settled(
            DesktopPlaybackRecordingStartSettlement {
                recovery,
                writer,
                journal_path,
                input: self.input,
                output: self.output,
            },
        )
    }
}

impl fmt::Display for DesktopPlaybackRecordingStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputNotPinned { .. } => {
                formatter.write_str("recording playback requires a selected output device")
            }
            Self::PlaybackUnavailable { .. } => {
                formatter.write_str("playback restart authority was already consumed")
            }
            Self::MonitoringUnavailable { .. } => {
                formatter.write_str("recording playback requires the existing monitored input")
            }
            Self::InputMismatch { .. } => {
                formatter.write_str("recording input does not match the monitored playback input")
            }
            Self::Monitoring { source, .. } => source.fmt(formatter),
            Self::RecordingConfig { source, .. } => {
                write!(formatter, "invalid recording session: {source}")
            }
            Self::RecordingTransport { source, .. } => {
                write!(formatter, "recording transport startup failed: {source}")
            }
            Self::RecordingWriter { source, .. } => {
                write!(formatter, "recording writer startup failed: {source}")
            }
            Self::Input {
                journal_path,
                source,
                ..
            } => write!(
                formatter,
                "recording input startup failed; recording was aborted and the last configured journal path was {}: {source}",
                journal_path.display()
            ),
            Self::Playback {
                journal_path,
                source,
                input_cleanup,
                ..
            } => {
                write!(
                    formatter,
                    "recording playback startup failed; recording was aborted and the last configured journal path was {}: {source}",
                    journal_path.display()
                )?;
                if let Some(cleanup) = input_cleanup {
                    write!(formatter, "; input cleanup also failed: {cleanup}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for DesktopPlaybackRecordingStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Monitoring { source, .. } => Some(source),
            Self::RecordingConfig { source, .. } => Some(source),
            Self::RecordingTransport { source, .. } => Some(source),
            Self::RecordingWriter { source, .. } => Some(source),
            Self::Input { source, .. } => Some(source),
            Self::Playback { source, .. } => Some(source.as_ref()),
            Self::OutputNotPinned { .. }
            | Self::PlaybackUnavailable { .. }
            | Self::MonitoringUnavailable { .. }
            | Self::InputMismatch { .. } => None,
        }
    }
}

/// A stop failure that keeps both the recording settlement evidence and the
/// playback restart authority together.
#[derive(Debug)]
pub enum DesktopPlaybackRecordingStopError {
    Input {
        playback: Box<DesktopPlaybackRecoveryStopError>,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
    Recording {
        source: Box<DesktopRecordingSessionStopError>,
        recovery: Box<DesktopPlaybackRecordingRecovery>,
    },
}

impl DesktopPlaybackRecordingStopError {
    #[must_use]
    pub fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        match self {
            Self::Input { playback, .. } => playback.recovery(),
            Self::Recording { recovery, .. } => recovery.recovery(),
        }
    }

    /// Extracts the sole playback restart authority after the caller has
    /// retained or reported the recording failure evidence.
    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecordingRecovery {
        match self {
            Self::Input { playback, .. } => {
                DesktopPlaybackRecordingRecovery::NativeShutdown(playback)
            }
            Self::Recording { recovery, .. } => *recovery,
        }
    }

    /// Returns writer settlement when input shutdown prevented an orderly
    /// recording disposition.
    #[must_use]
    pub fn writer_settlement(&self) -> Option<&DesktopRecordingWriterSettlement> {
        match self {
            Self::Input { writer, .. } => Some(writer),
            Self::Recording { source, .. } => match source.as_ref() {
                DesktopRecordingSessionStopError::Input { writer, .. }
                | DesktopRecordingSessionStopError::CaptureIntegrity { writer, .. }
                | DesktopRecordingSessionStopError::Disposition { writer, .. } => Some(writer),
                DesktopRecordingSessionStopError::Writer { .. } => None,
            },
        }
    }
}

impl fmt::Display for DesktopPlaybackRecordingStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input { playback, .. } => write!(
                formatter,
                "recording input shutdown failed; recording was not published: {playback}"
            ),
            Self::Recording { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopPlaybackRecordingStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input { playback, .. } => Some(playback),
            Self::Recording { source, .. } => Some(source.as_ref()),
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackRecordingCommitError {
    Stop(Box<DesktopPlaybackRecordingStopError>),
    Completion {
        source: Box<DesktopRecordingSessionCommitError>,
        recovery: Box<DesktopPlaybackRecordingRecovery>,
    },
}

impl fmt::Display for DesktopPlaybackRecordingCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stop(source) => source.fmt(formatter),
            Self::Completion { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopPlaybackRecordingCommitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stop(source) => Some(source.as_ref()),
            Self::Completion { source, .. } => Some(source.as_ref()),
        }
    }
}

impl DesktopPlaybackRecordingCommitError {
    /// Returns playback recovery when this error completed native shutdown.
    #[must_use]
    pub fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        match self {
            Self::Stop(source) => source.recovery(),
            Self::Completion { recovery, .. } => recovery.recovery(),
        }
    }

    /// Extracts the sole playback restart authority after the caller has
    /// retained or reported the commit failure evidence.
    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecordingRecovery {
        match self {
            Self::Stop(source) => source.into_recovery(),
            Self::Completion { recovery, .. } => *recovery,
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackRecordingFinalizeError {
    Commit(Box<DesktopPlaybackRecordingCommitError>),
    Metadata {
        source: Box<RecordingMetadataBuildError>,
        recovery: Box<DesktopPlaybackRecordingRecovery>,
    },
}

impl fmt::Display for DesktopPlaybackRecordingFinalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(source) => source.fmt(formatter),
            Self::Metadata { source, .. } => {
                write!(formatter, "recording metadata validation failed: {source}")
            }
        }
    }
}

impl Error for DesktopPlaybackRecordingFinalizeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Commit(source) => Some(source.as_ref()),
            Self::Metadata { source, .. } => Some(source.as_ref()),
        }
    }
}

impl DesktopPlaybackRecordingFinalizeError {
    /// Returns playback recovery when this error completed native shutdown.
    #[must_use]
    pub fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        match self {
            Self::Commit(source) => source.recovery(),
            Self::Metadata { recovery, .. } => recovery.recovery(),
        }
    }

    /// Extracts the sole playback restart authority after the caller has
    /// retained or reported the finalization failure evidence.
    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecordingRecovery {
        match self {
            Self::Commit(source) => source.into_recovery(),
            Self::Metadata { recovery, .. } => *recovery,
        }
    }
}

/// One native playback output and one shared input capture that records before
/// it publishes best-effort monitoring samples.
#[derive(Debug)]
pub struct DesktopPlaybackRecordingSession {
    playback: DesktopPlaybackSession,
    recording: RecordingSessionCore,
}

/// Sole restart authority while one or both native streams remain pending.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStop {
    input: Option<PendingNativeShutdown>,
    output: Option<PendingNativeShutdown>,
    input_failure: Option<NativeStopFailure>,
    output_failure: Option<NativeStopFailure>,
    recovery: DesktopPlaybackRecovery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeStopFailure {
    RuntimeFailed,
    ThreadPanicked,
}

/// Terminal native teardown evidence paired with the sole playback recovery.
///
/// Input and output failure remain distinct so callers can apply stream-specific
/// recovery policy without inspecting backend error variants.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStopFailure {
    input: Option<NativeStopFailure>,
    output: Option<NativeStopFailure>,
    recovery: DesktopPlaybackRecovery,
}

impl DesktopPlaybackRecoveryPendingStopFailure {
    /// Returns whether the pending monitoring-input shutdown ended terminally.
    #[must_use]
    pub const fn input_shutdown_failed(&self) -> bool {
        self.input.is_some()
    }

    /// Returns whether the pending output shutdown ended terminally.
    #[must_use]
    pub const fn output_shutdown_failed(&self) -> bool {
        self.output.is_some()
    }

    /// Returns whether either native owner panicked during teardown.
    #[must_use]
    pub const fn panicked(&self) -> bool {
        matches!(self.input, Some(NativeStopFailure::ThreadPanicked))
            || matches!(self.output, Some(NativeStopFailure::ThreadPanicked))
    }

    /// Consumes the terminal evidence and returns the sole restart authority.
    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecovery {
        self.recovery
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryPendingStopWait {
    Completed(DesktopPlaybackRecovery),
    Failed(DesktopPlaybackRecoveryPendingStopFailure),
    Panicked(DesktopPlaybackRecoveryPendingStopFailure),
    TimedOut(DesktopPlaybackRecoveryPendingStop),
}

impl DesktopPlaybackRecoveryPendingStop {
    #[must_use]
    pub fn input_status(&self) -> Option<super::StreamStatus> {
        self.input.as_ref().map(PendingNativeShutdown::status)
    }

    #[must_use]
    pub fn output_status(&self) -> Option<super::StreamStatus> {
        self.output.as_ref().map(PendingNativeShutdown::status)
    }

    /// Waits for teardown while retaining exactly one restart authority.
    #[must_use]
    pub fn wait(mut self, timeout: std::time::Duration) -> DesktopPlaybackRecoveryPendingStopWait {
        let started = std::time::Instant::now();
        for input in [true, false] {
            let remaining = timeout.saturating_sub(started.elapsed());
            let result = if input {
                wait_pending_shutdown(&mut self.input, remaining)
            } else {
                wait_pending_shutdown(&mut self.output, remaining)
            };
            match result {
                None | Some(PendingNativeShutdownWait::Completed) => {}
                Some(PendingNativeShutdownWait::Failed) => {
                    if input {
                        self.input_failure = Some(NativeStopFailure::RuntimeFailed);
                    } else {
                        self.output_failure = Some(NativeStopFailure::RuntimeFailed);
                    }
                }
                Some(PendingNativeShutdownWait::Panicked) => {
                    if input {
                        self.input_failure = Some(NativeStopFailure::ThreadPanicked);
                    } else {
                        self.output_failure = Some(NativeStopFailure::ThreadPanicked);
                    }
                }
                Some(PendingNativeShutdownWait::TimedOut) => {
                    return DesktopPlaybackRecoveryPendingStopWait::TimedOut(self);
                }
            }
        }
        let failure = DesktopPlaybackRecoveryPendingStopFailure {
            input: self.input_failure,
            output: self.output_failure,
            recovery: self.recovery,
        };
        if failure.panicked() {
            DesktopPlaybackRecoveryPendingStopWait::Panicked(failure)
        } else if failure.input_shutdown_failed() || failure.output_shutdown_failed() {
            DesktopPlaybackRecoveryPendingStopWait::Failed(failure)
        } else {
            DesktopPlaybackRecoveryPendingStopWait::Completed(failure.into_recovery())
        }
    }
}

fn wait_pending_shutdown(
    shutdown: &mut Option<PendingNativeShutdown>,
    timeout: std::time::Duration,
) -> Option<PendingNativeShutdownWait> {
    let result = shutdown.as_mut()?.wait(timeout);
    if result != PendingNativeShutdownWait::TimedOut {
        *shutdown = None;
    }
    Some(result)
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryStartError {
    OutputNotPinned,
    AlreadyRestarted,
    CancellationPending {
        pending: Box<DesktopPlaybackRecoveryPendingStart>,
    },
    Start(DesktopPlaybackSessionStartError),
    MonitoringConfig(super::DesktopMonitoringConfigError),
}

impl fmt::Display for DesktopPlaybackRecoveryStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputNotPinned => {
                formatter.write_str("desktop output recovery requires an explicitly pinned target")
            }
            Self::AlreadyRestarted => {
                formatter.write_str("desktop playback recovery was already restarted")
            }
            Self::CancellationPending { .. } => {
                formatter.write_str("desktop playback restart cancellation is still pending")
            }
            Self::Start(source) => source.fmt(formatter),
            Self::MonitoringConfig(source) => source.fmt(formatter),
        }
    }
}

impl Error for DesktopPlaybackRecoveryStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OutputNotPinned | Self::AlreadyRestarted | Self::CancellationPending { .. } => {
                None
            }
            Self::Start(source) => Some(source),
            Self::MonitoringConfig(source) => Some(source),
        }
    }
}

impl DesktopPlaybackRecoveryStartError {
    /// Returns the sole pending restart authority when native cancellation has
    /// not yet completed.
    #[must_use]
    pub fn into_pending(self) -> Option<DesktopPlaybackRecoveryPendingStart> {
        match self {
            Self::CancellationPending { pending } => Some(*pending),
            Self::OutputNotPinned
            | Self::AlreadyRestarted
            | Self::Start(_)
            | Self::MonitoringConfig(_) => None,
        }
    }
}

/// Sole restart authority while cancelled monitored playback startup still
/// owns one or both native threads.
///
/// This value is intentionally not cloneable. A retry becomes available only
/// after [`Self::wait`] observes terminal native-thread teardown.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStart {
    input: Option<PendingNativeShutdown>,
    output: Option<PendingNativeShutdown>,
    failed: bool,
    panicked: bool,
    recovery: DesktopPlaybackRecovery,
}

/// Result of waiting for a cancellation-pending playback restart.
#[derive(Debug)]
pub enum DesktopPlaybackRecoveryPendingWait {
    Completed(DesktopPlaybackRecovery),
    Failed(DesktopPlaybackRecovery),
    Panicked(DesktopPlaybackRecovery),
    TimedOut(DesktopPlaybackRecoveryPendingStart),
}

impl DesktopPlaybackRecoveryPendingStart {
    /// Returns the latest output state, or the input state when only input
    /// teardown remains pending.
    ///
    /// Use [`Self::input_status`] and [`Self::output_status`] when both native
    /// owners must be supervised independently.
    #[must_use]
    pub fn status(&self) -> Option<super::StreamStatus> {
        self.output
            .as_ref()
            .or(self.input.as_ref())
            .map(PendingNativeShutdown::status)
    }

    /// Returns the latest state reported by the pending monitor-input thread.
    #[must_use]
    pub fn input_status(&self) -> Option<super::StreamStatus> {
        self.input.as_ref().map(PendingNativeShutdown::status)
    }

    /// Returns the latest state reported by the pending playback-output thread.
    #[must_use]
    pub fn output_status(&self) -> Option<super::StreamStatus> {
        self.output.as_ref().map(PendingNativeShutdown::status)
    }

    /// Waits for native teardown while retaining exactly one retry authority.
    ///
    /// A timeout returns this pending value. Every terminal result returns the
    /// recovery value that may create the next output.
    #[must_use]
    pub fn wait(mut self, timeout: std::time::Duration) -> DesktopPlaybackRecoveryPendingWait {
        let started = std::time::Instant::now();
        let mut failed = self.failed;
        let mut panicked = self.panicked;
        for input in [true, false] {
            let remaining = timeout.saturating_sub(started.elapsed());
            let result = if input {
                wait_pending_shutdown(&mut self.input, remaining)
            } else {
                wait_pending_shutdown(&mut self.output, remaining)
            };
            match result {
                None | Some(PendingNativeShutdownWait::Completed) => {}
                Some(PendingNativeShutdownWait::Failed) => failed = true,
                Some(PendingNativeShutdownWait::Panicked) => panicked = true,
                Some(PendingNativeShutdownWait::TimedOut) => {
                    self.failed = failed;
                    self.panicked = panicked;
                    return DesktopPlaybackRecoveryPendingWait::TimedOut(self);
                }
            }
        }
        if panicked {
            DesktopPlaybackRecoveryPendingWait::Panicked(self.recovery)
        } else if failed {
            DesktopPlaybackRecoveryPendingWait::Failed(self.recovery)
        } else {
            DesktopPlaybackRecoveryPendingWait::Completed(self.recovery)
        }
    }
}

impl DesktopPlaybackRecovery {
    #[must_use]
    pub const fn checkpoint(&self) -> PlaybackRecoverySnapshot {
        *self.checkpoint
    }

    /// Returns the verified project retained for a future restart, when this
    /// recovery originated from [`DesktopPlaybackSession::open_project`] or
    /// [`DesktopPlaybackSession::start_project`].
    #[must_use]
    pub fn prepared_project(&self) -> Option<&PreparedProjectPlayback> {
        self.source
            .as_deref()
            .and_then(DesktopPlaybackSource::project)
    }

    #[must_use]
    pub fn monitoring_config(&self) -> Option<&DesktopMonitoringConfig> {
        self.monitoring
            .as_ref()
            .map(|monitoring| &monitoring.config)
    }

    /// Rebuilds playback against the same explicitly selected output.
    ///
    /// A joined failed attempt retains restart authority for another try. A
    /// successful attempt consumes it. Cancellation pending returns the sole
    /// authority together with its native-thread monitor.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopPlaybackRecoveryStartError::OutputNotPinned`] when the
    /// original stream used the system policy default,
    /// [`DesktopPlaybackRecoveryStartError::AlreadyRestarted`] after a prior
    /// success, or wraps any replacement session startup failure.
    pub fn restart(&mut self) -> Result<DesktopPlaybackSession, DesktopPlaybackRecoveryStartError> {
        self.restart_with_output(self.config.clone())
    }

    /// Rebuilds playback against an explicitly selected replacement output.
    ///
    /// This is the only recovery path for a session originally opened against
    /// the system policy default.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::restart`].
    pub fn restart_with_output(
        &mut self,
        config: DesktopOutputConfig,
    ) -> Result<DesktopPlaybackSession, DesktopPlaybackRecoveryStartError> {
        if !output_is_pinned(&config) {
            return Err(DesktopPlaybackRecoveryStartError::OutputNotPinned);
        }
        if self.source.is_none() {
            return Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted);
        }
        let monitoring = self
            .monitoring
            .clone()
            .map(|monitoring| monitoring.with_output(config.clone()))
            .transpose()
            .map_err(DesktopPlaybackRecoveryStartError::MonitoringConfig)?;
        let source = self
            .source
            .take()
            .ok_or(DesktopPlaybackRecoveryStartError::AlreadyRestarted)?;
        let session = DesktopPlaybackSession::start_source(
            config.clone(),
            source.clone(),
            Some(Box::new(*self.checkpoint)),
            monitoring.clone(),
        );
        match session {
            Ok(session) => Ok(session),
            Err(error) => Err(self.failed_restart(config, monitoring, source, error)),
        }
    }

    fn failed_restart(
        &mut self,
        config: DesktopOutputConfig,
        monitoring: Option<PlaybackMonitoringStartup>,
        source: Arc<dyn DesktopPlaybackSource>,
        error: DesktopPlaybackSessionStartError,
    ) -> DesktopPlaybackRecoveryStartError {
        match error {
            DesktopPlaybackSessionStartError::OutputCleanup {
                source: _,
                cleanup: DesktopOutputStopError::ShutdownPending { shutdown },
            }
            | DesktopPlaybackSessionStartError::Output(
                DesktopOutputStartError::CancellationPending { shutdown },
            ) => DesktopPlaybackRecoveryStartError::CancellationPending {
                pending: Box::new(DesktopPlaybackRecoveryPendingStart {
                    input: None,
                    output: Some(*shutdown),
                    failed: false,
                    panicked: false,
                    recovery: DesktopPlaybackRecovery {
                        config,
                        monitoring,
                        source: Some(source),
                        checkpoint: self.checkpoint.clone(),
                    },
                }),
            },
            DesktopPlaybackSessionStartError::Monitored {
                source: nested,
                input_cleanup,
            } => {
                let (input, terminal_input_cleanup) = match input_cleanup {
                    Some(DesktopInputStopError::ShutdownPending { shutdown }) => {
                        (Some(*shutdown), None)
                    }
                    Some(
                        source @ (DesktopInputStopError::RuntimeFailed
                        | DesktopInputStopError::ThreadPanicked),
                    ) => (None, Some(source)),
                    None => (None, None),
                };
                let output = match *nested {
                    DesktopPlaybackSessionStartError::Output(
                        DesktopOutputStartError::CancellationPending { shutdown },
                    )
                    | DesktopPlaybackSessionStartError::OutputCleanup {
                        cleanup: DesktopOutputStopError::ShutdownPending { shutdown },
                        ..
                    } => Some(*shutdown),
                    nested => {
                        if input.is_none() {
                            self.source = Some(source);
                            return DesktopPlaybackRecoveryStartError::Start(
                                DesktopPlaybackSessionStartError::Monitored {
                                    source: Box::new(nested),
                                    input_cleanup: terminal_input_cleanup,
                                },
                            );
                        }
                        None
                    }
                };
                DesktopPlaybackRecoveryStartError::CancellationPending {
                    pending: Box::new(DesktopPlaybackRecoveryPendingStart {
                        input,
                        output,
                        failed: matches!(
                            terminal_input_cleanup,
                            Some(DesktopInputStopError::RuntimeFailed)
                        ),
                        panicked: matches!(
                            terminal_input_cleanup,
                            Some(DesktopInputStopError::ThreadPanicked)
                        ),
                        recovery: DesktopPlaybackRecovery {
                            config,
                            monitoring,
                            source: Some(source),
                            checkpoint: self.checkpoint.clone(),
                        },
                    }),
                }
            }
            error => {
                self.source = Some(source);
                DesktopPlaybackRecoveryStartError::Start(error)
            }
        }
    }

    /// Replaces stopped monitored playback with one owner that keeps the same
    /// output and selected input while writing a recoverable recording.
    ///
    /// The transition consumes recovery authority. Any failed attempt returns
    /// that authority together with writer settlement or pending native-start
    /// evidence, so callers cannot accidentally create a second output owner.
    ///
    /// # Errors
    ///
    /// Returns the complete restart authority for every rejected or failed
    /// transition. Failures after writer startup also retain writer settlement.
    #[allow(
        clippy::too_many_lines,
        reason = "the startup transaction keeps recovery, native input, writer, and output ownership ordered together"
    )]
    pub fn start_recording(
        self,
        recording: &DesktopRecordingSessionConfig,
    ) -> Result<DesktopPlaybackRecordingSession, DesktopPlaybackRecordingStartError> {
        if !output_is_pinned(&self.config) {
            return Err(DesktopPlaybackRecordingStartError::OutputNotPinned {
                recovery: Box::new(self),
            });
        }
        let Self {
            config,
            monitoring,
            source,
            checkpoint,
        } = self;
        let Some(monitoring) = monitoring else {
            return Err(DesktopPlaybackRecordingStartError::MonitoringUnavailable {
                recovery: Box::new(DesktopPlaybackRecovery {
                    config,
                    monitoring: None,
                    source,
                    checkpoint,
                }),
            });
        };
        let Some(source) = source else {
            return Err(DesktopPlaybackRecordingStartError::PlaybackUnavailable {
                recovery: Box::new(DesktopPlaybackRecovery {
                    config,
                    monitoring: Some(monitoring),
                    source: None,
                    checkpoint,
                }),
            });
        };
        if monitoring.config.input() != &recording.input {
            return Err(DesktopPlaybackRecordingStartError::InputMismatch {
                recovery: Box::new(DesktopPlaybackRecovery {
                    config,
                    monitoring: Some(monitoring),
                    source: Some(source),
                    checkpoint,
                }),
            });
        }
        let recovery = |source: Arc<dyn DesktopPlaybackSource>| DesktopPlaybackRecovery {
            config: config.clone(),
            monitoring: Some(monitoring.clone()),
            source: Some(source),
            checkpoint: checkpoint.clone(),
        };
        let start_config = config.clone();
        let start_checkpoint = checkpoint.clone();
        let (monitoring_producer, monitoring_consumer, monitoring_counters, control, meter) =
            match prepare_playback_monitoring_transport(&monitoring) {
                Ok(transport) => transport,
                Err(source_error) => {
                    return Err(DesktopPlaybackRecordingStartError::Monitoring {
                        source: source_error,
                        recovery: Box::new(recovery(source)),
                    });
                }
            };
        let (recording_producer, recording_core) = match start_recording_core(recording.clone()) {
            Ok(core) => core,
            Err(RecordingCoreStartError::Config(source_error)) => {
                return Err(DesktopPlaybackRecordingStartError::RecordingConfig {
                    source: source_error,
                    recovery: Box::new(recovery(source)),
                });
            }
            Err(RecordingCoreStartError::Transport(source_error)) => {
                return Err(DesktopPlaybackRecordingStartError::RecordingTransport {
                    source: source_error,
                    recovery: Box::new(recovery(source)),
                });
            }
            Err(RecordingCoreStartError::Writer(source_error)) => {
                return Err(DesktopPlaybackRecordingStartError::RecordingWriter {
                    source: source_error,
                    recovery: Box::new(recovery(source)),
                });
            }
        };
        let journal_path = recording_core.journal_path().to_owned();
        let mut producer = MonitoredRecordingProducer::new(recording_producer, monitoring_producer);
        let input = match NativeInputStream::start(
            NativeInputConfig::from(&recording.input),
            move |samples: &[f32]| producer.push(samples),
        ) {
            Ok(input) => input,
            Err(source_error) => {
                return Err(DesktopPlaybackRecordingStartError::Input {
                    journal_path,
                    source: source_error.into(),
                    writer: Box::new(recording_core.abort_and_settle()),
                    recovery: Box::new(recovery(source)),
                });
            }
        };
        let playback = match DesktopPlaybackSession::start_output_source(
            start_config,
            source.clone(),
            Some(start_checkpoint),
            Some(monitoring_consumer),
        ) {
            Ok(mut playback) => {
                playback.monitoring = Some(PlaybackMonitoringSession {
                    input,
                    config: monitoring.config,
                    counters: monitoring_counters,
                    control,
                    meter: Some(meter),
                });
                playback
            }
            Err(source_error) => {
                let input_cleanup = input.stop().err().map(DesktopInputStopError::from);
                return Err(DesktopPlaybackRecordingStartError::Playback {
                    journal_path,
                    source: Box::new(source_error),
                    input_cleanup,
                    writer: Box::new(recording_core.abort_and_settle()),
                    recovery: Box::new(recovery(source)),
                });
            }
        };
        Ok(DesktopPlaybackRecordingSession {
            playback,
            recording: recording_core,
        })
    }
}

impl DesktopPlaybackSession {
    /// Verifies, snapshots, decodes, and starts one project package.
    ///
    /// Project loading is a blocking application-thread operation and must not
    /// be called from an audio callback. The native output is not opened until
    /// interrupted editable-project transactions have been recovered and the
    /// complete project has passed structural and semantic verification.
    /// The verified package, decoded media, and recording metadata remain
    /// owned by the live session and any recovery value derived from it.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopProjectOpenError::Recovery`] when interrupted project
    /// transactions cannot be resolved without discarding evidence. Returns
    /// [`DesktopProjectOpenError::Load`] before native startup when the
    /// project is invalid, changes during capture, exceeds a limit, or cannot
    /// be decoded. Returns [`DesktopProjectOpenError::Start`] when the verified
    /// project cannot be bound to the selected output.
    pub fn open_project(
        config: DesktopOutputConfig,
        root: impl AsRef<Path>,
        limits: ProjectPlaybackLimits,
    ) -> Result<Self, DesktopProjectOpenError> {
        let (project, recovery) = recover_and_prepare_project(root.as_ref(), limits)?;
        match Self::start_project(config, project) {
            Ok(mut session) => {
                session.project_open_recovery = Some(recovery);
                Ok(session)
            }
            Err(source) => Err(DesktopProjectOpenError::Start {
                source: Box::new(source),
                recovery,
            }),
        }
    }

    /// Verifies and starts one project with bounded live input monitoring.
    ///
    /// # Errors
    ///
    /// Returns project loading or monitored playback startup errors without
    /// publishing a partial session.
    pub fn open_project_monitored(
        monitoring: DesktopMonitoringConfig,
        root: impl AsRef<Path>,
        limits: ProjectPlaybackLimits,
    ) -> Result<Self, DesktopProjectOpenError> {
        Self::open_project_monitored_with_monitor_state(monitoring, root, limits, 1.0, false)
    }

    /// Verifies and starts one project with bounded live input monitoring and
    /// an initial monitor state applied before native stream publication.
    ///
    /// # Errors
    ///
    /// Returns project loading or monitored playback startup errors without
    /// publishing a partial session. An invalid initial monitor gain is
    /// rejected before native input or output construction.
    pub fn open_project_monitored_with_monitor_state(
        monitoring: DesktopMonitoringConfig,
        root: impl AsRef<Path>,
        limits: ProjectPlaybackLimits,
        monitor_gain: f32,
        monitor_muted: bool,
    ) -> Result<Self, DesktopProjectOpenError> {
        let (project, recovery) = recover_and_prepare_project(root.as_ref(), limits)?;
        match Self::start_project_monitored_with_monitor_state(
            monitoring,
            project,
            monitor_gain,
            monitor_muted,
        ) {
            Ok(mut session) => {
                session.project_open_recovery = Some(recovery);
                Ok(session)
            }
            Err(source) => Err(DesktopProjectOpenError::Start {
                source: Box::new(source),
                recovery,
            }),
        }
    }

    /// Starts an already verified project while retaining its package and
    /// recording metadata for the complete native-output and recovery lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopPlaybackSessionStartError`] if session construction or
    /// output startup fails. No partial session is published.
    pub fn start_project(
        config: DesktopOutputConfig,
        project: PreparedProjectPlayback,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_source(config, Arc::new(project), None, None)
    }

    /// Starts an already verified project with one bounded input-monitor path
    /// mixed before final output metering.
    ///
    /// # Errors
    ///
    /// Returns a playback, monitoring-capacity, input, or output startup error.
    /// Any failure after input startup attempts bounded input cleanup.
    pub fn start_project_monitored(
        monitoring: DesktopMonitoringConfig,
        project: PreparedProjectPlayback,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_project_monitored_with_monitor_state(monitoring, project, 1.0, false)
    }

    /// Starts an already verified project with one bounded input-monitor path
    /// and applies monitor gain/mute before either native stream is published.
    ///
    /// # Errors
    ///
    /// Returns a playback, monitoring-control, monitoring-capacity, input, or
    /// output startup error. An invalid initial gain is rejected before native
    /// input or output construction.
    pub fn start_project_monitored_with_monitor_state(
        monitoring: DesktopMonitoringConfig,
        project: PreparedProjectPlayback,
        monitor_gain: f32,
        monitor_muted: bool,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_source(
            monitoring.output().clone(),
            Arc::new(project),
            None,
            Some(PlaybackMonitoringStartup::new(
                monitoring,
                monitor_gain,
                monitor_muted,
            )),
        )
    }

    /// Compiles and binds one prepared session entirely on the output thread.
    ///
    /// Controls are returned only after the native backend reports a usable
    /// stream state.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopPlaybackSessionStartError`] if session construction or
    /// output startup fails. No partial session is published.
    pub fn start(
        config: DesktopOutputConfig,
        input: DesktopPlaybackSessionInput,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_source(config, Arc::new(input), None, None)
    }

    /// Starts prepared playback with one bounded input-monitor path mixed
    /// before final output metering.
    ///
    /// # Errors
    ///
    /// Returns a playback, monitoring-capacity, input, or output startup error.
    pub fn start_monitored(
        monitoring: DesktopMonitoringConfig,
        input: DesktopPlaybackSessionInput,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_monitored_with_monitor_state(monitoring, input, 1.0, false)
    }

    /// Starts prepared playback with one bounded input-monitor path and an
    /// initial monitor state applied before native stream publication.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::start_project_monitored_with_monitor_state`].
    pub fn start_monitored_with_monitor_state(
        monitoring: DesktopMonitoringConfig,
        input: DesktopPlaybackSessionInput,
        monitor_gain: f32,
        monitor_muted: bool,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        Self::start_source(
            monitoring.output().clone(),
            Arc::new(input),
            None,
            Some(PlaybackMonitoringStartup::new(
                monitoring,
                monitor_gain,
                monitor_muted,
            )),
        )
    }

    fn start_source(
        config: DesktopOutputConfig,
        source: Arc<dyn DesktopPlaybackSource>,
        initial_checkpoint: Option<Box<PlaybackRecoverySnapshot>>,
        monitoring: Option<PlaybackMonitoringStartup>,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        let Some(monitoring) = monitoring else {
            return Self::start_output_source(config, source, initial_checkpoint, None);
        };
        let (mut producer, consumer, counters, control, meter) =
            prepare_playback_monitoring_transport(&monitoring)?;
        let input = NativeInputStream::start(
            NativeInputConfig::from(monitoring.config.input()),
            move |samples: &[f32]| {
                producer.push(samples);
                Ok::<(), core::convert::Infallible>(())
            },
        )
        .map_err(|source| DesktopPlaybackSessionStartError::Input(source.into()))?;
        match Self::start_output_source(config, source, initial_checkpoint, Some(consumer)) {
            Ok(mut session) => {
                session.monitoring = Some(PlaybackMonitoringSession {
                    input,
                    config: monitoring.config,
                    counters,
                    control,
                    meter: Some(meter),
                });
                Ok(session)
            }
            Err(source) => {
                let input_cleanup = input.stop().err().map(DesktopInputStopError::from);
                Err(DesktopPlaybackSessionStartError::Monitored {
                    source: Box::new(source),
                    input_cleanup,
                })
            }
        }
    }

    fn start_output_source(
        config: DesktopOutputConfig,
        source: Arc<dyn DesktopPlaybackSource>,
        initial_checkpoint: Option<Box<PlaybackRecoverySnapshot>>,
        monitoring: Option<MonitoringConsumer>,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        let sample_rate_hz = config.sample_rate_hz();
        let maximum_block_frames = config.maximum_block_frames();
        let control_mapping = source
            .project()
            .and_then(PreparedProjectPlayback::compiled_control_mapping)
            .map(CompiledControlMappingTable::try_to_owned)
            .transpose()
            .map_err(|source| {
                DesktopPlaybackSessionStartError::Build(
                    DesktopPlaybackSessionBuildError::ControlMappingCopy(source),
                )
            })?;
        let (publication_sender, publication_receiver) = sync_channel(1);
        let thread_source = source.clone();
        let output =
            DesktopOutputStream::start_with_thread_initializer(config.native(), move |runner| {
                initialize_session(
                    runner,
                    SessionInitialization {
                        input: thread_source.session(),
                        recordings: thread_source
                            .project()
                            .map_or(&[], PreparedProjectPlayback::recording_metadata),
                        device_chains: thread_source
                            .project()
                            .map_or(&[], PreparedProjectPlayback::device_chains),
                        sample_rate_hz,
                        maximum_block_frames,
                        initial_checkpoint: initial_checkpoint.as_deref(),
                        control_mapping,
                        monitoring,
                    },
                    &publication_sender,
                )
            });

        match output {
            Ok(output) => match publication_receiver.recv() {
                Ok(SessionPublication::Prepared(handles)) => Ok(Self {
                    output,
                    controls: handles.controls,
                    loudness_commands: handles.loudness_commands,
                    meters: Some(handles.meters),
                    mixer_parameter_observations: Some(handles.mixer_parameter_observations),
                    scene_launches: Some(handles.scene_launches),
                    recovery: handles.recovery,
                    preview: handles.preview,
                    config,
                    source,
                    monitoring: None,
                    project_open_recovery: None,
                }),
                Ok(SessionPublication::Failed(source)) => Err(failed_output_publication(
                    output,
                    DesktopPlaybackSessionStartError::Build(source),
                )),
                Err(_) => Err(failed_output_publication(
                    output,
                    DesktopPlaybackSessionStartError::PublicationUnavailable,
                )),
            },
            Err(source) => Err(classify_output_start_failure(
                source.into(),
                &publication_receiver,
            )),
        }
    }

    /// Sends one ordinary IDE transport command through the session's only
    /// control owner.
    ///
    /// # Errors
    ///
    /// Returns the original bounded-queue rejection.
    pub fn try_send_transport_command(
        &mut self,
        command: TransportCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.controls.try_send_transport(command)
    }

    /// Queues one capture-specific transport start through the session's only
    /// control owner.
    ///
    /// The returned sequence is acknowledged only through a successful output
    /// cycle's recovery publication.
    ///
    /// # Errors
    ///
    /// Returns queue, disconnection, or sequence-space exhaustion evidence.
    pub fn try_start_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError> {
        self.controls.try_start_capture_transport()
    }

    /// Starts dry audition of one recording retained by the verified project.
    ///
    /// # Errors
    ///
    /// Rejects non-project sessions, unknown recording identities, an active
    /// audition, or exhausted command sequencing.
    pub fn try_start_audio_preview(
        &self,
        asset_id: &str,
    ) -> Result<(), super::DesktopAudioPreviewCommandError> {
        let project = self
            .prepared_project()
            .ok_or(super::DesktopAudioPreviewCommandError::ProjectUnavailable)?;
        let (recording_index, recording) = project
            .recording_metadata()
            .iter()
            .enumerate()
            .find(|(_, recording)| recording.asset_id() == asset_id)
            .ok_or(super::DesktopAudioPreviewCommandError::RecordingUnavailable)?;
        let source_frame_count = usize::try_from(recording.frame_count())
            .map_err(|_| super::DesktopAudioPreviewCommandError::SourceRangeUnavailable)?;
        self.preview
            .request_start(recording_index, 0, source_frame_count)
    }

    /// Validates dry audition of one reusable audio clip's current source view.
    ///
    /// # Errors
    ///
    /// Rejects a non-project session, unknown clip, or active audition without
    /// changing callback state.
    pub fn validate_audio_clip_preview(
        &self,
        clip_id: &str,
    ) -> Result<(), super::DesktopAudioPreviewCommandError> {
        let project = self
            .prepared_project()
            .ok_or(super::DesktopAudioPreviewCommandError::ProjectUnavailable)?;
        let source = resolve_audio_clip_preview_source(project, clip_id)?;
        let _ = source.full_range()?;
        if self.audio_preview_snapshot().state().is_active() {
            return Err(super::DesktopAudioPreviewCommandError::PreviewBusy);
        }
        Ok(())
    }

    /// Validates dry audition of one exact range inside a reusable audio clip's
    /// current source view.
    ///
    /// # Errors
    ///
    /// Rejects a non-project session, unknown clip, empty/overflowing/outside
    /// source range, or active audition without changing callback state.
    pub fn validate_audio_clip_source_preview(
        &self,
        clip_id: &str,
        source_start_frame: usize,
        source_frame_count: usize,
    ) -> Result<(), super::DesktopAudioPreviewCommandError> {
        let project = self
            .prepared_project()
            .ok_or(super::DesktopAudioPreviewCommandError::ProjectUnavailable)?;
        let source = resolve_audio_clip_preview_source(project, clip_id)?;
        let _ = source.selected_range(source_start_frame, source_frame_count)?;
        if self.audio_preview_snapshot().state().is_active() {
            return Err(super::DesktopAudioPreviewCommandError::PreviewBusy);
        }
        Ok(())
    }

    /// Starts dry audition of one reusable audio clip's current source view.
    ///
    /// Track processing, transport state, clip gain, fades, direction, and
    /// looping deliberately remain outside this dedicated audition path.
    ///
    /// # Errors
    ///
    /// Rejects non-project sessions, unknown clips, an active audition, or an
    /// unrepresentable catalog position without changing callback state.
    pub fn try_start_audio_clip_preview(
        &self,
        clip_id: &str,
    ) -> Result<(), super::DesktopAudioPreviewCommandError> {
        let project = self
            .prepared_project()
            .ok_or(super::DesktopAudioPreviewCommandError::ProjectUnavailable)?;
        let source = resolve_audio_clip_preview_source(project, clip_id)?;
        let (relative_start_frame, source_frame_count) = source.full_range()?;
        self.preview.request_start(
            source.source_index,
            relative_start_frame,
            source_frame_count,
        )
    }

    /// Starts dry audition of one exact range inside a reusable audio clip's
    /// current source view.
    ///
    /// Track processing, transport state, clip gain, fades, direction, and
    /// looping deliberately remain outside this dedicated audition path.
    ///
    /// # Errors
    ///
    /// Rejects non-project sessions, unknown clips, empty/overflowing/outside
    /// source ranges, active audition, or exhausted command sequencing without
    /// changing callback state.
    pub fn try_start_audio_clip_source_preview(
        &self,
        clip_id: &str,
        source_start_frame: usize,
        source_frame_count: usize,
    ) -> Result<(), super::DesktopAudioPreviewCommandError> {
        let project = self
            .prepared_project()
            .ok_or(super::DesktopAudioPreviewCommandError::ProjectUnavailable)?;
        let source = resolve_audio_clip_preview_source(project, clip_id)?;
        let (relative_start_frame, source_frame_count) =
            source.selected_range(source_start_frame, source_frame_count)?;
        self.preview.request_start(
            source.source_index,
            relative_start_frame,
            source_frame_count,
        )
    }

    /// Requests a five-millisecond ramp to silence for the active audition.
    ///
    /// # Errors
    ///
    /// Rejects an inactive audition or exhausted command sequencing.
    pub fn try_stop_audio_preview(&self) -> Result<(), super::DesktopAudioPreviewCommandError> {
        self.preview.request_stop()
    }

    #[must_use]
    pub fn audio_preview_snapshot(&self) -> super::DesktopAudioPreviewSnapshot {
        self.preview.snapshot()
    }

    #[must_use]
    pub const fn audio_preview_control(&self) -> &super::DesktopAudioPreviewControl {
        &self.preview
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.controls.transport().scene_count()
    }

    /// Returns one validated scene's retained project name.
    #[must_use]
    pub fn scene_name(&self, scene_index: usize) -> Option<&str> {
        self.source.session().arrangement().scene_name(scene_index)
    }

    #[must_use]
    pub fn scene_availability(&self, scene_index: usize) -> Option<DesktopSceneAvailability> {
        self.controls
            .transport()
            .scene_availability(scene_index)
            .map(DesktopSceneAvailability::from)
    }

    /// Returns the fixed channel count in this session's compiled mixer layout.
    #[must_use]
    pub fn mixer_channel_count(&self) -> usize {
        self.controls.mixer().channel_count()
    }

    /// Returns one compiled mixer channel's retained project track name.
    #[must_use]
    pub fn mixer_channel_name(&self, channel_index: usize) -> Option<&str> {
        self.source.session().mixer_channel_name_at(channel_index)
    }

    /// Returns the fixed send count for one compiled mixer channel.
    #[must_use]
    pub fn mixer_send_count(&self, channel_index: usize) -> Option<usize> {
        self.source.session().mixer_send_count_at(channel_index)
    }

    /// Returns immutable routing facts and the generation-bound live handle
    /// for one compiled mixer send.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel or missing-send rejection when the live
    /// control layout disagrees with the prepared mixer graph.
    pub fn mixer_send(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Result<Option<DesktopMixerSend<'_>>, MixerParameterResolveError> {
        let Some(send) = self
            .source
            .session()
            .mixer_send_at(channel_index, send_index)
        else {
            return Ok(None);
        };
        let handle = self.resolve_mixer_send_address(channel_index, send_index)?;
        Ok(Some(DesktopMixerSend {
            id: send.id(),
            target_name: send.target_name(),
            tap: match send.tap() {
                PlaybackMixerSendTap::PreFader => DesktopMixerSendTap::PreFader,
                PlaybackMixerSendTap::PostFader => DesktopMixerSendTap::PostFader,
            },
            handle,
        }))
    }

    /// Returns the number of prepared native devices in compiled channel and
    /// chain-processing order. Non-project sessions have no device catalog.
    #[must_use]
    pub fn native_device_count(&self) -> usize {
        let Some(project) = self.prepared_project() else {
            return 0;
        };
        (0..self.mixer_channel_count())
            .filter_map(|index| self.source.session().mixer_channel_track_id_at(index))
            .filter_map(|track_id| {
                project
                    .device_chains()
                    .iter()
                    .find(|chain| chain.track_id() == track_id)
            })
            .map(|chain| chain.devices().len())
            .sum()
    }

    /// Returns immutable device facts and the matching prepared command
    /// handle for one flattened Runtime catalog row.
    ///
    /// # Errors
    ///
    /// Returns a missing-device rejection when project metadata and the live
    /// prepared control layout disagree.
    pub fn native_device(
        &self,
        index: usize,
    ) -> Result<Option<DesktopNativeDevice<'_>>, NativeDeviceResolveError> {
        let Some(project) = self.prepared_project() else {
            return Ok(None);
        };
        let mut current = 0;
        for channel_index in 0..self.mixer_channel_count() {
            let Some(track_id) = self
                .source
                .session()
                .mixer_channel_track_id_at(channel_index)
            else {
                return Ok(None);
            };
            let Some(track_name) = self.mixer_channel_name(channel_index) else {
                return Ok(None);
            };
            let Some(chain) = project
                .device_chains()
                .iter()
                .find(|chain| chain.track_id() == track_id)
            else {
                continue;
            };
            for device in chain.devices() {
                if current == index {
                    return self
                        .resolve_native_device(track_id, device.id())
                        .map(|handle| {
                            Some(DesktopNativeDevice {
                                track_name,
                                device_id: device.id(),
                                kind_id: device.device_id(),
                                handle,
                            })
                        });
                }
                current += 1;
            }
        }
        Ok(None)
    }

    /// Resolves one compiled channel index through this session's mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel rejection for an out-of-range index.
    pub fn resolve_mixer_channel_index(
        &self,
        channel_index: usize,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.controls.resolve_channel_index(channel_index)
    }

    /// Reports whether one compiled mixer channel accepts live Solo commands.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel rejection for an out-of-range index.
    pub fn mixer_channel_supports_solo(
        &self,
        channel_index: usize,
    ) -> Result<bool, MixerParameterResolveError> {
        self.controls.mixer().channel_supports_solo(channel_index)
    }

    /// Resolves one track identity through this session's mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel rejection.
    pub fn resolve_mixer_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        self.controls.resolve_channel(track_id)
    }

    /// Resolves one send identity through this session's mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-send rejection.
    pub fn resolve_mixer_send(
        &self,
        send_id: &str,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.controls.resolve_send(send_id)
    }

    /// Resolves one compiled channel/send address through this session's mixer layout.
    ///
    /// # Errors
    ///
    /// Returns a missing-channel or missing-send rejection for an out-of-range address.
    pub fn resolve_mixer_send_address(
        &self,
        channel_index: usize,
        send_index: usize,
    ) -> Result<MixerSendHandle, MixerParameterResolveError> {
        self.controls
            .resolve_send_address(channel_index, send_index)
    }

    /// Sends one ordinary IDE mixer command through the session's only control
    /// owner.
    ///
    /// # Errors
    ///
    /// Returns the original target or bounded-queue rejection.
    pub fn try_send_mixer_parameter_command(
        &mut self,
        command: MixerParameterCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.controls.try_send_mixer(command)
    }

    /// Resolves one prepared native device through its owning track and stable
    /// device identity.
    ///
    /// # Errors
    ///
    /// Returns a missing-device rejection when the current prepared graph does
    /// not own that device.
    pub fn resolve_native_device(
        &self,
        track_id: &str,
        device_id: &str,
    ) -> Result<NativeDeviceHandle, NativeDeviceResolveError> {
        self.controls.resolve_native_device(track_id, device_id)
    }

    /// Sends one session-only native-device bypass command through the
    /// session's sole control owner.
    ///
    /// # Errors
    ///
    /// Returns the original target or bounded-queue rejection.
    pub fn try_send_native_device_command(
        &mut self,
        command: NativeDeviceCommand,
    ) -> Result<(), DesktopDirectControlError> {
        self.controls.try_send_native_device(command)
    }

    /// Allocates and enqueues a FIFO fence after all previously accepted mixer
    /// commands. Its sequence is published only by a successful process cycle.
    ///
    /// # Errors
    ///
    /// Returns `Ok(None)` only after the nonzero sequence space is exhausted,
    /// or a bounded-queue/disconnection rejection.
    pub fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.controls.try_send_mixer_fence()
    }

    /// Allocates and enqueues a FIFO fence after all previously accepted
    /// native-device commands. Its sequence reaches recovery only after a
    /// successful process cycle, paired with the coherent native-device state
    /// that the cycle committed.
    ///
    /// # Errors
    ///
    /// Returns `Ok(None)` only after the nonzero sequence space is exhausted,
    /// or a bounded-queue/disconnection rejection. It also refuses sessions
    /// whose prepared graph has no native-device command authority.
    pub fn try_send_native_device_fence(
        &mut self,
    ) -> Result<Option<u64>, DesktopDirectControlError> {
        self.controls.try_send_native_device_fence()
    }

    /// Allocates and enqueues a FIFO fence after all previously accepted
    /// transport commands. Its sequence reaches recovery only after a
    /// successful process cycle.
    ///
    /// # Errors
    ///
    /// Returns `Ok(None)` only after the nonzero sequence space is exhausted,
    /// or a bounded-queue/disconnection rejection.
    pub fn try_send_transport_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.controls.try_send_transport_fence()
    }

    /// Returns the greatest mixer fence committed by a successful process
    /// cycle in this session.
    #[must_use]
    pub fn committed_mixer_fence(&self) -> u64 {
        self.controls.committed_mixer_fence()
    }

    /// Returns immutable availability evidence for every enabled compiled
    /// control binding. Empty when the project has no mapping table.
    #[must_use]
    pub fn control_mapping_statuses(&self) -> &[hyper_control_runtime::ControlBindingStatus] {
        self.controls.mapping_statuses()
    }

    /// Dispatches one software-supplied source event through this session's
    /// exclusive mapping owner.
    ///
    /// A project with no compiled mapping table matches no binding rather than
    /// having no owner. This method does not imply physical input ingestion or
    /// select an IDE context implicitly.
    ///
    /// # Errors
    ///
    /// Returns the mapping owner's dispatch rejection.
    pub fn dispatch_control_event(
        &mut self,
        event: hyper_control_runtime::ControlEvent,
        context: hyper_control_runtime::ControlContext,
    ) -> Result<
        hyper_control_runtime::ControlDispatchOutcome,
        hyper_control_runtime::ControlDispatchError<DesktopControlSinkError>,
    > {
        self.controls.dispatch_mapping_event(event, context)
    }

    /// Installs one discovered panel's bindings against this session's sole
    /// control owner.
    ///
    /// # Errors
    ///
    /// Returns [`hyper_control_runtime::PanelConnectError`] when a panel is
    /// still connected or shadow state cannot be allocated.
    pub fn attach_panel(
        &mut self,
        descriptor: hyper_panel_protocol::PanelDescriptor,
        bindings: &[hyper_control_runtime::PanelBinding],
    ) -> Result<
        hyper_control_runtime::PanelConnection<DesktopControlSinkError>,
        hyper_control_runtime::PanelConnectError,
    > {
        self.controls
            .runtime_mut()
            .connect_panel(descriptor, bindings)
    }

    /// Releases anything the panel still holds and forgets its bindings.
    ///
    /// A non-empty result means a release was refused and the panel is still
    /// installed so a later call retries it.
    #[must_use]
    pub fn detach_panel(
        &mut self,
    ) -> Vec<hyper_control_runtime::PanelReleaseFailure<DesktopControlSinkError>> {
        self.controls.runtime_mut().disconnect_panel()
    }

    /// Collects what the connected panel should be showing.
    pub fn panel_display(&mut self, out: &mut Vec<hyper_control_runtime::PanelDisplay>) {
        self.controls.runtime_mut().panel_display(out);
    }

    #[must_use]
    pub fn has_panel(&self) -> bool {
        self.controls.runtime().panel_connected()
    }

    /// Dispatches one panel message against the installed bindings.
    ///
    /// # Errors
    ///
    /// Returns the sink's rejection, or reports that the target's present state
    /// could not be read.
    pub fn deliver_panel_message(
        &mut self,
        message: hyper_panel_protocol::Message,
    ) -> Result<
        hyper_control_runtime::PanelDispatchOutcome,
        hyper_control_runtime::PanelDispatchError<DesktopControlSinkError>,
    > {
        self.controls.runtime_mut().dispatch_panel(message)
    }

    #[must_use]
    pub fn loudness_command_sender(&mut self) -> &mut LoudnessCommandSender {
        &mut self.loudness_commands
    }

    /// Borrows the final-output meter consumer while this session still owns it.
    ///
    /// # Panics
    ///
    /// Panics after [`Self::take_meter_receiver`] explicitly transferred the
    /// single consumer to another presentation owner.
    #[must_use]
    pub fn meter_receiver(&mut self) -> &mut OutputMeterReceiver {
        self.meters
            .as_mut()
            .expect("the output meter receiver was explicitly transferred")
    }

    /// Transfers the final-output meter consumer away from the session owner.
    ///
    /// The audio thread keeps the only producer. A presentation owner may take
    /// the bounded consumer once without receiving playback or control
    /// authority.
    #[must_use]
    pub fn take_meter_receiver(&mut self) -> Option<OutputMeterReceiver> {
        self.meters.take()
    }

    /// Borrows committed live mixer parameter changes while this session owns
    /// their loss-detecting control-thread consumer.
    ///
    /// # Panics
    ///
    /// Panics after [`Self::take_mixer_parameter_receiver`] explicitly
    /// transferred the single consumer to another owner.
    #[must_use]
    pub fn mixer_parameter_receiver(&mut self) -> &mut InteractiveMixerParameterReceiver {
        self.mixer_parameter_observations
            .as_mut()
            .expect("the interactive mixer parameter receiver was explicitly transferred")
    }

    /// Transfers the committed live mixer parameter consumer away from the
    /// session owner without transferring audio or transport authority.
    #[must_use]
    pub fn take_mixer_parameter_receiver(&mut self) -> Option<InteractiveMixerParameterReceiver> {
        self.mixer_parameter_observations.take()
    }

    /// Borrows committed interactive-scene launches while this session owns
    /// their loss-detecting control-thread consumer.
    ///
    /// # Panics
    ///
    /// Panics after [`Self::take_scene_launch_receiver`] explicitly transferred
    /// the single consumer to another owner.
    #[must_use]
    pub fn scene_launch_receiver(&mut self) -> &mut InteractiveSceneLaunchReceiver {
        self.scene_launches
            .as_mut()
            .expect("the interactive scene launch receiver was explicitly transferred")
    }

    /// Transfers the committed interactive-scene launch consumer away from the
    /// session owner without transferring audio or transport authority.
    #[must_use]
    pub fn take_scene_launch_receiver(&mut self) -> Option<InteractiveSceneLaunchReceiver> {
        self.scene_launches.take()
    }

    /// Returns the verified project retained by this session when it was
    /// started through [`Self::open_project`] or [`Self::start_project`].
    #[must_use]
    pub fn prepared_project(&self) -> Option<&PreparedProjectPlayback> {
        self.source.project()
    }

    /// Returns the transaction recovery evidence produced by a project-open
    /// operation. Sessions started from an already prepared project have none.
    #[must_use]
    pub const fn project_open_recovery(&self) -> Option<&ProjectTransactionRecoveryReport> {
        self.project_open_recovery.as_ref()
    }

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.output.counters()
    }

    #[must_use]
    pub fn output_status(&self) -> OutputStreamStatus {
        self.output.status()
    }

    #[must_use]
    pub fn monitoring_control(&self) -> Option<DesktopMonitoringControl> {
        self.monitoring
            .as_ref()
            .map(|monitoring| monitoring.control.clone())
    }

    #[must_use]
    pub fn monitoring_snapshot(&self) -> Option<MonitoringSnapshot> {
        self.monitoring
            .as_ref()
            .map(|monitoring| monitoring.counters.snapshot())
    }

    /// Returns the current accepted-input meter receiver when input cueing is
    /// active for this playback session.
    #[must_use]
    pub fn monitoring_input_meter_receiver(&mut self) -> Option<&mut InputMeterReceiver> {
        self.monitoring
            .as_mut()
            .and_then(|monitoring| monitoring.meter.as_mut())
    }

    /// Transfers the accepted-input meter receiver to one presentation owner.
    ///
    /// The capture callback keeps the only producer. This deliberately moves
    /// only display telemetry; it does not transfer input, output, or monitor
    /// control authority.
    #[must_use]
    pub fn take_monitoring_input_meter_receiver(&mut self) -> Option<InputMeterReceiver> {
        self.monitoring
            .as_mut()
            .and_then(|monitoring| monitoring.meter.take())
    }

    #[must_use]
    pub fn monitoring_input_status(&self) -> Option<super::StreamStatus> {
        self.monitoring
            .as_ref()
            .map(|monitoring| monitoring.input.status())
    }

    #[must_use]
    pub fn monitoring_input_counters(&self) -> Option<super::CaptureStreamCounters> {
        self.monitoring
            .as_ref()
            .map(|monitoring| monitoring.input.counters())
    }

    /// Returns an observer for the last successfully committed transport cycle.
    #[must_use]
    pub fn recovery_checkpoint(&self) -> PlaybackRecoveryCheckpoint {
        self.recovery.clone()
    }

    /// Stops the current backend and captures its final committed transport
    /// state together with the immutable source required for a restart.
    ///
    /// # Errors
    ///
    /// A joined backend panic returns an error that retains recovery authority.
    /// A shutdown timeout returns one pending value that retains both the
    /// native-thread owner and recovery authority; a replacement cannot start
    /// until a later wait observes terminal teardown.
    pub fn into_recovery(
        self,
    ) -> Result<DesktopPlaybackRecovery, DesktopPlaybackRecoveryStopError> {
        let Self {
            output,
            controls: _,
            loudness_commands: _,
            meters: _,
            recovery,
            preview: _,
            config,
            source,
            monitoring,
            project_open_recovery: _,
            mixer_parameter_observations: _,
            scene_launches: _,
        } = self;
        let monitoring_recovery = monitoring.as_ref().map(|monitoring| {
            playback_monitoring_recovery_startup(&monitoring.config, &monitoring.control)
        });
        let input = monitoring
            .map(|monitoring| monitoring.input.stop().map_err(DesktopInputStopError::from))
            .transpose()
            .err();
        let output = output.stop().err().map(DesktopOutputStopError::from);
        let recovery = DesktopPlaybackRecovery {
            config,
            monitoring: monitoring_recovery,
            source: Some(source),
            checkpoint: Box::new(recovery.snapshot()),
        };
        if input.is_none() && output.is_none() {
            Ok(recovery)
        } else {
            Err(DesktopPlaybackRecoveryStopError {
                input,
                output,
                recovery: Box::new(recovery),
            })
        }
    }

    #[cfg(target_os = "windows")]
    /// Returns the asynchronous Windows session-disconnect reason, if
    /// available. Generic runtime failures have no disconnect reason.
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.output.disconnect_reason()
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn monitoring_input_disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.monitoring
            .as_ref()
            .and_then(|monitoring| monitoring.input.disconnect_reason())
    }

    /// Stops monitoring input before output and attempts both bounded joins.
    ///
    /// # Errors
    ///
    /// Returns every input or output shutdown failure, including any pending
    /// native ownership needed to observe eventual teardown.
    pub fn stop(self) -> Result<(), DesktopPlaybackSessionStopError> {
        let Self {
            output,
            controls: _,
            loudness_commands: _,
            meters: _,
            recovery: _,
            preview: _,
            config: _,
            source: _,
            monitoring,
            project_open_recovery: _,
            mixer_parameter_observations: _,
            scene_launches: _,
        } = self;
        let input = monitoring
            .map(|monitoring| monitoring.input.stop().map_err(DesktopInputStopError::from))
            .transpose()
            .err();
        let output = output.stop().err().map(DesktopOutputStopError::from);
        if input.is_none() && output.is_none() {
            Ok(())
        } else {
            Err(DesktopPlaybackSessionStopError { input, output })
        }
    }
}

impl DesktopPlaybackRecordingSession {
    /// Returns the live playback controls without transferring native stream
    /// ownership out of this recording-capable owner.
    #[must_use]
    pub const fn playback(&self) -> &DesktopPlaybackSession {
        &self.playback
    }

    /// Returns the live playback controls without transferring native stream
    /// ownership out of this recording-capable owner.
    #[must_use]
    pub fn playback_mut(&mut self) -> &mut DesktopPlaybackSession {
        &mut self.playback
    }

    /// Returns authoritative lossless recording transport progress.
    #[must_use]
    pub fn recording_snapshot(&self) -> RecordingTransportSnapshot {
        self.recording.transport_snapshot()
    }

    /// Returns the active recording journal path.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        self.recording.journal_path()
    }

    /// Stops capture, publishes the recording, and retains the sole playback
    /// restart authority with the publication.
    ///
    /// # Errors
    ///
    /// Native input failure aborts and settles the journal. Writer failures
    /// retain their own recoverable settlement alongside playback recovery.
    pub fn commit(
        self,
    ) -> Result<DesktopPlaybackPublishedRecording, DesktopPlaybackRecordingCommitError> {
        let completed = self
            .finish(true)
            .map_err(|source| DesktopPlaybackRecordingCommitError::Stop(Box::new(source)))?;
        let (completion, recovery) = completed.into_parts();
        let recording = match super::recording_session::published_recording(completion) {
            Ok(recording) => recording,
            Err(source) => {
                return Err(DesktopPlaybackRecordingCommitError::Completion {
                    source: Box::new(source),
                    recovery: Box::new(recovery),
                });
            }
        };
        Ok(DesktopPlaybackRecordingOutcome {
            recording,
            recovery,
        })
    }

    /// Publishes the recording with validated project provenance while
    /// retaining playback recovery authority.
    ///
    /// # Errors
    ///
    /// Metadata failures leave the already-settled playback recovery attached
    /// to the error so the caller cannot lose native shutdown state.
    pub fn finalize(
        self,
        context: RecordingMetadataContext,
    ) -> Result<DesktopPlaybackFinalizedRecording, DesktopPlaybackRecordingFinalizeError> {
        let committed = self
            .commit()
            .map_err(|source| DesktopPlaybackRecordingFinalizeError::Commit(Box::new(source)))?;
        let (publication, recovery) = committed.into_parts();
        let recording = match FinalizedRecordingMetadata::try_new(publication, context) {
            Ok(recording) => recording,
            Err(source) => {
                return Err(DesktopPlaybackRecordingFinalizeError::Metadata {
                    source: Box::new(source),
                    recovery: Box::new(recovery),
                });
            }
        };
        Ok(DesktopPlaybackRecordingOutcome {
            recording,
            recovery,
        })
    }

    /// Stops capture without publishing its journal and retains playback
    /// restart authority.
    ///
    /// # Errors
    ///
    /// Returns native-input or writer settlement evidence together with the
    /// only playback recovery authority.
    pub fn abort(self) -> Result<DesktopPlaybackRecordingAbort, DesktopPlaybackRecordingStopError> {
        self.finish(false)
    }

    fn finish(
        self,
        commit: bool,
    ) -> Result<DesktopPlaybackRecordingAbort, DesktopPlaybackRecordingStopError> {
        let Self {
            playback,
            recording,
        } = self;
        let capture = playback
            .monitoring
            .as_ref()
            .expect("recording-capable playback owns its capture input")
            .input
            .counters()
            .snapshot();
        let recovery = match playback.into_recovery() {
            Ok(recovery) => DesktopPlaybackRecordingRecovery::Ready(recovery),
            Err(source) if source.input.is_some() => {
                return Err(DesktopPlaybackRecordingStopError::Input {
                    playback: Box::new(source),
                    writer: Box::new(recording.abort_and_settle()),
                });
            }
            Err(source) => DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(source)),
        };
        let completion = match recording.finish(capture, commit) {
            Ok(completion) => completion,
            Err(source) => {
                return Err(DesktopPlaybackRecordingStopError::Recording {
                    source: Box::new(source),
                    recovery: Box::new(recovery),
                });
            }
        };
        Ok(DesktopPlaybackRecordingOutcome {
            recording: completion,
            recovery,
        })
    }
}

fn failed_build(
    receiver: &Receiver<SessionPublication>,
) -> Option<DesktopPlaybackSessionBuildError> {
    match receiver.try_recv() {
        Ok(SessionPublication::Failed(source)) => Some(source),
        Ok(SessionPublication::Prepared(_))
        | Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
    }
}

fn classify_output_start_failure(
    output: DesktopOutputStartError,
    receiver: &Receiver<SessionPublication>,
) -> DesktopPlaybackSessionStartError {
    if matches!(output, DesktopOutputStartError::CancellationPending { .. }) {
        DesktopPlaybackSessionStartError::Output(output)
    } else if let Some(build) = failed_build(receiver) {
        DesktopPlaybackSessionStartError::Build(build)
    } else {
        DesktopPlaybackSessionStartError::Output(output)
    }
}

fn failed_output_publication(
    output: DesktopOutputStream,
    source: DesktopPlaybackSessionStartError,
) -> DesktopPlaybackSessionStartError {
    settle_failed_output_publication(
        || output.stop().map_err(DesktopOutputStopError::from),
        source,
    )
}

fn settle_failed_output_publication(
    stop: impl FnOnce() -> Result<(), DesktopOutputStopError>,
    source: DesktopPlaybackSessionStartError,
) -> DesktopPlaybackSessionStartError {
    match stop().err() {
        Some(cleanup) => DesktopPlaybackSessionStartError::OutputCleanup {
            source: Box::new(source),
            cleanup,
        },
        None => source,
    }
}

struct SessionInitialization<'a> {
    input: &'a DesktopPlaybackSessionInput,
    recordings: &'a [hyper_recording_metadata::RecordingMetadataDocument],
    device_chains: &'a [DeviceChain],
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    initial_checkpoint: Option<&'a PlaybackRecoverySnapshot>,
    control_mapping: Option<CompiledControlMappingTable>,
    monitoring: Option<MonitoringConsumer>,
}

#[allow(
    clippy::too_many_lines,
    reason = "one linear startup transaction keeps preparation, realtime allocation, recovery, and publication ordering reviewable"
)]
fn initialize_session(
    runner: DesktopThreadRunner,
    initialization: SessionInitialization<'_>,
    publication: &SyncSender<SessionPublication>,
) -> Result<(), NativeDesktopOutputStartError> {
    let SessionInitialization {
        input,
        recordings,
        device_chains,
        sample_rate_hz,
        maximum_block_frames,
        initial_checkpoint,
        control_mapping,
        monitoring,
    } = initialization;
    let (project, mixer, automation, media) = input.parts();
    let persisted_loop = input.arrangement().loop_range();
    validate_session_sample_rate(project, sample_rate_hz)
        .or_else(|source| fail_build(publication, source))?;
    let metronome = MetronomePlan::try_new(
        sample_rate_hz,
        project.tempo_events(),
        project.meter_events(),
    )
    .map_err(DesktopPlaybackSessionBuildError::Metronome)
    .or_else(|source| fail_build(publication, source))?;

    let facts =
        collect_media_asset_facts(media).or_else(|source| fail_build(publication, source))?;

    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(DesktopPlaybackSessionBuildError::Playback)
        .or_else(|source| fail_build(publication, source))?;
    let prepared = media
        .prepare_playback(&graph)
        .map_err(DesktopPlaybackSessionBuildError::Media)
        .or_else(|source| fail_build(publication, source))?;
    let runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated_with_devices(
            &prepared,
            mixer,
            automation.clone(),
            device_chains,
            maximum_block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback_with_devices(
            &prepared,
            mixer,
            device_chains,
            maximum_block_frames,
        ),
    }
    .map_err(DesktopPlaybackSessionBuildError::Runtime)
    .or_else(|source| fail_build(publication, source))?;

    let slot_count = mixer.slots().len();
    let mut left = allocate_scratch_planes(slot_count, maximum_block_frames)
        .or_else(|source| fail_build(publication, source))?;
    let mut right = allocate_scratch_planes(slot_count, maximum_block_frames)
        .or_else(|source| fail_build(publication, source))?;
    let mut scratch: Vec<RenderScratchSlot<'_>> = Vec::new();
    scratch
        .try_reserve_exact(slot_count)
        .map_err(|_| DesktopPlaybackSessionBuildError::ScratchCapacityUnavailable)
        .or_else(|source| fail_build(publication, source))?;
    scratch.extend(
        left.iter_mut()
            .zip(&mut right)
            .map(|(left, right)| RenderScratchSlot::new(left, right)),
    );

    let (commands, mixer_parameter_commands, native_device_commands, mut transport) =
        RoutedTransportRuntime::with_command_queues_native_device_commands_and_metronome(
            runtime, metronome,
        );
    prepare_initial_transport_authority(
        &commands,
        &mut transport,
        persisted_loop,
        initial_checkpoint,
    )
    .or_else(|source| fail_build(publication, source))?;
    let initial_mixer_parameter_state = transport.mixer_parameter_state();
    let initial_native_device_state = transport.native_device_state();
    let initial_loop_range = transport.loop_range();
    let initial_loop_enabled = transport.loop_enabled();
    let initial_metronome = transport
        .metronome_gain()
        .map_or(InitialMetronomeState::unavailable(), |gain| {
            InitialMetronomeState::prepared(transport.metronome_enabled(), gain)
        });
    let (mut meter_sender, meters) = output_meter_channel();
    let (mixer_parameter_publisher, mixer_parameter_observations) =
        interactive_mixer_parameter_channel();
    let (scene_launch_publisher, scene_launches) = interactive_scene_launch_channel();
    let (loudness_commands, loudness_command_receiver) = loudness_command_channel();
    let processor = TransportOutputProcessor::try_new(
        &mut transport,
        &mut scratch,
        &mut meter_sender,
        loudness_command_receiver,
        sample_rate_hz,
    )
    .map_err(DesktopPlaybackSessionBuildError::Output)
    .or_else(|source| fail_build(publication, source))?;
    let monitoring = monitoring
        .map(|monitoring| PlaybackMonitoringMixer::try_new(monitoring, maximum_block_frames))
        .transpose()
        .or_else(|source| fail_build(publication, source))?;
    let (preview_control, preview) =
        prepare_audio_preview(media, recordings, project, sample_rate_hz, publication)?;
    let (publisher, recovery) =
        playback_recovery_channel_with_initial_loop_metronome_and_native_device_state(
            initial_checkpoint,
            Some(&initial_mixer_parameter_state),
            Some(&initial_native_device_state),
            initial_loop_range,
            initial_loop_enabled,
            initial_metronome,
        );
    let controls = build_session_controls(
        commands,
        mixer_parameter_commands,
        Some(native_device_commands),
        recovery.clone(),
        control_mapping,
    )
    .or_else(|source| fail_build(publication, source))?;
    publish_session_handles(
        publication,
        SessionHandles {
            controls,
            loudness_commands,
            meters,
            mixer_parameter_observations,
            scene_launches,
            recovery: recovery.clone(),
            preview: preview_control,
        },
    )?;
    runner.run_transport_output_with_checkpoint(
        PreviewingTransportOutputProcessor {
            playback: processor,
            monitoring,
            preview,
        },
        publisher,
        mixer_parameter_publisher,
        scene_launch_publisher,
    )
}

fn prepare_audio_preview<'a>(
    media: &'a DecodedMediaCache,
    recordings: &[hyper_recording_metadata::RecordingMetadataDocument],
    project: &LoadedPlaybackProject,
    sample_rate_hz: u32,
    publication: &SyncSender<SessionPublication>,
) -> Result<
    (
        super::DesktopAudioPreviewControl,
        DesktopAudioPreviewProcessor<'a>,
    ),
    NativeDesktopOutputStartError,
> {
    let control = super::DesktopAudioPreviewControl::new();
    let processor = DesktopAudioPreviewProcessor::try_new(
        media,
        recordings,
        project,
        sample_rate_hz,
        control.clone(),
    )
    .map_err(DesktopPlaybackSessionBuildError::AudioPreview)
    .or_else(|source| fail_build(publication, source))?;
    Ok((control, processor))
}

fn validate_session_sample_rate(
    project: &LoadedPlaybackProject,
    output_sample_rate_hz: u32,
) -> Result<(), DesktopPlaybackSessionBuildError> {
    if project.sample_rate_hz() == output_sample_rate_hz {
        Ok(())
    } else {
        Err(DesktopPlaybackSessionBuildError::SampleRateMismatch {
            project: project.sample_rate_hz(),
            output: output_sample_rate_hz,
        })
    }
}

fn collect_media_asset_facts(
    media: &DecodedMediaCache,
) -> Result<Vec<MediaAssetFact<'_>>, DesktopPlaybackSessionBuildError> {
    let mut facts = Vec::new();
    facts
        .try_reserve_exact(media.asset_count())
        .map_err(|_| DesktopPlaybackSessionBuildError::AssetFactCapacityUnavailable)?;
    facts.extend(media.media_asset_facts());
    Ok(facts)
}

fn publish_session_handles(
    publication: &SyncSender<SessionPublication>,
    handles: SessionHandles,
) -> Result<(), NativeDesktopOutputStartError> {
    publication
        .try_send(SessionPublication::Prepared(handles))
        .map_err(|_| callback_error("playback control publication failed".to_owned()))
}

fn restore_transport_checkpoint<R: hyper_mixer_runtime::TrackWindowRenderer>(
    transport: &mut RoutedTransportRuntime<R>,
    checkpoint: Option<&PlaybackRecoverySnapshot>,
) -> Result<(), MixerParameterStateRestoreError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(());
    };
    for command in [
        TransportCommand::SetLoopRange(checkpoint.loop_range()),
        TransportCommand::SetLoopEnabled(checkpoint.loop_enabled()),
        checkpoint.restore_command(),
    ] {
        transport
            .apply_command(command)
            .expect("recovery checkpoints never contain scene-launch commands");
    }
    if let Some(mixer_parameter_state) = checkpoint.mixer_parameter_state() {
        transport.restore_mixer_parameter_state(&mixer_parameter_state)?;
    }
    if let Some(native_device_state) = checkpoint.native_device_state() {
        transport.restore_native_device_state(&native_device_state)?;
    }
    Ok(())
}

fn prepare_initial_transport_authority<R: hyper_mixer_runtime::TrackWindowRenderer>(
    sender: &TransportCommandSender,
    transport: &mut RoutedTransportRuntime<R>,
    persisted: Option<hyper_playback_session::PlaybackArrangementLoopRange>,
    checkpoint: Option<&PlaybackRecoverySnapshot>,
) -> Result<(), DesktopPlaybackSessionBuildError> {
    configure_persisted_loop(sender, transport, persisted)?;
    restore_transport_checkpoint(transport, checkpoint)
        .map_err(DesktopPlaybackSessionBuildError::MixerRecovery)
}

fn configure_persisted_loop<R: hyper_mixer_runtime::TrackWindowRenderer>(
    sender: &TransportCommandSender,
    transport: &mut RoutedTransportRuntime<R>,
    persisted: Option<hyper_playback_session::PlaybackArrangementLoopRange>,
) -> Result<(), DesktopPlaybackSessionBuildError> {
    let Some(persisted) = persisted else {
        return Ok(());
    };
    let range = TransportLoopRange::new(persisted.start_frame(), persisted.end_frame())
        .map_err(DesktopPlaybackSessionBuildError::PersistedLoopRange)?;
    let range_command = TransportCommand::SetLoopRange(Some(range));
    sender
        .validate_command(range_command)
        .map_err(DesktopPlaybackSessionBuildError::PersistedLoopCommand)?;
    transport
        .apply_command(range_command)
        .expect("a validated loop command cannot contain a scene failure");
    transport
        .apply_command(TransportCommand::SetLoopEnabled(persisted.enabled()))
        .expect("a loop toggle cannot contain a scene failure");
    Ok(())
}

fn build_session_controls(
    transport: TransportCommandSender,
    mixer: MixerParameterCommandSender,
    native_devices: Option<NativeDeviceCommandSender>,
    recovery: PlaybackRecoveryCheckpoint,
    control_mapping: Option<CompiledControlMappingTable>,
) -> Result<SessionControlHandles, DesktopPlaybackSessionBuildError> {
    // Every project gets a control runtime, mapping document or not: it is the
    // sole owner of live mixer shadow state and of any panel bindings, and a
    // panel is discovered hardware rather than something a project declares.
    let table = control_mapping.unwrap_or_else(CompiledControlMappingTable::empty);
    let sink = match native_devices {
        Some(native_devices) => {
            DesktopControlSink::new_with_native_devices(transport, mixer, native_devices, recovery)
        }
        None => DesktopControlSink::new(transport, mixer, recovery),
    };
    let runtime =
        ControlRuntime::try_new(table, sink).map_err(classify_control_mapping_build_error)?;
    let mut runtimes = Vec::new();
    runtimes
        .try_reserve_exact(1)
        .map_err(|_| DesktopPlaybackSessionBuildError::ControlMappingOwnerCapacityUnavailable)?;
    runtimes.push(runtime);
    Ok(SessionControlHandles(runtimes))
}

fn classify_control_mapping_build_error(
    error: hyper_control_runtime::ControlRuntimeBuildError<DesktopControlSink>,
) -> DesktopPlaybackSessionBuildError {
    let (reason, _table, _sink) = error.into_parts();
    let source = match reason {
        ControlRuntimeBuildErrorReason::CapacityUnavailable => {
            DesktopControlMappingBuildError::CapacityUnavailable
        }
        ControlRuntimeBuildErrorReason::Target {
            binding_index,
            source,
        } => DesktopControlMappingBuildError::Target {
            binding_index,
            source,
        },
    };
    DesktopPlaybackSessionBuildError::ControlMapping(source)
}

fn output_is_pinned(config: &DesktopOutputConfig) -> bool {
    config.routing_id().is_some()
}

fn allocate_scratch_planes(
    slot_count: usize,
    maximum_block_frames: usize,
) -> Result<Vec<Vec<f32>>, DesktopPlaybackSessionBuildError> {
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(slot_count)
        .map_err(|_| DesktopPlaybackSessionBuildError::ScratchCapacityUnavailable)?;
    for _ in 0..slot_count {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(maximum_block_frames)
            .map_err(|_| DesktopPlaybackSessionBuildError::ScratchCapacityUnavailable)?;
        plane.resize(maximum_block_frames, 0.0);
        planes.push(plane);
    }
    Ok(planes)
}

fn fail_build<T>(
    publication: &SyncSender<SessionPublication>,
    source: DesktopPlaybackSessionBuildError,
) -> Result<T, NativeDesktopOutputStartError> {
    let detail = source.to_string();
    let _ = publication.try_send(SessionPublication::Failed(source));
    Err(callback_error(detail))
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
fn callback_error(detail: String) -> NativeDesktopOutputStartError {
    PipeWireStartError::Backend {
        stage: PipeWireStartupStage::Callback,
        detail,
    }
}

#[cfg(target_os = "windows")]
fn callback_error(detail: String) -> NativeDesktopOutputStartError {
    WasapiStartError::Backend {
        stage: WasapiStartupStage::Callback,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitoring_session::realtime_allocator::HeapMeasurement;
    use crate::{BoundedBackendThread, acquire_backend_thread_permit};
    use crate::{
        DesktopAudioPreviewCommandError, DesktopAudioPreviewControl, DesktopMonitoringConfigError,
    };
    use hyper_audio_source::DecodeLimits;
    use hyper_audio_source::DecodedAudio;
    use hyper_control_mappings::{
        ChannelParameter, ControlBinding, ControlMappingDocument, ControlMappingProject,
        ControlScope, ControlSource, ControlTarget, ControlTransform, Edge, TriggerBehavior,
    };
    use hyper_control_runtime::RuntimeControlSink;
    use hyper_mapping_compiler::{
        ControlMappingResolver, MixerSendAddress, compile_control_mapping,
    };
    use hyper_media_cache::CacheLimits;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    use hyper_mixer_runtime::{LocateState, PlaybackRecoveryTransportState, TransportCommand};
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;
    use hyper_playback_session::{
        DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyInput,
        assemble_playback_session,
    };
    use hyper_recording_journal::JournalConfig;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::mpsc::SyncSender;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Debug)]
    struct UnusedPlaybackSource;

    const LIVE_PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337500";
    const LIVE_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337504";
    const LIVE_TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337501",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
    }"#;
    const LIVE_TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337502",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122337503"],"extensions":{}
    }"#;
    const LIVE_CLIP: &[u8] = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337503",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","name":"Clip","enabled":true,
        "duration_ticks":4,"source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337504","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":4,"source_frame_offset":100}],
        "extensions":{}
    }"#;
    const LIVE_ARRANGEMENT: &[u8] = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
        "placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}],
        "scene_launches":[],"extensions":{}
    }"#;
    const LIVE_LOOPED_ARRANGEMENT: &[u8] = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
        "placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}],
        "scene_launches":[],"loop_range":{"enabled":true,"start_tick":0,"end_tick":4},"extensions":{}
    }"#;
    const LIVE_MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
        "channels":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":1000000,"pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}],
        "return_buses":[],"main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},"extensions":{}
    }"#;
    const LIVE_MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337508";
    const LIVE_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337509";

    struct LiveMappingProject;

    impl ControlMappingProject for LiveMappingProject {
        fn project_id(&self) -> &str {
            LIVE_PROJECT_ID
        }

        fn cv_inputs(&self) -> u8 {
            4
        }

        fn has_clock_input(&self) -> bool {
            true
        }

        fn has_reset_input(&self) -> bool {
            true
        }

        fn has_mixer(&self) -> bool {
            true
        }

        fn contains_nested_entity(&self, id: &str) -> bool {
            id == "018f47a2-6b2d-7c11-8a44-001122337502"
        }

        fn contains_entity(&self, _id: &str) -> bool {
            false
        }

        fn contains_track(&self, id: &str) -> bool {
            self.contains_nested_entity(id)
        }

        fn contains_scene(&self, _id: &str) -> bool {
            false
        }

        fn contains_mixer_channel(&self, track_id: &str) -> bool {
            self.contains_track(track_id)
        }

        fn contains_mixer_send(&self, _send_id: &str) -> bool {
            false
        }

        fn is_solo_source_track(&self, track_id: &str) -> bool {
            self.contains_track(track_id)
        }
    }

    impl ControlMappingResolver for LiveMappingProject {
        fn track_index(&self, track_id: &str) -> Option<usize> {
            self.contains_track(track_id).then_some(0)
        }

        fn scene_index(&self, _scene_id: &str) -> Option<usize> {
            None
        }

        fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
            self.contains_mixer_channel(track_id).then_some(0)
        }

        fn mixer_send_address(&self, _send_id: &str) -> Option<MixerSendAddress> {
            None
        }
    }

    fn compiled_channel_mute_mapping() -> CompiledControlMappingTable {
        compiled_channel_mute_mapping_with(TriggerBehavior::Toggle)
    }

    fn compiled_channel_mute_mapping_with(
        behavior: TriggerBehavior,
    ) -> CompiledControlMappingTable {
        let binding = ControlBinding::try_new(
            LIVE_BINDING_ID.to_owned(),
            true,
            ControlSource::ClockInput { edge: Edge::Rising },
            ControlScope::Global,
            ControlTransform::Trigger { behavior },
            ControlTarget::MixerChannel {
                track_id: "018f47a2-6b2d-7c11-8a44-001122337502".to_owned(),
                parameter: ChannelParameter::Mute,
            },
        )
        .unwrap();
        let document = ControlMappingDocument::try_new(
            LIVE_MAPPING_ID.to_owned(),
            LIVE_PROJECT_ID.to_owned(),
            vec![binding],
            BTreeMap::new(),
        )
        .unwrap();
        compile_control_mapping(&document, LIVE_MAPPING_ID, &LiveMappingProject).unwrap()
    }

    impl DesktopPlaybackSource for UnusedPlaybackSource {
        fn session(&self) -> &PreparedPlaybackSession {
            unreachable!("the authority-state fixture never builds a playback session")
        }

        fn project(&self) -> Option<&PreparedProjectPlayback> {
            None
        }
    }

    fn pending_shutdown_fixture() -> (PendingNativeShutdown, SyncSender<()>) {
        pending_shutdown_fixture_with_status(OutputStreamStatus::default())
    }

    fn pending_shutdown_fixture_with_status(
        status: OutputStreamStatus,
    ) -> (PendingNativeShutdown, SyncSender<()>) {
        let (release_sender, release_receiver) = sync_channel(1);
        let (completed_sender, completed_receiver) = sync_channel(1);
        let handle = thread::spawn(move || {
            release_receiver.recv().unwrap();
            completed_sender.send(()).unwrap();
        });
        let thread = BoundedBackendThread::new(
            handle,
            completed_receiver,
            acquire_backend_thread_permit().unwrap(),
        );
        (PendingNativeShutdown::new(thread, status), release_sender)
    }

    fn pending_shutdown_panic_fixture() -> (PendingNativeShutdown, SyncSender<()>) {
        pending_shutdown_panic_fixture_with_status(OutputStreamStatus::default())
    }

    fn pending_shutdown_panic_fixture_with_status(
        status: OutputStreamStatus,
    ) -> (PendingNativeShutdown, SyncSender<()>) {
        let (release_sender, release_receiver) = sync_channel(1);
        let (completed_sender, completed_receiver) = sync_channel(1);
        let handle = thread::spawn(move || {
            let _completed_sender = completed_sender;
            release_receiver.recv().unwrap();
            panic!("pending shutdown panic fixture");
        });
        let thread = BoundedBackendThread::new(
            handle,
            completed_receiver,
            acquire_backend_thread_permit().unwrap(),
        );
        (PendingNativeShutdown::new(thread, status), release_sender)
    }

    fn project_limits() -> ProjectPlaybackLimits {
        ProjectPlaybackLimits {
            max_total_resource_bytes: 1_048_576,
            audio: DecodeLimits::new(1_048_576, 48_000, 384_000).unwrap(),
            session: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: 16,
                    max_frames_per_asset: 48_000,
                    max_decoded_sample_bytes: 3_072_000,
                },
            },
        }
    }

    fn monitoring_config() -> DesktopMonitoringConfig {
        DesktopMonitoringConfig::try_new(
            super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            64,
        )
        .unwrap()
    }

    fn monitoring_transport() -> (
        super::super::monitoring_session::MonitoringProducer,
        MonitoringConsumer,
        MonitoringCounters,
    ) {
        let config = monitoring_config();
        let (producer, consumer, counters, _, _meter) = try_monitoring_transport(&config).unwrap();
        (producer, consumer, counters)
    }

    #[test]
    fn initial_playback_monitor_state_is_applied_before_native_startup() {
        let startup = PlaybackMonitoringStartup::new(monitoring_config(), 0.25, true);
        let (_producer, _consumer, _counters, control, _meter) =
            prepare_playback_monitoring_transport(&startup).unwrap();

        assert!((control.gain_linear() - 0.25).abs() <= f32::EPSILON);
        assert!(control.muted());

        let invalid = PlaybackMonitoringStartup::new(monitoring_config(), f32::NAN, false);
        assert!(matches!(
            prepare_playback_monitoring_transport(&invalid),
            Err(DesktopPlaybackSessionStartError::MonitoringControl(
                DesktopMonitoringControlError::NonFiniteGain
            ))
        ));
    }

    #[test]
    fn playback_monitor_recovery_retains_the_committed_control_state() {
        let config = monitoring_config();
        let startup = PlaybackMonitoringStartup::new(config.clone(), 1.0, true);
        let (_producer, _consumer, _counters, control, _meter) =
            prepare_playback_monitoring_transport(&startup).unwrap();
        control.set_gain_linear(0.625).unwrap();
        control.set_muted(false);

        let recovery = playback_monitoring_recovery_startup(&config, &control);
        assert!((recovery.gain_linear - 0.625).abs() <= f32::EPSILON);
        assert!(!recovery.muted);
        assert_eq!(recovery.config, config);

        let replacement = DesktopOutputConfig::try_new(48_000, 128)
            .unwrap()
            .with_routing_id("replacement-output")
            .unwrap();
        let rebound = recovery.with_output(replacement).unwrap();
        assert!((rebound.gain_linear - 0.625).abs() <= f32::EPSILON);
        assert!(!rebound.muted);
    }

    fn recording_input(routing_id: &str) -> super::super::DesktopInputConfig {
        super::super::DesktopInputConfig::try_new(48_000, 128)
            .unwrap()
            .with_routing_id(routing_id)
            .unwrap()
    }

    fn recording_config(
        input: super::super::DesktopInputConfig,
        directory: &Path,
    ) -> DesktopRecordingSessionConfig {
        DesktopRecordingSessionConfig {
            input,
            transport_capacity_frames: 1_024,
            writer: hyper_recording_worker::RecordingWorkerConfig {
                journal_path: directory.join("take.journal"),
                final_wav_path: directory.join("take.wav"),
                journal: JournalConfig {
                    sample_rate: 48_000,
                    max_chunk_frames: 256,
                    max_total_frames: 48_000,
                },
                drain_frames: 256,
            },
            completion_timeout: Duration::from_secs(1),
        }
    }

    fn recording_recovery(
        config: DesktopOutputConfig,
        monitoring: Option<PlaybackMonitoringStartup>,
    ) -> DesktopPlaybackRecovery {
        DesktopPlaybackRecovery {
            config,
            monitoring,
            source: Some(Arc::new(UnusedPlaybackSource)),
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        }
    }

    #[test]
    fn recording_start_refuses_policy_output_without_spending_recovery() {
        let recording = recording_config(recording_input("recording-input"), Path::new("unused"));
        let unpinned = recording_recovery(DesktopOutputConfig::try_new(48_000, 256).unwrap(), None);
        let error = unpinned.start_recording(&recording).unwrap_err();

        assert!(matches!(
            &error,
            DesktopPlaybackRecordingStartError::OutputNotPinned { .. }
        ));
        assert!(error.recovery().source.is_some());
        assert!(error.writer_settlement().is_none());
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("pre-native refusal cannot own a pending startup");
        };
        assert!(settled.recovery.source.is_some());
    }

    #[test]
    fn recording_start_refuses_missing_monitor_without_spending_recovery() {
        let output = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("recording-output")
            .unwrap();
        let recording = recording_config(recording_input("recording-input"), Path::new("unused"));
        let unmonitored = recording_recovery(output, None);
        let error = unmonitored.start_recording(&recording).unwrap_err();

        assert!(matches!(
            &error,
            DesktopPlaybackRecordingStartError::MonitoringUnavailable { .. }
        ));
        assert!(matches!(
            error.into_recovery().wait(Duration::ZERO),
            DesktopPlaybackRecordingStartRecoveryWait::Settled(
                DesktopPlaybackRecordingStartSettlement { writer: None, .. }
            )
        ));
    }

    #[test]
    fn recording_start_refuses_an_input_other_than_the_monitored_source() {
        let output = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("recording-output")
            .unwrap();
        let recording = recording_config(recording_input("recording-input"), Path::new("unused"));
        let mismatched_monitoring = DesktopMonitoringConfig::try_new(
            recording_input("other-input"),
            output.clone(),
            1_024,
            256,
            64,
        )
        .unwrap();
        let mismatched = recording_recovery(
            output,
            Some(PlaybackMonitoringStartup::new(
                mismatched_monitoring,
                1.0,
                false,
            )),
        );
        let error = mismatched.start_recording(&recording).unwrap_err();

        assert!(matches!(
            &error,
            DesktopPlaybackRecordingStartError::InputMismatch { .. }
        ));
        assert!(matches!(
            error.into_recovery().wait(Duration::ZERO),
            DesktopPlaybackRecordingStartRecoveryWait::Settled(
                DesktopPlaybackRecordingStartSettlement { writer: None, .. }
            )
        ));
    }

    #[test]
    fn recording_start_rejects_invalid_config_before_creating_files() {
        let directory = std::env::temp_dir().join(format!(
            "hyper-playback-recording-start-refusal-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).unwrap();
        let output = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("recording-output")
            .unwrap();
        let input = recording_input("recording-input");
        let monitoring =
            DesktopMonitoringConfig::try_new(input.clone(), output.clone(), 1_024, 256, 64)
                .unwrap();
        let invalid = recording_recovery(
            output,
            Some(PlaybackMonitoringStartup::new(monitoring, 1.0, false)),
        );
        let mut invalid_recording = recording_config(input, &directory);
        invalid_recording.transport_capacity_frames = 0;
        let error = invalid.start_recording(&invalid_recording).unwrap_err();

        assert!(matches!(
            &error,
            DesktopPlaybackRecordingStartError::RecordingConfig {
                source: DesktopRecordingSessionConfigError::ZeroTransportCapacity,
                ..
            }
        ));
        assert!(matches!(
            error.into_recovery().wait(Duration::ZERO),
            DesktopPlaybackRecordingStartRecoveryWait::Settled(
                DesktopPlaybackRecordingStartSettlement { writer: None, .. }
            )
        ));
        assert!(fs::read_dir(&directory).unwrap().next().is_none());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn recording_start_failure_keeps_input_shutdown_and_writer_before_recovery() {
        let (shutdown, release) = pending_shutdown_fixture();
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Input {
            journal_path: journal_path.clone(),
            source: DesktopInputStartError::CancellationPending {
                shutdown: Box::new(shutdown),
            },
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path: journal_path.clone(),
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("pending input must retain the restart authority");
        };
        release.send(()).unwrap();
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("joined input must release the restart authority");
        };
        assert!(settled.recovery.source.is_some());
        assert_eq!(
            settled.journal_path.as_deref(),
            Some(journal_path.as_path())
        );
        assert!(matches!(
            settled.writer,
            Some(DesktopRecordingWriterSettlement::Completed(completion))
                if matches!(*completion, RecordingWorkerCompletion::Aborted { .. })
        ));
    }

    #[test]
    fn recording_start_failure_waits_for_input_and_output_and_remembers_native_failure() {
        let status = OutputStreamStatus::default();
        status.mark_failed();
        let (input, release_input) = pending_shutdown_fixture_with_status(status);
        let (output, release_output) = pending_shutdown_fixture();
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Playback {
            journal_path: journal_path.clone(),
            source: Box::new(DesktopPlaybackSessionStartError::Output(
                DesktopOutputStartError::CancellationPending {
                    shutdown: Box::new(output),
                },
            )),
            input_cleanup: Some(DesktopInputStopError::ShutdownPending {
                shutdown: Box::new(input),
            }),
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path,
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        release_input.send(()).unwrap();
        let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) =
            error.into_recovery().wait(Duration::from_millis(50))
        else {
            panic!("pending output must retain restart authority after input settles");
        };
        release_output.send(()).unwrap();
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("both native owners must settle");
        };
        assert_eq!(
            settled.input,
            DesktopPlaybackRecordingNativeSettlement::Failed
        );
        assert_eq!(
            settled.output,
            DesktopPlaybackRecordingNativeSettlement::Clean
        );
        assert!(settled.recovery.source.is_some());
    }

    #[test]
    fn recording_start_failure_keeps_post_publication_output_cleanup() {
        let (shutdown, release) = pending_shutdown_fixture();
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Playback {
            journal_path: journal_path.clone(),
            source: Box::new(DesktopPlaybackSessionStartError::OutputCleanup {
                source: Box::new(DesktopPlaybackSessionStartError::PublicationUnavailable),
                cleanup: DesktopOutputStopError::ShutdownPending {
                    shutdown: Box::new(shutdown),
                },
            }),
            input_cleanup: None,
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path,
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("post-publication output shutdown must gate recovery");
        };
        release.send(()).unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecordingStartRecoveryWait::Settled(_)
        ));
    }

    #[test]
    fn failed_publication_stops_output_once_and_retains_pending_cleanup() {
        for source in [
            DesktopPlaybackSessionStartError::Build(
                DesktopPlaybackSessionBuildError::AssetFactCapacityUnavailable,
            ),
            DesktopPlaybackSessionStartError::PublicationUnavailable,
        ] {
            let expected_build = matches!(&source, DesktopPlaybackSessionStartError::Build(_));
            let (shutdown, release) = pending_shutdown_fixture();
            let stop_calls = std::cell::Cell::new(0);
            let error = settle_failed_output_publication(
                || {
                    stop_calls.set(stop_calls.get() + 1);
                    Err(DesktopOutputStopError::ShutdownPending {
                        shutdown: Box::new(shutdown),
                    })
                },
                source,
            );
            assert_eq!(stop_calls.get(), 1);
            let DesktopPlaybackSessionStartError::OutputCleanup {
                source,
                cleanup: DesktopOutputStopError::ShutdownPending { mut shutdown },
            } = error
            else {
                panic!("failed publication must retain pending output cleanup");
            };
            assert_eq!(
                matches!(*source, DesktopPlaybackSessionStartError::Build(_)),
                expected_build
            );
            release.send(()).unwrap();
            assert_eq!(
                shutdown.wait(Duration::from_secs(1)),
                PendingNativeShutdownWait::Completed
            );
        }
    }

    #[test]
    fn failed_publication_preserves_the_start_error_after_clean_output_stop() {
        let stop_calls = std::cell::Cell::new(0);
        let error = settle_failed_output_publication(
            || {
                stop_calls.set(stop_calls.get() + 1);
                Ok(())
            },
            DesktopPlaybackSessionStartError::PublicationUnavailable,
        );
        assert_eq!(stop_calls.get(), 1);
        assert!(matches!(
            error,
            DesktopPlaybackSessionStartError::PublicationUnavailable
        ));
    }

    #[test]
    fn recording_start_failure_preserves_terminal_input_and_output_cleanup_evidence() {
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Playback {
            journal_path: journal_path.clone(),
            source: Box::new(DesktopPlaybackSessionStartError::OutputCleanup {
                source: Box::new(DesktopPlaybackSessionStartError::PublicationUnavailable),
                cleanup: DesktopOutputStopError::RuntimeFailed,
            }),
            input_cleanup: Some(DesktopInputStopError::ThreadPanicked),
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path,
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("terminal cleanup must not invent pending ownership");
        };
        assert_eq!(
            settled.input,
            DesktopPlaybackRecordingNativeSettlement::Panicked
        );
        assert_eq!(
            settled.output,
            DesktopPlaybackRecordingNativeSettlement::Failed
        );
    }

    #[test]
    fn recording_start_failure_retains_terminal_output_panic_while_input_is_pending() {
        let (shutdown, release) = pending_shutdown_fixture();
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Playback {
            journal_path: journal_path.clone(),
            source: Box::new(DesktopPlaybackSessionStartError::OutputCleanup {
                source: Box::new(DesktopPlaybackSessionStartError::PublicationUnavailable),
                cleanup: DesktopOutputStopError::ThreadPanicked,
            }),
            input_cleanup: Some(DesktopInputStopError::ShutdownPending {
                shutdown: Box::new(shutdown),
            }),
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path,
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("input shutdown must remain pending");
        };
        release.send(()).unwrap();
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("input shutdown should complete");
        };
        assert_eq!(
            settled.output,
            DesktopPlaybackRecordingNativeSettlement::Panicked
        );
        assert_eq!(
            settled.input,
            DesktopPlaybackRecordingNativeSettlement::Clean
        );
    }

    #[test]
    fn recording_start_failure_preserves_input_panic_across_output_timeout() {
        let status = OutputStreamStatus::default();
        status.mark_failed();
        let (input, release_input) = pending_shutdown_panic_fixture_with_status(status);
        let (output, release_output) = pending_shutdown_fixture();
        let journal_path = Path::new("unpublished-take.journal").to_path_buf();
        let error = DesktopPlaybackRecordingStartError::Playback {
            journal_path: journal_path.clone(),
            source: Box::new(DesktopPlaybackSessionStartError::Output(
                DesktopOutputStartError::CancellationPending {
                    shutdown: Box::new(output),
                },
            )),
            input_cleanup: Some(DesktopInputStopError::ShutdownPending {
                shutdown: Box::new(input),
            }),
            writer: Box::new(DesktopRecordingWriterSettlement::Completed(Box::new(
                RecordingWorkerCompletion::Aborted {
                    frames: 0,
                    journal_path,
                },
            ))),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };

        release_input.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut pending = error.into_recovery();
        let pending = loop {
            let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(next) =
                pending.wait(Duration::from_millis(10))
            else {
                panic!("pending output must retain the panicked input settlement");
            };
            if next.input == DesktopPlaybackRecordingNativeSettlement::Panicked {
                break next;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "input panic was not observed before the test deadline"
            );
            pending = next;
        };
        assert_eq!(
            pending.input,
            DesktopPlaybackRecordingNativeSettlement::Panicked
        );
        release_output.send(()).unwrap();
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("released output must allow final settlement");
        };
        assert_eq!(
            settled.input,
            DesktopPlaybackRecordingNativeSettlement::Panicked
        );
        assert_eq!(
            settled.output,
            DesktopPlaybackRecordingNativeSettlement::Clean
        );
    }

    #[test]
    fn recording_start_failure_retains_a_real_pending_writer_until_journal_settlement() {
        let directory = std::env::temp_dir().join(format!(
            "hyper-playback-pending-writer-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir(&directory).unwrap();
        let mut recording = recording_config(recording_input("recording-input"), &directory);
        recording.completion_timeout = Duration::from_millis(1);
        let Ok((producer, core)) = start_recording_core(recording) else {
            panic!("the pending-writer fixture must start a real journal worker");
        };
        let journal_path = core.journal_path().to_path_buf();
        let writer = core.abort_and_settle();
        assert!(matches!(
            writer,
            DesktopRecordingWriterSettlement::Pending(_)
        ));

        let error = DesktopPlaybackRecordingStartError::Input {
            journal_path: journal_path.clone(),
            source: DesktopInputStartError::ThreadTerminated,
            writer: Box::new(writer),
            recovery: Box::new(recording_recovery(
                DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                None,
            )),
        };
        let DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) =
            error.into_recovery().wait(Duration::ZERO)
        else {
            panic!("an open producer must keep the abort writer pending");
        };
        drop(producer);
        let DesktopPlaybackRecordingStartRecoveryWait::Settled(settled) =
            pending.wait(Duration::from_secs(5))
        else {
            panic!("closing the producer must permit terminal writer settlement");
        };
        assert!(settled.recovery.source.is_some());
        assert!(matches!(
            settled.writer,
            Some(DesktopRecordingWriterSettlement::Completed(completion))
                if matches!(*completion, RecordingWorkerCompletion::Aborted { .. })
        ));
        fs::remove_file(journal_path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn playback_monitoring_mixes_into_the_final_stereo_output_without_allocating() {
        let (mut producer, consumer, _) = monitoring_transport();
        producer.push(&vec![0.5; 320 * 2]);
        let mut monitoring = PlaybackMonitoringMixer::try_new(consumer, 128).unwrap();
        let mut left = [0.75; 64];
        let mut right = [-0.5; 64];

        let measurement = HeapMeasurement::start_verified();
        monitoring.mix(StereoOutput {
            left: &mut left,
            right: &mut right,
        });
        let heap_operations = measurement.finish();

        assert_eq!(heap_operations, 0);
        assert!(
            left.iter()
                .all(|sample| (*sample - 1.25).abs() <= f32::EPSILON)
        );
        assert!(right.iter().all(|sample| sample.abs() <= f32::EPSILON));
    }

    #[test]
    fn invalid_monitor_sum_leaves_the_complete_project_block_unchanged() {
        let mut left = [f32::MAX, 0.25];
        let mut right = [-0.5, 0.75];
        let original_left = left;
        let original_right = right;

        assert!(!mix_monitoring_interleaved(
            StereoOutput {
                left: &mut left,
                right: &mut right,
            },
            &[f32::MAX, f32::MAX, 0.5, 0.5],
        ));

        assert!(
            left.iter()
                .zip(original_left)
                .all(|(actual, expected)| actual.to_bits() == expected.to_bits())
        );
        assert!(
            right
                .iter()
                .zip(original_right)
                .all(|(actual, expected)| actual.to_bits() == expected.to_bits())
        );
    }

    #[test]
    fn invalid_monitor_sum_disables_only_monitoring_without_allocating() {
        let (mut producer, consumer, counters) = monitoring_transport();
        producer.push(&vec![3.0e38; 320 * 2]);
        let mut monitoring = PlaybackMonitoringMixer::try_new(consumer, 128).unwrap();
        let mut callback =
            hyper_audio_engine::InterleavedStereoCallback::try_new(64, 48_000).unwrap();
        let mut output = [9.0; 128];

        let measurement = HeapMeasurement::start_verified();
        let processed = callback.process(&mut output, |project| {
            project.left.fill(1.0e38);
            project.right.fill(-0.5);
            monitoring.mix(project);
            Ok::<(), core::convert::Infallible>(())
        });
        let heap_operations = measurement.finish();

        assert_eq!(heap_operations, 0);
        assert!(processed.is_ok());
        assert!(output.chunks_exact(2).all(|frame| frame == [1.0e38, -0.5]));
        assert!(!monitoring.enabled);
        assert_eq!(counters.snapshot().processor_failure_count, 1);

        callback
            .process(&mut output, |project| {
                project.left.fill(0.25);
                project.right.fill(-0.25);
                monitoring.mix(project);
                Ok::<(), core::convert::Infallible>(())
            })
            .unwrap();
        assert!(output.chunks_exact(2).all(|frame| frame == [0.25, -0.25]));
        assert_eq!(counters.snapshot().processor_failure_count, 1);
    }

    #[test]
    fn invalid_project_output_does_not_consume_or_disable_monitoring() {
        let (mut producer, consumer, counters) = monitoring_transport();
        producer.push(&vec![0.5; 320 * 2]);
        let mut monitoring = PlaybackMonitoringMixer::try_new(consumer, 128).unwrap();
        let mut invalid_left = [f32::NAN; 64];
        let mut invalid_right = [0.25; 64];

        monitoring.mix(StereoOutput {
            left: &mut invalid_left,
            right: &mut invalid_right,
        });

        assert!(monitoring.enabled);
        assert_eq!(counters.snapshot().rendered_frame_count, 0);
        assert_eq!(counters.snapshot().processor_failure_count, 0);

        let mut valid_left = [0.5; 64];
        let mut valid_right = [-0.5; 64];
        monitoring.mix(StereoOutput {
            left: &mut valid_left,
            right: &mut valid_right,
        });
        assert!(
            valid_left
                .iter()
                .all(|sample| (*sample - 1.0).abs() <= f32::EPSILON)
        );
        assert!(
            valid_right
                .iter()
                .all(|sample| sample.abs() <= f32::EPSILON)
        );
        assert_eq!(counters.snapshot().rendered_frame_count, 64);
        assert_eq!(counters.snapshot().processor_failure_count, 0);
    }

    fn live_session_input() -> PreparedPlaybackSession {
        live_session_input_with_arrangement(LIVE_ARRANGEMENT)
    }

    fn live_session_input_with_arrangement(arrangement: &[u8]) -> PreparedPlaybackSession {
        assemble_playback_session(PlaybackSessionAssemblyInput {
            documents: PlaybackDocumentBytes {
                project_id: LIVE_PROJECT_ID.to_owned(),
                sample_rate_hz: 48_000,
                timeline: LIVE_TIMELINE.to_vec(),
                tracks: vec![LIVE_TRACK.to_vec()],
                clips: vec![LIVE_CLIP.to_vec()],
                scenes: Vec::new(),
                arrangement: Some(arrangement.to_vec()),
                mixer: LIVE_MIXER.to_vec(),
                automation_takes: Vec::new(),
            },
            decoded_assets: vec![DecodedPlaybackAsset::new(
                LIVE_ASSET_ID.to_owned(),
                DecodedAudio::from_mono(48_000, vec![0.25; 100]).unwrap(),
            )],
            limits: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: 1,
                    max_frames_per_asset: 100,
                    max_decoded_sample_bytes: 400,
                },
            },
        })
        .unwrap()
    }

    #[test]
    #[allow(
        clippy::float_cmp,
        reason = "the mono preview path copies each sample identically into both output channels"
    )]
    fn clip_audition_uses_the_current_source_view_not_the_full_asset() {
        let clip = String::from_utf8(LIVE_CLIP.to_vec())
            .unwrap()
            .replace(
                "\"start_frame\":0,\"frame_count\":100",
                "\"start_frame\":600,\"frame_count\":500",
            )
            .replace("\"source_frame_offset\":100", "\"source_frame_offset\":500");
        let session = assemble_playback_session(PlaybackSessionAssemblyInput {
            documents: PlaybackDocumentBytes {
                project_id: LIVE_PROJECT_ID.to_owned(),
                sample_rate_hz: 48_000,
                timeline: LIVE_TIMELINE.to_vec(),
                tracks: vec![LIVE_TRACK.to_vec()],
                clips: vec![clip.into_bytes()],
                scenes: Vec::new(),
                arrangement: None,
                mixer: LIVE_MIXER.to_vec(),
                automation_takes: Vec::new(),
            },
            decoded_assets: vec![DecodedPlaybackAsset::new(
                LIVE_ASSET_ID.to_owned(),
                DecodedAudio::from_mono(
                    48_000,
                    (0..1_200)
                        .map(|frame| if frame < 600 { 0.0 } else { 1.0 })
                        .collect(),
                )
                .unwrap(),
            )],
            limits: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: 1,
                    max_frames_per_asset: 1_200,
                    max_decoded_sample_bytes: 4_800,
                },
            },
        })
        .unwrap();
        let (project, _, _, media) = session.parts();
        let preview_source = resolve_audio_clip_preview_source_from_loaded(
            project,
            0,
            "018f47a2-6b2d-7c11-8a44-001122337503",
        )
        .unwrap();
        assert_eq!(preview_source.source_index, 0);
        assert_eq!(preview_source.view_start_frame, 600);
        assert_eq!(preview_source.view_frame_count, 500);
        assert_eq!(preview_source.selected_range(725, 125), Ok((125, 125)));
        let control = DesktopAudioPreviewControl::new();
        let mut preview =
            DesktopAudioPreviewProcessor::try_new(media, &[], project, 48_000, control.clone())
                .unwrap();
        let (relative_start_frame, source_frame_count) = preview_source.full_range().unwrap();
        control
            .request_start(
                preview_source.source_index,
                relative_start_frame,
                source_frame_count,
            )
            .unwrap();
        let mut left = [0.0_f32; 400];
        let mut right = [0.0_f32; 400];

        assert!(preview.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        assert!(
            left[300] > 0.0,
            "audition must begin at the clip source offset"
        );
        assert_eq!(left, right);
    }

    #[test]
    fn selected_clip_preview_range_requires_one_exact_current_source_subrange() {
        assert_eq!(
            selected_audio_clip_preview_range(600, 500, 725, 125),
            Ok((125, 125))
        );
        assert_eq!(
            selected_audio_clip_preview_range(600, 500, 600, 500),
            Ok((0, 500))
        );
        for (start, count) in [(599, 1), (600, 0), (1_099, 2), (usize::MAX, 1)] {
            assert_eq!(
                selected_audio_clip_preview_range(600, 500, start, count),
                Err(DesktopAudioPreviewCommandError::SourceRangeUnavailable),
                "selected source range {start}..{} must be refused",
                start.saturating_add(count)
            );
        }
        assert_eq!(
            selected_audio_clip_preview_range(usize::MAX, 1, usize::MAX, 1),
            Err(DesktopAudioPreviewCommandError::SourceRangeUnavailable)
        );
    }

    #[test]
    fn persisted_loop_is_initial_authority_and_recovery_overrides_its_toggle() {
        let input = live_session_input_with_arrangement(LIVE_LOOPED_ARRANGEMENT);
        let persisted = input.arrangement().loop_range().unwrap();
        assert_eq!((persisted.start_frame(), persisted.end_frame()), (0, 100));
        assert!(persisted.enabled());
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (sender, _mixer, mut transport) = RoutedTransportRuntime::with_command_queues(runtime);

        prepare_initial_transport_authority(&sender, &mut transport, Some(persisted), None)
            .unwrap();
        assert_eq!(
            transport.loop_range(),
            Some(TransportLoopRange::new(0, 100).unwrap())
        );
        assert!(transport.loop_enabled());

        transport
            .apply_command(TransportCommand::SetLoopEnabled(false))
            .unwrap();
        let mut scratch_left = [0.0_f32; 64];
        let mut scratch_right = [0.0_f32; 64];
        let mut scratch = [RenderScratchSlot::new(
            &mut scratch_left,
            &mut scratch_right,
        )];
        let mut output_left = [0.0_f32; 64];
        let mut output_right = [0.0_f32; 64];
        let cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut output_left,
                    right: &mut output_right,
                },
            )
            .unwrap();
        let (mut publisher, recovery) =
            hyper_mixer_runtime::playback_recovery_channel_with_initial_loop(
                None,
                None,
                transport.loop_range(),
                transport.loop_enabled(),
            );
        publisher.publish_cycle(cycle, None);
        let checkpoint = recovery.snapshot();
        assert_eq!(checkpoint.loop_range(), transport.loop_range());
        assert!(!checkpoint.loop_enabled());

        let replacement =
            RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (replacement_sender, _mixer, mut replacement) =
            RoutedTransportRuntime::with_command_queues(replacement);
        prepare_initial_transport_authority(
            &replacement_sender,
            &mut replacement,
            Some(persisted),
            Some(&checkpoint),
        )
        .unwrap();
        assert_eq!(replacement.loop_range(), checkpoint.loop_range());
        assert!(!replacement.loop_enabled());

        let (_clear_publisher, clear_recovery) = playback_recovery_channel(None, None);
        let clear_checkpoint = clear_recovery.snapshot();
        let cleared = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (cleared_sender, _mixer, mut cleared) =
            RoutedTransportRuntime::with_command_queues(cleared);
        prepare_initial_transport_authority(
            &cleared_sender,
            &mut cleared,
            Some(persisted),
            Some(&clear_checkpoint),
        )
        .unwrap();
        assert_eq!(cleared.position_frame(), 0);
        assert_eq!(cleared.loop_range(), None);
        assert!(!cleared.loop_enabled());
    }

    #[test]
    fn persisted_disabled_loop_retains_its_range_without_engaging() {
        let disabled = String::from_utf8(LIVE_LOOPED_ARRANGEMENT.to_vec())
            .unwrap()
            .replace(
                "\"loop_range\":{\"enabled\":true",
                "\"loop_range\":{\"enabled\":false",
            );
        let input = live_session_input_with_arrangement(disabled.as_bytes());
        let persisted = input.arrangement().loop_range().unwrap();
        assert!(!persisted.enabled());
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (sender, _mixer, mut transport) = RoutedTransportRuntime::with_command_queues(runtime);

        prepare_initial_transport_authority(&sender, &mut transport, Some(persisted), None)
            .unwrap();

        assert_eq!(
            transport.loop_range(),
            Some(TransportLoopRange::new(0, 100).unwrap())
        );
        assert!(!transport.loop_enabled());
    }

    #[test]
    fn persisted_loop_below_the_native_block_contract_fails_closed() {
        let short = String::from_utf8(LIVE_LOOPED_ARRANGEMENT.to_vec())
            .unwrap()
            .replace("\"end_tick\":4", "\"end_tick\":1");
        let input = live_session_input_with_arrangement(short.as_bytes());
        let persisted = input.arrangement().loop_range().unwrap();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (sender, _mixer, mut transport) = RoutedTransportRuntime::with_command_queues(runtime);

        assert!(matches!(
            prepare_initial_transport_authority(&sender, &mut transport, Some(persisted), None),
            Err(DesktopPlaybackSessionBuildError::PersistedLoopCommand(
                TransportCommandSendError::LoopRangeShorterThanBlock {
                    block_frames: 64,
                    ..
                }
            ))
        ));
        assert_eq!(transport.loop_range(), None);
        assert!(!transport.loop_enabled());
    }

    #[test]
    fn desktop_mapping_sink_binds_numeric_targets_and_both_command_domains() {
        let input = live_session_input();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (transport, mixer, _runtime) = RoutedTransportRuntime::with_command_queues(runtime);
        let (_publisher, recovery) = playback_recovery_channel(None, None);
        let mut sink = DesktopControlSink::new(transport, mixer, recovery);

        assert!(
            sink.validate_target(hyper_control_runtime::RuntimeControlTarget::Channel {
                channel_index: 0,
                parameter: hyper_control_mappings::ChannelParameter::Gain,
            })
            .is_ok()
        );
        assert!(matches!(
            sink.validate_target(hyper_control_runtime::RuntimeControlTarget::Channel {
                channel_index: 1,
                parameter: hyper_control_mappings::ChannelParameter::Gain,
            }),
            Err(DesktopControlSinkError::Resolve(_))
        ));
        assert_eq!(
            sink.target_availability(hyper_control_runtime::RuntimeControlTarget::Scene {
                scene_index: 0,
            }),
            hyper_control_runtime::ControlBindingAvailability::Unavailable(
                hyper_control_runtime::ControlBindingUnavailableReason::SceneAudioNotPrepared,
            )
        );
        sink.try_send(hyper_control_runtime::RuntimeControlCommand::ChannelGain {
            channel_index: 0,
            gain_millionths: 500_000,
        })
        .unwrap();
        sink.try_send(hyper_control_runtime::RuntimeControlCommand::TransportStart)
            .unwrap();
    }

    #[test]
    fn mapped_session_controls_coordinate_direct_and_mapped_commands() {
        let input = live_session_input();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (transport, mixer, _runtime) = RoutedTransportRuntime::with_command_queues(runtime);
        let (_publisher, recovery) = playback_recovery_channel(None, None);
        let mut controls = build_session_controls(
            transport,
            mixer,
            None,
            recovery,
            Some(compiled_channel_mute_mapping()),
        )
        .unwrap();

        assert_eq!(controls.transport().scene_count(), 0);
        assert_eq!(controls.transport().scene_availability(0), None);
        assert_eq!(controls.mixer().channel_count(), 1);
        assert!(controls.resolve_channel_index(0).is_ok());
        assert_eq!(
            controls.resolve_channel_index(1),
            Err(MixerParameterResolveError::ChannelNotFound)
        );
        let channel = controls
            .resolve_channel("018f47a2-6b2d-7c11-8a44-001122337502")
            .unwrap();
        controls
            .try_send_mixer(MixerParameterCommand::ChannelMute {
                channel,
                muted: true,
            })
            .unwrap();
        assert_eq!(controls.try_start_capture_transport().unwrap(), 1);
        assert_eq!(controls.try_send_transport_fence().unwrap(), Some(1));
        assert_eq!(controls.try_send_mixer_fence().unwrap(), Some(1));
        assert_eq!(controls.committed_mixer_fence(), 0);
        assert_eq!(controls.mapping_statuses().len(), 1);
        assert_eq!(
            controls
                .dispatch_mapping_event(
                    hyper_control_runtime::ControlEvent::Clock { edge: Edge::Rising },
                    hyper_control_runtime::ControlContext::default(),
                )
                .unwrap(),
            hyper_control_runtime::ControlDispatchOutcome::Sent {
                binding_index: 0,
                command: hyper_control_runtime::RuntimeControlCommand::ChannelMute {
                    channel_index: 0,
                    muted: false,
                },
            }
        );
    }

    #[test]
    fn native_device_fence_is_explicit_and_requires_native_authority() {
        let input = live_session_input();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime =
            RoutedMainRuntime::try_new_prepared_playback_with_devices(&prepared, mixer, &[], 64)
                .unwrap();
        let (transport, mixer, native_devices, _runtime) =
            RoutedTransportRuntime::with_command_queues_and_native_device_commands(runtime);
        let (_publisher, recovery) = playback_recovery_channel(None, None);
        let mut controls =
            build_session_controls(transport, mixer, Some(native_devices), recovery, None).unwrap();
        assert_eq!(controls.try_send_native_device_fence().unwrap(), Some(1));

        let input = live_session_input();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (transport, mixer, _runtime) = RoutedTransportRuntime::with_command_queues(runtime);
        let (_publisher, recovery) = playback_recovery_channel(None, None);
        let mut controls = build_session_controls(transport, mixer, None, recovery, None).unwrap();
        assert!(matches!(
            controls.try_send_native_device_fence(),
            Err(DesktopDirectControlError::NativeDeviceUnavailable)
        ));
    }

    #[test]
    fn mapped_desktop_session_delivers_momentary_activation_and_release() {
        let input = live_session_input();
        let (project, mixer, _, media) = input.parts();
        let facts = collect_media_asset_facts(media).unwrap();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime = RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, 64).unwrap();
        let (transport, mixer, _runtime) = RoutedTransportRuntime::with_command_queues(runtime);
        let (_publisher, recovery) = playback_recovery_channel(None, None);
        let mut controls = build_session_controls(
            transport,
            mixer,
            None,
            recovery,
            Some(compiled_channel_mute_mapping_with(
                TriggerBehavior::Momentary,
            )),
        )
        .unwrap();
        assert_eq!(controls.mapping_statuses().len(), 1);

        for (edge, muted) in [(Edge::Rising, true), (Edge::Falling, false)] {
            assert_eq!(
                controls
                    .dispatch_mapping_event(
                        hyper_control_runtime::ControlEvent::Clock { edge },
                        hyper_control_runtime::ControlContext::default(),
                    )
                    .unwrap(),
                hyper_control_runtime::ControlDispatchOutcome::Sent {
                    binding_index: 0,
                    command: hyper_control_runtime::RuntimeControlCommand::ChannelMute {
                        channel_index: 0,
                        muted,
                    },
                }
            );
        }
    }

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    fn wait_for_checkpoint(
        checkpoint: &PlaybackRecoveryCheckpoint,
        predicate: impl Fn(PlaybackRecoverySnapshot) -> bool,
    ) -> PlaybackRecoverySnapshot {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = checkpoint.snapshot();
            if predicate(snapshot) {
                return snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "playback checkpoint did not reach the expected state: {snapshot:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    #[ignore = "requires a live PipeWire daemon with the hyper-ci-sink null sink"]
    fn live_desktop_recovery_rebuilds_one_operable_session() {
        let targets = crate::discover_desktop_output_targets().unwrap();
        let target = targets
            .targets()
            .iter()
            .find(|target| target.display_name() == "Hyper CI Sink")
            .expect("CI null sink was not discovered");
        let config = DesktopOutputConfig::try_new(48_000, 64)
            .unwrap()
            .with_output_target(target)
            .unwrap();
        let mut session = DesktopPlaybackSession::start(config, live_session_input()).unwrap();
        let old_checkpoint = session.recovery_checkpoint();

        session
            .try_send_transport_command(TransportCommand::Locate {
                frame: 32,
                state: LocateState::Stopped,
            })
            .unwrap();
        let located = wait_for_checkpoint(&old_checkpoint, |snapshot| {
            snapshot.position_frame() == 32
                && snapshot.state() == PlaybackRecoveryTransportState::Stopped
                && snapshot.committed_cycle_count() > 0
        });

        let mut recovery = session.into_recovery().unwrap();
        let terminal_old = recovery.checkpoint();
        assert_eq!(terminal_old.position_frame(), 32);
        assert_eq!(
            terminal_old.state(),
            PlaybackRecoveryTransportState::Stopped
        );
        assert!(terminal_old.committed_cycle_count() >= located.committed_cycle_count());

        let mut replacement = recovery.restart().unwrap();
        assert!(matches!(
            recovery.restart(),
            Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted)
        ));
        let replacement_checkpoint = replacement.recovery_checkpoint();
        let restored = replacement_checkpoint.snapshot();
        assert_eq!(restored.position_frame(), 32);
        assert_eq!(restored.state(), PlaybackRecoveryTransportState::Stopped);
        assert!(
            restored.committed_cycle_count() >= terminal_old.committed_cycle_count(),
            "the rebuilt checkpoint must retain the prior publication sequence"
        );

        replacement
            .try_send_transport_command(TransportCommand::Start)
            .unwrap();
        let rolling = wait_for_checkpoint(&replacement_checkpoint, |snapshot| {
            snapshot.state() == PlaybackRecoveryTransportState::Rolling
                && snapshot.position_frame() > 32
        });
        assert!(rolling.committed_cycle_count() > terminal_old.committed_cycle_count());
        assert_eq!(old_checkpoint.snapshot(), terminal_old);
        replacement.stop().unwrap();
    }

    #[test]
    fn project_recovery_failure_precedes_loading_and_native_output_startup() {
        let not_a_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let config = DesktopOutputConfig::try_new(48_000, 256).unwrap();
        let error = DesktopPlaybackSession::open_project(config, &not_a_project, project_limits())
            .expect_err("a non-project path must fail before native output startup");

        assert!(matches!(error, DesktopProjectOpenError::Recovery(_)));
    }

    #[test]
    fn transaction_free_open_reaches_loading_without_creating_recovery_storage() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hyper-desktop-project-open-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();

        let config = DesktopOutputConfig::try_new(48_000, 256).unwrap();
        let error = DesktopPlaybackSession::open_project(config, &root, project_limits())
            .expect_err("an empty project must fail during loading");

        assert!(matches!(
            &error,
            DesktopProjectOpenError::Load { source, recovery }
                if matches!(source.as_ref(), ProjectPlaybackError::Storage(_))
                    && recovery.is_empty()
        ));
        assert!(!root.join("backups").exists());
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn project_open_error_preserves_start_failure_source() {
        let error = DesktopProjectOpenError::Start {
            source: Box::new(DesktopPlaybackSessionStartError::Build(
                DesktopPlaybackSessionBuildError::SampleRateMismatch {
                    project: 44_100,
                    output: 48_000,
                },
            )),
            recovery: ProjectTransactionRecoveryReport::default(),
        };

        assert_eq!(
            error.to_string(),
            "desktop playback startup failed: project sample rate is 44100 Hz; output is configured for 48000 Hz"
        );
        assert!(matches!(
            error.source(),
            Some(source) if source.to_string()
                == "project sample rate is 44100 Hz; output is configured for 48000 Hz"
        ));
        assert!(error.recovery_report().is_empty());
    }

    #[test]
    fn scratch_planes_have_the_exact_session_shape() {
        let planes = allocate_scratch_planes(3, 128).unwrap();

        assert_eq!(planes.len(), 3);
        assert!(planes.iter().all(|plane| plane == &[0.0; 128]));
    }

    #[test]
    fn build_failure_is_published_before_backend_failure() {
        let (sender, receiver) = sync_channel(1);
        let result = fail_build::<()>(
            &sender,
            DesktopPlaybackSessionBuildError::SampleRateMismatch {
                project: 44_100,
                output: 48_000,
            },
        );

        assert!(matches!(
            result,
            Err(NativeDesktopOutputStartError::Backend { detail, .. })
                if detail == "project sample rate is 44100 Hz; output is configured for 48000 Hz"
        ));
        assert!(matches!(
            failed_build(&receiver),
            Some(DesktopPlaybackSessionBuildError::SampleRateMismatch {
                project: 44_100,
                output: 48_000,
            })
        ));
    }

    #[test]
    fn cancellation_pending_precedes_a_published_build_failure() {
        let (publication_sender, publication_receiver) = sync_channel(1);
        publication_sender
            .send(SessionPublication::Failed(
                DesktopPlaybackSessionBuildError::SampleRateMismatch {
                    project: 44_100,
                    output: 48_000,
                },
            ))
            .unwrap();
        let (shutdown, release) = pending_shutdown_fixture();

        let error = classify_output_start_failure(
            DesktopOutputStartError::CancellationPending {
                shutdown: Box::new(shutdown),
            },
            &publication_receiver,
        );
        let DesktopPlaybackSessionStartError::Output(
            DesktopOutputStartError::CancellationPending { mut shutdown },
        ) = error
        else {
            panic!("pending native ownership must take precedence")
        };
        release.send(()).unwrap();
        assert_eq!(
            shutdown.wait(Duration::from_secs(1)),
            PendingNativeShutdownWait::Completed
        );
    }

    #[test]
    fn pending_restart_moves_and_returns_the_only_retry_authority() {
        let pinned = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("opaque-route")
            .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let mut original = DesktopPlaybackRecovery {
            config: pinned.clone(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release) = pending_shutdown_fixture();

        let error = original.failed_restart(
            pinned,
            None,
            source,
            DesktopPlaybackSessionStartError::Output(
                DesktopOutputStartError::CancellationPending {
                    shutdown: Box::new(shutdown),
                },
            ),
        );
        assert!(matches!(
            original.restart(),
            Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted)
        ));
        let pending = error
            .into_pending()
            .expect("pending cancellation must retain retry authority");
        let pending = match pending.wait(Duration::from_millis(1)) {
            DesktopPlaybackRecoveryPendingWait::TimedOut(pending) => pending,
            other => panic!("unreleased native thread cannot be terminal: {other:?}"),
        };

        release.send(()).unwrap();
        let recovered = match pending.wait(Duration::from_secs(1)) {
            DesktopPlaybackRecoveryPendingWait::Completed(recovery) => recovery,
            other => panic!("released native thread must return authority: {other:?}"),
        };
        assert!(recovered.source.is_some());
        assert!(matches!(
            original.restart(),
            Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted)
        ));
    }

    #[test]
    fn pending_monitored_restart_retains_a_joined_input_failure() {
        let pinned = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("opaque-route")
            .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let monitoring = DesktopMonitoringConfig::try_new(
            super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            pinned.clone(),
            1_024,
            256,
            64,
        )
        .unwrap();
        let mut original = DesktopPlaybackRecovery {
            config: pinned.clone(),
            monitoring: Some(PlaybackMonitoringStartup::new(monitoring, 1.0, false)),
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release) = pending_shutdown_fixture();

        let error = original.failed_restart(
            pinned,
            original.monitoring.clone(),
            source,
            DesktopPlaybackSessionStartError::Monitored {
                source: Box::new(DesktopPlaybackSessionStartError::Output(
                    DesktopOutputStartError::CancellationPending {
                        shutdown: Box::new(shutdown),
                    },
                )),
                input_cleanup: Some(DesktopInputStopError::RuntimeFailed),
            },
        );
        let pending = error
            .into_pending()
            .expect("output cancellation must retain retry authority");
        assert!(pending.status().is_some());
        assert!(pending.input_status().is_none());
        assert!(pending.output_status().is_some());
        release.send(()).unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingWait::Failed(_)
        ));
    }

    #[test]
    fn pending_restart_carries_observed_failure_across_a_later_timeout() {
        let pinned = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("opaque-route")
            .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let monitoring = DesktopMonitoringConfig::try_new(
            super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            pinned.clone(),
            1_024,
            256,
            64,
        )
        .unwrap();
        let mut original = DesktopPlaybackRecovery {
            config: pinned.clone(),
            monitoring: Some(PlaybackMonitoringStartup::new(
                monitoring.clone(),
                1.0,
                false,
            )),
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let input_status = OutputStreamStatus::default();
        input_status.mark_failed();
        let (input_shutdown, release_input) = pending_shutdown_fixture_with_status(input_status);
        let (output_shutdown, release_output) = pending_shutdown_fixture();

        let error = original.failed_restart(
            pinned,
            Some(PlaybackMonitoringStartup::new(monitoring, 1.0, false)),
            source,
            DesktopPlaybackSessionStartError::Monitored {
                source: Box::new(DesktopPlaybackSessionStartError::Output(
                    DesktopOutputStartError::CancellationPending {
                        shutdown: Box::new(output_shutdown),
                    },
                )),
                input_cleanup: Some(DesktopInputStopError::ShutdownPending {
                    shutdown: Box::new(input_shutdown),
                }),
            },
        );
        let pending = error.into_pending().unwrap();
        assert_eq!(
            pending.input_status().unwrap().state(),
            crate::StreamState::Failed
        );
        assert_eq!(
            pending.output_status().unwrap().state(),
            crate::StreamState::Starting
        );
        release_input.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingWait::TimedOut(pending) =
            pending.wait(Duration::from_millis(50))
        else {
            panic!("the unreleased output must keep restart pending");
        };

        release_output.send(()).unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingWait::Failed(_)
        ));
    }

    #[test]
    fn joined_restart_failure_restores_retry_authority_in_place() {
        let pinned = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("opaque-route")
            .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let mut recovery = DesktopPlaybackRecovery {
            config: pinned.clone(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };

        let error = recovery.failed_restart(
            pinned,
            None,
            source,
            DesktopPlaybackSessionStartError::PublicationUnavailable,
        );

        assert!(matches!(
            error,
            DesktopPlaybackRecoveryStartError::Start(
                DesktopPlaybackSessionStartError::PublicationUnavailable
            )
        ));
        assert!(recovery.source.is_some());
    }

    #[test]
    fn incompatible_monitoring_rebind_preserves_retry_authority() {
        let original_output = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("original-output")
            .unwrap();
        let monitoring = DesktopMonitoringConfig::try_new(
            super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            original_output.clone(),
            1_024,
            256,
            64,
        )
        .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let mut recovery = DesktopPlaybackRecovery {
            config: original_output.clone(),
            monitoring: Some(PlaybackMonitoringStartup::new(
                monitoring.clone(),
                1.0,
                false,
            )),
            source: Some(source),
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let incompatible = DesktopOutputConfig::try_new(44_100, 256)
            .unwrap()
            .with_routing_id("incompatible-output")
            .unwrap();

        assert!(matches!(
            recovery.restart_with_output(incompatible),
            Err(DesktopPlaybackRecoveryStartError::MonitoringConfig(
                DesktopMonitoringConfigError::SampleRateMismatch {
                    input_hz: 48_000,
                    output_hz: 44_100,
                }
            ))
        ));
        assert!(recovery.source.is_some());
        assert_eq!(recovery.config, original_output);
        assert_eq!(recovery.monitoring_config(), Some(&monitoring));
    }

    #[test]
    fn desktop_start_error_preserves_native_backend_source() {
        let error = DesktopOutputStartError::from(callback_error("fixture rejected".to_owned()));

        assert!(matches!(error, DesktopOutputStartError::Backend { .. }));
        assert_eq!(
            error.source().map(ToString::to_string),
            Some(
                if cfg!(target_os = "windows") {
                    "WASAPI callback failed: fixture rejected"
                } else {
                    "PipeWire callback failed: fixture rejected"
                }
                .to_owned()
            )
        );
    }

    #[test]
    fn recovery_rejects_policy_defaults_and_accepts_explicit_outputs() {
        let policy_default = DesktopOutputConfig::try_new(48_000, 256).unwrap();
        assert!(!output_is_pinned(&policy_default));

        let pinned = policy_default.with_routing_id("opaque-route").unwrap();
        assert!(output_is_pinned(&pinned));

        let mut spent = DesktopPlaybackRecovery {
            config: pinned,
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        assert!(matches!(
            spent.restart(),
            Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted)
        ));
    }

    #[test]
    fn shutdown_timeout_retains_restart_authority_until_join() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release_sender) = pending_shutdown_fixture();
        let error = DesktopPlaybackRecoveryStopError::from_stop(
            DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(shutdown),
            },
            recovery,
        );

        assert!(error.is_shutdown_pending());
        assert!(error.recovery().is_none());
        let pending = error.into_pending().unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::TimedOut(pending) =
            pending.wait(Duration::from_millis(1))
        else {
            panic!("the fixture must remain pending before release");
        };
        release_sender.send(()).unwrap();
        assert!(matches!(
            pending.wait(Duration::MAX),
            DesktopPlaybackRecoveryPendingStopWait::Completed(_)
        ));
    }

    #[test]
    fn recording_outcome_cannot_release_pending_playback_shutdown() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release) = pending_shutdown_fixture();
        let recovery = DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(
            DesktopPlaybackRecoveryStopError::from_stop(
                DesktopOutputStopError::ShutdownPending {
                    shutdown: Box::new(shutdown),
                },
                recovery,
            ),
        ));

        assert!(recovery.recovery().is_none());
        let error = recovery.into_recovery().unwrap_err();
        assert!(error.is_shutdown_pending());
        release.send(()).unwrap();
        let pending = error.into_pending().unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingStopWait::Completed(_)
        ));
    }

    #[test]
    fn recording_recovery_reports_only_terminal_output_shutdown_failure() {
        fn recovery() -> DesktopPlaybackRecovery {
            DesktopPlaybackRecovery {
                config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                monitoring: None,
                source: None,
                checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
            }
        }

        assert!(!DesktopPlaybackRecordingRecovery::Ready(recovery()).output_shutdown_failed());
        assert!(
            DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(
                DesktopPlaybackRecoveryStopError {
                    input: None,
                    output: Some(DesktopOutputStopError::RuntimeFailed),
                    recovery: Box::new(recovery()),
                }
            ))
            .output_shutdown_failed()
        );
        assert!(
            DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(
                DesktopPlaybackRecoveryStopError {
                    input: None,
                    output: Some(DesktopOutputStopError::ThreadPanicked),
                    recovery: Box::new(recovery()),
                }
            ))
            .output_shutdown_failed()
        );
        assert!(
            !DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(
                DesktopPlaybackRecoveryStopError {
                    input: Some(DesktopInputStopError::RuntimeFailed),
                    output: None,
                    recovery: Box::new(recovery()),
                }
            ))
            .output_shutdown_failed()
        );

        let (shutdown, release) = pending_shutdown_fixture();
        let pending = DesktopPlaybackRecordingRecovery::NativeShutdown(Box::new(
            DesktopPlaybackRecoveryStopError {
                input: None,
                output: Some(DesktopOutputStopError::ShutdownPending {
                    shutdown: Box::new(shutdown),
                }),
                recovery: Box::new(recovery()),
            },
        ));
        assert!(!pending.output_shutdown_failed());
        release.send(()).unwrap();
        let pending = pending.into_recovery().unwrap_err().into_pending().unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingStopWait::Completed(_)
        ));
    }

    #[test]
    fn pending_recovery_preserves_stream_failures_across_timeout() {
        let input_status = OutputStreamStatus::default();
        let (failed_input, release_input) =
            pending_shutdown_fixture_with_status(input_status.clone());
        let output_status = OutputStreamStatus::default();
        let (failed_output, release_output) =
            pending_shutdown_fixture_with_status(output_status.clone());
        let pending = DesktopPlaybackRecoveryStopError {
            input: Some(DesktopInputStopError::ShutdownPending {
                shutdown: Box::new(failed_input),
            }),
            output: Some(DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(failed_output),
            }),
            recovery: Box::new(DesktopPlaybackRecovery {
                config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                monitoring: None,
                source: None,
                checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
            }),
        }
        .into_pending()
        .unwrap();
        input_status.mark_failed();
        release_input.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::TimedOut(pending) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("pending output must retain joined input failure across a timeout")
        };
        assert!(pending.input_status().is_none());
        assert!(pending.output_status().is_some());

        output_status.mark_failed();
        release_output.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::Failed(failure) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("both per-stream failures must survive the resumed wait")
        };
        assert!(failure.input_shutdown_failed());
        assert!(failure.output_shutdown_failed());
        assert!(!failure.panicked());
    }

    #[test]
    fn pending_recovery_reports_stream_specific_terminal_evidence() {
        fn recovery() -> DesktopPlaybackRecovery {
            DesktopPlaybackRecovery {
                config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
                monitoring: None,
                source: None,
                checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
            }
        }

        let (clean_output, release) = pending_shutdown_fixture();
        let pending = DesktopPlaybackRecoveryStopError {
            input: Some(DesktopInputStopError::RuntimeFailed),
            output: Some(DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(clean_output),
            }),
            recovery: Box::new(recovery()),
        }
        .into_pending()
        .unwrap();
        release.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::Failed(failure) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("joined input failure must remain terminal after output settles")
        };
        assert!(failure.input_shutdown_failed());
        assert!(!failure.output_shutdown_failed());
        assert!(!failure.panicked());

        let output_status = OutputStreamStatus::default();
        let (failed_output, release) = pending_shutdown_fixture_with_status(output_status.clone());
        let pending = DesktopPlaybackRecoveryStopError {
            input: None,
            output: Some(DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(failed_output),
            }),
            recovery: Box::new(recovery()),
        }
        .into_pending()
        .unwrap();
        output_status.mark_failed();
        release.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::Failed(failure) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("failed pending output must retain stream-specific evidence")
        };
        assert!(!failure.input_shutdown_failed());
        assert!(failure.output_shutdown_failed());
        assert!(!failure.panicked());

        let (panicked_output, release) = pending_shutdown_panic_fixture();
        let pending = DesktopPlaybackRecoveryStopError {
            input: None,
            output: Some(DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(panicked_output),
            }),
            recovery: Box::new(recovery()),
        }
        .into_pending()
        .unwrap();
        release.send(()).unwrap();
        let DesktopPlaybackRecoveryPendingStopWait::Panicked(failure) =
            pending.wait(Duration::from_secs(1))
        else {
            panic!("panicked pending output must retain stream-specific evidence")
        };
        assert!(!failure.input_shutdown_failed());
        assert!(failure.output_shutdown_failed());
        assert!(failure.panicked());
    }

    #[test]
    fn recording_finalize_failure_returns_its_restart_authority() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let error = DesktopPlaybackRecordingFinalizeError::Commit(Box::new(
            DesktopPlaybackRecordingCommitError::Completion {
                source: Box::new(DesktopRecordingSessionCommitError::UnexpectedAbort {
                    frames: 0,
                    journal_path: std::path::PathBuf::from("take.journal"),
                }),
                recovery: Box::new(DesktopPlaybackRecordingRecovery::Ready(recovery)),
            },
        ));

        assert!(error.recovery().is_some());
        assert!(error.into_recovery().into_recovery().is_ok());
    }

    #[test]
    fn pending_recovery_stop_retains_a_joined_input_failure() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release) = pending_shutdown_fixture();
        let error = DesktopPlaybackRecoveryStopError {
            input: Some(DesktopInputStopError::RuntimeFailed),
            output: Some(DesktopOutputStopError::ShutdownPending {
                shutdown: Box::new(shutdown),
            }),
            recovery: Box::new(recovery),
        };

        let pending = error.into_pending().unwrap();
        assert!(pending.output_status().is_some());
        release.send(()).unwrap();
        assert!(matches!(
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingStopWait::Failed(_)
        ));
    }

    #[test]
    fn joined_panic_retains_restart_authority() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let error = DesktopPlaybackRecoveryStopError::from_stop(
            DesktopOutputStopError::ThreadPanicked,
            recovery,
        );

        assert!(!error.is_shutdown_pending());
        assert!(error.recovery().is_some());
        assert!(error.into_recovery().is_ok());
    }

    #[test]
    fn joined_runtime_failure_retains_restart_authority() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
            monitoring: None,
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let error = DesktopPlaybackRecoveryStopError::from_stop(
            DesktopOutputStopError::RuntimeFailed,
            recovery,
        );

        assert!(!error.is_shutdown_pending());
        assert!(error.recovery().is_some());
        assert!(error.into_recovery().is_ok());
    }
}
