use core::error::Error;
use core::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc::{
    Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hyper_mixer_runtime::{LatestOutputMeterSnapshot, PlaybackRecoverySnapshot, TransportCommand};
use hyper_thread_supervisor::{ThreadPermit, ThreadSupervisor, ThreadSupervisorSnapshot};

use super::recording::{IdeRecordingPlan, IdeRecordingTarget, RecordingDesktopIdeApplication};
use super::{
    DesktopIdeApplication, DesktopIdePendingClose, DesktopIdePendingSaveClose,
    DesktopPlaybackRecovery, IdeApplicationConfig, IdeAudioInputInventory, IdeAudioOutputInventory,
    IdeCloseOutcome, IdeInputDiscoveryError, IdeInputSelection, IdeInputSelectionError,
    IdeMixerChannelKind, IdeMixerChannelState, IdeMixerCommand, IdeMixerCommandError,
    IdeMixerMainState, IdeMonitoringCommand, IdeMonitoringCommandError, IdeMonitoringState,
    IdeNativeCloseFailure, IdeOutputDiscoveryError, IdeOutputSelection, IdeOutputSelectionError,
    IdePendingCloseWait, IdePendingSaveCloseWait, IdeProjectOpenError, IdeProjectSaveError,
    IdeRecordingAbortError, IdeRecordingAbortOutcome, IdeRecordingError, IdeRecordingImportOutcome,
    IdeRecordingObservation, IdeSaveCloseOutcome, IdeSceneAvailability, InputInventoryError,
    LocateState, OutputInventoryError, OutputStreamSnapshot, OutputStreamState,
    RunningDesktopIdeApplication,
};

const WORKER_SHUTDOWN_POLL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const MIXER_SAVE_COMMIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIXER_SAVE_COMMIT_TIMEOUT: Duration = Duration::from_millis(50);
pub const MAX_IDE_WORKER_COMMANDS: usize = 64;
pub const MAX_IDE_WORKER_EVENTS: usize = 64;

#[derive(Debug, Default)]
pub struct IdeWorkerShutdownReport {
    native_failure: Option<IdeNativeCloseFailure>,
    recording_abort: Option<Result<IdeRecordingAbortOutcome, IdeRecordingAbortError>>,
}

impl IdeWorkerShutdownReport {
    #[must_use]
    pub const fn native_failure(&self) -> Option<IdeNativeCloseFailure> {
        self.native_failure
    }

    #[must_use]
    pub fn recording_abort(
        &self,
    ) -> Option<Result<&IdeRecordingAbortOutcome, &IdeRecordingAbortError>> {
        self.recording_abort.as_ref().map(|result| result.as_ref())
    }

    #[must_use]
    pub fn into_recording_abort(
        self,
    ) -> Option<Result<IdeRecordingAbortOutcome, IdeRecordingAbortError>> {
        self.recording_abort
    }
}

fn worker_supervisor() -> io::Result<&'static ThreadSupervisor<IdeWorkerShutdownReport>> {
    const CAPACITY: usize = 16;
    static SUPERVISOR: OnceLock<Result<ThreadSupervisor<IdeWorkerShutdownReport>, io::Error>> =
        OnceLock::new();
    SUPERVISOR
        .get_or_init(|| {
            ThreadSupervisor::new("hyper-desktop-ide-shutdown", CAPACITY, WORKER_SHUTDOWN_POLL)
        })
        .as_ref()
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

fn acquire_worker_permit() -> io::Result<ThreadPermit<IdeWorkerShutdownReport>> {
    worker_supervisor()?
        .acquire()
        .map_err(|error| io::Error::new(io::ErrorKind::WouldBlock, error))
}

/// Returns bounded application-worker ownership telemetry after initialization.
#[must_use]
pub fn desktop_ide_worker_supervisor_snapshot() -> Option<ThreadSupervisorSnapshot> {
    worker_supervisor().ok().map(ThreadSupervisor::snapshot)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeWorkerCommandKind {
    Initialize,
    RetryDiscovery,
    RefreshOutputs,
    RefreshInputs,
    SelectSystemDefault,
    SelectOutput,
    DisableInputMonitoring,
    SelectSystemDefaultInput,
    SelectInput,
    OpenProject,
    Transport,
    Mixer,
    Monitoring,
    SaveProject,
    StartRecording,
    StopRecording,
    AbortRecording,
    Observe,
    CloseProject,
    Shutdown,
}

#[derive(Debug)]
pub enum IdeWorkerCommand {
    RetryDiscovery,
    RefreshOutputs,
    RefreshInputs,
    SelectSystemDefault,
    SelectOutput(String),
    DisableInputMonitoring,
    SelectSystemDefaultInput,
    SelectInput(String),
    OpenProject(PathBuf),
    Transport(TransportCommand),
    Mixer(IdeMixerCommand),
    Monitoring(IdeMonitoringCommand),
    SaveProject,
    StartRecording { channel_index: usize },
    StopRecording,
    AbortRecording,
    Observe,
    CloseProject,
    Shutdown,
}

impl IdeWorkerCommand {
    #[must_use]
    pub const fn kind(&self) -> IdeWorkerCommandKind {
        match self {
            Self::RetryDiscovery => IdeWorkerCommandKind::RetryDiscovery,
            Self::RefreshOutputs => IdeWorkerCommandKind::RefreshOutputs,
            Self::RefreshInputs => IdeWorkerCommandKind::RefreshInputs,
            Self::SelectSystemDefault => IdeWorkerCommandKind::SelectSystemDefault,
            Self::SelectOutput(_) => IdeWorkerCommandKind::SelectOutput,
            Self::DisableInputMonitoring => IdeWorkerCommandKind::DisableInputMonitoring,
            Self::SelectSystemDefaultInput => IdeWorkerCommandKind::SelectSystemDefaultInput,
            Self::SelectInput(_) => IdeWorkerCommandKind::SelectInput,
            Self::OpenProject(_) => IdeWorkerCommandKind::OpenProject,
            Self::Transport(_) => IdeWorkerCommandKind::Transport,
            Self::Mixer(_) => IdeWorkerCommandKind::Mixer,
            Self::Monitoring(_) => IdeWorkerCommandKind::Monitoring,
            Self::SaveProject => IdeWorkerCommandKind::SaveProject,
            Self::StartRecording { .. } => IdeWorkerCommandKind::StartRecording,
            Self::StopRecording => IdeWorkerCommandKind::StopRecording,
            Self::AbortRecording => IdeWorkerCommandKind::AbortRecording,
            Self::Observe => IdeWorkerCommandKind::Observe,
            Self::CloseProject => IdeWorkerCommandKind::CloseProject,
            Self::Shutdown => IdeWorkerCommandKind::Shutdown,
        }
    }
}

#[derive(Debug)]
pub struct IdeClosedSnapshot {
    outputs: IdeAudioOutputInventory,
    output_selection: IdeOutputSelection,
    inputs: Option<IdeAudioInputInventory>,
    input_selection: IdeInputSelection,
}

impl IdeClosedSnapshot {
    #[must_use]
    pub const fn outputs(&self) -> &IdeAudioOutputInventory {
        &self.outputs
    }

    #[must_use]
    pub const fn output_selection(&self) -> &IdeOutputSelection {
        &self.output_selection
    }

    #[must_use]
    pub const fn selection(&self) -> &IdeOutputSelection {
        self.output_selection()
    }

    #[must_use]
    pub const fn inputs(&self) -> Option<&IdeAudioInputInventory> {
        self.inputs.as_ref()
    }

    #[must_use]
    pub const fn input_selection(&self) -> &IdeInputSelection {
        &self.input_selection
    }
}

#[derive(Debug)]
pub struct IdeRunningObservation {
    project_id: String,
    project_name: String,
    sample_rate_hz: u32,
    scenes: Vec<IdeScene>,
    mixer_channels: Vec<IdeMixerChannel>,
    mixer_main: Option<IdeMixerMainState>,
    output_state: OutputStreamState,
    output: OutputStreamSnapshot,
    monitoring: Option<IdeMonitoringState>,
    recovery: PlaybackRecoverySnapshot,
    meter: LatestOutputMeterSnapshot,
}

#[derive(Debug)]
pub struct IdeRecordingState {
    project_id: String,
    project_name: String,
    track_id: String,
    track_name: String,
    sample_rate_hz: u32,
    start_frame: usize,
    start_tick: u64,
    journal_path: PathBuf,
    observation: IdeRecordingObservation,
}

impl IdeRecordingState {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub fn track_name(&self) -> &str {
        &self.track_name
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn start_frame(&self) -> usize {
        self.start_frame
    }

    #[must_use]
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    #[must_use]
    pub fn journal_path(&self) -> &std::path::Path {
        &self.journal_path
    }

    #[must_use]
    pub const fn observation(&self) -> IdeRecordingObservation {
        self.observation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Owned UI-thread observation of one validated project scene.
pub struct IdeScene {
    name: String,
    availability: IdeSceneAvailability,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdeMixerChannel {
    id: String,
    name: String,
    kind: IdeMixerChannelKind,
    state: Option<IdeMixerChannelState>,
    sends: Vec<IdeMixerSend>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdeMixerSend {
    target_name: String,
    tap: super::IdeMixerSendTap,
    state: Option<super::IdeMixerSendState>,
}

impl IdeMixerChannel {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn kind(&self) -> IdeMixerChannelKind {
        self.kind
    }

    #[must_use]
    pub const fn state(&self) -> Option<IdeMixerChannelState> {
        self.state
    }

    #[must_use]
    pub fn sends(&self) -> &[IdeMixerSend] {
        &self.sends
    }
}

impl IdeMixerSend {
    #[must_use]
    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    #[must_use]
    pub const fn tap(&self) -> super::IdeMixerSendTap {
        self.tap
    }

    #[must_use]
    pub const fn state(&self) -> Option<super::IdeMixerSendState> {
        self.state
    }
}

impl IdeScene {
    /// Returns the validated project-supplied scene name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the session's prepared launch status for this scene.
    #[must_use]
    pub const fn availability(&self) -> IdeSceneAvailability {
        self.availability
    }
}

impl IdeRunningObservation {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn scenes(&self) -> &[IdeScene] {
        &self.scenes
    }

    #[must_use]
    pub fn mixer_channels(&self) -> &[IdeMixerChannel] {
        &self.mixer_channels
    }

    #[must_use]
    pub const fn mixer_main(&self) -> Option<IdeMixerMainState> {
        self.mixer_main
    }

    #[must_use]
    pub const fn output_state(&self) -> &OutputStreamState {
        &self.output_state
    }

    #[must_use]
    pub const fn output(&self) -> OutputStreamSnapshot {
        self.output
    }

    #[must_use]
    pub const fn monitoring(&self) -> Option<IdeMonitoringState> {
        self.monitoring
    }

    #[must_use]
    pub const fn recovery(&self) -> PlaybackRecoverySnapshot {
        self.recovery
    }

    #[must_use]
    pub const fn meter(&self) -> LatestOutputMeterSnapshot {
        self.meter
    }
}

#[derive(Debug)]
pub enum IdeWorkerCommandFailure {
    Discovery(IdeOutputDiscoveryError),
    InputDiscovery(IdeInputDiscoveryError),
    OutputSelection(IdeOutputSelectionError),
    InputSelection(IdeInputSelectionError),
    ProjectOpen(IdeProjectOpenError),
    Transport(super::DesktopDirectControlError),
    Mixer(IdeMixerCommandError),
    Monitoring(IdeMonitoringCommandError),
    ProjectSave(IdeProjectSaveError),
    Recording(IdeRecordingError),
    InvalidState,
    Publication(OutputInventoryError),
    InputPublication(InputInventoryError),
    ProjectPublicationCapacityUnavailable,
    ProjectSnapshotUnavailable,
}

impl fmt::Display for IdeWorkerCommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(source) => source.fmt(formatter),
            Self::InputDiscovery(source) => source.fmt(formatter),
            Self::OutputSelection(source) => source.fmt(formatter),
            Self::InputSelection(source) => source.fmt(formatter),
            Self::ProjectOpen(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
            Self::Mixer(source) => source.fmt(formatter),
            Self::Monitoring(source) => source.fmt(formatter),
            Self::ProjectSave(source) => source.fmt(formatter),
            Self::Recording(source) => source.fmt(formatter),
            Self::InvalidState => {
                formatter.write_str("command is invalid for the current IDE state")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::InputPublication(source) => source.fmt(formatter),
            Self::ProjectPublicationCapacityUnavailable => {
                formatter.write_str("project observation capacity is unavailable")
            }
            Self::ProjectSnapshotUnavailable => {
                formatter.write_str("running playback exposes no verified project snapshot")
            }
        }
    }
}

impl Error for IdeWorkerCommandFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Discovery(source) => Some(source),
            Self::InputDiscovery(source) => Some(source),
            Self::OutputSelection(source) => Some(source),
            Self::InputSelection(source) => Some(source),
            Self::ProjectOpen(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::Monitoring(source) => Some(source),
            Self::ProjectSave(source) => Some(source),
            Self::Recording(source) => Some(source),
            Self::Publication(source) => Some(source),
            Self::InputPublication(source) => Some(source),
            Self::InvalidState
            | Self::ProjectPublicationCapacityUnavailable
            | Self::ProjectSnapshotUnavailable => None,
        }
    }
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "events transfer fixed-size owned observations without an additional infallible allocation"
)]
pub enum IdeWorkerEvent {
    Closed {
        snapshot: IdeClosedSnapshot,
        native_failure: Option<IdeNativeCloseFailure>,
        input_failure: Option<IdeInputDiscoveryError>,
    },
    Running(IdeRunningObservation),
    TransportQueued(TransportCommand),
    MixerQueued(IdeMixerCommand),
    MonitoringApplied(IdeMonitoringCommand),
    ProjectSaved {
        revision: u64,
        changed: bool,
    },
    RecordingPreparing,
    Recording(IdeRecordingState),
    RecordingFinalizing,
    RecordingImported(IdeRecordingImportOutcome),
    RecordingAborted(IdeRecordingAbortOutcome),
    Saving,
    Closing,
    CommandFailed {
        command: IdeWorkerCommandKind,
        reason: IdeWorkerCommandFailure,
    },
}

#[derive(Debug)]
pub enum IdeWorkerStartError {
    InvalidCommandCapacity { actual: usize, maximum: usize },
    InvalidEventCapacity { actual: usize, maximum: usize },
    ThreadSpawn(io::Error),
}

impl fmt::Display for IdeWorkerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCommandCapacity { actual, maximum } => write!(
                formatter,
                "IDE worker command capacity is {actual}; expected 1 through {maximum}"
            ),
            Self::InvalidEventCapacity { actual, maximum } => write!(
                formatter,
                "IDE worker event capacity is {actual}; expected 1 through {maximum}"
            ),
            Self::ThreadSpawn(source) => {
                write!(formatter, "IDE worker thread start failed: {source}")
            }
        }
    }
}

