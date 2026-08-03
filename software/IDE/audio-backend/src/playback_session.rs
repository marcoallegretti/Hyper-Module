use super::monitoring_session::{MonitoringConsumer, MonitoringCounters, try_monitoring_transport};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::pipewire_output::PipeWireThreadRunner;
#[cfg(target_os = "windows")]
use super::wasapi_output::WasapiThreadRunner;
use super::{
    CheckpointedOutputProcessor, DesktopControlMappingRuntime, DesktopControlSink,
    DesktopControlSinkError, DesktopDirectControlCommand, DesktopDirectControlError,
    DesktopInputStartError, DesktopInputStopError, DesktopMonitoringConfig,
    DesktopMonitoringControl, DesktopOutputConfig, MonitoringSnapshot, OutputStreamCounters,
    OutputStreamStatus, PendingNativeShutdown, PendingNativeShutdownWait,
    PlaybackRecoveryCheckpoint, PlaybackRecoverySnapshot,
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
    LoudnessCommandSender, MixerChannelHandle, MixerParameterCommand, MixerParameterCommandSender,
    MixerParameterResolveError, MixerParameterStateRestoreError, MixerSendHandle,
    OutputMeterReceiver, RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError,
    TransportCommand, TransportCommandSender, TransportOutputCreationError,
    TransportOutputProcessor, loudness_command_channel, output_meter_channel,
    playback_recovery_channel,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, LoadedPlaybackProject, MediaAssetFact};
use hyper_playback_session::PreparedPlaybackSession;
use hyper_project_loader::{
    PreparedProjectPlayback, ProjectPlaybackError, ProjectPlaybackLimits, prepare_project_playback,
};
use hyper_storage_engine::{
    ProjectTransactionRecoveryError, ProjectTransactionRecoveryReport, recover_project_transactions,
};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

pub type DesktopPlaybackSessionInput = PreparedPlaybackSession;

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
    MixerRecovery(MixerParameterStateRestoreError),
    ControlMappingCopy(ControlMappingCompileError),
    ControlMapping(DesktopControlMappingBuildError),
    ScratchCapacityUnavailable,
    Output(TransportOutputCreationError),
    MonitoringScratchCapacityUnavailable,
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
            Self::MixerRecovery(source) => {
                write!(formatter, "mixer recovery failed: {source}")
            }
            Self::ControlMappingCopy(source) => {
                write!(formatter, "control mapping copy failed: {source}")
            }
            Self::ControlMapping(source) => {
                write!(formatter, "control mapping runtime failed: {source}")
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
        }
    }
}

