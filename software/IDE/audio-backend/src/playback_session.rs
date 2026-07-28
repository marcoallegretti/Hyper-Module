use super::{
    OutputStreamCounters, OutputStreamStatus, PlaybackRecoveryCheckpoint, PlaybackRecoverySnapshot,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{
    PipeWireOutputConfig, PipeWireOutputConfigError, PipeWireOutputStream, PipeWireStartError,
    PipeWireStartupStage, PipeWireStopError, PipeWireThreadRunner,
};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiOutputConfig, WasapiOutputConfigError, WasapiOutputStream,
    WasapiStartError, WasapiStartupStage, WasapiStopError, WasapiThreadRunner,
};
use core::error::Error;
use core::fmt;
use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_media_cache::PreparePlaybackError;
use hyper_mixer_runtime::{
    OutputMeterReceiver, RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError,
    TransportCommandSender, TransportOutputCreationError, TransportOutputProcessor,
    output_meter_channel,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, MediaAssetFact};
use hyper_playback_session::PreparedPlaybackSession;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};

pub type DesktopPlaybackSessionInput = PreparedPlaybackSession;

#[cfg(all(target_os = "linux", target_endian = "little"))]
pub type DesktopOutputConfig = PipeWireOutputConfig;
#[cfg(target_os = "windows")]
pub type DesktopOutputConfig = WasapiOutputConfig;

#[cfg(all(target_os = "linux", target_endian = "little"))]
pub type DesktopOutputConfigError = PipeWireOutputConfigError;
#[cfg(target_os = "windows")]
pub type DesktopOutputConfigError = WasapiOutputConfigError;

#[cfg(all(target_os = "linux", target_endian = "little"))]
pub type DesktopOutputStartError = PipeWireStartError;
#[cfg(target_os = "windows")]
pub type DesktopOutputStartError = WasapiStartError;

#[cfg(all(target_os = "linux", target_endian = "little"))]
pub type DesktopOutputStopError = PipeWireStopError;
#[cfg(target_os = "windows")]
pub type DesktopOutputStopError = WasapiStopError;

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

#[derive(Debug)]
struct SessionHandles {
    commands: TransportCommandSender,
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
    meters: OutputMeterReceiver,
    recovery: PlaybackRecoveryCheckpoint,
    config: DesktopOutputConfig,
    source: Arc<DesktopPlaybackSessionInput>,
}

/// Restart authority retained after a desktop output has been fully torn down.
///
/// Decoded media remains shared and immutable. Each restart rebuilds only the
/// compiled callback/runtime state and restores the final committed checkpoint
/// before the replacement stream is published.
#[derive(Debug)]
pub struct DesktopPlaybackRecovery {
    config: DesktopOutputConfig,
    source: Option<Arc<DesktopPlaybackSessionInput>>,
    checkpoint: PlaybackRecoverySnapshot,
}

#[derive(Debug)]
pub struct DesktopPlaybackRecoveryStopError {
    source: DesktopOutputStopError,
    recovery: DesktopPlaybackRecovery,
}

impl DesktopPlaybackRecoveryStopError {
    #[must_use]
    pub const fn recovery(&self) -> &DesktopPlaybackRecovery {
        &self.recovery
    }

    #[must_use]
    pub fn into_recovery(self) -> DesktopPlaybackRecovery {
        self.recovery
    }
}

impl fmt::Display for DesktopPlaybackRecoveryStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for DesktopPlaybackRecoveryStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug)]
pub enum DesktopPlaybackRecoveryStartError {
    OutputNotPinned,
    AlreadyRestarted,
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
            Self::Start(source) => source.fmt(formatter),
        }
    }
}

impl Error for DesktopPlaybackRecoveryStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OutputNotPinned | Self::AlreadyRestarted => None,
            Self::Start(source) => Some(source),
        }
    }
}

impl DesktopPlaybackRecovery {
    #[must_use]
    pub const fn checkpoint(&self) -> PlaybackRecoverySnapshot {
        self.checkpoint
    }

    /// Rebuilds playback against the same explicitly selected output.
    ///
    /// A failed attempt retains restart authority for another try. A successful
    /// attempt consumes it so the same recovery cannot create parallel outputs.
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
            .as_ref()
            .ok_or(DesktopPlaybackRecoveryStartError::AlreadyRestarted)?;
        let session =
            DesktopPlaybackSession::start_retained(config, source.clone(), Some(self.checkpoint))
                .map_err(DesktopPlaybackRecoveryStartError::Start)?;
        self.source = None;
        Ok(session)
    }
}