impl Error for IdeWorkerStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(source) => Some(source),
            Self::InvalidCommandCapacity { .. } | Self::InvalidEventCapacity { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeWorkerSendError {
    Full(IdeWorkerCommand),
    Disconnected(IdeWorkerCommand),
}

impl IdeWorkerSendError {
    #[must_use]
    pub const fn command(&self) -> &IdeWorkerCommand {
        match self {
            Self::Full(command) | Self::Disconnected(command) => command,
        }
    }
}

impl fmt::Display for IdeWorkerSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Full(_) => "IDE worker command queue is full",
            Self::Disconnected(_) => "IDE worker command channel is disconnected",
        })
    }
}

impl Error for IdeWorkerSendError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdeWorkerEventReceiveError;

impl fmt::Display for IdeWorkerEventReceiveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IDE worker event channel is disconnected")
    }
}

impl Error for IdeWorkerEventReceiveError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeWorkerJoinError {
    ThreadPanicked,
}

impl fmt::Display for IdeWorkerJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IDE worker thread panicked")
    }
}

impl Error for IdeWorkerJoinError {}

pub struct DesktopIdeWorker {
    commands: SyncSender<IdeWorkerCommand>,
    events: Option<Receiver<IdeWorkerEvent>>,
    completion: Receiver<()>,
    handle: Option<JoinHandle<IdeWorkerShutdownReport>>,
    permit: Option<ThreadPermit<IdeWorkerShutdownReport>>,
}

