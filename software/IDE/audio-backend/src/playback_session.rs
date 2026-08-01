#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::pipewire_output::PipeWireThreadRunner;
#[cfg(target_os = "windows")]
use super::wasapi_output::WasapiThreadRunner;
use super::{
    DesktopOutputConfig, OutputStreamCounters, OutputStreamStatus, PendingNativeShutdown,
    PendingNativeShutdownWait, PlaybackRecoveryCheckpoint, PlaybackRecoverySnapshot,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{PipeWireOutputStream, PipeWireStartError, PipeWireStartupStage, PipeWireStopError};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiOutputStream, WasapiStartError, WasapiStartupStage,
    WasapiStopError,
};
use core::error::Error;
use core::fmt;
use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_media_cache::PreparePlaybackError;
use hyper_mixer_runtime::{
    LoudnessCommandSender, MixerParameterCommandSender, MixerParameterStateRestoreError,
    OutputMeterReceiver, RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError,
    TransportCommandSender, TransportOutputCreationError, TransportOutputProcessor,
    loudness_command_channel, output_meter_channel, playback_recovery_channel,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, MediaAssetFact};
use hyper_playback_session::PreparedPlaybackSession;
use hyper_project_loader::{
    PreparedProjectPlayback, ProjectPlaybackError, ProjectPlaybackLimits, prepare_project_playback,
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
    ScratchCapacityUnavailable,
    Output(TransportOutputCreationError),
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
            Self::ScratchCapacityUnavailable => {
                formatter.write_str("render scratch capacity is unavailable")
            }
            Self::Output(source) => {
                write!(formatter, "transport output construction failed: {source}")
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
            Self::Output(source) => Some(source),
            Self::SampleRateMismatch { .. }
            | Self::AssetFactCapacityUnavailable
            | Self::ScratchCapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackSessionStartError {
    Build(DesktopPlaybackSessionBuildError),
    Output(DesktopOutputStartError),
    PublicationUnavailable,
}

impl fmt::Display for DesktopPlaybackSessionStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build(source) => source.fmt(formatter),
            Self::Output(source) => source.fmt(formatter),
            Self::PublicationUnavailable => {
                formatter.write_str("playback controls were unavailable after output startup")
            }
        }
    }
}

impl Error for DesktopPlaybackSessionStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Build(source) => Some(source),
            Self::Output(source) => Some(source),
            Self::PublicationUnavailable => None,
        }
    }
}

/// Failure to load and start one verified project on a desktop output.
#[derive(Debug)]
pub enum DesktopProjectOpenError {
    Load(ProjectPlaybackError),
    Start(DesktopPlaybackSessionStartError),
}

impl fmt::Display for DesktopProjectOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(source) => write!(formatter, "project loading failed: {source}"),
            Self::Start(source) => write!(formatter, "desktop playback startup failed: {source}"),
        }
    }
}

impl Error for DesktopProjectOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(source) => Some(source),
            Self::Start(source) => Some(source),
        }
    }
}

#[derive(Debug)]
struct SessionHandles {
    commands: TransportCommandSender,
    mixer_parameter_commands: MixerParameterCommandSender,
    loudness_commands: LoudnessCommandSender,
    meters: OutputMeterReceiver,
    recovery: PlaybackRecoveryCheckpoint,
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
    commands: TransportCommandSender,
    mixer_parameter_commands: MixerParameterCommandSender,
    loudness_commands: LoudnessCommandSender,
    meters: OutputMeterReceiver,
    recovery: PlaybackRecoveryCheckpoint,
    config: DesktopOutputConfig,
    source: Arc<dyn DesktopPlaybackSource>,
}