impl DesktopPlaybackSession {
    /// Compiles and binds one verified session entirely on the output thread.
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
        Self::start_retained(config, Arc::new(input), None)
    }

    fn start_retained(
        config: DesktopOutputConfig,
        source: Arc<DesktopPlaybackSessionInput>,
        initial_checkpoint: Option<PlaybackRecoverySnapshot>,
    ) -> Result<Self, DesktopPlaybackSessionStartError> {
        let sample_rate_hz = config.sample_rate_hz();
        let maximum_block_frames = config.maximum_block_frames();
        let (publication_sender, publication_receiver) = sync_channel(1);
        let thread_source = source.clone();
        let output =
            DesktopOutputStream::start_with_thread_initializer(config.clone(), move |runner| {
                initialize_session(
                    runner,
                    &thread_source,
                    sample_rate_hz,
                    maximum_block_frames,
                    initial_checkpoint,
                    &publication_sender,
                )
            });

        match output {
            Ok(output) => match publication_receiver.recv() {
                Ok(SessionPublication::Prepared(handles)) => Ok(Self {
                    output,
                    commands: handles.commands,
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
            Err(source) => match failed_build(&publication_receiver) {
                Some(build) => Err(DesktopPlaybackSessionStartError::Build(build)),
                None => Err(DesktopPlaybackSessionStartError::Output(source)),
            },
        }
    }

    #[must_use]
    pub fn command_sender(&mut self) -> &mut TransportCommandSender {
        &mut self.commands
    }

    #[must_use]
    pub fn meter_receiver(&mut self) -> &mut OutputMeterReceiver {
        &mut self.meters
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
    /// Returns a shutdown error that still owns the recovery authority if the
    /// backend thread panicked.
    pub fn into_recovery(
        self,
    ) -> Result<DesktopPlaybackRecovery, DesktopPlaybackRecoveryStopError> {
        let Self {
            output,
            commands: _,
            meters: _,
            recovery,
            config,
            source,
        } = self;
        let stop_result = output.stop();
        let recovery = DesktopPlaybackRecovery {
            config,
            source: Some(source),
            checkpoint: recovery.snapshot(),
        };
        match stop_result {
            Ok(()) => Ok(recovery),
            Err(source) => Err(DesktopPlaybackRecoveryStopError { source, recovery }),
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
    /// Returns [`DesktopOutputStopError`] if the output thread panicked.
    pub fn stop(self) -> Result<(), DesktopOutputStopError> {
        let Self {
            output,
            commands: _,
            meters: _,
            recovery: _,
            config: _,
            source: _,
        } = self;
        output.stop()
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

fn initialize_session(
    runner: DesktopThreadRunner,
    input: &DesktopPlaybackSessionInput,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    initial_checkpoint: Option<PlaybackRecoverySnapshot>,
    publication: &SyncSender<SessionPublication>,
) -> Result<(), DesktopOutputStartError> {
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

    let (commands, mut transport) = RoutedTransportRuntime::with_command_queue(runtime);
    if let Some(checkpoint) = initial_checkpoint {
        transport.apply_command(checkpoint.restore_command());
    }
    let (mut meter_sender, meters) = output_meter_channel();
    let processor =
        TransportOutputProcessor::try_new(&mut transport, &mut scratch, &mut meter_sender)
            .map_err(DesktopPlaybackSessionBuildError::Output)
            .or_else(|source| fail_build(publication, source))?;
    let recovery = initial_checkpoint
        .map(PlaybackRecoveryCheckpoint::from_snapshot)
        .unwrap_or_default();
    publication
        .try_send(SessionPublication::Prepared(SessionHandles {
            commands,
            meters,
            recovery: recovery.clone(),
        }))
        .map_err(|_| callback_error("playback control publication failed".to_owned()))?;
    runner.run_transport_output_with_checkpoint(processor, recovery)
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
fn output_is_pinned(config: &DesktopOutputConfig) -> bool {
    config.target().is_some()
}

#[cfg(target_os = "windows")]
fn output_is_pinned(config: &DesktopOutputConfig) -> bool {
    config.device_id().is_some()
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
) -> Result<T, DesktopOutputStartError> {
    let detail = source.to_string();
    let _ = publication.try_send(SessionPublication::Failed(source));
    Err(callback_error(detail))
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
fn callback_error(detail: String) -> DesktopOutputStartError {
    PipeWireStartError::Backend {
        stage: PipeWireStartupStage::Callback,
        detail,
    }
}

#[cfg(target_os = "windows")]
fn callback_error(detail: String) -> DesktopOutputStartError {
    WasapiStartError::Backend {
        stage: WasapiStartupStage::Callback,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Err(DesktopOutputStartError::Backend { detail, .. })
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
    fn recovery_rejects_policy_defaults_and_accepts_explicit_outputs() {
        let policy_default = DesktopOutputConfig::try_new(48_000, 256).unwrap();
        assert!(!output_is_pinned(&policy_default));

        #[cfg(target_os = "windows")]
        let pinned = policy_default.with_device_id("endpoint-id").unwrap();
        #[cfg(all(target_os = "linux", target_endian = "little"))]
        let pinned = policy_default.with_target("sink-name").unwrap();
        assert!(output_is_pinned(&pinned));

        let mut spent = DesktopPlaybackRecovery {
            config: pinned,
            source: None,
            checkpoint: PlaybackRecoveryCheckpoint::default().snapshot(),
        };
        assert!(matches!(
            spent.restart(),
            Err(DesktopPlaybackRecoveryStartError::AlreadyRestarted)
        ));
    }
}