impl fmt::Debug for DesktopIdeWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DesktopIdeWorker")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl DesktopIdeWorker {
    /// Starts the sole blocking application owner on a dedicated worker thread.
    ///
    /// # Errors
    ///
    /// Returns validation, supervisor-capacity, or thread-spawn failure.
    pub fn start(
        application: IdeApplicationConfig,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, IdeWorkerStartError> {
        Self::start_state(
            WorkerState::Undiscovered(application),
            command_capacity,
            event_capacity,
        )
    }

    fn start_state(
        state: WorkerState,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, IdeWorkerStartError> {
        if !(1..=MAX_IDE_WORKER_COMMANDS).contains(&command_capacity) {
            return Err(IdeWorkerStartError::InvalidCommandCapacity {
                actual: command_capacity,
                maximum: MAX_IDE_WORKER_COMMANDS,
            });
        }
        if !(1..=MAX_IDE_WORKER_EVENTS).contains(&event_capacity) {
            return Err(IdeWorkerStartError::InvalidEventCapacity {
                actual: event_capacity,
                maximum: MAX_IDE_WORKER_EVENTS,
            });
        }
        let (command_sender, command_receiver) = sync_channel(command_capacity);
        let (event_sender, event_receiver) = sync_channel(event_capacity);
        let (completion_sender, completion_receiver) = sync_channel(1);
        let permit = acquire_worker_permit().map_err(IdeWorkerStartError::ThreadSpawn)?;
        let handle = thread::Builder::new()
            .name("hyper-desktop-ide".to_owned())
            .spawn(move || {
                let _completion = CompletionSignal(completion_sender);
                run_worker(state, &command_receiver, &event_sender)
            })
            .map_err(IdeWorkerStartError::ThreadSpawn)?;
        Ok(Self {
            commands: command_sender,
            events: Some(event_receiver),
            completion: completion_receiver,
            handle: Some(handle),
            permit: Some(permit),
        })
    }

    /// Enqueues one UI command without blocking its event loop.
    ///
    /// # Errors
    ///
    /// Returns the unchanged command when the bounded queue is full or closed.
    pub fn try_send(&self, command: IdeWorkerCommand) -> Result<(), IdeWorkerSendError> {
        self.commands
            .try_send(command)
            .map_err(|source| match source {
                TrySendError::Full(command) => IdeWorkerSendError::Full(command),
                TrySendError::Disconnected(command) => IdeWorkerSendError::Disconnected(command),
            })
    }

    /// Reads one worker event without blocking the UI event loop.
    ///
    /// Events are reliable: a full event queue pauses worker command processing
    /// until the receiver drains or disconnects. UI integrations should drain all
    /// available events before submitting periodic observation commands.
    ///
    /// # Errors
    ///
    /// Returns disconnection only after every queued event has been consumed.
    pub fn try_recv(&self) -> Result<Option<IdeWorkerEvent>, IdeWorkerEventReceiveError> {
        let Some(events) = &self.events else {
            return Err(IdeWorkerEventReceiveError);
        };
        match events.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(IdeWorkerEventReceiveError),
        }
    }

    /// Requests orderly project close and waits for worker termination.
    ///
    /// This disconnects the event receiver before waiting so reliable event
    /// backpressure cannot prevent shutdown. Any native close failure that could
    /// not be delivered as an event is returned in the terminal report.
    #[must_use]
    pub fn shutdown(mut self, timeout: Duration) -> IdeWorkerShutdownOutcome {
        let _ = self.commands.try_send(IdeWorkerCommand::Shutdown);
        drop(self.events.take());
        match self.completion.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                IdeWorkerShutdownOutcome::Stopped(self.join_terminal())
            }
            Err(RecvTimeoutError::Timeout) => {
                IdeWorkerShutdownOutcome::Pending(DesktopIdeWorkerPendingShutdown { worker: self })
            }
        }
    }

    fn join_terminal(&mut self) -> Result<IdeWorkerShutdownReport, IdeWorkerJoinError> {
        let Some(handle) = self.handle.take() else {
            return Ok(IdeWorkerShutdownReport::default());
        };
        let joined = handle.join();
        self.permit.take();
        joined.map_err(|_| IdeWorkerJoinError::ThreadPanicked)
    }
}

impl Drop for DesktopIdeWorker {
    fn drop(&mut self) {
        let _ = self.commands.try_send(IdeWorkerCommand::Shutdown);
        let Some(handle) = self.handle.take() else {
            return;
        };
        let Some(permit) = self.permit.take() else {
            std::process::abort();
        };
        if handle.is_finished() {
            let _ = handle.join();
        } else {
            permit.retain(handle);
        }
    }
}

#[derive(Debug)]
pub enum IdeWorkerShutdownOutcome {
    Stopped(Result<IdeWorkerShutdownReport, IdeWorkerJoinError>),
    Pending(DesktopIdeWorkerPendingShutdown),
}

#[derive(Debug)]
pub struct DesktopIdeWorkerPendingShutdown {
    worker: DesktopIdeWorker,
}

#[derive(Debug)]
pub enum IdeWorkerPendingShutdownWait {
    Stopped(Result<IdeWorkerShutdownReport, IdeWorkerJoinError>),
    TimedOut(DesktopIdeWorkerPendingShutdown),
}

impl DesktopIdeWorkerPendingShutdown {
    #[must_use]
    pub fn wait(self, timeout: Duration) -> IdeWorkerPendingShutdownWait {
        match self.worker.shutdown(timeout) {
            IdeWorkerShutdownOutcome::Stopped(result) => {
                IdeWorkerPendingShutdownWait::Stopped(result)
            }
            IdeWorkerShutdownOutcome::Pending(worker) => {
                IdeWorkerPendingShutdownWait::TimedOut(worker)
            }
        }
    }
}

#[allow(
    clippy::large_enum_variant,
    reason = "state transitions move the sole application owner without allocating after native startup or shutdown"
)]
enum WorkerState {
    Undiscovered(IdeApplicationConfig),
    Closed(DesktopIdeApplication),
    Running(RunningDesktopIdeApplication),
    SaveFence {
        application: RunningDesktopIdeApplication,
        sequence: u64,
        deadline: Instant,
    },
    RecordingFence {
        application: RunningDesktopIdeApplication,
        target: IdeRecordingTarget,
        sequence: u64,
        deadline: Instant,
    },
    RecordingClosing {
        pending: DesktopIdePendingSaveClose,
        target: IdeRecordingTarget,
    },
    Recording(RecordingDesktopIdeApplication),
    Saving(DesktopIdePendingSaveClose),
    Closing(DesktopIdePendingClose),
    Empty,
}

