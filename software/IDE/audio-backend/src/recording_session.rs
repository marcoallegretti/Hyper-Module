use super::{CaptureStreamCounters, CaptureStreamSnapshot, DesktopInputConfig, StreamStatus};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{PipeWireInputConfig, PipeWireInputStream, PipeWireStartError, PipeWireStopError};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiInputConfig, WasapiInputStream, WasapiStartError, WasapiStopError,
};
use core::error::Error;
use core::fmt;
use hyper_recording_journal::PublishedRecording;
use hyper_recording_metadata::{
    FinalizedRecordingMetadata, RecordingMetadataBuildError, RecordingMetadataContext,
};
use hyper_recording_transport::{
    RecordingProducer, RecordingPushError, RecordingTransportBuildError,
    RecordingTransportCounters, RecordingTransportSnapshot, try_recording_transport,
};
use hyper_recording_worker::{
    RecordingDispositionError, RecordingWorker, RecordingWorkerCompletion, RecordingWorkerConfig,
    RecordingWorkerFailure, RecordingWorkerStartError, RecordingWorkerWaitError,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeInputConfig = PipeWireInputConfig;
#[cfg(target_os = "windows")]
type NativeInputConfig = WasapiInputConfig;
#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeInputStream = PipeWireInputStream;
#[cfg(target_os = "windows")]
type NativeInputStream = WasapiInputStream;
#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeInputStartError = PipeWireStartError;
#[cfg(target_os = "windows")]
type NativeInputStartError = WasapiStartError;
#[cfg(all(target_os = "linux", target_endian = "little"))]
type NativeInputStopError = PipeWireStopError;
#[cfg(target_os = "windows")]
type NativeInputStopError = WasapiStopError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopRecordingSessionConfig {
    pub input: DesktopInputConfig,
    pub transport_capacity_frames: usize,
    pub writer: RecordingWorkerConfig,
    pub completion_timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopRecordingSessionConfigError {
    ZeroTransportCapacity,
    CallbackExceedsTransport {
        callback_frames: usize,
        transport_frames: usize,
    },
    SampleRateMismatch {
        input_hz: u32,
        journal_hz: u32,
    },
    ZeroCompletionTimeout,
}

impl fmt::Display for DesktopRecordingSessionConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTransportCapacity => {
                formatter.write_str("recording transport capacity must be nonzero")
            }
            Self::CallbackExceedsTransport {
                callback_frames,
                transport_frames,
            } => write!(
                formatter,
                "input callback capacity is {callback_frames} frames; recording transport capacity is {transport_frames}"
            ),
            Self::SampleRateMismatch {
                input_hz,
                journal_hz,
            } => write!(
                formatter,
                "desktop input is {input_hz} Hz; recording journal is {journal_hz} Hz"
            ),
            Self::ZeroCompletionTimeout => {
                formatter.write_str("recording completion timeout must be nonzero")
            }
        }
    }
}

impl Error for DesktopRecordingSessionConfigError {}

#[derive(Debug)]
pub enum DesktopRecordingSessionStartError {
    Config(DesktopRecordingSessionConfigError),
    Transport(RecordingTransportBuildError),
    Writer(RecordingWorkerStartError),
    Input {
        journal_path: PathBuf,
        source: Box<NativeInputStartError>,
    },
}

impl fmt::Display for DesktopRecordingSessionStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => write!(formatter, "invalid recording session: {source}"),
            Self::Transport(source) => {
                write!(formatter, "recording transport startup failed: {source}")
            }
            Self::Writer(source) => write!(formatter, "recording writer startup failed: {source}"),
            Self::Input {
                journal_path,
                source,
            } => write!(
                formatter,
                "desktop input startup failed; last configured journal path was {}: {source}",
                journal_path.display()
            ),
        }
    }
}

impl Error for DesktopRecordingSessionStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Writer(source) => Some(source),
            Self::Input { source, .. } => Some(source.as_ref()),
        }
    }
}

