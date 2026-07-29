#![doc = "Owned non-real-time recording persistence workers."]

use hyper_recording_journal::{
    JournalConfig, JournalError, JournalWriter, PublishedRecording, publish_wav,
};
use hyper_recording_transport::{
    RECORDING_CHANNELS, RecordingConsumer, RecordingDrainError, RecordingTransportSnapshot,
    RecordingWakeRegistrationError,
};
use std::collections::TryReserveError;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

const IDLE_RECHECK_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingWorkerConfig {
    pub journal_path: PathBuf,
    pub final_wav_path: PathBuf,
    pub journal: JournalConfig,
    pub drain_frames: usize,
}

#[derive(Debug)]
pub enum RecordingWorkerStartError {
    InvalidConfig(&'static str),
    Allocation(TryReserveError),
    Journal(JournalError),
    WakeRegistration(RecordingWakeRegistrationError),
    PathValidation(io::Error),
    ThreadSpawn(io::Error),
    ThreadTerminated,
}

impl fmt::Display for RecordingWorkerStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid recording worker config: {reason}")
            }
            Self::Allocation(_) => {
                formatter.write_str("recording worker drain buffer allocation failed")
            }
            Self::Journal(source) => {
                write!(formatter, "recording journal startup failed: {source}")
            }
            Self::WakeRegistration(source) => {
                write!(
                    formatter,
                    "recording worker wake registration failed: {source}"
                )
            }
            Self::PathValidation(source) => {
                write!(
                    formatter,
                    "recording worker path validation failed: {source}"
                )
            }
            Self::ThreadSpawn(source) => {
                write!(
                    formatter,
                    "recording worker thread creation failed: {source}"
                )
            }
            Self::ThreadTerminated => {
                formatter.write_str("recording worker terminated during startup")
            }
        }
    }
}

impl Error for RecordingWorkerStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Allocation(source) => Some(source),
            Self::Journal(source) => Some(source),
            Self::WakeRegistration(source) => Some(source),
            Self::PathValidation(source) | Self::ThreadSpawn(source) => Some(source),
            Self::InvalidConfig(_) | Self::ThreadTerminated => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDisposition {
    Commit,
    Abort,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDispositionError {
    AlreadyRequested(RecordingDisposition),
    WorkerStopped,
}

impl fmt::Display for RecordingDispositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRequested(disposition) => {
                write!(
                    formatter,
                    "recording disposition is already {disposition:?}"
                )
            }
            Self::WorkerStopped => formatter.write_str("recording worker has already stopped"),
        }
    }
}

impl Error for RecordingDispositionError {}

#[derive(Debug)]
pub enum RecordingWorkerFailure {
    Journal(JournalError),
    Drain(RecordingDrainError),
    TransportIntegrity {
        transport: RecordingTransportSnapshot,
        journal_frames: u64,
    },
}

impl fmt::Display for RecordingWorkerFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Journal(source) => write!(formatter, "recording journal failed: {source}"),
            Self::Drain(source) => write!(formatter, "recording transport drain failed: {source}"),
            Self::TransportIntegrity {
                transport,
                journal_frames,
            } => write!(
                formatter,
                "recording transport integrity failed: accepted={}, rejected={}, drained={}, journal={journal_frames}",
                transport.accepted_frame_count,
                transport.rejected_frame_count,
                transport.drained_frame_count
            ),
        }
    }
}

impl Error for RecordingWorkerFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Journal(source) => Some(source),
            Self::Drain(source) => Some(source),
            Self::TransportIntegrity { .. } => None,
        }
    }
}

impl From<JournalError> for RecordingWorkerFailure {
    fn from(source: JournalError) -> Self {
        Self::Journal(source)
    }
}

impl From<RecordingDrainError> for RecordingWorkerFailure {
    fn from(source: RecordingDrainError) -> Self {
        Self::Drain(source)
    }
}