fn run_worker(
    mut state: WorkerState,
    commands: &Receiver<IdeWorkerCommand>,
    events: &SyncSender<IdeWorkerEvent>,
) -> IdeWorkerShutdownReport {
    let mut unpublished_native_failure = None;
    if !initialize_state(&mut state, events, &mut unpublished_native_failure) {
        return finish_worker(state, unpublished_native_failure);
    }
    loop {
        if !poll_pending_state(&mut state, events, &mut unpublished_native_failure) {
            return finish_worker(state, unpublished_native_failure);
        }
        let command = if matches!(
            state,
            WorkerState::SaveFence { .. }
                | WorkerState::RecordingFence { .. }
                | WorkerState::RecordingClosing { .. }
                | WorkerState::Closing(_)
                | WorkerState::Saving(_)
        ) {
            match commands.recv_timeout(WORKER_SHUTDOWN_POLL) {
                Ok(command) => command,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match commands.recv() {
                Ok(command) => command,
                Err(RecvError) => break,
            }
        };
        if matches!(command, IdeWorkerCommand::Shutdown) {
            break;
        }
        if !process_command(&mut state, command, events, &mut unpublished_native_failure) {
            break;
        }
    }
    finish_worker(state, unpublished_native_failure)
}

fn initialize_state(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    if matches!(state, WorkerState::Undiscovered(_)) {
        discover(
            state,
            events,
            IdeWorkerCommandKind::Initialize,
            unpublished_native_failure,
        )
    } else {
        publish_state(state, events, unpublished_native_failure)
    }
}

fn retry_discovery(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    discover(
        state,
        events,
        IdeWorkerCommandKind::RetryDiscovery,
        unpublished_native_failure,
    )
}

fn discover(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    let WorkerState::Undiscovered(config) = state else {
        return publish_failure(events, command, IdeWorkerCommandFailure::InvalidState);
    };
    match DesktopIdeApplication::discover(*config) {
        Ok(discovery) => {
            let (application, input_failure) = discovery.into_parts();
            *state = WorkerState::Closed(application);
            publish_closed_state(state, events, unpublished_native_failure, input_failure)
        }
        Err(reason) => send_event(
            events,
            IdeWorkerEvent::CommandFailed {
                command,
                reason: IdeWorkerCommandFailure::Discovery(reason),
            },
        ),
    }
}

fn publish_closed_state(
    state: &WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    input_failure: Option<IdeInputDiscoveryError>,
) -> bool {
    let WorkerState::Closed(application) = state else {
        return true;
    };
    match closed_snapshot(application) {
        Ok(snapshot) => {
            let event = IdeWorkerEvent::Closed {
                snapshot,
                native_failure: *unpublished_native_failure,
                input_failure,
            };
            if send_event(events, event) {
                *unpublished_native_failure = None;
                true
            } else {
                false
            }
        }
        Err(reason) => publish_failure(events, IdeWorkerCommandKind::Initialize, reason),
    }
}

fn process_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    let kind = command.kind();
    match command {
        IdeWorkerCommand::RetryDiscovery => {
            if matches!(state, WorkerState::Undiscovered(_)) {
                retry_discovery(state, events, unpublished_native_failure)
            } else {
                publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState)
            }
        }
        IdeWorkerCommand::RefreshOutputs => match state {
            WorkerState::Closed(application) => match application.refresh_outputs() {
                Ok(()) => publish_state(state, events, unpublished_native_failure),
                Err(reason) => {
                    publish_failure(events, kind, IdeWorkerCommandFailure::Discovery(reason))
                }
            },
            _ => publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState),
        },
        IdeWorkerCommand::SelectSystemDefault => match state {
            WorkerState::Closed(application) => {
                application.select_system_default();
                publish_state(state, events, unpublished_native_failure)
            }
            _ => publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState),
        },
        IdeWorkerCommand::SelectOutput(routing_id) => match state {
            WorkerState::Closed(application) => match application.select_output(&routing_id) {
                Ok(()) => publish_state(state, events, unpublished_native_failure),
                Err(reason) => publish_failure(
                    events,
                    kind,
                    IdeWorkerCommandFailure::OutputSelection(reason),
                ),
            },
            _ => publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState),
        },
        command @ (IdeWorkerCommand::RefreshInputs
        | IdeWorkerCommand::DisableInputMonitoring
        | IdeWorkerCommand::SelectSystemDefaultInput
        | IdeWorkerCommand::SelectInput(_)) => {
            process_input_command(state, command, events, unpublished_native_failure)
        }
        IdeWorkerCommand::OpenProject(path) => {
            if !matches!(state, WorkerState::Closed(_)) {
                return publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState);
            }
            let WorkerState::Closed(application) = std::mem::replace(state, WorkerState::Empty)
            else {
                unreachable!("closed state was checked before ownership transfer");
            };
            match application.open_project(path) {
                Ok(running) => {
                    *state = WorkerState::Running(running);
                    publish_state(state, events, unpublished_native_failure)
                }
                Err(failure) => {
                    let (application, reason) = failure.into_parts();
                    *state = WorkerState::Closed(application);
                    publish_failure(events, kind, IdeWorkerCommandFailure::ProjectOpen(reason))
                }
            }
        }
        IdeWorkerCommand::Transport(command) => match state {
            WorkerState::Running(application) => match application.try_send_transport(command) {
                Ok(()) => send_event(events, IdeWorkerEvent::TransportQueued(command)),
                Err(reason) => {
                    publish_failure(events, kind, IdeWorkerCommandFailure::Transport(reason))
                }
            },
            _ => publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState),
        },
        IdeWorkerCommand::Mixer(command) => match state {
            WorkerState::Running(application) => match application.try_send_mixer(command) {
                Ok(()) => send_event(events, IdeWorkerEvent::MixerQueued(command)),
                Err(reason) => {
                    publish_failure(events, kind, IdeWorkerCommandFailure::Mixer(reason))
                }
            },
            _ => publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState),
        },
        IdeWorkerCommand::Monitoring(command) => process_monitoring_command(state, command, events),
        IdeWorkerCommand::SaveProject => save_project(state, events),
        IdeWorkerCommand::StartRecording { channel_index } => {
            start_recording(state, channel_index, events)
        }
        IdeWorkerCommand::StopRecording => finalize_recording(state, events),
        IdeWorkerCommand::AbortRecording => abort_recording(state, events),
        IdeWorkerCommand::Observe => publish_state(state, events, unpublished_native_failure),
        IdeWorkerCommand::CloseProject => close_project(
            state,
            events,
            unpublished_native_failure,
            IdeWorkerCommandKind::CloseProject,
        ),
        IdeWorkerCommand::Shutdown => unreachable!("shutdown is handled by the worker loop"),
    }
}

fn process_monitoring_command(
    state: &mut WorkerState,
    command: IdeMonitoringCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let result = match state {
        WorkerState::Running(application) => application.apply_monitoring(command),
        WorkerState::Recording(application) => application.apply_monitoring(command),
        _ => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::Monitoring,
                IdeWorkerCommandFailure::InvalidState,
            );
        }
    };
    match result {
        Ok(()) => send_event(events, IdeWorkerEvent::MonitoringApplied(command)),
        Err(reason) => publish_failure(
            events,
            IdeWorkerCommandKind::Monitoring,
            IdeWorkerCommandFailure::Monitoring(reason),
        ),
    }
}

fn process_input_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    let kind = command.kind();
    let WorkerState::Closed(application) = state else {
        return publish_failure(events, kind, IdeWorkerCommandFailure::InvalidState);
    };
    let result = match command {
        IdeWorkerCommand::RefreshInputs => application
            .refresh_inputs()
            .map_err(IdeWorkerCommandFailure::InputDiscovery),
        IdeWorkerCommand::DisableInputMonitoring => {
            application.disable_input_monitoring();
            Ok(())
        }
        IdeWorkerCommand::SelectSystemDefaultInput => application
            .select_system_default_input()
            .map_err(IdeWorkerCommandFailure::InputSelection),
        IdeWorkerCommand::SelectInput(routing_id) => application
            .select_input(&routing_id)
            .map_err(IdeWorkerCommandFailure::InputSelection),
        _ => unreachable!("caller admits only input commands"),
    };
    match result {
        Ok(()) => publish_state(state, events, unpublished_native_failure),
        Err(reason) => publish_failure(events, kind, reason),
    }
}

fn start_recording(
    state: &mut WorkerState,
    channel_index: usize,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let WorkerState::Running(application) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartRecording,
            IdeWorkerCommandFailure::InvalidState,
        );
    };
    if !application.can_save_mixer() {
        return publish_recording_failure(
            events,
            IdeWorkerCommandKind::StartRecording,
            IdeRecordingError::Preparation(
                super::IdeRecordingPreparationError::ProjectSnapshotUnavailable,
            ),
        );
    }
    let target = match application.prepare_recording_target(channel_index) {
        Ok(target) => target,
        Err(source) => {
            return publish_recording_failure(
                events,
                IdeWorkerCommandKind::StartRecording,
                IdeRecordingError::Preparation(source),
            );
        }
    };
    if let Err(source) = application.try_send_transport(TransportCommand::Stop) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartRecording,
            IdeWorkerCommandFailure::Transport(source),
        );
    }
    let sequence = match application.try_send_mixer_fence() {
        Ok(Some(sequence)) => sequence,
        Ok(None) => {
            return publish_recording_failure(
                events,
                IdeWorkerCommandKind::StartRecording,
                IdeRecordingError::MixerSave(Box::new(
                    IdeProjectSaveError::MixerFenceSequenceUnavailable,
                )),
            );
        }
        Err(source) => {
            return publish_recording_failure(
                events,
                IdeWorkerCommandKind::StartRecording,
                IdeRecordingError::MixerSave(Box::new(IdeProjectSaveError::MixerCommit(source))),
            );
        }
    };
    let WorkerState::Running(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("running state was checked before recording ownership transfer");
    };
    *state = WorkerState::RecordingFence {
        application,
        target,
        sequence,
        deadline: Instant::now() + MIXER_SAVE_COMMIT_TIMEOUT,
    };
    send_event(events, IdeWorkerEvent::RecordingPreparing)
}

fn begin_recording_close(
    state: &mut WorkerState,
    application: RunningDesktopIdeApplication,
    target: IdeRecordingTarget,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match application.close_for_save() {
        IdeSaveCloseOutcome::Closed {
            application,
            recovery,
            failure,
        } => complete_recording_start(state, application, recovery, target, failure, events),
        IdeSaveCloseOutcome::Pending(pending) => {
            *state = WorkerState::RecordingClosing { pending, target };
            send_event(events, IdeWorkerEvent::RecordingPreparing)
        }
        IdeSaveCloseOutcome::Unavailable {
            application,
            failure,
        } => {
            let observed_frame = target.observed_frame();
            reopen_after_recording_failure(
                state,
                application,
                target.project_path(),
                observed_frame,
                failure.map_or(
                    IdeRecordingError::Preparation(
                        super::IdeRecordingPreparationError::ProjectSnapshotUnavailable,
                    ),
                    IdeRecordingError::NativeClose,
                ),
                events,
                IdeWorkerCommandKind::StartRecording,
            )
        }
    }
}