#[derive(Debug)]
pub enum DesktopRecordingSessionStopError {
    Input {
        journal_path: PathBuf,
        source: Box<NativeInputStopError>,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
    CaptureIntegrity {
        journal_path: PathBuf,
        capture: Box<CaptureStreamSnapshot>,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
    Disposition {
        journal_path: PathBuf,
        source: RecordingDispositionError,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
    Writer {
        journal_path: PathBuf,
        source: RecordingWorkerWaitError,
        pending: Option<Box<DesktopRecordingPendingCompletion>>,
    },
}

impl fmt::Display for DesktopRecordingSessionStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input {
                journal_path,
                source,
                ..
            } => write!(
                formatter,
                "desktop input shutdown failed; recording was not published and the last configured journal path was {}: {source}",
                journal_path.display()
            ),
            Self::CaptureIntegrity {
                journal_path,
                capture,
                ..
            } => write!(
                formatter,
                "capture integrity failed; recording was not published and the last configured journal path was {}: callbacks={}, captured={}, dropped_frames={}, discontinuities={}, first_discontinuity_at_frame={:?}, timestamp_errors={}, unavailable_buffers={}, malformed_buffers={}",
                journal_path.display(),
                capture.callback_count,
                capture.captured_callback_count,
                capture.dropped_frame_count,
                capture.discontinuity_count,
                capture.first_discontinuity_at_frame,
                capture.timestamp_error_count,
                capture.unavailable_buffer_count,
                capture.malformed_buffer_count
            ),
            Self::Disposition {
                journal_path,
                source,
                ..
            } => write!(
                formatter,
                "recording disposition failed; last configured journal path was {}: {source}",
                journal_path.display()
            ),
            Self::Writer {
                journal_path,
                source,
                ..
            } => write!(
                formatter,
                "recording writer shutdown failed; last configured journal path was {}: {source}",
                journal_path.display()
            ),
        }
    }
}

impl Error for DesktopRecordingSessionStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input { source, .. } => Some(source.as_ref()),
            Self::CaptureIntegrity { .. } => None,
            Self::Disposition { source, .. } => Some(source),
            Self::Writer { source, .. } => Some(source),
        }
    }
}

impl DesktopRecordingSessionStopError {
    #[must_use]
    pub fn pending_completion(&self) -> Option<&DesktopRecordingPendingCompletion> {
        match self {
            Self::Input { writer, .. }
            | Self::CaptureIntegrity { writer, .. }
            | Self::Disposition { writer, .. } => writer.pending_completion(),
            Self::Writer { pending, .. } => pending.as_deref(),
        }
    }

    /// Extracts retryable writer ownership without discarding a terminal
    /// settlement or the primary stop error.
    ///
    /// # Errors
    ///
    /// Returns the unchanged boxed error when the writer is already settled.
    pub fn into_pending_completion(self) -> Result<DesktopRecordingPendingCompletion, Box<Self>> {
        match self {
            Self::Input {
                journal_path,
                source,
                writer,
            } => (*writer).into_pending_completion().map_err(|writer| {
                Box::new(Self::Input {
                    journal_path,
                    source,
                    writer: Box::new(writer),
                })
            }),
            Self::CaptureIntegrity {
                journal_path,
                capture,
                writer,
            } => (*writer).into_pending_completion().map_err(|writer| {
                Box::new(Self::CaptureIntegrity {
                    journal_path,
                    capture,
                    writer: Box::new(writer),
                })
            }),
            Self::Disposition {
                journal_path,
                source,
                writer,
            } => (*writer).into_pending_completion().map_err(|writer| {
                Box::new(Self::Disposition {
                    journal_path,
                    source,
                    writer: Box::new(writer),
                })
            }),
            Self::Writer {
                pending: Some(pending),
                ..
            } => Ok(*pending),
            settled @ Self::Writer { pending: None, .. } => Err(Box::new(settled)),
        }
    }

    /// Returns the terminal or pending writer outcome retained alongside an
    /// earlier native, integrity, or disposition failure.
    #[must_use]
    pub const fn writer_settlement(&self) -> Option<&DesktopRecordingWriterSettlement> {
        match self {
            Self::Input { writer, .. }
            | Self::CaptureIntegrity { writer, .. }
            | Self::Disposition { writer, .. } => Some(writer),
            Self::Writer { .. } => None,
        }
    }
}