#[derive(Debug)]
pub enum RecordingWorkerCompletion {
    Published(PublishedRecording),
    Aborted {
        frames: u64,
        journal_path: PathBuf,
    },
    Failed {
        frames: u64,
        journal_path: PathBuf,
        error: RecordingWorkerFailure,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingWorkerWaitError {
    TimedOut,
    ThreadPanicked,
    CompletionAlreadyTaken,
}

impl fmt::Display for RecordingWorkerWaitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => formatter.write_str("recording worker did not stop before timeout"),
            Self::ThreadPanicked => formatter.write_str("recording worker thread panicked"),
            Self::CompletionAlreadyTaken => {
                formatter.write_str("recording worker completion was already taken")
            }
        }
    }
}

impl Error for RecordingWorkerWaitError {}

pub struct RecordingWorker {
    disposition_sender: SyncSender<RecordingDisposition>,
    wake_thread: Thread,
    completion_receiver: Receiver<()>,
    handle: Option<JoinHandle<RecordingWorkerCompletion>>,
    requested: Option<RecordingDisposition>,
    hard_cancel: Arc<AtomicBool>,
}

impl fmt::Debug for RecordingWorker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingWorker")
            .field("requested", &self.requested)
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl RecordingWorker {
    /// Creates the journal and starts its sole non-real-time writer thread.
    ///
    /// The returned worker does not publish when the producer closes until
    /// [`Self::request_commit`] explicitly authorizes publication.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds or paths, unavailable allocation,
    /// journal initialization failure, reused transport wake registration, or
    /// thread creation/startup failure.
    pub fn start(
        config: RecordingWorkerConfig,
        consumer: RecordingConsumer,
    ) -> Result<Self, RecordingWorkerStartError> {
        validate_config(&config, &consumer)?;
        let sample_count = config.drain_frames.checked_mul(RECORDING_CHANNELS).ok_or(
            RecordingWorkerStartError::InvalidConfig("drain sample count overflowed"),
        )?;
        let mut drain_buffer = Vec::new();
        drain_buffer
            .try_reserve_exact(sample_count)
            .map_err(RecordingWorkerStartError::Allocation)?;
        drain_buffer.resize(sample_count, 0.0);

        let journal_path = config.journal_path;
        let final_wav_path = config.final_wav_path;
        let thread_journal_path = journal_path.clone();
        let (consumer_sender, consumer_receiver) = sync_channel(1);
        let (disposition_sender, disposition_receiver) = sync_channel(1);
        let (completion_sender, completion_receiver) = sync_channel(1);
        let hard_cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = Arc::clone(&hard_cancel);
        let handle = thread::Builder::new()
            .name("hyper-recording-writer".to_owned())
            .spawn(move || {
                let _completion = CompletionSignal(completion_sender);
                let Ok((consumer, writer)) = consumer_receiver.recv() else {
                    return RecordingWorkerCompletion::Failed {
                        frames: 0,
                        journal_path: thread_journal_path,
                        error: RecordingWorkerFailure::Journal(JournalError::InvalidJournal(
                            "recording startup ownership was unavailable",
                        )),
                    };
                };
                run_worker(
                    consumer,
                    writer,
                    drain_buffer,
                    &disposition_receiver,
                    thread_journal_path,
                    &final_wav_path,
                    &thread_cancel,
                )
            })
            .map_err(RecordingWorkerStartError::ThreadSpawn)?;
        let wake_thread = handle.thread().clone();
        if let Err(source) = consumer.register_thread_waker(wake_thread.clone()) {
            drop(consumer_sender);
            let _ = handle.join();
            return Err(RecordingWorkerStartError::WakeRegistration(source));
        }
        let writer = match JournalWriter::create(&journal_path, config.journal) {
            Ok(writer) => writer,
            Err(source) => {
                drop(consumer_sender);
                let _ = handle.join();
                return Err(RecordingWorkerStartError::Journal(source));
            }
        };
        if let Err(error) = consumer_sender.send((consumer, writer)) {
            let (_, writer) = error.0;
            drop(writer);
            fs::remove_file(&journal_path).map_err(RecordingWorkerStartError::PathValidation)?;
            let _ = handle.join();
            return Err(RecordingWorkerStartError::ThreadTerminated);
        }
        Ok(Self {
            disposition_sender,
            wake_thread,
            completion_receiver,
            handle: Some(handle),
            requested: None,
            hard_cancel,
        })
    }

    /// Requests publication after the producer closes and all frames drain.
    ///
    /// # Errors
    ///
    /// Returns an error if a disposition was already requested or the worker
    /// has stopped.
    pub fn request_commit(&mut self) -> Result<(), RecordingDispositionError> {
        self.request(RecordingDisposition::Commit)
    }

    /// Requests an unpublished, recoverable journal after producer closure.
    ///
    /// # Errors
    ///
    /// Returns an error if a disposition was already requested or the worker
    /// has stopped.
    pub fn request_abort(&mut self) -> Result<(), RecordingDispositionError> {
        self.request(RecordingDisposition::Abort)
    }

    /// Waits for one terminal result without releasing ownership on timeout.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingWorkerWaitError::TimedOut`] while retaining the
    /// thread handle, or reports a panic or repeated completion retrieval.
    pub fn wait(
        &mut self,
        timeout: Duration,
    ) -> Result<RecordingWorkerCompletion, RecordingWorkerWaitError> {
        if self.handle.is_none() {
            return Err(RecordingWorkerWaitError::CompletionAlreadyTaken);
        }
        match self.completion_receiver.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let handle = self
                    .handle
                    .take()
                    .ok_or(RecordingWorkerWaitError::CompletionAlreadyTaken)?;
                handle
                    .join()
                    .map_err(|_| RecordingWorkerWaitError::ThreadPanicked)
            }
            Err(RecvTimeoutError::Timeout) => Err(RecordingWorkerWaitError::TimedOut),
        }
    }

    fn request(
        &mut self,
        disposition: RecordingDisposition,
    ) -> Result<(), RecordingDispositionError> {
        if let Some(requested) = self.requested {
            return Err(RecordingDispositionError::AlreadyRequested(requested));
        }
        match self.disposition_sender.try_send(disposition) {
            Ok(()) => {
                self.requested = Some(disposition);
                self.wake_thread.unpark();
                Ok(())
            }
            Err(TrySendError::Disconnected(_)) => Err(RecordingDispositionError::WorkerStopped),
            Err(TrySendError::Full(_)) => {
                Err(RecordingDispositionError::AlreadyRequested(disposition))
            }
        }
    }
}