fn complete_recording_start(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    recovery: DesktopPlaybackRecovery,
    target: IdeRecordingTarget,
    native_failure: Option<IdeNativeCloseFailure>,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let project_path = target.project_path().to_path_buf();
    let start_frame = recovery.checkpoint().position_frame();
    if let Some(failure) = native_failure {
        drop(recovery);
        return reopen_after_recording_failure(
            state,
            application,
            &project_path,
            start_frame,
            IdeRecordingError::NativeClose(failure),
            events,
            IdeWorkerCommandKind::StartRecording,
        );
    }
    let plan = match IdeRecordingPlan::prepare(target, &recovery) {
        Ok(plan) => plan,
        Err(source) => {
            drop(recovery);
            return reopen_after_recording_failure(
                state,
                application,
                &project_path,
                start_frame,
                IdeRecordingError::Preparation(source),
                events,
                IdeWorkerCommandKind::StartRecording,
            );
        }
    };
    let publication = RunningDesktopIdeApplication::prepare_mixer_save(&application, &recovery)
        .and_then(|save| {
            save.publish()
                .map_err(|source| IdeProjectSaveError::Publication(Box::new(source)))
        });
    drop(recovery);
    if let Err(source) = publication {
        return reopen_after_recording_failure(
            state,
            application,
            &project_path,
            start_frame,
            IdeRecordingError::MixerSave(Box::new(source)),
            events,
            IdeWorkerCommandKind::StartRecording,
        );
    }
    match application.start_recording(plan) {
        Ok(recording) => {
            *state = WorkerState::Recording(recording);
            let mut no_native_failure = None;
            publish_state(state, events, &mut no_native_failure)
        }
        Err(failure) => {
            let super::recording::IdeRecordingStartFailure {
                application,
                project_path,
                start_frame,
                reason,
            } = *failure;
            reopen_after_recording_failure(
                state,
                application,
                &project_path,
                start_frame,
                IdeRecordingError::Start(Box::new(reason)),
                events,
                IdeWorkerCommandKind::StartRecording,
            )
        }
    }
}

fn finalize_recording(state: &mut WorkerState, events: &SyncSender<IdeWorkerEvent>) -> bool {
    if !matches!(state, WorkerState::Recording(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StopRecording,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    if !send_event(events, IdeWorkerEvent::RecordingFinalizing) {
        return false;
    }
    let WorkerState::Recording(recording) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("recording state was checked before finalization");
    };
    match recording.finalize() {
        Ok(finalized) => reopen_after_recording_import(state, finalized, events),
        Err(failure) => {
            let super::recording::IdeRecordingFinalizeFailure {
                application,
                project_path,
                start_frame,
                reason,
            } = *failure;
            reopen_after_recording_failure(
                state,
                application,
                &project_path,
                start_frame,
                IdeRecordingError::Finalize(Box::new(reason)),
                events,
                IdeWorkerCommandKind::StopRecording,
            )
        }
    }
}

fn abort_recording(state: &mut WorkerState, events: &SyncSender<IdeWorkerEvent>) -> bool {
    if !matches!(state, WorkerState::Recording(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AbortRecording,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    if !send_event(events, IdeWorkerEvent::RecordingFinalizing) {
        return false;
    }
    let WorkerState::Recording(recording) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("recording state was checked before abort");
    };
    match recording.abort() {
        Ok(aborted) => reopen_after_recording_abort(state, aborted, events),
        Err(failure) => {
            let super::recording::IdeRecordingAbortFailure {
                application,
                project_path,
                start_frame,
                reason,
            } = *failure;
            reopen_after_recording_failure(
                state,
                application,
                &project_path,
                start_frame,
                IdeRecordingError::Abort(Box::new(reason)),
                events,
                IdeWorkerCommandKind::AbortRecording,
            )
        }
    }
}

fn reopen_after_recording_failure(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    project_path: &std::path::Path,
    start_frame: usize,
    operation: IdeRecordingError,
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
) -> bool {
    match application.open_project(project_path) {
        Ok(mut running) => {
            let restore = running.try_send_transport(TransportCommand::Locate {
                frame: start_frame,
                state: LocateState::Stopped,
            });
            *state = WorkerState::Running(running);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            let reason = match restore {
                Ok(()) => operation,
                Err(restore) => IdeRecordingError::RestoreAfterFailure {
                    operation: Box::new(operation),
                    restore,
                },
            };
            publish_recording_failure(events, command, reason)
        }
        Err(failure) => {
            let (application, reopen) = failure.into_parts();
            *state = WorkerState::Closed(application);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            publish_recording_failure(
                events,
                command,
                IdeRecordingError::ReopenAfterFailure {
                    operation: Box::new(operation),
                    reopen: Box::new(reopen),
                },
            )
        }
    }
}

fn reopen_after_recording_import(
    state: &mut WorkerState,
    finalized: super::recording::IdeRecordingFinalized,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match finalized.application.open_project(&finalized.project_path) {
        Ok(mut running) => {
            let restore = running.try_send_transport(TransportCommand::Locate {
                frame: finalized.start_frame,
                state: LocateState::Stopped,
            });
            *state = WorkerState::Running(running);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure)
                || !send_event(events, IdeWorkerEvent::RecordingImported(finalized.outcome))
            {
                return false;
            }
            match restore {
                Ok(()) => true,
                Err(source) => publish_recording_failure(
                    events,
                    IdeWorkerCommandKind::StopRecording,
                    IdeRecordingError::Restore(source),
                ),
            }
        }
        Err(failure) => {
            let (application, reopen) = failure.into_parts();
            *state = WorkerState::Closed(application);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            publish_recording_failure(
                events,
                IdeWorkerCommandKind::StopRecording,
                IdeRecordingError::ImportedButReopenFailed {
                    outcome: Box::new(finalized.outcome),
                    reopen: Box::new(reopen),
                },
            )
        }
    }
}

fn reopen_after_recording_abort(
    state: &mut WorkerState,
    aborted: super::recording::IdeRecordingAborted,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match aborted.application.open_project(&aborted.project_path) {
        Ok(mut running) => {
            let restore = running.try_send_transport(TransportCommand::Locate {
                frame: aborted.start_frame,
                state: LocateState::Stopped,
            });
            *state = WorkerState::Running(running);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure)
                || !send_event(events, IdeWorkerEvent::RecordingAborted(aborted.outcome))
            {
                return false;
            }
            match restore {
                Ok(()) => true,
                Err(source) => publish_recording_failure(
                    events,
                    IdeWorkerCommandKind::AbortRecording,
                    IdeRecordingError::Restore(source),
                ),
            }
        }
        Err(failure) => {
            let (application, reopen) = failure.into_parts();
            *state = WorkerState::Closed(application);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            publish_recording_failure(
                events,
                IdeWorkerCommandKind::AbortRecording,
                IdeRecordingError::AbortedButReopenFailed {
                    outcome: Box::new(aborted.outcome),
                    reopen: Box::new(reopen),
                },
            )
        }
    }
}

fn close_project(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    command: IdeWorkerCommandKind,
) -> bool {
    if !matches!(state, WorkerState::Running(_)) {
        return publish_failure(events, command, IdeWorkerCommandFailure::InvalidState);
    }
    let WorkerState::Running(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("running state was checked before ownership transfer");
    };
    match application.close() {
        IdeCloseOutcome::Closed {
            application,
            failure,
        } => {
            *state = WorkerState::Closed(application);
            *unpublished_native_failure = failure;
            publish_state(state, events, unpublished_native_failure)
        }
        IdeCloseOutcome::Pending(pending) => {
            *state = WorkerState::Closing(pending);
            send_event(events, IdeWorkerEvent::Closing)
        }
    }
}

fn save_project(state: &mut WorkerState, events: &SyncSender<IdeWorkerEvent>) -> bool {
    let WorkerState::Running(application) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SaveProject,
            IdeWorkerCommandFailure::InvalidState,
        );
    };
    if !application.can_save_mixer() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SaveProject,
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::ProjectSnapshotUnavailable),
        );
    }
    let sequence = match application.try_send_mixer_fence() {
        Ok(Some(sequence)) => sequence,
        Ok(None) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::SaveProject,
                IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::MixerFenceSequenceUnavailable,
                ),
            );
        }
        Err(source) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::SaveProject,
                IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::MixerCommit(source)),
            );
        }
    };
    let WorkerState::Running(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("running state was checked before save ownership transfer");
    };
    *state = WorkerState::SaveFence {
        application,
        sequence,
        deadline: Instant::now() + MIXER_SAVE_COMMIT_TIMEOUT,
    };
    send_event(events, IdeWorkerEvent::Saving)
}

