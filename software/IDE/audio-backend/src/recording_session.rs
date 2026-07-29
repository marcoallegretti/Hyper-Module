use super::{CaptureStreamCounters, DesktopInputConfig, StreamStatus};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{PipeWireInputConfig, PipeWireInputStream, PipeWireStartError, PipeWireStopError};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiInputConfig, WasapiInputStream, WasapiStartError, WasapiStopError,
};
use core::error::Error;
use core::fmt;
use hyper_recording_journal::PublishedRecording;
use hyper_recording_transport::{RecordingTransportBuildError, try_recording_transport};
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
                "desktop input startup failed; recoverable journal remains at {}: {source}",
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
    },
    Disposition {
        journal_path: PathBuf,
        source: RecordingDispositionError,
    },
    Writer {
        journal_path: PathBuf,
        source: RecordingWorkerWaitError,
    },
}

impl fmt::Display for DesktopRecordingSessionStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input {
                journal_path,
                source,
            } => write!(
                formatter,
                "desktop input shutdown failed; recording was not published and its journal remains at {}: {source}",
                journal_path.display()
            ),
            Self::Disposition {
                journal_path,
                source,
            } => write!(
                formatter,
                "recording disposition failed; journal remains at {}: {source}",
                journal_path.display()
            ),
            Self::Writer {
                journal_path,
                source,
            } => write!(
                formatter,
                "recording writer shutdown failed; journal remains at {}: {source}",
                journal_path.display()
            ),
        }
    }
}

impl Error for DesktopRecordingSessionStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input { source, .. } => Some(source.as_ref()),
            Self::Disposition { source, .. } => Some(source),
            Self::Writer { source, .. } => Some(source),
        }
    }
}

pub type DesktopPublishedRecording = PublishedRecording;

#[derive(Debug)]
pub enum DesktopRecordingSessionCommitError {
    Stop(DesktopRecordingSessionStopError),
    UnexpectedAbort {
        frames: u64,
        journal_path: PathBuf,
    },
    Failed {
        frames: u64,
        journal_path: PathBuf,
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
            } => write!(
                formatter,
                "recording commit failed after {frames} frames; journal remains at {}: {source}",
                journal_path.display()
            ),
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
pub struct DesktopRecordingSession {
    input: NativeInputStream,
    writer: RecordingWorker,
    journal_path: PathBuf,
    completion_timeout: Duration,
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
        validate_config(&config).map_err(DesktopRecordingSessionStartError::Config)?;
        let (mut producer, consumer) = try_recording_transport(config.transport_capacity_frames)
            .map_err(DesktopRecordingSessionStartError::Transport)?;
        let journal_path = config.writer.journal_path.clone();
        let writer = RecordingWorker::start(config.writer, consumer)
            .map_err(DesktopRecordingSessionStartError::Writer)?;
        let native = NativeInputConfig::from(&config.input);
        let input =
            match NativeInputStream::start(native, move |samples: &[f32]| producer.push(samples)) {
                Ok(input) => input,
                Err(source) => {
                    drop(writer);
                    return Err(DesktopRecordingSessionStartError::Input {
                        journal_path: journal_path.clone(),
                        source: Box::new(source),
                    });
                }
            };
        Ok(Self {
            input,
            writer,
            journal_path,
            completion_timeout: config.completion_timeout,
        })
    }

    #[must_use]
    pub fn counters(&self) -> CaptureStreamCounters {
        self.input.counters()
    }

    #[must_use]
    pub fn input_status(&self) -> StreamStatus {
        self.input.status()
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
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
        let Self {
            input,
            mut writer,
            journal_path,
            completion_timeout,
        } = self;
        if let Err(source) = input.stop() {
            let _ = writer.request_abort();
            let _ = writer.wait(completion_timeout);
            return Err(DesktopRecordingSessionStopError::Input {
                journal_path,
                source: Box::new(source),
            });
        }
        let disposition = if commit {
            writer.request_commit()
        } else {
            writer.request_abort()
        };
        disposition.map_err(|source| DesktopRecordingSessionStopError::Disposition {
            journal_path: journal_path.clone(),
            source,
        })?;
        writer
            .wait(completion_timeout)
            .map_err(|source| DesktopRecordingSessionStopError::Writer {
                journal_path,
                source,
            })
    }
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

fn published_recording(
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

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_recording_journal::{JournalConfig, JournalWriter, PublicationOutcome, publish_wav};
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
}