impl Error for DesktopPlaybackSessionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Playback(source) => Some(source),
            Self::Media(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::MixerRecovery(source) => Some(source),
            Self::ControlMappingCopy(source) => Some(source),
            Self::ControlMapping(source) => Some(source),
            Self::Output(source) => Some(source),
            Self::SampleRateMismatch { .. }
            | Self::AssetFactCapacityUnavailable
            | Self::ScratchCapacityUnavailable
            | Self::MonitoringScratchCapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackSessionStartError {
    Build(DesktopPlaybackSessionBuildError),
    Output(DesktopOutputStartError),
    PublicationUnavailable,
    MonitoringCapacityUnavailable,
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
            Self::PublicationUnavailable => {
                formatter.write_str("playback controls were unavailable after output startup")
            }
            Self::MonitoringCapacityUnavailable => {
                formatter.write_str("playback monitoring capacity is unavailable")
            }
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
            Self::Input(source) => Some(source),
            Self::Monitored { source, .. } => Some(source.as_ref()),
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
    recovery: PlaybackRecoveryCheckpoint,
}

#[derive(Debug)]
enum SessionControlHandles {
    Direct {
        transport: TransportCommandSender,
        mixer: MixerParameterCommandSender,
    },
    Mapping(DesktopControlMappingRuntime),
}

impl SessionControlHandles {
    fn mapping_statuses(&self) -> Option<&[hyper_control_runtime::ControlBindingStatus]> {
        match self {
            Self::Mapping(runtime) => Some(runtime.statuses()),
            Self::Direct { .. } => None,
        }
    }

    fn dispatch_mapping_event(
        &mut self,
        event: hyper_control_runtime::ControlEvent,
        context: hyper_control_runtime::ControlContext,
    ) -> Option<
        Result<
            hyper_control_runtime::ControlDispatchOutcome,
            hyper_control_runtime::ControlDispatchError<DesktopControlSinkError>,
        >,
    > {
        match self {
            Self::Mapping(runtime) => Some(runtime.dispatch(event, context)),
            Self::Direct { .. } => None,
        }
    }

    fn try_send_transport(
        &mut self,
        command: TransportCommand,
    ) -> Result<(), DesktopDirectControlError> {
        match self {
            Self::Direct { transport, .. } => transport
                .try_send(command)
                .map_err(DesktopDirectControlError::Transport),
            Self::Mapping(runtime) => {
                runtime.try_send_direct(DesktopDirectControlCommand::Transport(command))
            }
        }
    }

    fn resolve_channel(
        &self,
        track_id: &str,
    ) -> Result<MixerChannelHandle, MixerParameterResolveError> {
        match self {
            Self::Direct { mixer, .. } => mixer.resolve_channel(track_id),
            Self::Mapping(runtime) => runtime.sink().resolve_channel(track_id),
        }
    }

    fn resolve_send(&self, send_id: &str) -> Result<MixerSendHandle, MixerParameterResolveError> {
        match self {
            Self::Direct { mixer, .. } => mixer.resolve_send(send_id),
            Self::Mapping(runtime) => runtime.sink().resolve_send(send_id),
        }
    }

    fn try_send_mixer(
        &mut self,
        command: MixerParameterCommand,
    ) -> Result<(), DesktopDirectControlError> {
        match self {
            Self::Direct { mixer, .. } => mixer
                .try_send(command)
                .map_err(DesktopDirectControlError::Mixer),
            Self::Mapping(runtime) => {
                runtime.try_send_direct(DesktopDirectControlCommand::Mixer(command))
            }
        }
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
    meters: OutputMeterReceiver,
    recovery: PlaybackRecoveryCheckpoint,
    config: DesktopOutputConfig,
    source: Arc<dyn DesktopPlaybackSource>,
    monitoring: Option<PlaybackMonitoringSession>,
    project_open_recovery: Option<ProjectTransactionRecoveryReport>,
}

#[derive(Debug)]
struct PlaybackMonitoringSession {
    input: NativeInputStream,
    config: DesktopMonitoringConfig,
    counters: MonitoringCounters,
    control: DesktopMonitoringControl,
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

struct MonitoredTransportOutputProcessor<'state, 'buffers, R> {
    playback: TransportOutputProcessor<'state, 'buffers, R>,
    monitoring: PlaybackMonitoringMixer,
}

impl<R: hyper_mixer_runtime::TrackWindowRenderer> CheckpointedOutputProcessor
    for MonitoredTransportOutputProcessor<'_, '_, R>
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
        } = self;
        playback.process_with_post_mix(output, |output| monitoring.mix(output))
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
    monitoring: Option<DesktopMonitoringConfig>,
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
        let failed = matches!(self.input, Some(DesktopInputStopError::RuntimeFailed))
            || matches!(self.output, Some(DesktopOutputStopError::RuntimeFailed));
        let panicked = matches!(self.input, Some(DesktopInputStopError::ThreadPanicked))
            || matches!(self.output, Some(DesktopOutputStopError::ThreadPanicked));
        let input = match self.input {
            Some(DesktopInputStopError::ShutdownPending { shutdown }) => Some(*shutdown),
            Some(DesktopInputStopError::RuntimeFailed | DesktopInputStopError::ThreadPanicked)
            | None => None,
        };
        let output = match self.output {
            Some(DesktopOutputStopError::ShutdownPending { shutdown }) => Some(*shutdown),
            Some(
                DesktopOutputStopError::RuntimeFailed | DesktopOutputStopError::ThreadPanicked,
            )
            | None => None,
        };
        Ok(DesktopPlaybackRecoveryPendingStop {
            input,
            output,
            failed,
            panicked,
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

/// Sole restart authority while one or both native streams remain pending.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStop {
    input: Option<PendingNativeShutdown>,
    output: Option<PendingNativeShutdown>,
    failed: bool,
    panicked: bool,
    recovery: DesktopPlaybackRecovery,
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryPendingStopWait {
    Completed(DesktopPlaybackRecovery),
    Failed(DesktopPlaybackRecovery),
    Panicked(DesktopPlaybackRecovery),
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
        let deadline = std::time::Instant::now() + timeout;
        let mut failed = self.failed;
        let mut panicked = self.panicked;
        for input in [true, false] {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let result = if input {
                wait_pending_shutdown(&mut self.input, remaining)
            } else {
                wait_pending_shutdown(&mut self.output, remaining)
            };
            match result {
                None | Some(PendingNativeShutdownWait::Completed) => {}
                Some(PendingNativeShutdownWait::Failed) => {
                    failed = true;
                }
                Some(PendingNativeShutdownWait::Panicked) => {
                    panicked = true;
                }
                Some(PendingNativeShutdownWait::TimedOut) => {
                    self.failed = failed;
                    self.panicked = panicked;
                    return DesktopPlaybackRecoveryPendingStopWait::TimedOut(self);
                }
            }
        }
        if panicked {
            DesktopPlaybackRecoveryPendingStopWait::Panicked(self.recovery)
        } else if failed {
            DesktopPlaybackRecoveryPendingStopWait::Failed(self.recovery)
        } else {
            DesktopPlaybackRecoveryPendingStopWait::Completed(self.recovery)
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
        let deadline = std::time::Instant::now() + timeout;
        let mut failed = self.failed;
        let mut panicked = self.panicked;
        for input in [true, false] {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
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
    pub const fn monitoring_config(&self) -> Option<&DesktopMonitoringConfig> {
        self.monitoring.as_ref()
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
        monitoring: Option<DesktopMonitoringConfig>,
        source: Arc<dyn DesktopPlaybackSource>,
        error: DesktopPlaybackSessionStartError,
    ) -> DesktopPlaybackRecoveryStartError {
        match error {
            DesktopPlaybackSessionStartError::Output(
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
                    ) => Some(*shutdown),
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
        let (project, recovery) = recover_and_prepare_project(root.as_ref(), limits)?;
        match Self::start_project_monitored(monitoring, project) {
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
        Self::start_source(
            monitoring.output().clone(),
            Arc::new(project),
            None,
            Some(monitoring),
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
        Self::start_source(
            monitoring.output().clone(),
            Arc::new(input),
            None,
            Some(monitoring),
        )
    }

    fn start_source(
        config: DesktopOutputConfig,
        source: Arc<dyn DesktopPlaybackSource>,
        initial_checkpoint: Option<Box<PlaybackRecoverySnapshot>>,
        monitoring: Option<DesktopMonitoringConfig>,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        let Some(monitoring_config) = monitoring else {
            return Self::start_output_source(config, source, initial_checkpoint, None);
        };
        let (mut producer, consumer, counters, control) =
            try_monitoring_transport(&monitoring_config)
                .map_err(|_| DesktopPlaybackSessionStartError::MonitoringCapacityUnavailable)?;
        let input = NativeInputStream::start(
            NativeInputConfig::from(monitoring_config.input()),
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
                    config: monitoring_config,
                    counters,
                    control,
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
                    meters: handles.meters,
                    recovery: handles.recovery,
                    config,
                    source,
                    monitoring: None,
                    project_open_recovery: None,
                }),
                Ok(SessionPublication::Failed(source)) => {
                    Err(DesktopPlaybackSessionStartError::Build(source))
                }
                Err(_) => Err(DesktopPlaybackSessionStartError::PublicationUnavailable),
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

    /// Returns immutable availability evidence for every enabled compiled
    /// control binding, or `None` when the project has no mapping table.
    #[must_use]
    pub fn control_mapping_statuses(
        &self,
    ) -> Option<&[hyper_control_runtime::ControlBindingStatus]> {
        self.controls.mapping_statuses()
    }

    /// Dispatches one software-supplied source event through this session's
    /// exclusive mapping owner.
    ///
    /// `None` means the project has no compiled mapping table. This method does
    /// not imply physical input ingestion or select an IDE context implicitly.
    pub fn dispatch_control_event(
        &mut self,
        event: hyper_control_runtime::ControlEvent,
        context: hyper_control_runtime::ControlContext,
    ) -> Option<
        Result<
            hyper_control_runtime::ControlDispatchOutcome,
            hyper_control_runtime::ControlDispatchError<DesktopControlSinkError>,
        >,
    > {
        self.controls.dispatch_mapping_event(event, context)
    }

    #[must_use]
    pub fn loudness_command_sender(&mut self) -> &mut LoudnessCommandSender {
        &mut self.loudness_commands
    }

    #[must_use]
    pub fn meter_receiver(&mut self) -> &mut OutputMeterReceiver {
        &mut self.meters
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
            config,
            source,
            monitoring,
            project_open_recovery: _,
        } = self;
        let monitoring_config = monitoring
            .as_ref()
            .map(|monitoring| monitoring.config.clone());
        let input = monitoring
            .map(|monitoring| monitoring.input.stop().map_err(DesktopInputStopError::from))
            .transpose()
            .err();
        let output = output.stop().err().map(DesktopOutputStopError::from);
        let recovery = DesktopPlaybackRecovery {
            config,
            monitoring: monitoring_config,
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
            config: _,
            source: _,
            monitoring,
            project_open_recovery: _,
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

struct SessionInitialization<'a> {
    input: &'a DesktopPlaybackSessionInput,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    initial_checkpoint: Option<&'a PlaybackRecoverySnapshot>,
    control_mapping: Option<CompiledControlMappingTable>,
    monitoring: Option<MonitoringConsumer>,
}

fn initialize_session(
    runner: DesktopThreadRunner,
    initialization: SessionInitialization<'_>,
    publication: &SyncSender<SessionPublication>,
) -> Result<(), NativeDesktopOutputStartError> {
    let SessionInitialization {
        input,
        sample_rate_hz,
        maximum_block_frames,
        initial_checkpoint,
        control_mapping,
        monitoring,
    } = initialization;
    let (project, mixer, automation, media) = input.parts();
    validate_session_sample_rate(project, sample_rate_hz)
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
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated(
            &prepared,
            mixer,
            automation.clone(),
            maximum_block_frames,
        ),
        None => {
            RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, maximum_block_frames)
        }
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

    let (commands, mixer_parameter_commands, mut transport) =
        RoutedTransportRuntime::with_command_queues(runtime);
    restore_transport_checkpoint(&mut transport, initial_checkpoint)
        .map_err(DesktopPlaybackSessionBuildError::MixerRecovery)
        .or_else(|source| fail_build(publication, source))?;
    let (mut meter_sender, meters) = output_meter_channel();
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
    let (publisher, recovery) = playback_recovery_channel(initial_checkpoint);
    let controls = build_session_controls(
        commands,
        mixer_parameter_commands,
        recovery.clone(),
        control_mapping,
    )
    .or_else(|source| fail_build(publication, source))?;
    publish_session_handles(
        publication,
        controls,
        loudness_commands,
        meters,
        recovery.clone(),
    )?;
    if let Some(monitoring) = monitoring {
        runner.run_transport_output_with_checkpoint(
            MonitoredTransportOutputProcessor {
                playback: processor,
                monitoring,
            },
            publisher,
        )
    } else {
        runner.run_transport_output_with_checkpoint(processor, publisher)
    }
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
    controls: SessionControlHandles,
    loudness_commands: LoudnessCommandSender,
    meters: OutputMeterReceiver,
    recovery: PlaybackRecoveryCheckpoint,
) -> Result<(), NativeDesktopOutputStartError> {
    publication
        .try_send(SessionPublication::Prepared(SessionHandles {
            controls,
            loudness_commands,
            meters,
            recovery,
        }))
        .map_err(|_| callback_error("playback control publication failed".to_owned()))
}

fn restore_transport_checkpoint<R: hyper_mixer_runtime::TrackWindowRenderer>(
    transport: &mut RoutedTransportRuntime<R>,
    checkpoint: Option<&PlaybackRecoverySnapshot>,
) -> Result<(), MixerParameterStateRestoreError> {
    let Some(checkpoint) = checkpoint else {
        return Ok(());
    };
    transport.apply_command(checkpoint.restore_command());
    if let Some(mixer_parameter_state) = checkpoint.mixer_parameter_state() {
        transport.restore_mixer_parameter_state(&mixer_parameter_state)?;
    }
    Ok(())
}

fn build_session_controls(
    transport: TransportCommandSender,
    mixer: MixerParameterCommandSender,
    recovery: PlaybackRecoveryCheckpoint,
    control_mapping: Option<CompiledControlMappingTable>,
) -> Result<SessionControlHandles, DesktopPlaybackSessionBuildError> {
    let Some(table) = control_mapping else {
        return Ok(SessionControlHandles::Direct { transport, mixer });
    };
    let sink = DesktopControlSink::new(transport, mixer, recovery);
    ControlRuntime::try_new(table, sink)
        .map(SessionControlHandles::Mapping)
        .map_err(classify_control_mapping_build_error)
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
    use crate::DesktopMonitoringConfigError;
    use crate::monitoring_session::realtime_allocator::HeapMeasurement;
    use crate::{BoundedBackendThread, acquire_backend_thread_permit};
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

    fn monitoring_transport() -> (
        super::super::monitoring_session::MonitoringProducer,
        MonitoringConsumer,
        MonitoringCounters,
    ) {
        let config = DesktopMonitoringConfig::try_new(
            super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            64,
        )
        .unwrap();
        let (producer, consumer, counters, _) = try_monitoring_transport(&config).unwrap();
        (producer, consumer, counters)
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
        assemble_playback_session(PlaybackSessionAssemblyInput {
            documents: PlaybackDocumentBytes {
                project_id: LIVE_PROJECT_ID.to_owned(),
                sample_rate_hz: 48_000,
                timeline: LIVE_TIMELINE.to_vec(),
                tracks: vec![LIVE_TRACK.to_vec()],
                clips: vec![LIVE_CLIP.to_vec()],
                scenes: Vec::new(),
                arrangement: Some(LIVE_ARRANGEMENT.to_vec()),
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
        let (_publisher, recovery) = playback_recovery_channel(None);
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
        let (_publisher, recovery) = playback_recovery_channel(None);
        let mut controls = build_session_controls(
            transport,
            mixer,
            recovery,
            Some(compiled_channel_mute_mapping()),
        )
        .unwrap();

        let channel = controls
            .resolve_channel("018f47a2-6b2d-7c11-8a44-001122337502")
            .unwrap();
        controls
            .try_send_mixer(MixerParameterCommand::ChannelMute {
                channel,
                muted: true,
            })
            .unwrap();
        controls
            .try_send_transport(TransportCommand::Start)
            .unwrap();
        assert_eq!(controls.mapping_statuses().unwrap().len(), 1);
        assert_eq!(
            controls
                .dispatch_mapping_event(
                    hyper_control_runtime::ControlEvent::Clock { edge: Edge::Rising },
                    hyper_control_runtime::ControlContext::default(),
                )
                .unwrap()
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
        let (_publisher, recovery) = playback_recovery_channel(None);
        let mut controls = build_session_controls(
            transport,
            mixer,
            recovery,
            Some(compiled_channel_mute_mapping_with(
                TriggerBehavior::Momentary,
            )),
        )
        .unwrap();
        assert_eq!(controls.mapping_statuses().unwrap().len(), 1);

        for (edge, muted) in [(Edge::Rising, true), (Edge::Falling, false)] {
            assert_eq!(
                controls
                    .dispatch_mapping_event(
                        hyper_control_runtime::ControlEvent::Clock { edge },
                        hyper_control_runtime::ControlContext::default(),
                    )
                    .unwrap()
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
            monitoring: Some(monitoring),
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
            monitoring: Some(monitoring.clone()),
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let input_status = OutputStreamStatus::default();
        input_status.mark_failed();
        let (input_shutdown, release_input) = pending_shutdown_fixture_with_status(input_status);
        let (output_shutdown, release_output) = pending_shutdown_fixture();

        let error = original.failed_restart(
            pinned,
            Some(monitoring),
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
            monitoring: Some(monitoring.clone()),
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
            pending.wait(Duration::from_secs(1)),
            DesktopPlaybackRecoveryPendingStopWait::Completed(_)
        ));
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