fn begin_save_close(
    state: &mut WorkerState,
    application: RunningDesktopIdeApplication,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match application.close_for_save() {
        IdeSaveCloseOutcome::Closed {
            application,
            recovery,
            failure,
        } => complete_save(state, application, recovery, failure, events),
        IdeSaveCloseOutcome::Pending(pending) => {
            *state = WorkerState::Saving(pending);
            send_event(events, IdeWorkerEvent::Saving)
        }
        IdeSaveCloseOutcome::Unavailable {
            application,
            failure,
        } => publish_closed_save_failure(
            state,
            application,
            events,
            failure.map_or(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
                IdeProjectSaveError::NativeClose,
            ),
        ),
    }
}

fn publish_closed_save_failure(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    events: &SyncSender<IdeWorkerEvent>,
    reason: IdeProjectSaveError,
) -> bool {
    *state = WorkerState::Closed(application);
    let mut no_native_failure = None;
    if !publish_state(state, events, &mut no_native_failure) {
        return false;
    }
    publish_failure(
        events,
        IdeWorkerCommandKind::SaveProject,
        IdeWorkerCommandFailure::ProjectSave(reason),
    )
}

fn complete_save(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    recovery: DesktopPlaybackRecovery,
    native_failure: Option<IdeNativeCloseFailure>,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let Some(project_path) = recovery
        .prepared_project()
        .map(|project| project.package().root().to_path_buf())
    else {
        return publish_closed_save_failure(
            state,
            application,
            events,
            IdeProjectSaveError::ProjectSnapshotUnavailable,
        );
    };
    let attempt = if let Some(failure) = native_failure {
        Err(IdeProjectSaveError::NativeClose(failure))
    } else {
        RunningDesktopIdeApplication::prepare_mixer_save(&application, &recovery).and_then(|plan| {
            plan.publish()
                .map_err(|source| IdeProjectSaveError::Publication(Box::new(source)))
        })
    };
    drop(recovery);
    match application.open_project(&project_path) {
        Ok(running) => {
            *state = WorkerState::Running(running);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            match attempt {
                Ok(outcome) => send_event(
                    events,
                    IdeWorkerEvent::ProjectSaved {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                    },
                ),
                Err(reason) => publish_failure(
                    events,
                    IdeWorkerCommandKind::SaveProject,
                    IdeWorkerCommandFailure::ProjectSave(reason),
                ),
            }
        }
        Err(failure) => {
            let (application, reopen) = failure.into_parts();
            *state = WorkerState::Closed(application);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            let reason = match attempt {
                Ok(outcome) => IdeProjectSaveError::ReopenAfterPublication {
                    outcome,
                    source: Box::new(reopen),
                },
                Err(IdeProjectSaveError::Publication(publication)) => {
                    IdeProjectSaveError::PublicationAndReopen {
                        publication,
                        reopen: Box::new(reopen),
                    }
                }
                Err(IdeProjectSaveError::NativeClose(close)) => {
                    IdeProjectSaveError::NativeCloseAndReopen {
                        close,
                        reopen: Box::new(reopen),
                    }
                }
                Err(preparation) => IdeProjectSaveError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            };
            publish_failure(
                events,
                IdeWorkerCommandKind::SaveProject,
                IdeWorkerCommandFailure::ProjectSave(reason),
            )
        }
    }
}

fn poll_pending_state(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    match std::mem::replace(state, WorkerState::Empty) {
        WorkerState::Closing(pending) => match pending.wait(Duration::ZERO) {
            IdePendingCloseWait::Closed {
                application,
                failure,
            } => {
                *state = WorkerState::Closed(application);
                *unpublished_native_failure = failure;
                publish_state(state, events, unpublished_native_failure)
            }
            IdePendingCloseWait::TimedOut(pending) => {
                *state = WorkerState::Closing(pending);
                true
            }
        },
        WorkerState::Saving(pending) => match pending.wait(Duration::ZERO) {
            IdePendingSaveCloseWait::Closed {
                application,
                recovery,
                failure,
            } => complete_save(state, application, recovery, failure, events),
            IdePendingSaveCloseWait::TimedOut(pending) => {
                *state = WorkerState::Saving(pending);
                true
            }
        },
        WorkerState::RecordingClosing { pending, target } => match pending.wait(Duration::ZERO) {
            IdePendingSaveCloseWait::Closed {
                application,
                recovery,
                failure,
            } => complete_recording_start(state, application, recovery, target, failure, events),
            IdePendingSaveCloseWait::TimedOut(pending) => {
                *state = WorkerState::RecordingClosing { pending, target };
                true
            }
        },
        WorkerState::SaveFence {
            application,
            sequence,
            deadline,
        } => {
            if application.committed_mixer_fence() >= sequence {
                begin_save_close(state, application, events)
            } else if Instant::now() >= deadline {
                *state = WorkerState::Running(application);
                publish_failure(
                    events,
                    IdeWorkerCommandKind::SaveProject,
                    IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::MixerCommitTimeout),
                )
            } else {
                *state = WorkerState::SaveFence {
                    application,
                    sequence,
                    deadline,
                };
                true
            }
        }
        WorkerState::RecordingFence {
            application,
            target,
            sequence,
            deadline,
        } => {
            if application.committed_mixer_fence() >= sequence {
                begin_recording_close(state, application, target, events)
            } else if Instant::now() >= deadline {
                *state = WorkerState::Running(application);
                publish_recording_failure(
                    events,
                    IdeWorkerCommandKind::StartRecording,
                    IdeRecordingError::MixerSave(Box::new(IdeProjectSaveError::MixerCommitTimeout)),
                )
            } else {
                *state = WorkerState::RecordingFence {
                    application,
                    target,
                    sequence,
                    deadline,
                };
                true
            }
        }
        current => {
            *state = current;
            true
        }
    }
}

fn publish_state(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    let event = match state {
        WorkerState::Closed(application) => match closed_snapshot(application) {
            Ok(snapshot) => IdeWorkerEvent::Closed {
                snapshot,
                native_failure: *unpublished_native_failure,
                input_failure: None,
            },
            Err(reason) => {
                let delivered = publish_failure(events, IdeWorkerCommandKind::Observe, reason);
                // A close failure must remain the only outstanding teardown result.
                // Stop rather than allowing another project to open before that
                // evidence can be returned by the terminal join report.
                return delivered && unpublished_native_failure.is_none();
            }
        },
        WorkerState::Running(application) => match running_observation(application) {
            Ok(observation) => IdeWorkerEvent::Running(observation),
            Err(reason) => {
                return publish_failure(events, IdeWorkerCommandKind::Observe, reason);
            }
        },
        WorkerState::Recording(application) => match recording_observation(application) {
            Ok(observation) => IdeWorkerEvent::Recording(observation),
            Err(reason) => {
                return publish_failure(events, IdeWorkerCommandKind::Observe, reason);
            }
        },
        WorkerState::Closing(_) => IdeWorkerEvent::Closing,
        WorkerState::SaveFence { .. } | WorkerState::Saving(_) => IdeWorkerEvent::Saving,
        WorkerState::RecordingFence { .. } | WorkerState::RecordingClosing { .. } => {
            IdeWorkerEvent::RecordingPreparing
        }
        WorkerState::Undiscovered(_) | WorkerState::Empty => return true,
    };
    if send_event(events, event) {
        if matches!(state, WorkerState::Closed(_)) {
            *unpublished_native_failure = None;
        }
        true
    } else {
        false
    }
}

fn closed_snapshot(
    application: &DesktopIdeApplication,
) -> Result<IdeClosedSnapshot, IdeWorkerCommandFailure> {
    let outputs = application
        .outputs()
        .try_to_owned()
        .map_err(IdeWorkerCommandFailure::Publication)?;
    let output_selection = match application.output_selection() {
        IdeOutputSelection::SystemDefault => IdeOutputSelection::SystemDefault,
        IdeOutputSelection::Routed(routing_id) => IdeOutputSelection::Routed(
            super::try_owned(routing_id).map_err(IdeWorkerCommandFailure::Publication)?,
        ),
    };
    let inputs = application
        .inputs()
        .map(IdeAudioInputInventory::try_to_owned)
        .transpose()
        .map_err(IdeWorkerCommandFailure::InputPublication)?;
    let input_selection = match application.input_selection() {
        IdeInputSelection::Disabled => IdeInputSelection::Disabled,
        IdeInputSelection::SystemDefault => IdeInputSelection::SystemDefault,
        IdeInputSelection::Routed(routing_id) => IdeInputSelection::Routed(
            super::try_input_owned(routing_id)
                .map_err(IdeWorkerCommandFailure::InputPublication)?,
        ),
    };
    Ok(IdeClosedSnapshot {
        outputs,
        output_selection,
        inputs,
        input_selection,
    })
}