/// Restart authority retained after a desktop output has been fully torn down.
///
/// Decoded media remains shared and immutable. Each restart rebuilds only the
/// compiled callback/runtime state and restores the final committed checkpoint
/// before the replacement stream is published.
#[derive(Debug)]
pub struct DesktopPlaybackRecovery {
    config: DesktopOutputConfig,
    source: Option<Arc<dyn DesktopPlaybackSource>>,
    checkpoint: Box<PlaybackRecoverySnapshot>,
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryStopError {
    RuntimeFailed {
        source: DesktopOutputStopError,
        recovery: DesktopPlaybackRecovery,
    },
    ThreadPanicked {
        source: DesktopOutputStopError,
        recovery: DesktopPlaybackRecovery,
    },
    ShutdownPending {
        source: DesktopOutputStopError,
        recovery: DesktopPlaybackRecovery,
    },
}

impl DesktopPlaybackRecoveryStopError {
    fn from_stop(source: DesktopOutputStopError, recovery: DesktopPlaybackRecovery) -> Self {
        match source {
            DesktopOutputStopError::RuntimeFailed => Self::RuntimeFailed { source, recovery },
            DesktopOutputStopError::ThreadPanicked => Self::ThreadPanicked { source, recovery },
            DesktopOutputStopError::ShutdownPending { .. } => {
                Self::ShutdownPending { source, recovery }
            }
        }
    }

    /// Returns restart authority only when the backend thread is known to have
    /// terminated.
    #[must_use]
    pub const fn recovery(&self) -> Option<&DesktopPlaybackRecovery> {
        match self {
            Self::RuntimeFailed { recovery, .. } | Self::ThreadPanicked { recovery, .. } => {
                Some(recovery)
            }
            Self::ShutdownPending { .. } => None,
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
        match self {
            Self::RuntimeFailed { recovery, .. } | Self::ThreadPanicked { recovery, .. } => {
                Ok(recovery)
            }
            pending @ Self::ShutdownPending { .. } => Err(pending),
        }
    }

    #[must_use]
    pub const fn is_shutdown_pending(&self) -> bool {
        matches!(self, Self::ShutdownPending { .. })
    }

    fn source_error(&self) -> &DesktopOutputStopError {
        match self {
            Self::RuntimeFailed { source, .. }
            | Self::ThreadPanicked { source, .. }
            | Self::ShutdownPending { source, .. } => source,
        }
    }

    /// Returns the sole pending native owner together with restart authority.
    ///
    /// # Errors
    ///
    /// Returns the unchanged error when native teardown already completed.
    pub fn into_pending(self) -> Result<DesktopPlaybackRecoveryPendingStop, Self> {
        match self {
            Self::ShutdownPending {
                source: DesktopOutputStopError::ShutdownPending { shutdown },
                recovery,
            } => Ok(DesktopPlaybackRecoveryPendingStop {
                shutdown: *shutdown,
                recovery,
            }),
            other => Err(other),
        }
    }
}

impl fmt::Display for DesktopPlaybackRecoveryStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source_error().fmt(formatter)
    }
}

impl Error for DesktopPlaybackRecoveryStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source_error())
    }
}

/// Sole restart authority while ordinary output shutdown remains pending.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStop {
    shutdown: PendingNativeShutdown,
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
    pub fn status(&self) -> OutputStreamStatus {
        self.shutdown.status()
    }

    /// Waits for teardown while retaining exactly one restart authority.
    #[must_use]
    pub fn wait(mut self, timeout: std::time::Duration) -> DesktopPlaybackRecoveryPendingStopWait {
        match self.shutdown.wait(timeout) {
            PendingNativeShutdownWait::Completed => {
                DesktopPlaybackRecoveryPendingStopWait::Completed(self.recovery)
            }
            PendingNativeShutdownWait::Failed => {
                DesktopPlaybackRecoveryPendingStopWait::Failed(self.recovery)
            }
            PendingNativeShutdownWait::Panicked => {
                DesktopPlaybackRecoveryPendingStopWait::Panicked(self.recovery)
            }
            PendingNativeShutdownWait::TimedOut => {
                DesktopPlaybackRecoveryPendingStopWait::TimedOut(self)
            }
        }
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryStartError {
    OutputNotPinned,
    AlreadyRestarted,
    CancellationPending {
        pending: Box<DesktopPlaybackRecoveryPendingStart>,
    },
    Start(DesktopPlaybackSessionStartError),
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
            Self::OutputNotPinned | Self::AlreadyRestarted | Self::Start(_) => None,
        }
    }
}

