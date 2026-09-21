use super::monitoring_session::{MonitoringCounters, try_monitoring_transport};
use super::recording_session::{
    MonitoredRecordingProducer, RecordingCoreStartError, RecordingSessionCore, start_recording_core,
};
use super::{
    CaptureStreamCounters, DesktopFinalizedRecording, DesktopInputStartError,
    DesktopInputStopError, DesktopMonitoringConfig, DesktopMonitoringConfigError,
    DesktopMonitoringControl, DesktopOutputConfig, DesktopOutputStartError, DesktopOutputStopError,
    DesktopPublishedRecording, DesktopRecordingSessionCommitError, DesktopRecordingSessionConfig,
    DesktopRecordingSessionConfigError, DesktopRecordingSessionStopError,
    DesktopRecordingWriterSettlement, InputMeterReceiver, MonitoringSnapshot, OutputStreamCounters,
    OutputStreamStatus, StreamStatus,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{
    PipeWireInputConfig as NativeInputConfig, PipeWireInputStream as NativeInputStream,
    PipeWireOutputStream as NativeOutputStream,
};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiInputConfig as NativeInputConfig,
    WasapiInputStream as NativeInputStream, WasapiOutputStream as NativeOutputStream,
};
use core::error::Error;
use core::fmt;
use hyper_recording_metadata::{RecordingMetadataBuildError, RecordingMetadataContext};
use hyper_recording_transport::{RecordingTransportBuildError, RecordingTransportSnapshot};
use hyper_recording_worker::{RecordingWorkerCompletion, RecordingWorkerStartError};
use std::path::{Path, PathBuf};

/// Configuration for one capture stream feeding both recording and monitoring.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopMonitoredRecordingConfig {
    recording: DesktopRecordingSessionConfig,
    monitoring: DesktopMonitoringConfig,
}

impl DesktopMonitoredRecordingConfig {
    /// Builds monitoring from the recording input, preventing divergent input
    /// identities or callback contracts.
    ///
    /// # Errors
    ///
    /// Returns a monitoring configuration error when the output, latency, or
    /// bounded bridge settings are inconsistent with the recording input.
    pub fn try_new(
        recording: DesktopRecordingSessionConfig,
        output: DesktopOutputConfig,
        monitoring_transport_capacity_frames: usize,
        target_latency_frames: usize,
        processing_chunk_frames: usize,
    ) -> Result<Self, DesktopMonitoringConfigError> {
        let monitoring = DesktopMonitoringConfig::try_new(
            recording.input.clone(),
            output,
            monitoring_transport_capacity_frames,
            target_latency_frames,
            processing_chunk_frames,
        )?;
        Ok(Self {
            recording,
            monitoring,
        })
    }

    /// Sets the bounded clock-drift correction for the monitor path.
    ///
    /// # Errors
    ///
    /// Returns an error when the correction is zero or exceeds one percent.
    pub fn with_maximum_clock_drift_ppm(
        mut self,
        maximum_clock_drift_ppm: u32,
    ) -> Result<Self, DesktopMonitoringConfigError> {
        self.monitoring = self
            .monitoring
            .with_maximum_clock_drift_ppm(maximum_clock_drift_ppm)?;
        Ok(self)
    }

    #[must_use]
    pub const fn recording(&self) -> &DesktopRecordingSessionConfig {
        &self.recording
    }

    #[must_use]
    pub const fn monitoring(&self) -> &DesktopMonitoringConfig {
        &self.monitoring
    }
}

/// Monitoring evidence returned alongside every completed recording action.
#[derive(Debug)]
pub struct DesktopMonitoringShutdown {
    snapshot: MonitoringSnapshot,
    output_error: Option<DesktopOutputStopError>,
}

impl DesktopMonitoringShutdown {
    #[must_use]
    pub const fn snapshot(&self) -> MonitoringSnapshot {
        self.snapshot
    }