fn running_observation(
    application: &mut RunningDesktopIdeApplication,
) -> Result<IdeRunningObservation, IdeWorkerCommandFailure> {
    let snapshot = application
        .snapshot()
        .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
    let project_id = try_project_owned(snapshot.project_id)?;
    let project_name = try_project_owned(snapshot.project_name)?;
    let mut scenes = Vec::new();
    scenes
        .try_reserve_exact(snapshot.scene_count)
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for scene_index in 0..snapshot.scene_count {
        scenes.push(IdeScene {
            name: try_project_owned(snapshot.scene_names[scene_index])?,
            availability: snapshot.scene_availability[scene_index],
        });
    }
    let mut mixer_channels = Vec::new();
    mixer_channels
        .try_reserve_exact(snapshot.mixer_channel_count)
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for channel_index in 0..snapshot.mixer_channel_count {
        let mut sends = Vec::new();
        sends
            .try_reserve_exact(snapshot.mixer_send_counts[channel_index])
            .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
        for send_index in 0..snapshot.mixer_send_counts[channel_index] {
            sends.push(IdeMixerSend {
                target_name: try_project_owned(
                    snapshot.mixer_send_target_names[channel_index][send_index],
                )?,
                tap: snapshot.mixer_send_taps[channel_index][send_index],
                state: snapshot.mixer_send_states[channel_index][send_index],
            });
        }
        mixer_channels.push(IdeMixerChannel {
            id: try_project_owned(snapshot.mixer_channel_ids[channel_index])?,
            name: try_project_owned(snapshot.mixer_channel_names[channel_index])?,
            kind: snapshot.mixer_channel_kinds[channel_index],
            state: snapshot.mixer_channel_states[channel_index],
            sends,
        });
    }
    Ok(IdeRunningObservation {
        project_id,
        project_name,
        sample_rate_hz: snapshot.sample_rate_hz,
        scenes,
        mixer_channels,
        mixer_main: snapshot.mixer_main_state,
        output_state: snapshot.output_state,
        output: snapshot.output,
        monitoring: snapshot.monitoring,
        recovery: snapshot.recovery,
        meter: application.read_latest_meter(),
    })
}

fn recording_observation(
    application: &RecordingDesktopIdeApplication,
) -> Result<IdeRecordingState, IdeWorkerCommandFailure> {
    Ok(IdeRecordingState {
        project_id: try_project_owned(application.project_id())?,
        project_name: try_project_owned(application.project_name())?,
        track_id: try_project_owned(application.track_id())?,
        track_name: try_project_owned(application.track_name())?,
        sample_rate_hz: application.sample_rate_hz(),
        start_frame: application.start_frame(),
        start_tick: application.start_tick(),
        journal_path: application.journal_path().to_path_buf(),
        observation: application.observation(),
    })
}

fn try_project_owned(value: &str) -> Result<String, IdeWorkerCommandFailure> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

fn publish_failure(
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
    reason: IdeWorkerCommandFailure,
) -> bool {
    send_event(events, IdeWorkerEvent::CommandFailed { command, reason })
}

fn publish_recording_failure(
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
    reason: IdeRecordingError,
) -> bool {
    publish_failure(events, command, IdeWorkerCommandFailure::Recording(reason))
}

fn send_event(events: &SyncSender<IdeWorkerEvent>, event: IdeWorkerEvent) -> bool {
    events.send(event).is_ok()
}

fn finish_worker(
    state: WorkerState,
    unpublished_native_failure: Option<IdeNativeCloseFailure>,
) -> IdeWorkerShutdownReport {
    let (shutdown_native_failure, recording_abort) = shutdown_state(state);
    IdeWorkerShutdownReport {
        native_failure: unpublished_native_failure.or(shutdown_native_failure),
        recording_abort,
    }
}