/// Writer authority retained when an earlier recording shutdown step fails.
#[derive(Debug)]
pub enum DesktopRecordingWriterSettlement {
    Completed(Box<RecordingWorkerCompletion>),
    Failed(RecordingWorkerWaitError),
    Pending(Box<DesktopRecordingPendingCompletion>),
}

impl DesktopRecordingWriterSettlement {
    #[must_use]
    pub const fn pending_completion(&self) -> Option<&DesktopRecordingPendingCompletion> {
        match self {
            Self::Pending(pending) => Some(pending),
            Self::Completed(_) | Self::Failed(_) => None,
        }
    }

    /// Extracts retryable writer ownership without discarding terminal evidence.
    ///
    /// # Errors
    ///
    /// Returns this settlement unchanged when the writer already completed or
    /// failed.
    pub fn into_pending_completion(self) -> Result<DesktopRecordingPendingCompletion, Self> {
        match self {
            Self::Pending(pending) => Ok(*pending),
            settled @ (Self::Completed(_) | Self::Failed(_)) => Err(settled),
        }
    }
}

#[derive(Debug)]
pub struct DesktopRecordingPendingCompletion {
    writer: RecordingWorker,
    journal_path: PathBuf,
}

impl DesktopRecordingPendingCompletion {
    /// Returns the last configured path for presentation while the worker is
    /// still pending. Only a terminal completion can prove recoverability.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Waits again for the terminal writer result without relinquishing
    /// ownership on timeout.
    ///
    /// # Errors
    ///
    /// Returns a bounded wait error while preserving this owner for retry.
    pub fn wait(
        &mut self,
        timeout: Duration,
    ) -> Result<RecordingWorkerCompletion, RecordingWorkerWaitError> {
        self.writer.wait(timeout)
    }
}

pub type DesktopPublishedRecording = PublishedRecording;
pub type DesktopFinalizedRecording = FinalizedRecordingMetadata;

#[derive(Debug)]
pub enum DesktopRecordingSessionCommitError {
    Stop(DesktopRecordingSessionStopError),
    UnexpectedAbort {
        frames: u64,
        journal_path: PathBuf,
    },
    Failed {
        frames: u64,
        journal_path: Option<PathBuf>,
        source: RecordingWorkerFailure,
    },
}

impl fmt::Display for DesktopRecordingSessionCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stop(source) => source.fmt(formatter),
            Self::UnexpectedAbort {
                frames,
                journal_path,
            } => write!(
                formatter,
                "recording commit ended as abort after {frames} frames; journal remains at {}",
                journal_path.display()
            ),
            Self::Failed {
                frames,
                journal_path,
                source,
            } => match journal_path {
                Some(journal_path) => write!(
                    formatter,
                    "recording commit failed after {frames} frames; journal remains at {}: {source}",
                    journal_path.display()
                ),
                None => write!(
                    formatter,
                    "recording commit failed after {frames} frames and no recovery journal path can be proven: {source}"
                ),
            },
        }
    }
}

impl Error for DesktopRecordingSessionCommitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stop(source) => Some(source),
            Self::Failed { source, .. } => Some(source),
            Self::UnexpectedAbort { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopRecordingSessionFinalizeError {
    Commit(DesktopRecordingSessionCommitError),
    Metadata(RecordingMetadataBuildError),
}

impl fmt::Display for DesktopRecordingSessionFinalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(source) => source.fmt(formatter),
            Self::Metadata(source) => {
                write!(formatter, "recording metadata validation failed: {source}")
            }
        }
    }
}

impl Error for DesktopRecordingSessionFinalizeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Commit(source) => Some(source),
            Self::Metadata(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub struct DesktopRecordingSession {
    input: NativeInputStream,
    recording: RecordingSessionCore,
}

#[derive(Debug)]
pub(crate) struct RecordingSessionCore {
    writer: RecordingWorker,
    journal_path: PathBuf,
    completion_timeout: Duration,
    transport_counters: RecordingTransportCounters,
}

/// One input callback fan-out with lossless recording as the first consumer.
///
/// Monitoring is intentionally best-effort, while recording must surface every
/// transport refusal to the native input owner before it can claim a finalized
/// take. Keeping this ordering in the shared recording module lets every
/// playback owner use the same one-input authority.
pub(crate) struct MonitoredRecordingProducer {
    recording: RecordingProducer,
    monitoring: super::monitoring_session::MonitoringProducer,
}

impl MonitoredRecordingProducer {
    pub(crate) const fn new(
        recording: RecordingProducer,
        monitoring: super::monitoring_session::MonitoringProducer,
    ) -> Self {
        Self {
            recording,
            monitoring,
        }
    }

    pub(crate) fn push(&mut self, samples: &[f32]) -> Result<(), RecordingPushError> {
        self.recording.push(samples)?;
        self.monitoring.push(samples);
        Ok(())
    }
}

pub(crate) enum RecordingCoreStartError {
    Config(DesktopRecordingSessionConfigError),
    Transport(RecordingTransportBuildError),
    Writer(RecordingWorkerStartError),
}

impl From<RecordingCoreStartError> for DesktopRecordingSessionStartError {
    fn from(source: RecordingCoreStartError) -> Self {
        match source {
            RecordingCoreStartError::Config(source) => Self::Config(source),
            RecordingCoreStartError::Transport(source) => Self::Transport(source),
            RecordingCoreStartError::Writer(source) => Self::Writer(source),
        }
    }
}

pub(crate) fn start_recording_core(
    config: DesktopRecordingSessionConfig,
) -> Result<(RecordingProducer, RecordingSessionCore), RecordingCoreStartError> {
    validate_config(&config).map_err(RecordingCoreStartError::Config)?;
    let (producer, consumer) = try_recording_transport(config.transport_capacity_frames)
        .map_err(RecordingCoreStartError::Transport)?;
    let transport_counters = producer.counters();
    let journal_path = config.writer.journal_path.clone();
    let writer =
        RecordingWorker::start(config.writer, consumer).map_err(RecordingCoreStartError::Writer)?;
    Ok((
        producer,
        RecordingSessionCore {
            writer,
            journal_path,
            completion_timeout: config.completion_timeout,
            transport_counters,
        },
    ))
}

impl RecordingSessionCore {
    #[must_use]
    pub(crate) fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[must_use]
    pub(crate) fn transport_snapshot(&self) -> RecordingTransportSnapshot {
        self.transport_counters.snapshot()
    }

    pub(crate) fn abort_and_settle(mut self) -> DesktopRecordingWriterSettlement {
        let _ = self.writer.request_abort();
        settle_writer(self.writer, &self.journal_path, self.completion_timeout)
    }

    pub(crate) fn finish(
        mut self,
        capture: CaptureStreamSnapshot,
        commit: bool,
    ) -> Result<RecordingWorkerCompletion, DesktopRecordingSessionStopError> {
        if commit && !capture_is_publishable(capture) {
            let _ = self.writer.request_abort();
            let writer = Box::new(settle_writer(
                self.writer,
                &self.journal_path,
                self.completion_timeout,
            ));
            return Err(DesktopRecordingSessionStopError::CaptureIntegrity {
                journal_path: self.journal_path,
                capture: Box::new(capture),
                writer,
            });
        }
        let disposition = if commit {
            self.writer.request_commit()
        } else {
            self.writer.request_abort()
        };
        if let Err(source) = disposition {
            let writer = Box::new(settle_writer(
                self.writer,
                &self.journal_path,
                self.completion_timeout,
            ));
            return Err(DesktopRecordingSessionStopError::Disposition {
                journal_path: self.journal_path,
                source,
                writer,
            });
        }
        match self.writer.wait(self.completion_timeout) {
            Ok(completion) => Ok(completion),
            Err(source) => {
                let pending = (source == RecordingWorkerWaitError::TimedOut).then(|| {
                    Box::new(DesktopRecordingPendingCompletion {
                        writer: self.writer,
                        journal_path: self.journal_path.clone(),
                    })
                });
                Err(DesktopRecordingSessionStopError::Writer {
                    journal_path: self.journal_path,
                    source,
                    pending,
                })
            }
        }
    }
}

impl DesktopRecordingSession {
    /// Starts native capture only after the bounded transport and recoverable
    /// journal writer own their resources.
    ///
    /// # Errors
    ///
    /// Returns an error without publishing media when configuration,
    /// allocation, journal ownership, writer startup, or native input startup
    /// fails.
    pub fn start(
        config: DesktopRecordingSessionConfig,
    ) -> Result<Self, DesktopRecordingSessionStartError> {
        let native = NativeInputConfig::from(&config.input);
        let (mut producer, recording) =
            start_recording_core(config).map_err(DesktopRecordingSessionStartError::from)?;
        let journal_path = recording.journal_path.clone();
        let input =
            match NativeInputStream::start(native, move |samples: &[f32]| producer.push(samples)) {
                Ok(input) => input,
                Err(source) => {
                    drop(recording);
                    return Err(DesktopRecordingSessionStartError::Input {
                        journal_path: journal_path.clone(),
                        source: Box::new(source),
                    });
                }
            };
        Ok(Self { input, recording })
    }

    #[must_use]
    pub fn counters(&self) -> CaptureStreamCounters {
        self.input.counters()
    }

    /// Returns authoritative producer/writer transport progress for this take.
    #[must_use]
    pub fn recording_snapshot(&self) -> RecordingTransportSnapshot {
        self.recording.transport_snapshot()
    }

    #[must_use]
    pub fn input_status(&self) -> StreamStatus {
        self.input.status()
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        self.recording.journal_path()
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.input.disconnect_reason()
    }

    /// Stops capture, drains every accepted frame, and publishes verified WAV.
    ///
    /// # Errors
    ///
    /// Native shutdown failure prevents commit and leaves the journal
    /// unpublished. Disposition, writer completion, abort, and terminal writer
    /// failures also retain the journal path and cannot produce a publication
    /// token.
    pub fn commit(self) -> Result<DesktopPublishedRecording, DesktopRecordingSessionCommitError> {
        let completion = self
            .finish(true)
            .map_err(DesktopRecordingSessionCommitError::Stop)?;
        published_recording(completion)
    }

    /// Stops capture, publishes verified WAV, and binds project-owned
    /// provenance to the publication authority.
    ///
    /// This does not mutate a project package. The returned value is the
    /// validated input to the project-storage recording transaction.
    ///
    /// # Errors
    ///
    /// Returns a commit error when capture cannot be published. Metadata
    /// validation errors retain both the verified publication authority and
    /// rejected context so the caller can correct provenance and retry.
    pub fn finalize(
        self,
        context: RecordingMetadataContext,
    ) -> Result<DesktopFinalizedRecording, DesktopRecordingSessionFinalizeError> {
        let publication = self
            .commit()
            .map_err(DesktopRecordingSessionFinalizeError::Commit)?;
        finalized_recording(publication, context)
    }

    /// Stops capture, drains every accepted frame, and preserves the journal
    /// without publishing WAV.
    ///
    /// # Errors
    ///
    /// Returns an error for native shutdown, disposition, or bounded writer
    /// completion failure.
    pub fn abort(self) -> Result<RecordingWorkerCompletion, DesktopRecordingSessionStopError> {
        self.finish(false)
    }

    fn finish(
        self,
        commit: bool,
    ) -> Result<RecordingWorkerCompletion, DesktopRecordingSessionStopError> {
        let Self { input, recording } = self;
        let capture = input.counters();
        if let Err(source) = input.stop() {
            let journal_path = recording.journal_path.clone();
            let writer = Box::new(recording.abort_and_settle());
            return Err(DesktopRecordingSessionStopError::Input {
                journal_path,
                source: Box::new(source),
                writer,
            });
        }
        let capture = capture.snapshot();
        recording.finish(capture, commit)
    }
}

fn settle_writer(
    mut writer: RecordingWorker,
    journal_path: &Path,
    timeout: Duration,
) -> DesktopRecordingWriterSettlement {
    match writer.wait(timeout) {
        Ok(completion) => DesktopRecordingWriterSettlement::Completed(Box::new(completion)),
        Err(RecordingWorkerWaitError::TimedOut) => {
            DesktopRecordingWriterSettlement::Pending(Box::new(DesktopRecordingPendingCompletion {
                writer,
                journal_path: journal_path.to_owned(),
            }))
        }
        Err(source) => DesktopRecordingWriterSettlement::Failed(source),
    }
}

fn capture_is_publishable(capture: CaptureStreamSnapshot) -> bool {
    capture.callback_count == capture.captured_callback_count
        && capture.dropped_frame_count == 0
        && capture.processor_failure_count == 0
        && capture.invalid_shape_count == 0
        && capture.oversized_block_count == 0
        && capture.non_finite_block_count == 0
        && capture.unavailable_buffer_count == 0
        && capture.malformed_buffer_count == 0
        && capture.discontinuity_count == 0
        && capture.timestamp_error_count == 0
        && capture.stream_error_count == 0
}

fn validate_config(
    config: &DesktopRecordingSessionConfig,
) -> Result<(), DesktopRecordingSessionConfigError> {
    if config.transport_capacity_frames == 0 {
        return Err(DesktopRecordingSessionConfigError::ZeroTransportCapacity);
    }
    if config.input.maximum_block_frames() > config.transport_capacity_frames {
        return Err(
            DesktopRecordingSessionConfigError::CallbackExceedsTransport {
                callback_frames: config.input.maximum_block_frames(),
                transport_frames: config.transport_capacity_frames,
            },
        );
    }
    if config.input.sample_rate_hz() != config.writer.journal.sample_rate {
        return Err(DesktopRecordingSessionConfigError::SampleRateMismatch {
            input_hz: config.input.sample_rate_hz(),
            journal_hz: config.writer.journal.sample_rate,
        });
    }
    if config.completion_timeout.is_zero() {
        return Err(DesktopRecordingSessionConfigError::ZeroCompletionTimeout);
    }
    Ok(())
}

pub(crate) fn published_recording(
    completion: RecordingWorkerCompletion,
) -> Result<DesktopPublishedRecording, DesktopRecordingSessionCommitError> {
    match completion {
        RecordingWorkerCompletion::Published(recording) => Ok(recording),
        RecordingWorkerCompletion::Aborted {
            frames,
            journal_path,
        } => Err(DesktopRecordingSessionCommitError::UnexpectedAbort {
            frames,
            journal_path,
        }),
        RecordingWorkerCompletion::Failed {
            frames,
            journal_path,
            error,
        } => Err(DesktopRecordingSessionCommitError::Failed {
            frames,
            journal_path,
            source: error,
        }),
    }
}

fn finalized_recording(
    publication: DesktopPublishedRecording,
    context: RecordingMetadataContext,
) -> Result<DesktopFinalizedRecording, DesktopRecordingSessionFinalizeError> {
    FinalizedRecordingMetadata::try_new(publication, context)
        .map_err(DesktopRecordingSessionFinalizeError::Metadata)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_recording_journal::{JournalConfig, JournalWriter, PublicationOutcome, publish_wav};
    use hyper_recording_metadata::{RecordingCaptureKind, RecordingMetadataError};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn config() -> DesktopRecordingSessionConfig {
        DesktopRecordingSessionConfig {
            input: DesktopInputConfig::try_new(48_000, 256).unwrap(),
            transport_capacity_frames: 1_024,
            writer: RecordingWorkerConfig {
                journal_path: PathBuf::from("unused.journal"),
                final_wav_path: PathBuf::from("unused.wav"),
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

    fn metadata_context() -> RecordingMetadataContext {
        RecordingMetadataContext {
            id: "018f47a2-6b2d-7c11-8a44-001122334477".to_owned(),
            project_id: "018f47a2-6b2d-7c11-8a44-001122334455".to_owned(),
            asset_id: "018f47a2-6b2d-7c11-8a44-001122334478".to_owned(),
            track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
            capture_kind: RecordingCaptureKind::New,
            captured_at_utc: "2026-07-29T14:00:00.000Z".to_owned(),
            timeline_start_tick: 7_680,
            source_input_indices: vec![0, 1],
        }
    }

    #[test]
    fn session_config_rejects_every_cross_boundary_mismatch() {
        assert_eq!(validate_config(&config()), Ok(()));

        let mut zero_transport = config();
        zero_transport.transport_capacity_frames = 0;
        assert_eq!(
            validate_config(&zero_transport),
            Err(DesktopRecordingSessionConfigError::ZeroTransportCapacity)
        );

        let mut small_transport = config();
        small_transport.transport_capacity_frames = 128;
        assert!(matches!(
            validate_config(&small_transport),
            Err(DesktopRecordingSessionConfigError::CallbackExceedsTransport { .. })
        ));

        let mut wrong_rate = config();
        wrong_rate.writer.journal.sample_rate = 44_100;
        assert!(matches!(
            validate_config(&wrong_rate),
            Err(DesktopRecordingSessionConfigError::SampleRateMismatch { .. })
        ));

        let mut zero_timeout = config();
        zero_timeout.completion_timeout = Duration::ZERO;
        assert_eq!(
            validate_config(&zero_timeout),
            Err(DesktopRecordingSessionConfigError::ZeroCompletionTimeout)
        );
    }

    #[test]
    fn publication_requires_a_lossless_capture_snapshot() {
        assert!(capture_is_publishable(CaptureStreamSnapshot {
            callback_count: 2,
            captured_callback_count: 2,
            captured_frame_count: 512,
            ..CaptureStreamSnapshot::default()
        }));

        for rejected in [
            CaptureStreamSnapshot {
                callback_count: 2,
                captured_callback_count: 1,
                ..CaptureStreamSnapshot::default()
            },
            CaptureStreamSnapshot {
                unavailable_buffer_count: 1,
                ..CaptureStreamSnapshot::default()
            },
            CaptureStreamSnapshot {
                discontinuity_count: 1,
                ..CaptureStreamSnapshot::default()
            },
            CaptureStreamSnapshot {
                timestamp_error_count: 1,
                ..CaptureStreamSnapshot::default()
            },
            CaptureStreamSnapshot {
                non_finite_block_count: 1,
                ..CaptureStreamSnapshot::default()
            },
        ] {
            assert!(!capture_is_publishable(rejected));
        }
    }

    #[test]
    fn pending_completion_retains_the_eventual_writer_result() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-pending-recording-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let journal_path = directory.join("take.journal");
        let (producer, consumer) = try_recording_transport(2).unwrap();
        let mut writer = RecordingWorker::start(
            RecordingWorkerConfig {
                journal_path: journal_path.clone(),
                final_wav_path: directory.join("take.wav"),
                journal: JournalConfig {
                    sample_rate: 48_000,
                    max_chunk_frames: 2,
                    max_total_frames: 2,
                },
                drain_frames: 2,
            },
            consumer,
        )
        .unwrap();
        writer.request_abort().unwrap();
        let mut pending = DesktopRecordingPendingCompletion {
            writer,
            journal_path: journal_path.clone(),
        };

        assert_eq!(pending.journal_path(), journal_path);
        assert!(matches!(
            pending.wait(Duration::from_millis(1)),
            Err(RecordingWorkerWaitError::TimedOut)
        ));
        drop(producer);
        assert!(matches!(
            pending.wait(Duration::from_secs(1)).unwrap(),
            RecordingWorkerCompletion::Aborted { frames: 0, .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exceptional_shutdown_retains_a_completed_writer_result() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-settled-recording-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let journal_path = directory.join("take.journal");
        let (producer, consumer) = try_recording_transport(2).unwrap();
        let mut writer = RecordingWorker::start(
            RecordingWorkerConfig {
                journal_path: journal_path.clone(),
                final_wav_path: directory.join("take.wav"),
                journal: JournalConfig {
                    sample_rate: 48_000,
                    max_chunk_frames: 2,
                    max_total_frames: 2,
                },
                drain_frames: 2,
            },
            consumer,
        )
        .unwrap();
        writer.request_abort().unwrap();
        drop(producer);

        let settlement = Box::new(settle_writer(writer, &journal_path, Duration::from_secs(1)));
        let settlement = (*settlement).into_pending_completion().unwrap_err();
        let DesktopRecordingWriterSettlement::Completed(completion) = settlement else {
            panic!("the completed writer result must remain available");
        };
        assert!(matches!(
            *completion,
            RecordingWorkerCompletion::Aborted { frames: 0, .. }
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_session_fails_before_journal_or_device_startup() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-recording-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut invalid = config();
        invalid.writer.journal_path = directory.join("take.journal");
        invalid.writer.final_wav_path = directory.join("take.wav");
        invalid.writer.journal.sample_rate = 44_100;

        assert!(matches!(
            DesktopRecordingSession::start(invalid),
            Err(DesktopRecordingSessionStartError::Config(
                DesktopRecordingSessionConfigError::SampleRateMismatch { .. }
            ))
        ));
        assert!(fs::read_dir(&directory).unwrap().next().is_none());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn recording_core_exposes_authoritative_transport_progress() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-recording-progress-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut config = config();
        config.writer.journal_path = directory.join("take.journal");
        config.writer.final_wav_path = directory.join("take.wav");
        let Ok((mut producer, recording)) = start_recording_core(config) else {
            panic!("valid recording core must start");
        };

        producer.push(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        assert_eq!(recording.transport_snapshot().accepted_frame_count, 2);

        drop(producer);
        assert!(matches!(
            recording.abort_and_settle(),
            DesktopRecordingWriterSettlement::Completed(completion)
                if matches!(*completion, RecordingWorkerCompletion::Aborted { frames: 2, .. })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn publication_token_exists_only_for_published_completion() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-publication-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let journal = directory.join("take.journal");
        let wav = directory.join("take.wav");
        let mut writer = JournalWriter::create(
            &journal,
            JournalConfig {
                sample_rate: 48_000,
                max_chunk_frames: 2,
                max_total_frames: 2,
            },
        )
        .unwrap();
        writer.append(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        drop(writer);
        let authority = publish_wav(&journal, &wav).unwrap();

        let published =
            published_recording(RecordingWorkerCompletion::Published(authority)).unwrap();
        assert_eq!(published.final_wav_path(), wav);
        assert_eq!(published.journal_path(), journal);
        assert_eq!(published.sample_rate_hz(), 48_000);
        assert_eq!(published.channels(), 2);
        assert_eq!(published.frames(), 2);
        assert_eq!(published.outcome(), PublicationOutcome::Published);

        assert!(matches!(
            published_recording(RecordingWorkerCompletion::Aborted {
                frames: 511,
                journal_path: PathBuf::from("take.journal"),
            },),
            Err(DesktopRecordingSessionCommitError::UnexpectedAbort { frames: 511, .. })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn finalized_recording_binds_metadata_without_reissuing_media_facts() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-finalized-recording-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let journal = directory.join("take.journal");
        let wav = directory.join("take.wav");
        let mut writer = JournalWriter::create(
            &journal,
            JournalConfig {
                sample_rate: 48_000,
                max_chunk_frames: 2,
                max_total_frames: 2,
            },
        )
        .unwrap();
        writer.append(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        drop(writer);
        let authority = publish_wav(&journal, &wav).unwrap();

        let finalized = finalized_recording(authority.clone(), metadata_context()).unwrap();
        assert_eq!(finalized.publication().final_wav_path(), wav);
        assert_eq!(
            finalized.publication().byte_length(),
            authority.byte_length()
        );
        assert_eq!(finalized.publication().wav_sha256(), authority.wav_sha256());
        assert_eq!(finalized.publication().frames(), 2);
        let json = String::from_utf8(finalized.to_canonical_json().unwrap()).unwrap();
        assert!(json.contains("\"frame_count\":2"));
        assert!(json.contains("\"sample_rate_hz\":48000"));

        let mut rejected_context = metadata_context();
        rejected_context.project_id.make_ascii_uppercase();
        let error = finalized_recording(authority, rejected_context).unwrap_err();
        let DesktopRecordingSessionFinalizeError::Metadata(error) = error else {
            panic!("metadata validation must fail after verified publication");
        };
        assert_eq!(
            error.reason(),
            RecordingMetadataError::InvalidId("project_id")
        );
        assert_eq!(error.publication().final_wav_path(), wav);
        assert_eq!(error.publication().frames(), 2);
        assert_eq!(
            error.context().project_id,
            "018F47A2-6B2D-7C11-8A44-001122334455"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