    #[must_use]
    pub const fn output_error(&self) -> Option<&DesktopOutputStopError> {
        self.output_error.as_ref()
    }

    #[must_use]
    pub fn into_output_error(self) -> Option<DesktopOutputStopError> {
        self.output_error
    }
}

/// A recording result paired with its independent monitor-path settlement.
#[derive(Debug)]
pub struct DesktopMonitoredRecordingOutcome<T> {
    recording: T,
    monitoring: DesktopMonitoringShutdown,
}

impl<T> DesktopMonitoredRecordingOutcome<T> {
    #[must_use]
    pub const fn recording(&self) -> &T {
        &self.recording
    }

    #[must_use]
    pub const fn monitoring(&self) -> &DesktopMonitoringShutdown {
        &self.monitoring
    }

    #[must_use]
    pub fn into_parts(self) -> (T, DesktopMonitoringShutdown) {
        (self.recording, self.monitoring)
    }
}

pub type DesktopMonitoredPublishedRecording =
    DesktopMonitoredRecordingOutcome<DesktopPublishedRecording>;
pub type DesktopMonitoredFinalizedRecording =
    DesktopMonitoredRecordingOutcome<DesktopFinalizedRecording>;
pub type DesktopMonitoredRecordingAbort =
    DesktopMonitoredRecordingOutcome<RecordingWorkerCompletion>;