impl Drop for RecordingWorker {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.hard_cancel.store(true, Ordering::Release);
            self.wake_thread.unpark();
            let _ = handle.join();
        }
    }
}

struct CompletionSignal(SyncSender<()>);

impl Drop for CompletionSignal {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn validate_config(
    config: &RecordingWorkerConfig,
    consumer: &RecordingConsumer,
) -> Result<(), RecordingWorkerStartError> {
    config
        .journal
        .validate()
        .map_err(RecordingWorkerStartError::Journal)?;
    if config.drain_frames == 0 {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "drain frames must be nonzero",
        ));
    }
    let maximum_chunk_frames = usize::try_from(config.journal.max_chunk_frames).map_err(|_| {
        RecordingWorkerStartError::InvalidConfig("journal chunk limit exceeds addressable memory")
    })?;
    if config.drain_frames > maximum_chunk_frames {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "drain frames exceeds the journal chunk limit",
        ));
    }
    if config.drain_frames > consumer.capacity_frames() {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "drain frames exceeds transport capacity",
        ));
    }
    if !config
        .final_wav_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("wav"))
    {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "final media path must have a WAV extension",
        ));
    }
    let journal_path = resolve_destination(&config.journal_path)?;
    let final_path = resolve_destination(&config.final_wav_path)?;
    if journal_path == final_path || journal_path == staging_path(&final_path)? {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "journal path overlaps final or staging media",
        ));
    }
    Ok(())
}