fn shutdown_state(
    mut state: WorkerState,
) -> (
    Option<IdeNativeCloseFailure>,
    Option<Result<IdeRecordingAbortOutcome, IdeRecordingAbortError>>,
) {
    loop {
        state = match state {
            WorkerState::Running(application) => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return (failure, None),
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::SaveFence { application, .. } => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return (failure, None),
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::RecordingFence { application, .. } => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return (failure, None),
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Closing(pending) => match pending.wait(WORKER_SHUTDOWN_POLL) {
                IdePendingCloseWait::Closed { failure, .. } => return (failure, None),
                IdePendingCloseWait::TimedOut(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Saving(pending) => match pending.wait(WORKER_SHUTDOWN_POLL) {
                IdePendingSaveCloseWait::Closed { failure, .. } => return (failure, None),
                IdePendingSaveCloseWait::TimedOut(pending) => WorkerState::Saving(pending),
            },
            WorkerState::RecordingClosing { pending, target } => {
                match pending.wait(WORKER_SHUTDOWN_POLL) {
                    IdePendingSaveCloseWait::Closed { failure, .. } => return (failure, None),
                    IdePendingSaveCloseWait::TimedOut(pending) => {
                        WorkerState::RecordingClosing { pending, target }
                    }
                }
            }
            WorkerState::Recording(recording) => {
                return match recording.abort() {
                    Ok(aborted) => (None, Some(Ok(aborted.outcome))),
                    Err(failure) => (None, Some(Err(failure.reason))),
                };
            }
            WorkerState::Undiscovered(_) | WorkerState::Closed(_) | WorkerState::Empty => {
                return (None, None);
            }
        };
    }
}

struct CompletionSignal(SyncSender<()>);

impl Drop for CompletionSignal {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IdeAudioOutputInventory, IdePlaybackSessionOwner, TestPlaybackSession};
    use hyper_audio_source::DecodeLimits;
    use hyper_media_cache::CacheLimits;
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;
    use hyper_project_loader::ProjectPlaybackLimits;

    const WAIT: Duration = Duration::from_secs(2);

    fn application_config() -> IdeApplicationConfig {
        IdeApplicationConfig::try_new(
            256,
            ProjectPlaybackLimits {
                max_total_resource_bytes: 1_048_576,
                audio: DecodeLimits::new(1_048_576, 48_000, 384_000).unwrap(),
                session: PlaybackSessionAssemblyLimits {
                    documents: LoaderLimits::VERSION_1,
                    media_cache: CacheLimits {
                        max_assets: 8,
                        max_frames_per_asset: 48_000,
                        max_decoded_sample_bytes: 3_072_000,
                    },
                },
            },
        )
        .unwrap()
    }

    fn closed_application() -> DesktopIdeApplication {
        DesktopIdeApplication::from_inventories(
            application_config(),
            IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap(),
            IdeAudioInputInventory::try_new(&[("input-a", "Input A")]).unwrap(),
        )
    }

    fn recv(worker: &DesktopIdeWorker) -> IdeWorkerEvent {
        worker.events.as_ref().unwrap().recv_timeout(WAIT).unwrap()
    }

    fn shutdown_report(worker: DesktopIdeWorker) -> IdeWorkerShutdownReport {
        match worker.shutdown(WAIT) {
            IdeWorkerShutdownOutcome::Stopped(Ok(report)) => report,
            outcome => panic!("worker did not stop cleanly: {outcome:?}"),
        }
    }

    #[test]
    fn worker_validates_bounds_before_starting_a_thread() {
        assert!(matches!(
            DesktopIdeWorker::start(application_config(), 0, 1),
            Err(IdeWorkerStartError::InvalidCommandCapacity { actual: 0, .. })
        ));
        assert!(matches!(
            DesktopIdeWorker::start(application_config(), 1, 0),
            Err(IdeWorkerStartError::InvalidEventCapacity { actual: 0, .. })
        ));
        assert!(matches!(
            DesktopIdeWorker::start(application_config(), MAX_IDE_WORKER_COMMANDS + 1, 1),
            Err(IdeWorkerStartError::InvalidCommandCapacity { .. })
        ));
        assert!(matches!(
            DesktopIdeWorker::start(application_config(), 1, MAX_IDE_WORKER_EVENTS + 1),
            Err(IdeWorkerStartError::InvalidEventCapacity { .. })
        ));
    }

    #[test]
    fn closed_worker_publishes_selection_and_retains_failed_project_open() {
        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 4, 4).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::SelectOutput("route-a".to_owned()))
            .unwrap();
        let IdeWorkerEvent::Closed { snapshot, .. } = recv(&worker) else {
            panic!("selection must publish closed state");
        };
        assert_eq!(
            snapshot.selection(),
            &IdeOutputSelection::Routed("route-a".to_owned())
        );

        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Start))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Transport,
                reason: IdeWorkerCommandFailure::InvalidState,
            }
        ));
        worker.try_send(IdeWorkerCommand::Observe).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));

        let missing = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-worker-missing-project-{}",
            std::process::id()
        ));
        assert!(!missing.exists());
        worker
            .try_send(IdeWorkerCommand::OpenProject(missing))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::OpenProject,
                reason: IdeWorkerCommandFailure::ProjectOpen(_),
            }
        ));
        worker.try_send(IdeWorkerCommand::Observe).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn closed_worker_publishes_explicit_input_monitoring_selection() {
        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 4, 4).unwrap();
        let IdeWorkerEvent::Closed { snapshot, .. } = recv(&worker) else {
            panic!("initial closed state must be published");
        };
        assert_eq!(snapshot.input_selection(), &IdeInputSelection::Disabled);
        assert_eq!(snapshot.inputs().unwrap().inputs().len(), 1);

        worker
            .try_send(IdeWorkerCommand::SelectSystemDefaultInput)
            .unwrap();
        let IdeWorkerEvent::Closed { snapshot, .. } = recv(&worker) else {
            panic!("system input selection must publish closed state");
        };
        assert_eq!(
            snapshot.input_selection(),
            &IdeInputSelection::SystemDefault
        );

        worker
            .try_send(IdeWorkerCommand::SelectInput("input-a".to_owned()))
            .unwrap();
        let IdeWorkerEvent::Closed { snapshot, .. } = recv(&worker) else {
            panic!("routed input selection must publish closed state");
        };
        assert_eq!(
            snapshot.input_selection(),
            &IdeInputSelection::Routed("input-a".to_owned())
        );

        worker
            .try_send(IdeWorkerCommand::DisableInputMonitoring)
            .unwrap();
        let IdeWorkerEvent::Closed { snapshot, .. } = recv(&worker) else {
            panic!("monitor disable must publish closed state");
        };
        assert_eq!(snapshot.input_selection(), &IdeInputSelection::Disabled);
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn closed_publication_reports_input_discovery_failure_without_hiding_playback() {
        let state = WorkerState::Closed(closed_application());
        let (events, receiver) = sync_channel(1);
        let mut unpublished_native_failure = None;

        assert!(publish_closed_state(
            &state,
            &events,
            &mut unpublished_native_failure,
            Some(IdeInputDiscoveryError::Inventory(
                InputInventoryError::EmptyRoutingId,
            )),
        ));
        assert!(matches!(
            receiver.recv_timeout(WAIT).unwrap(),
            IdeWorkerEvent::Closed {
                snapshot,
                native_failure: None,
                input_failure: Some(IdeInputDiscoveryError::Inventory(
                    InputInventoryError::EmptyRoutingId,
                )),
            } if snapshot.outputs().outputs().len() == 1
        ));
    }

    #[test]
    fn running_worker_rejects_monitoring_when_no_input_cue_is_active() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 2).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::Monitoring(IdeMonitoringCommand::Mute {
                muted: true,
            }))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Monitoring,
                reason: IdeWorkerCommandFailure::Monitoring(IdeMonitoringCommandError::Unavailable,),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Stop))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Stop)
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn recording_preparation_failure_retains_the_running_owner() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 3, 3).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::StartRecording { channel_index: 0 })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::StartRecording,
                reason: IdeWorkerCommandFailure::Recording(IdeRecordingError::Preparation(
                    crate::IdeRecordingPreparationError::ProjectSnapshotUnavailable
                )),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Stop))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Stop)
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn recording_commands_are_rejected_while_closed() {
        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 4, 4).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        for command in [
            IdeWorkerCommand::StartRecording { channel_index: 0 },
            IdeWorkerCommand::StopRecording,
            IdeWorkerCommand::AbortRecording,
        ] {
            let kind = command.kind();
            worker.try_send(command).unwrap();
            assert!(matches!(
                recv(&worker),
                IdeWorkerEvent::CommandFailed {
                    command,
                    reason: IdeWorkerCommandFailure::InvalidState,
                } if command == kind
            ));
        }
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn running_worker_queues_runtime_commands_and_closes_before_shutdown() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 4, 4).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));
        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Start))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Start)
        ));
        worker
            .try_send(IdeWorkerCommand::Mixer(IdeMixerCommand::MainMute {
                muted: true,
            }))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::MixerQueued(IdeMixerCommand::MainMute { muted: true })
        ));
        worker.try_send(IdeWorkerCommand::SaveProject).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SaveProject,
                reason: IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::ProjectSnapshotUnavailable
                ),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Stop))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Stop)
        ));
        worker.try_send(IdeWorkerCommand::CloseProject).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn save_waits_for_a_committed_mixer_fence_before_closing() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 4).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker.try_send(IdeWorkerCommand::SaveProject).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Saving));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SaveProject,
                reason: IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::ProjectSnapshotUnavailable
                ),
            }
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn save_fence_timeout_retains_the_running_application() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: false,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 4).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker.try_send(IdeWorkerCommand::SaveProject).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Saving));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SaveProject,
                reason: IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::MixerCommitTimeout
                ),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::Transport(TransportCommand::Stop))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Stop)
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn exhausted_save_fence_sequence_does_not_change_running_state() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                next_mixer_fence: 0,
                ..TestPlaybackSession::default()
            }),
        };
        let mut state = WorkerState::Running(running);
        let (events, receiver) = sync_channel(1);

        assert!(save_project(&mut state, &events));
        assert!(matches!(state, WorkerState::Running(_)));
        assert!(matches!(
            receiver.recv_timeout(WAIT).unwrap(),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SaveProject,
                reason: IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::MixerFenceSequenceUnavailable
                ),
            }
        ));
    }

    #[test]
    fn terminal_save_failure_publishes_closed_before_the_reason() {
        let mut state = WorkerState::Empty;
        let (events, receiver) = sync_channel(2);

        assert!(publish_closed_save_failure(
            &mut state,
            closed_application(),
            &events,
            IdeProjectSaveError::ProjectSnapshotUnavailable,
        ));

        assert!(matches!(state, WorkerState::Closed(_)));
        assert!(matches!(
            receiver.recv_timeout(WAIT).unwrap(),
            IdeWorkerEvent::Closed { .. }
        ));
        assert!(matches!(
            receiver.recv_timeout(WAIT).unwrap(),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SaveProject,
                reason: IdeWorkerCommandFailure::ProjectSave(
                    IdeProjectSaveError::ProjectSnapshotUnavailable
                ),
            }
        ));
    }

    #[test]
    fn shutdown_reports_native_close_failure() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                close_failure: Some(IdeNativeCloseFailure::RuntimeFailed),
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 2).unwrap();

        assert_eq!(
            shutdown_report(worker).native_failure(),
            Some(IdeNativeCloseFailure::RuntimeFailed)
        );
    }

    #[test]
    fn shutdown_retains_failure_from_an_undelivered_close_event() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                close_failure: Some(IdeNativeCloseFailure::ThreadPanicked),
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 1, 1).unwrap();
        worker.try_send(IdeWorkerCommand::CloseProject).unwrap();

        let deadline = std::time::Instant::now() + WAIT;
        let mut queued = IdeWorkerCommand::Observe;
        loop {
            match worker.try_send(queued) {
                Ok(()) => break,
                Err(IdeWorkerSendError::Full(command)) => {
                    assert!(std::time::Instant::now() < deadline);
                    queued = command;
                    std::thread::yield_now();
                }
                Err(IdeWorkerSendError::Disconnected(_)) => {
                    panic!("worker disconnected before close was consumed");
                }
            }
        }

        assert_eq!(
            shutdown_report(worker).native_failure(),
            Some(IdeNativeCloseFailure::ThreadPanicked)
        );
    }

    #[test]
    fn saturation_returns_commands_and_cannot_prevent_shutdown() {
        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 1, 1).unwrap();
        worker.try_send(IdeWorkerCommand::Observe).unwrap();
        let deadline = std::time::Instant::now() + WAIT;
        let mut queued = IdeWorkerCommand::RefreshOutputs;
        loop {
            match worker.try_send(queued) {
                Ok(()) => break,
                Err(IdeWorkerSendError::Full(command)) => {
                    assert!(std::time::Instant::now() < deadline);
                    queued = command;
                    std::thread::yield_now();
                }
                Err(IdeWorkerSendError::Disconnected(_)) => {
                    panic!("worker disconnected before saturation was observed");
                }
            }
        }
        let error = worker
            .try_send(IdeWorkerCommand::SelectSystemDefault)
            .unwrap_err();
        assert!(matches!(error, IdeWorkerSendError::Full(_)));
        assert_eq!(
            error.command().kind(),
            IdeWorkerCommandKind::SelectSystemDefault
        );
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }
}