#[derive(Debug)]
pub enum DesktopMonitoredRecordingStartError {
    MonitoringCapacityUnavailable,
    RecordingConfig(DesktopRecordingSessionConfigError),
    RecordingTransport(RecordingTransportBuildError),
    RecordingWriter(RecordingWorkerStartError),
    MonitoringControl(super::DesktopMonitoringControlError),
    Input {
        journal_path: PathBuf,
        source: DesktopInputStartError,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
    Output {
        journal_path: PathBuf,
        source: DesktopOutputStartError,
        input_cleanup: Option<DesktopInputStopError>,
        writer: Box<DesktopRecordingWriterSettlement>,
    },
}

impl DesktopMonitoredRecordingStartError {
    /// Returns writer settlement retained after a native startup failure.
    #[must_use]
    pub const fn writer_settlement(&self) -> Option<&DesktopRecordingWriterSettlement> {
        match self {
            Self::Input { writer, .. } | Self::Output { writer, .. } => Some(writer),
            Self::MonitoringCapacityUnavailable
            | Self::RecordingConfig(_)
            | Self::RecordingTransport(_)
            | Self::RecordingWriter(_)
            | Self::MonitoringControl(_) => None,
        }
    }
}

impl fmt::Display for DesktopMonitoredRecordingStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MonitoringCapacityUnavailable => {
                formatter.write_str("desktop monitoring capacity is unavailable")
            }
            Self::RecordingConfig(source) => {
                write!(formatter, "invalid recording session: {source}")
            }
            Self::RecordingTransport(source) => {
                write!(formatter, "recording transport startup failed: {source}")
            }
            Self::RecordingWriter(source) => {
                write!(formatter, "recording writer startup failed: {source}")
            }
            Self::MonitoringControl(source) => {
                write!(formatter, "recording monitor control is invalid: {source}")
            }
            Self::Input {
                journal_path,
                source,
                ..
            } => write!(
                formatter,
                "desktop input startup failed; recording was aborted and the last configured journal path was {}: {source}",
                journal_path.display()
            ),
            Self::Output {
                journal_path,
                source,
                input_cleanup,
                ..
            } => {
                write!(
                    formatter,
                    "desktop monitor output startup failed; recording was aborted and the last configured journal path was {}: {source}",
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

impl Error for DesktopMonitoredRecordingStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RecordingConfig(source) => Some(source),
            Self::RecordingTransport(source) => Some(source),
            Self::RecordingWriter(source) => Some(source),
            Self::MonitoringControl(source) => Some(source),
            Self::Input { source, .. } => Some(source),
            Self::Output { source, .. } => Some(source),
            Self::MonitoringCapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub enum DesktopMonitoredRecordingStopError {
    Input {
        journal_path: PathBuf,
        source: DesktopInputStopError,
        writer: Box<DesktopRecordingWriterSettlement>,
        monitoring: Box<DesktopMonitoringShutdown>,
    },
    Recording {
        source: Box<DesktopRecordingSessionStopError>,
        monitoring: Box<DesktopMonitoringShutdown>,
    },
}

impl DesktopMonitoredRecordingStopError {
    #[must_use]
    pub const fn monitoring(&self) -> &DesktopMonitoringShutdown {
        match self {
            Self::Input { monitoring, .. } | Self::Recording { monitoring, .. } => monitoring,
        }
    }

    /// Returns writer settlement retained specifically after input shutdown
    /// failed. Other recording failures expose it through their source.
    #[must_use]
    pub const fn writer_settlement(&self) -> Option<&DesktopRecordingWriterSettlement> {
        match self {
            Self::Input { writer, .. } => Some(writer),
            Self::Recording { .. } => None,
        }
    }
}

impl fmt::Display for DesktopMonitoredRecordingStopError {
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
            Self::Recording { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopMonitoredRecordingStopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Input { source, .. } => Some(source),
            Self::Recording { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum DesktopMonitoredRecordingCommitError {
    Stop(Box<DesktopMonitoredRecordingStopError>),
    Completion {
        source: Box<DesktopRecordingSessionCommitError>,
        monitoring: Box<DesktopMonitoringShutdown>,
    },
}

impl DesktopMonitoredRecordingCommitError {
    #[must_use]
    pub const fn monitoring(&self) -> &DesktopMonitoringShutdown {
        match self {
            Self::Stop(source) => source.monitoring(),
            Self::Completion { monitoring, .. } => monitoring,
        }
    }
}

impl fmt::Display for DesktopMonitoredRecordingCommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stop(source) => source.fmt(formatter),
            Self::Completion { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopMonitoredRecordingCommitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Stop(source) => Some(source),
            Self::Completion { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum DesktopMonitoredRecordingFinalizeError {
    Commit(Box<DesktopMonitoredRecordingCommitError>),
    Metadata {
        source: Box<RecordingMetadataBuildError>,
        monitoring: Box<DesktopMonitoringShutdown>,
    },
}

impl DesktopMonitoredRecordingFinalizeError {
    #[must_use]
    pub const fn monitoring(&self) -> &DesktopMonitoringShutdown {
        match self {
            Self::Commit(source) => source.monitoring(),
            Self::Metadata { monitoring, .. } => monitoring,
        }
    }
}

impl fmt::Display for DesktopMonitoredRecordingFinalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Commit(source) => source.fmt(formatter),
            Self::Metadata { source, .. } => {
                write!(formatter, "recording metadata validation failed: {source}")
            }
        }
    }
}

impl Error for DesktopMonitoredRecordingFinalizeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Commit(source) => Some(source),
            Self::Metadata { source, .. } => Some(source),
        }
    }
}

/// Exclusive native capture owner for simultaneous lossless recording and
/// bounded stereo input monitoring.
#[derive(Debug)]
pub struct DesktopMonitoredRecordingSession {
    input: NativeInputStream,
    output: NativeOutputStream,
    recording: RecordingSessionCore,
    config: DesktopMonitoredRecordingConfig,
    monitoring_counters: MonitoringCounters,
    control: DesktopMonitoringControl,
    meter: InputMeterReceiver,
}

impl DesktopMonitoredRecordingSession {
    /// Starts the recording writer and both bounded transports before native
    /// capture. The recording producer receives every callback before the
    /// best-effort monitor producer.
    ///
    /// # Errors
    ///
    /// Returns all configuration, allocation, writer, and native startup
    /// failures. A failure after writer startup aborts and settles the writer.
    pub fn start(
        config: DesktopMonitoredRecordingConfig,
    ) -> Result<Self, DesktopMonitoredRecordingStartError> {
        Self::start_with_monitor_state(config, 1.0, false)
    }

    /// Starts capture with an initial monitor state applied before either
    /// native stream is published.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::start`] and rejects an invalid
    /// initial gain before the recording writer or native streams are started.
    pub fn start_with_monitor_state(
        config: DesktopMonitoredRecordingConfig,
        monitor_gain: f32,
        monitor_muted: bool,
    ) -> Result<Self, DesktopMonitoredRecordingStartError> {
        let (monitoring_producer, mut monitoring_consumer, monitoring_counters, control, meter) =
            try_monitoring_transport(&config.monitoring)
                .map_err(|_| DesktopMonitoredRecordingStartError::MonitoringCapacityUnavailable)?;
        control
            .set_gain_linear(monitor_gain)
            .map_err(DesktopMonitoredRecordingStartError::MonitoringControl)?;
        control.set_muted(monitor_muted);
        let native_input = NativeInputConfig::from(&config.recording.input);
        let (recording_producer, recording) = start_recording_core(config.recording.clone())
            .map_err(|source| match source {
                RecordingCoreStartError::Config(source) => {
                    DesktopMonitoredRecordingStartError::RecordingConfig(source)
                }
                RecordingCoreStartError::Transport(source) => {
                    DesktopMonitoredRecordingStartError::RecordingTransport(source)
                }
                RecordingCoreStartError::Writer(source) => {
                    DesktopMonitoredRecordingStartError::RecordingWriter(source)
                }
            })?;
        let mut producer = MonitoredRecordingProducer::new(recording_producer, monitoring_producer);
        let input = match NativeInputStream::start(native_input, move |samples: &[f32]| {
            producer.push(samples)
        }) {
            Ok(input) => input,
            Err(source) => {
                let journal_path = recording.journal_path().to_owned();
                let writer = Box::new(recording.abort_and_settle());
                return Err(DesktopMonitoredRecordingStartError::Input {
                    journal_path,
                    source: source.into(),
                    writer,
                });
            }
        };
        let output = match NativeOutputStream::start(
            config.monitoring.output().native(),
            move |samples: &mut [f32]| monitoring_consumer.process(samples),
        ) {
            Ok(output) => output,
            Err(source) => {
                let input_cleanup = input.stop().err().map(DesktopInputStopError::from);
                let journal_path = recording.journal_path().to_owned();
                let writer = Box::new(recording.abort_and_settle());
                return Err(DesktopMonitoredRecordingStartError::Output {
                    journal_path,
                    source: source.into(),
                    input_cleanup,
                    writer,
                });
            }
        };
        Ok(Self {
            input,
            output,
            recording,
            config,
            monitoring_counters,
            control,
            meter,
        })
    }

    #[must_use]
    pub const fn config(&self) -> &DesktopMonitoredRecordingConfig {
        &self.config
    }

    #[must_use]
    pub fn control(&self) -> DesktopMonitoringControl {
        self.control.clone()
    }

    #[must_use]
    pub fn monitoring_snapshot(&self) -> MonitoringSnapshot {
        self.monitoring_counters.snapshot()
    }

    /// Returns accepted-input meter evidence for this active recording owner.
    #[must_use]
    pub fn input_meter_receiver(&mut self) -> &mut InputMeterReceiver {
        &mut self.meter
    }

    /// Returns authoritative lossless recording transport progress.
    #[must_use]
    pub fn recording_snapshot(&self) -> RecordingTransportSnapshot {
        self.recording.transport_snapshot()
    }

    #[must_use]
    pub fn input_counters(&self) -> CaptureStreamCounters {
        self.input.counters()
    }

    #[must_use]
    pub fn output_counters(&self) -> OutputStreamCounters {
        self.output.counters()
    }

    #[must_use]
    pub fn input_status(&self) -> StreamStatus {
        self.input.status()
    }

    #[must_use]
    pub fn output_status(&self) -> OutputStreamStatus {
        self.output.status()
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        self.recording.journal_path()
    }

    /// Stops capture, publishes a lossless recording, and returns independent
    /// monitor shutdown evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when capture or writer integrity prevents publication.
    pub fn commit(
        self,
    ) -> Result<DesktopMonitoredPublishedRecording, DesktopMonitoredRecordingCommitError> {
        let completion = self
            .finish(true)
            .map_err(|source| DesktopMonitoredRecordingCommitError::Stop(Box::new(source)))?;
        let (completion, monitoring) = completion.into_parts();
        let recording = match super::recording_session::published_recording(completion) {
            Ok(recording) => recording,
            Err(source) => {
                return Err(DesktopMonitoredRecordingCommitError::Completion {
                    source: Box::new(source),
                    monitoring: Box::new(monitoring),
                });
            }
        };
        Ok(DesktopMonitoredRecordingOutcome {
            recording,
            monitoring,
        })
    }

    /// Publishes the recording and binds validated project provenance while
    /// retaining monitor shutdown evidence.
    ///
    /// # Errors
    ///
    /// Returns a commit error or a metadata validation error.
    pub fn finalize(
        self,
        context: RecordingMetadataContext,
    ) -> Result<DesktopMonitoredFinalizedRecording, DesktopMonitoredRecordingFinalizeError> {
        let committed = self
            .commit()
            .map_err(|source| DesktopMonitoredRecordingFinalizeError::Commit(Box::new(source)))?;
        let (publication, monitoring) = committed.into_parts();
        let recording = match hyper_recording_metadata::FinalizedRecordingMetadata::try_new(
            publication,
            context,
        ) {
            Ok(recording) => recording,
            Err(source) => {
                return Err(DesktopMonitoredRecordingFinalizeError::Metadata {
                    source: Box::new(source),
                    monitoring: Box::new(monitoring),
                });
            }
        };
        Ok(DesktopMonitoredRecordingOutcome {
            recording,
            monitoring,
        })
    }

    /// Stops capture and preserves the recoverable journal without publishing.
    ///
    /// # Errors
    ///
    /// Returns native shutdown, recording disposition, or writer errors with
    /// all retryable ownership retained.
    pub fn abort(
        self,
    ) -> Result<DesktopMonitoredRecordingAbort, DesktopMonitoredRecordingStopError> {
        self.finish(false)
    }

    fn finish(
        self,
        commit: bool,
    ) -> Result<DesktopMonitoredRecordingAbort, DesktopMonitoredRecordingStopError> {
        let Self {
            input,
            output,
            recording,
            monitoring_counters,
            ..
        } = self;
        let capture = input.counters();
        let input_result = input.stop().map_err(DesktopInputStopError::from);
        let output_error = output.stop().err().map(DesktopOutputStopError::from);
        let monitoring = DesktopMonitoringShutdown {
            snapshot: monitoring_counters.snapshot(),
            output_error,
        };
        if let Err(source) = input_result {
            let journal_path = recording.journal_path().to_owned();
            let writer = Box::new(recording.abort_and_settle());
            return Err(DesktopMonitoredRecordingStopError::Input {
                journal_path,
                source,
                writer,
                monitoring: Box::new(monitoring),
            });
        }
        let completion = match recording.finish(capture.snapshot(), commit) {
            Ok(completion) => completion,
            Err(source) => {
                return Err(DesktopMonitoredRecordingStopError::Recording {
                    source: Box::new(source),
                    monitoring: Box::new(monitoring),
                });
            }
        };
        Ok(DesktopMonitoredRecordingOutcome {
            recording: completion,
            monitoring,
        })
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn input_disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.input.disconnect_reason()
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn output_disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.output.disconnect_reason()
    }
}

#[cfg(test)]
mod tests {
    use super::super::monitoring_session::realtime_allocator::HeapMeasurement;
    use super::*;
    use crate::DesktopMonitoringControlError;
    use hyper_recording_journal::JournalConfig;
    use hyper_recording_transport::{RecordingPushError, try_recording_transport};
    use hyper_recording_worker::RecordingWorkerConfig;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    fn recording_config() -> DesktopRecordingSessionConfig {
        DesktopRecordingSessionConfig {
            input: super::super::DesktopInputConfig::try_new(48_000, 128).unwrap(),
            transport_capacity_frames: 1_024,
            writer: RecordingWorkerConfig {
                journal_path: PathBuf::from("unused.journal"),
                final_wav_path: PathBuf::from("unused.wav"),
                journal: JournalConfig {
                    sample_rate: 48_000,
                    max_chunk_frames: 128,
                    max_total_frames: 48_000,
                },
                drain_frames: 128,
            },
            completion_timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn monitored_recording_derives_the_only_input_contract() {
        let config = DesktopMonitoredRecordingConfig::try_new(
            recording_config(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            128,
        )
        .unwrap();

        assert_eq!(config.recording().input, *config.monitoring().input());
    }

    #[test]
    fn monitored_recording_rejects_a_divergent_output_rate() {
        let error = DesktopMonitoredRecordingConfig::try_new(
            recording_config(),
            DesktopOutputConfig::try_new(44_100, 128).unwrap(),
            1_024,
            256,
            128,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DesktopMonitoringConfigError::SampleRateMismatch {
                input_hz: 48_000,
                output_hz: 44_100
            }
        ));
    }

    #[test]
    fn invalid_initial_monitor_gain_precedes_writer_and_native_startup() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hyper-desktop-monitored-recording-{id}-{}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        let mut recording = recording_config();
        recording.writer.journal_path = directory.join("take.journal");
        recording.writer.final_wav_path = directory.join("take.wav");
        let config = DesktopMonitoredRecordingConfig::try_new(
            recording,
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            128,
        )
        .unwrap();

        assert!(matches!(
            DesktopMonitoredRecordingSession::start_with_monitor_state(config, f32::NAN, false),
            Err(DesktopMonitoredRecordingStartError::MonitoringControl(
                DesktopMonitoringControlError::NonFiniteGain
            ))
        ));
        assert!(fs::read_dir(&directory).unwrap().next().is_none());
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn recording_acceptance_precedes_best_effort_monitoring() {
        let config = DesktopMonitoredRecordingConfig::try_new(
            recording_config(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            128,
        )
        .unwrap();
        let (recording, _recording_consumer) = try_recording_transport(2).unwrap();
        let (monitoring, _monitoring_consumer, counters, _control, mut meter) =
            try_monitoring_transport(config.monitoring()).unwrap();
        let mut producer = MonitoredRecordingProducer::new(recording, monitoring);
        let samples = [0.25; 4];

        producer.push(&samples).unwrap();
        assert_eq!(counters.snapshot().accepted_frame_count, 2);
        assert!(matches!(
            producer.push(&samples),
            Err(RecordingPushError::Saturated { .. })
        ));
        assert_eq!(counters.snapshot().accepted_frame_count, 2);
        let meter = meter
            .read_latest()
            .snapshot()
            .expect("recording-accepted input meter snapshot");
        assert_eq!(meter.accepted_frame_end(), 2);
        assert_eq!(meter.frame_count(), 2);
    }

    #[test]
    fn capture_fanout_performs_zero_heap_operations() {
        let config = DesktopMonitoredRecordingConfig::try_new(
            recording_config(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            128,
        )
        .unwrap();
        let (recording, _recording_consumer) = try_recording_transport(256).unwrap();
        let (monitoring, _monitoring_consumer, _counters, _control, _meter) =
            try_monitoring_transport(config.monitoring()).unwrap();
        let mut producer = MonitoredRecordingProducer::new(recording, monitoring);
        let samples = [0.25; 128];
        let measurement = HeapMeasurement::start_verified();

        producer.push(&samples).unwrap();

        assert_eq!(measurement.finish(), 0);
    }
}