fn resolve_destination(path: &Path) -> Result<PathBuf, RecordingWorkerStartError> {
    if !path.is_absolute() {
        return Err(RecordingWorkerStartError::InvalidConfig(
            "recording paths must be absolute",
        ));
    }
    let name = path
        .file_name()
        .ok_or(RecordingWorkerStartError::InvalidConfig(
            "recording path must have a file name",
        ))?;
    let parent = path
        .parent()
        .ok_or(RecordingWorkerStartError::InvalidConfig(
            "recording path must have a parent directory",
        ))?;
    let parent = fs::canonicalize(parent).map_err(RecordingWorkerStartError::PathValidation)?;
    Ok(parent.join(name))
}

fn staging_path(final_path: &Path) -> Result<PathBuf, RecordingWorkerStartError> {
    let name = final_path
        .file_name()
        .ok_or(RecordingWorkerStartError::InvalidConfig(
            "final WAV path must have a file name",
        ))?
        .to_string_lossy();
    Ok(final_path.with_file_name(format!(".{name}.hyper-staging")))
}

fn run_worker(
    mut consumer: RecordingConsumer,
    mut writer: JournalWriter,
    mut drain_buffer: Vec<f32>,
    disposition_receiver: &Receiver<RecordingDisposition>,
    journal_path: PathBuf,
    final_wav_path: &Path,
    hard_cancel: &AtomicBool,
) -> RecordingWorkerCompletion {
    let counters = consumer.counters();
    let mut disposition = None;
    loop {
        if hard_cancel.load(Ordering::Acquire) {
            return RecordingWorkerCompletion::Aborted {
                frames: writer.committed_frames(),
                journal_path,
            };
        }
        receive_disposition(disposition_receiver, &mut disposition);
        let drained = match consumer.drain(&mut drain_buffer) {
            Ok(drained) => drained,
            Err(error) => {
                return failed(writer.committed_frames(), journal_path, error.into());
            }
        };
        if drained.frames > 0
            && let Err(error) = writer.append(&drain_buffer[..drained.frames * RECORDING_CHANNELS])
        {
            return failed(writer.committed_frames(), journal_path, error.into());
        }
        if drained.producer_closed && drained.frames == 0 {
            let Some(disposition) = disposition else {
                thread::park_timeout(IDLE_RECHECK_INTERVAL);
                continue;
            };
            let frames = writer.committed_frames();
            if disposition == RecordingDisposition::Abort {
                return RecordingWorkerCompletion::Aborted {
                    frames,
                    journal_path,
                };
            }
            let snapshot = counters.snapshot();
            if snapshot.rejected_block_count != 0
                || snapshot.accepted_frame_count != snapshot.drained_frame_count
                || snapshot.drained_frame_count != frames
            {
                return failed(
                    frames,
                    journal_path,
                    RecordingWorkerFailure::TransportIntegrity {
                        transport: snapshot,
                        journal_frames: frames,
                    },
                );
            }
            drop(writer);
            return match publish_wav(&journal_path, final_wav_path) {
                Ok(recording) => RecordingWorkerCompletion::Published(recording),
                Err(error) => failed(frames, journal_path, error.into()),
            };
        }
        if drained.frames == 0 {
            thread::park_timeout(IDLE_RECHECK_INTERVAL);
        }
    }
}

fn receive_disposition(
    receiver: &Receiver<RecordingDisposition>,
    disposition: &mut Option<RecordingDisposition>,
) {
    if disposition.is_some() {
        return;
    }
    match receiver.try_recv() {
        Ok(requested) => *disposition = Some(requested),
        Err(TryRecvError::Disconnected) => *disposition = Some(RecordingDisposition::Abort),
        Err(TryRecvError::Empty) => {}
    }
}

fn failed(
    frames: u64,
    journal_path: PathBuf,
    error: RecordingWorkerFailure,
) -> RecordingWorkerCompletion {
    RecordingWorkerCompletion::Failed {
        frames,
        journal_path,
        error,
    }
}
