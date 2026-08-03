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

use super::{
    DesktopIdeApplication, DesktopIdePendingClose, DesktopIdePendingSaveClose,
    DesktopPlaybackRecovery, IdeApplicationConfig, IdeAudioOutputInventory, IdeCloseOutcome,
    IdeMixerChannelKind, IdeMixerChannelState, IdeMixerCommand, IdeMixerCommandError,
    IdeMixerMainState, IdeNativeCloseFailure, IdeOutputDiscoveryError, IdeOutputSelection,
    IdeOutputSelectionError, IdePendingCloseWait, IdePendingSaveCloseWait, IdeProjectOpenError,
    IdeProjectSaveError, IdeSaveCloseOutcome, IdeSceneAvailability, OutputInventoryError,
    OutputStreamSnapshot, OutputStreamState, RunningDesktopIdeApplication,
};

const WORKER_SHUTDOWN_POLL: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const MIXER_SAVE_COMMIT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const MIXER_SAVE_COMMIT_TIMEOUT: Duration = Duration::from_millis(50);
pub const MAX_IDE_WORKER_COMMANDS: usize = 64;
pub const MAX_IDE_WORKER_EVENTS: usize = 64;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IdeWorkerShutdownReport {
    native_failure: Option<IdeNativeCloseFailure>,
}

impl IdeWorkerShutdownReport {
    #[must_use]
    pub const fn native_failure(self) -> Option<IdeNativeCloseFailure> {
        self.native_failure
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
    SelectSystemDefault,
    SelectOutput,
    OpenProject,
    Transport,
    Mixer,
    SaveProject,
    Observe,
    CloseProject,
    Shutdown,
}

#[derive(Debug)]
pub enum IdeWorkerCommand {
    RetryDiscovery,
    RefreshOutputs,
    SelectSystemDefault,
    SelectOutput(String),
    OpenProject(PathBuf),
    Transport(TransportCommand),
    Mixer(IdeMixerCommand),
    SaveProject,
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
            Self::SelectSystemDefault => IdeWorkerCommandKind::SelectSystemDefault,
            Self::SelectOutput(_) => IdeWorkerCommandKind::SelectOutput,
            Self::OpenProject(_) => IdeWorkerCommandKind::OpenProject,
            Self::Transport(_) => IdeWorkerCommandKind::Transport,
            Self::Mixer(_) => IdeWorkerCommandKind::Mixer,
            Self::SaveProject => IdeWorkerCommandKind::SaveProject,
            Self::Observe => IdeWorkerCommandKind::Observe,
            Self::CloseProject => IdeWorkerCommandKind::CloseProject,
            Self::Shutdown => IdeWorkerCommandKind::Shutdown,
        }
    }
}

#[derive(Debug)]
pub struct IdeClosedSnapshot {
    outputs: IdeAudioOutputInventory,
    selection: IdeOutputSelection,
}

impl IdeClosedSnapshot {
    #[must_use]
    pub const fn outputs(&self) -> &IdeAudioOutputInventory {
        &self.outputs
    }

    #[must_use]
    pub const fn selection(&self) -> &IdeOutputSelection {
        &self.selection
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
    recovery: PlaybackRecoverySnapshot,
    meter: LatestOutputMeterSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Owned UI-thread observation of one validated project scene.
pub struct IdeScene {
    name: String,
    availability: IdeSceneAvailability,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdeMixerChannel {
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
    OutputSelection(IdeOutputSelectionError),
    ProjectOpen(IdeProjectOpenError),
    Transport(super::DesktopDirectControlError),
    Mixer(IdeMixerCommandError),
    ProjectSave(IdeProjectSaveError),
    InvalidState,
    Publication(OutputInventoryError),
    ProjectPublicationCapacityUnavailable,
    ProjectSnapshotUnavailable,
}

impl fmt::Display for IdeWorkerCommandFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovery(source) => source.fmt(formatter),
            Self::OutputSelection(source) => source.fmt(formatter),
            Self::ProjectOpen(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
            Self::Mixer(source) => source.fmt(formatter),
            Self::ProjectSave(source) => source.fmt(formatter),
            Self::InvalidState => {
                formatter.write_str("command is invalid for the current IDE state")
            }
            Self::Publication(source) => source.fmt(formatter),
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
            Self::OutputSelection(source) => Some(source),
            Self::ProjectOpen(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::ProjectSave(source) => Some(source),
            Self::Publication(source) => Some(source),
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
    },
    Running(IdeRunningObservation),
    TransportQueued(TransportCommand),
    MixerQueued(IdeMixerCommand),
    ProjectSaved {
        revision: u64,
        changed: bool,
    },
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
            WorkerState::SaveFence { .. } | WorkerState::Closing(_) | WorkerState::Saving(_)
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
        Ok(application) => {
            *state = WorkerState::Closed(application);
            publish_state(state, events, unpublished_native_failure)
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
        IdeWorkerCommand::SaveProject => save_project(state, events),
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
            },
            Err(reason) => {
                let delivered = publish_failure(
                    events,
                    IdeWorkerCommandKind::Observe,
                    IdeWorkerCommandFailure::Publication(reason),
                );
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
        WorkerState::Closing(_) => IdeWorkerEvent::Closing,
        WorkerState::SaveFence { .. } | WorkerState::Saving(_) => IdeWorkerEvent::Saving,
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
) -> Result<IdeClosedSnapshot, OutputInventoryError> {
    let outputs = application.outputs().try_to_owned()?;
    let selection = match application.selection() {
        IdeOutputSelection::SystemDefault => IdeOutputSelection::SystemDefault,
        IdeOutputSelection::Routed(routing_id) => {
            IdeOutputSelection::Routed(super::try_owned(routing_id)?)
        }
    };
    Ok(IdeClosedSnapshot { outputs, selection })
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
        recovery: snapshot.recovery,
        meter: application.read_latest_meter(),
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

fn send_event(events: &SyncSender<IdeWorkerEvent>, event: IdeWorkerEvent) -> bool {
    events.send(event).is_ok()
}

fn finish_worker(
    state: WorkerState,
    unpublished_native_failure: Option<IdeNativeCloseFailure>,
) -> IdeWorkerShutdownReport {
    let shutdown_native_failure = shutdown_state(state);
    IdeWorkerShutdownReport {
        native_failure: unpublished_native_failure.or(shutdown_native_failure),
    }
}

fn shutdown_state(mut state: WorkerState) -> Option<IdeNativeCloseFailure> {
    loop {
        state = match state {
            WorkerState::Running(application) => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return failure,
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::SaveFence { application, .. } => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return failure,
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Closing(pending) => match pending.wait(WORKER_SHUTDOWN_POLL) {
                IdePendingCloseWait::Closed { failure, .. } => return failure,
                IdePendingCloseWait::TimedOut(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Saving(pending) => match pending.wait(WORKER_SHUTDOWN_POLL) {
                IdePendingSaveCloseWait::Closed { failure, .. } => return failure,
                IdePendingSaveCloseWait::TimedOut(pending) => WorkerState::Saving(pending),
            },
            WorkerState::Undiscovered(_) | WorkerState::Closed(_) | WorkerState::Empty => {
                return None;
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
        DesktopIdeApplication::from_inventory(
            application_config(),
            IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap(),
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