/// Sole restart authority while a cancelled native startup still owns its
/// output thread.
///
/// This value is intentionally not cloneable. A retry becomes available only
/// after [`Self::wait`] observes terminal native-thread teardown.
#[derive(Debug)]
pub struct DesktopPlaybackRecoveryPendingStart {
    shutdown: PendingNativeShutdown,
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
    /// Returns the latest state reported by the native output thread.
    #[must_use]
    pub fn status(&self) -> OutputStreamStatus {
        self.shutdown.status()
    }

    /// Waits for native teardown while retaining exactly one retry authority.
    ///
    /// A timeout returns this pending value. Every terminal result returns the
    /// recovery value that may create the next output.
    #[must_use]
    pub fn wait(mut self, timeout: std::time::Duration) -> DesktopPlaybackRecoveryPendingWait {
        match self.shutdown.wait(timeout) {
            PendingNativeShutdownWait::Completed => {
                DesktopPlaybackRecoveryPendingWait::Completed(self.recovery)
            }
            PendingNativeShutdownWait::Failed => {
                DesktopPlaybackRecoveryPendingWait::Failed(self.recovery)
            }
            PendingNativeShutdownWait::Panicked => {
                DesktopPlaybackRecoveryPendingWait::Panicked(self.recovery)
            }
            PendingNativeShutdownWait::TimedOut => {
                DesktopPlaybackRecoveryPendingWait::TimedOut(self)
            }
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
        let source = self
            .source
            .take()
            .ok_or(DesktopPlaybackRecoveryStartError::AlreadyRestarted)?;
        let session = DesktopPlaybackSession::start_source(
            config.clone(),
            source.clone(),
            Some(Box::new(*self.checkpoint)),
        );
        match session {
            Ok(session) => Ok(session),
            Err(error) => Err(self.failed_restart(config, source, error)),
        }
    }

    fn failed_restart(
        &mut self,
        config: DesktopOutputConfig,
        source: Arc<dyn DesktopPlaybackSource>,
        error: DesktopPlaybackSessionStartError,
    ) -> DesktopPlaybackRecoveryStartError {
        match error {
            DesktopPlaybackSessionStartError::Output(
                DesktopOutputStartError::CancellationPending { shutdown },
            ) => DesktopPlaybackRecoveryStartError::CancellationPending {
                pending: Box::new(DesktopPlaybackRecoveryPendingStart {
                    shutdown: *shutdown,
                    recovery: DesktopPlaybackRecovery {
                        config,
                        source: Some(source),
                        checkpoint: self.checkpoint.clone(),
                    },
                }),
            },
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
    /// the complete project has passed structural and semantic verification.
    /// The verified package, decoded media, and recording metadata remain
    /// owned by the live session and any recovery value derived from it.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopProjectOpenError::Load`] before native startup when the
    /// project is invalid, changes during capture, exceeds a limit, or cannot
    /// be decoded. Returns [`DesktopProjectOpenError::Start`] when the verified
    /// project cannot be bound to the selected output.
    pub fn open_project(
        config: DesktopOutputConfig,
        root: impl AsRef<Path>,
        limits: ProjectPlaybackLimits,
    ) -> Result<Self, DesktopProjectOpenError> {
        let project =
            prepare_project_playback(root, limits).map_err(DesktopProjectOpenError::Load)?;
        Self::start_project(config, project).map_err(DesktopProjectOpenError::Start)
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
        Self::start_source(config, Arc::new(project), None)
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
        Self::start_source(config, Arc::new(input), None)
    }

    fn start_source(
        config: DesktopOutputConfig,
        source: Arc<dyn DesktopPlaybackSource>,
        initial_checkpoint: Option<Box<PlaybackRecoverySnapshot>>,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        let sample_rate_hz = config.sample_rate_hz();
        let maximum_block_frames = config.maximum_block_frames();
        let (publication_sender, publication_receiver) = sync_channel(1);
        let thread_source = source.clone();
        let output =
            DesktopOutputStream::start_with_thread_initializer(config.native(), move |runner| {
                initialize_session(
                    runner,
                    thread_source.session(),
                    sample_rate_hz,
                    maximum_block_frames,
                    initial_checkpoint.as_deref(),
                    &publication_sender,
                )
            });

        match output {
            Ok(output) => match publication_receiver.recv() {
                Ok(SessionPublication::Prepared(handles)) => Ok(Self {
                    output,
                    commands: handles.commands,
                    mixer_parameter_commands: handles.mixer_parameter_commands,
                    loudness_commands: handles.loudness_commands,
                    meters: handles.meters,
                    recovery: handles.recovery,
                    config,
                    source,
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

    #[must_use]
    pub fn command_sender(&mut self) -> &mut TransportCommandSender {
        &mut self.commands
    }

    #[must_use]
    pub fn mixer_parameter_command_sender(&mut self) -> &mut MixerParameterCommandSender {
        &mut self.mixer_parameter_commands
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

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.output.counters()
    }

    #[must_use]
    pub fn output_status(&self) -> OutputStreamStatus {
        self.output.status()
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
            commands: _,
            mixer_parameter_commands: _,
            loudness_commands: _,
            meters: _,
            recovery,
            config,
            source,
        } = self;
        let stop_result = output.stop().map_err(DesktopOutputStopError::from);
        let recovery = DesktopPlaybackRecovery {
            config,
            source: Some(source),
            checkpoint: Box::new(recovery.snapshot()),
        };
        match stop_result {
            Ok(()) => Ok(recovery),
            Err(source) => Err(DesktopPlaybackRecoveryStopError::from_stop(
                source, recovery,
            )),
        }
    }

    #[cfg(target_os = "windows")]
    /// Returns the asynchronous Windows session-disconnect reason, if
    /// available. Generic runtime failures have no disconnect reason.
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.output.disconnect_reason()
    }

    /// Stops the output thread and waits for its complete teardown.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputStopError`] if the output thread panicked or did
    /// not terminate before the bounded shutdown deadline.
    pub fn stop(self) -> Result<(), DesktopOutputStopError> {
        let Self {
            output,
            commands: _,
            mixer_parameter_commands: _,
            loudness_commands: _,
            meters: _,
            recovery: _,
            config: _,
            source: _,
        } = self;
        output.stop().map_err(DesktopOutputStopError::from)
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

fn initialize_session(
    runner: DesktopThreadRunner,
    input: &DesktopPlaybackSessionInput,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    initial_checkpoint: Option<&PlaybackRecoverySnapshot>,
    publication: &SyncSender<SessionPublication>,
) -> Result<(), NativeDesktopOutputStartError> {
    let (project, mixer, automation, media) = input.parts();
    if project.sample_rate_hz() != sample_rate_hz {
        return fail_build(
            publication,
            DesktopPlaybackSessionBuildError::SampleRateMismatch {
                project: project.sample_rate_hz(),
                output: sample_rate_hz,
            },
        );
    }

    let mut facts: Vec<MediaAssetFact<'_>> = Vec::new();
    facts
        .try_reserve_exact(media.asset_count())
        .map_err(|_| DesktopPlaybackSessionBuildError::AssetFactCapacityUnavailable)
        .or_else(|source| fail_build(publication, source))?;
    facts.extend(media.media_asset_facts());

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
    if let Some(checkpoint) = initial_checkpoint {
        transport.apply_command(checkpoint.restore_command());
        if let Some(mixer_parameter_state) = checkpoint.mixer_parameter_state() {
            transport
                .restore_mixer_parameter_state(&mixer_parameter_state)
                .map_err(DesktopPlaybackSessionBuildError::MixerRecovery)
                .or_else(|source| fail_build(publication, source))?;
        }
    }
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
    let (publisher, recovery) = playback_recovery_channel(initial_checkpoint);
    publication
        .try_send(SessionPublication::Prepared(SessionHandles {
            commands,
            mixer_parameter_commands,
            loudness_commands,
            meters,
            recovery: recovery.clone(),
        }))
        .map_err(|_| callback_error("playback control publication failed".to_owned()))?;
    runner.run_transport_output_with_checkpoint(processor, publisher)
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
    use crate::{BoundedBackendThread, acquire_backend_thread_permit};
    use hyper_audio_source::DecodeLimits;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    use hyper_audio_source::DecodedAudio;
    use hyper_media_cache::CacheLimits;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    use hyper_mixer_runtime::{LocateState, PlaybackRecoveryTransportState, TransportCommand};
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    use hyper_playback_session::{
        DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyInput,
        assemble_playback_session,
    };
    use std::sync::mpsc::SyncSender;
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    struct UnusedPlaybackSource;

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337500";
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337504";
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337501",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
    }"#;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337502",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122337503"],"extensions":{}
    }"#;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
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
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_ARRANGEMENT: &[u8] = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
        "placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503","start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true}],
        "scene_launches":[],"extensions":{}
    }"#;
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    const LIVE_MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
        "channels":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":1000000,"pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}],
        "return_buses":[],"main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},"extensions":{}
    }"#;

    impl DesktopPlaybackSource for UnusedPlaybackSource {
        fn session(&self) -> &PreparedPlaybackSession {
            unreachable!("the authority-state fixture never builds a playback session")
        }

        fn project(&self) -> Option<&PreparedProjectPlayback> {
            None
        }
    }

    fn pending_shutdown_fixture() -> (PendingNativeShutdown, SyncSender<()>) {
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
        (
            PendingNativeShutdown::new(thread, OutputStreamStatus::default()),
            release_sender,
        )
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

    #[cfg(all(target_os = "linux", target_endian = "little"))]
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
            .command_sender()
            .try_send(TransportCommand::Locate {
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
            .command_sender()
            .try_send(TransportCommand::Start)
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
    fn project_load_failure_precedes_native_output_startup() {
        let not_a_project = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let config = DesktopOutputConfig::try_new(48_000, 256).unwrap();
        let error = DesktopPlaybackSession::open_project(config, &not_a_project, project_limits())
            .expect_err("a non-project path must fail before native output startup");

        assert!(matches!(
            error,
            DesktopProjectOpenError::Load(ProjectPlaybackError::Storage(_))
        ));
    }

    #[test]
    fn project_open_error_preserves_start_failure_source() {
        let error = DesktopProjectOpenError::Start(DesktopPlaybackSessionStartError::Build(
            DesktopPlaybackSessionBuildError::SampleRateMismatch {
                project: 44_100,
                output: 48_000,
            },
        ));

        assert_eq!(
            error.to_string(),
            "desktop playback startup failed: project sample rate is 44100 Hz; output is configured for 48000 Hz"
        );
        assert!(matches!(
            error.source(),
            Some(source) if source.to_string()
                == "project sample rate is 44100 Hz; output is configured for 48000 Hz"
        ));
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
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };
        let (shutdown, release) = pending_shutdown_fixture();

        let error = original.failed_restart(
            pinned,
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
    fn joined_restart_failure_restores_retry_authority_in_place() {
        let pinned = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("opaque-route")
            .unwrap();
        let source: Arc<dyn DesktopPlaybackSource> = Arc::new(UnusedPlaybackSource);
        let mut recovery = DesktopPlaybackRecovery {
            config: pinned.clone(),
            source: None,
            checkpoint: Box::new(PlaybackRecoveryCheckpoint::default().snapshot()),
        };

        let error = recovery.failed_restart(
            pinned,
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
    fn joined_panic_retains_restart_authority() {
        let recovery = DesktopPlaybackRecovery {
            config: DesktopOutputConfig::try_new(48_000, 256).unwrap(),
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
