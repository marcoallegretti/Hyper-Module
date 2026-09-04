use core::error::Error;
use core::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};
use hyper_desktop_audio::{
    CaptureStreamSnapshot, DesktopMonitoredRecordingConfig, DesktopMonitoredRecordingFinalizeError,
    DesktopMonitoredRecordingSession, DesktopMonitoredRecordingStartError,
    DesktopMonitoredRecordingStopError, DesktopMonitoringConfigError, DesktopOutputStopError,
    DesktopRecordingSessionConfig, MonitoringSnapshot, OutputStreamSnapshot, StreamState,
};
use hyper_playback_compiler::{FrameTimeline, FrameTimelineError};
use hyper_recording_journal::JournalConfig;
use hyper_recording_metadata::{
    RecordingCaptureKind, RecordingHandoffError, RecordingMetadataContext,
};
use hyper_recording_worker::{
    RecordingWorkerCompletion, RecordingWorkerConfig, RecordingWorkerFailure,
};
use hyper_storage_engine::{
    RecordingImportError, RecordingImportOutcome, import_finalized_recording_handoff,
};
use uuid::Uuid;

use super::{
    DESKTOP_MONITORING_CHUNK_FRAMES, DESKTOP_MONITORING_TARGET_FRAMES,
    DESKTOP_MONITORING_TRANSPORT_FRAMES, DESKTOP_RECORDING_COMPLETION_TIMEOUT,
    DESKTOP_RECORDING_DIRECTORY, DESKTOP_RECORDING_DRAIN_FRAMES, DESKTOP_RECORDING_MAXIMUM_FRAMES,
    DESKTOP_RECORDING_TRANSPORT_FRAMES, DesktopDirectControlError, DesktopIdeApplication,
    IdeInputMeterObservation, IdeMixerChannelKind, IdeMonitoringCommand, IdeMonitoringCommandError,
    IdeNativeCloseFailure, IdeProjectOpenError, IdeProjectSaveError,
    MONITOR_GAIN_MILLIONTHS_MAXIMUM, RunningDesktopIdeApplication,
};

const BACKUPS_DIRECTORY: &str = "backups";
const RECORDING_JOURNAL_FILE: &str = "capture.journal";
const RECORDING_WAV_FILE: &str = "capture.wav";

#[derive(Clone, Debug)]
pub(crate) struct IdeRecordingTarget {
    project_path: PathBuf,
    project_id: String,
    project_name: String,
    channel_index: usize,
    track_id: String,
    track_name: String,
    monitor_gain: f32,
    monitor_muted: bool,
    observed_frame: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct IdeRecordingPlan {
    project_path: PathBuf,
    project_id: String,
    project_name: String,
    track_id: String,
    track_name: String,
    sample_rate_hz: u32,
    start_frame: usize,
    start_tick: u64,
    monitor_gain: f32,
    monitor_muted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeRecordingPreparationError {
    ProjectSnapshotUnavailable,
    InputCueUnavailable,
    InputCueNotRunning(StreamState),
    TrackUnavailable,
    TrackNotAudio,
    ProjectChanged,
    CapacityUnavailable,
    Timeline(FrameTimelineError),
}

impl fmt::Display for IdeRecordingPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectSnapshotUnavailable => {
                formatter.write_str("running playback exposes no verified project snapshot")
            }
            Self::InputCueUnavailable => {
                formatter.write_str("recording requires an explicitly selected input cue")
            }
            Self::InputCueNotRunning(state) => {
                write!(formatter, "recording input cue is not running ({state:?})")
            }
            Self::TrackUnavailable => {
                formatter.write_str("recording target is absent from the compiled mixer")
            }
            Self::TrackNotAudio => formatter.write_str("recording target must be an audio track"),
            Self::ProjectChanged => {
                formatter.write_str("verified project changed while recording was prepared")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("recording preparation capacity is unavailable")
            }
            Self::Timeline(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeRecordingPreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeRecordingStartError {
    AudioConfiguration(Box<IdeProjectOpenError>),
    InputCueUnavailable,
    MonitoringConfiguration(DesktopMonitoringConfigError),
    Workspace {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    UnsafeWorkspace(PathBuf),
    Backend(Box<DesktopMonitoredRecordingStartError>),
}

impl fmt::Display for IdeRecordingStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AudioConfiguration(source) => source.fmt(formatter),
            Self::InputCueUnavailable => {
                formatter.write_str("recording requires an explicitly selected input")
            }
            Self::MonitoringConfiguration(source) => source.fmt(formatter),
            Self::Workspace {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
            Self::UnsafeWorkspace(path) => write!(
                formatter,
                "recording workspace is not a direct project-owned directory: {}",
                path.display()
            ),
            Self::Backend(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeRecordingStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AudioConfiguration(source) => Some(source.as_ref()),
            Self::MonitoringConfiguration(source) => Some(source),
            Self::Workspace { source, .. } => Some(source),
            Self::Backend(source) => Some(source.as_ref()),
            Self::InputCueUnavailable | Self::UnsafeWorkspace(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeRecordingFinalizeError {
    Backend(Box<DesktopMonitoredRecordingFinalizeError>),
    Handoff {
        journal_path: PathBuf,
        source: RecordingHandoffError,
    },
    Import {
        journal_path: PathBuf,
        source: RecordingImportError,
    },
}

impl IdeRecordingFinalizeError {
    /// Returns whether the recording import became the active project state
    /// before finalization reported an error.
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Import { source, .. } => source.published(),
            Self::Backend(_) | Self::Handoff { .. } => false,
        }
    }
}

impl fmt::Display for IdeRecordingFinalizeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(source) => source.fmt(formatter),
            Self::Handoff {
                journal_path,
                source,
            } => write!(
                formatter,
                "recording was published but its project handoff failed; recovery journal {} remains available: {source}",
                journal_path.display()
            ),
            Self::Import {
                journal_path,
                source,
            } => write!(
                formatter,
                "recording was published but project import failed; recovery journal {} remains available: {source}",
                journal_path.display()
            ),
        }
    }
}

impl Error for IdeRecordingFinalizeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source.as_ref()),
            Self::Handoff { source, .. } => Some(source),
            Self::Import { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeRecordingAbortError {
    Backend(Box<DesktopMonitoredRecordingStopError>),
    Writer {
        frames: u64,
        journal_path: Option<PathBuf>,
        source: RecordingWorkerFailure,
    },
    UnexpectedPublication,
}

impl fmt::Display for IdeRecordingAbortError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(source) => source.fmt(formatter),
            Self::Writer {
                frames,
                journal_path,
                source,
            } => {
                write!(formatter, "recording abort failed after {frames} frames")?;
                if let Some(path) = journal_path {
                    write!(formatter, "; recovery journal: {}", path.display())?;
                }
                write!(formatter, ": {source}")
            }
            Self::UnexpectedPublication => {
                formatter.write_str("recording abort unexpectedly published media")
            }
        }
    }
}

impl Error for IdeRecordingAbortError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source.as_ref()),
            Self::Writer { source, .. } => Some(source),
            Self::UnexpectedPublication => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeRecordingError {
    Preparation(IdeRecordingPreparationError),
    MixerSave(Box<IdeProjectSaveError>),
    NativeClose(IdeNativeCloseFailure),
    Start(Box<IdeRecordingStartError>),
    Finalize(Box<IdeRecordingFinalizeError>),
    Abort(Box<IdeRecordingAbortError>),
    Restore(DesktopDirectControlError),
    RestoreAfterFailure {
        operation: Box<IdeRecordingError>,
        restore: DesktopDirectControlError,
    },
    ReopenAfterFailure {
        operation: Box<IdeRecordingError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ImportedButReopenFailed {
        outcome: Box<IdeRecordingImportOutcome>,
        reopen: Box<IdeProjectOpenError>,
    },
    AbortedButReopenFailed {
        outcome: Box<IdeRecordingAbortOutcome>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeRecordingError {
    /// Returns whether a finalized recording import became the active project
    /// state before this error was observed.
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::Finalize(source) => source.publication_committed(),
            Self::RestoreAfterFailure { operation, .. }
            | Self::ReopenAfterFailure { operation, .. } => operation.publication_committed(),
            Self::ImportedButReopenFailed { .. } => true,
            Self::Preparation(_)
            | Self::MixerSave(_)
            | Self::NativeClose(_)
            | Self::Start(_)
            | Self::Abort(_)
            | Self::Restore(_)
            | Self::AbortedButReopenFailed { .. } => false,
        }
    }
}

impl fmt::Display for IdeRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Preparation(source) => source.fmt(formatter),
            Self::MixerSave(source) => write!(formatter, "prepare recording mixer state: {source}"),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before recording: {source:?}"
            ),
            Self::Start(source) => source.fmt(formatter),
            Self::Finalize(source) => source.fmt(formatter),
            Self::Abort(source) => source.fmt(formatter),
            Self::Restore(source) => write!(
                formatter,
                "recording completed but its stopped transport position could not be restored: {source}"
            ),
            Self::RestoreAfterFailure { operation, restore } => write!(
                formatter,
                "recording operation failed ({operation}) and its stopped transport position could not be restored ({restore})"
            ),
            Self::ReopenAfterFailure { operation, reopen } => write!(
                formatter,
                "recording operation failed ({operation}) and the project could not be reopened ({reopen})"
            ),
            Self::ImportedButReopenFailed { outcome, reopen } => write!(
                formatter,
                "recording was imported as project revision {} but the project could not be reopened ({reopen}); recovery journal: {}",
                outcome.project_revision(),
                outcome.journal_path().display()
            ),
            Self::AbortedButReopenFailed { outcome, reopen } => write!(
                formatter,
                "recording was aborted but the project could not be reopened ({reopen}); recovery journal: {}",
                outcome.journal_path().display()
            ),
        }
    }
}

impl Error for IdeRecordingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Preparation(source) => Some(source),
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Start(source) => Some(source.as_ref()),
            Self::Finalize(source) => Some(source.as_ref()),
            Self::Abort(source) => Some(source.as_ref()),
            Self::Restore(source) => Some(source),
            Self::RestoreAfterFailure { operation, .. }
            | Self::ReopenAfterFailure { operation, .. } => Some(operation.as_ref()),
            Self::ImportedButReopenFailed { reopen, .. }
            | Self::AbortedButReopenFailed { reopen, .. } => Some(reopen.as_ref()),
            Self::NativeClose(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdeRecordingObservation {
    accepted_frames: u64,
    rejected_frames: u64,
    monitor_gain: f32,
    monitor_muted: bool,
    input_state: StreamState,
    input: CaptureStreamSnapshot,
    output_state: StreamState,
    output: OutputStreamSnapshot,
    monitoring: MonitoringSnapshot,
    input_meter: IdeInputMeterObservation,
}

impl IdeRecordingObservation {
    #[must_use]
    pub const fn accepted_frames(self) -> u64 {
        self.accepted_frames
    }

    #[must_use]
    pub const fn rejected_frames(self) -> u64 {
        self.rejected_frames
    }

    #[must_use]
    pub const fn monitor_gain(self) -> f32 {
        self.monitor_gain
    }

    #[must_use]
    pub const fn monitor_muted(self) -> bool {
        self.monitor_muted
    }

    #[must_use]
    pub const fn input_state(self) -> StreamState {
        self.input_state
    }

    #[must_use]
    pub const fn input(self) -> CaptureStreamSnapshot {
        self.input
    }

    #[must_use]
    pub const fn output_state(self) -> StreamState {
        self.output_state
    }

    #[must_use]
    pub const fn output(self) -> OutputStreamSnapshot {
        self.output
    }

    #[must_use]
    pub const fn monitoring(self) -> MonitoringSnapshot {
        self.monitoring
    }

    /// Returns raw accepted-input sample-peak presentation for this active
    /// monitored recording session. It is not evidence of finalized file data.
    #[must_use]
    pub const fn input_meter(self) -> IdeInputMeterObservation {
        self.input_meter
    }
}

#[derive(Debug)]
pub struct IdeRecordingImportOutcome {
    project_revision: u64,
    frames: u64,
    recording_path: String,
    metadata_path: String,
    journal_path: PathBuf,
    monitor_output_error: Option<DesktopOutputStopError>,
}

impl IdeRecordingImportOutcome {
    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    #[must_use]
    pub fn recording_path(&self) -> &str {
        &self.recording_path
    }

    #[must_use]
    pub fn metadata_path(&self) -> &str {
        &self.metadata_path
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[must_use]
    pub const fn monitor_output_error(&self) -> Option<&DesktopOutputStopError> {
        self.monitor_output_error.as_ref()
    }
}

#[derive(Debug)]
pub struct IdeRecordingAbortOutcome {
    frames: u64,
    journal_path: PathBuf,
    monitor_output_error: Option<DesktopOutputStopError>,
}

impl IdeRecordingAbortOutcome {
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[must_use]
    pub const fn monitor_output_error(&self) -> Option<&DesktopOutputStopError> {
        self.monitor_output_error.as_ref()
    }
}

#[derive(Debug)]
pub(crate) struct RecordingDesktopIdeApplication {
    application: DesktopIdeApplication,
    session: DesktopMonitoredRecordingSession,
    context: RecordingMetadataContext,
    project_path: PathBuf,
    project_id: String,
    project_name: String,
    track_id: String,
    track_name: String,
    sample_rate_hz: u32,
    start_frame: usize,
    start_tick: u64,
}

#[derive(Debug)]
pub(crate) struct IdeRecordingStartFailure {
    pub(crate) application: DesktopIdeApplication,
    pub(crate) project_path: PathBuf,
    pub(crate) start_frame: usize,
    pub(crate) reason: IdeRecordingStartError,
}

#[derive(Debug)]
pub(crate) struct IdeRecordingFinalizeFailure {
    pub(crate) application: DesktopIdeApplication,
    pub(crate) project_path: PathBuf,
    pub(crate) start_frame: usize,
    pub(crate) reason: IdeRecordingFinalizeError,
}

#[derive(Debug)]
pub(crate) struct IdeRecordingAbortFailure {
    pub(crate) application: DesktopIdeApplication,
    pub(crate) project_path: PathBuf,
    pub(crate) start_frame: usize,
    pub(crate) reason: IdeRecordingAbortError,
}

#[derive(Debug)]
pub(crate) struct IdeRecordingFinalized {
    pub(crate) application: DesktopIdeApplication,
    pub(crate) project_path: PathBuf,
    pub(crate) start_frame: usize,
    pub(crate) outcome: IdeRecordingImportOutcome,
}

#[derive(Debug)]
pub(crate) struct IdeRecordingAborted {
    pub(crate) application: DesktopIdeApplication,
    pub(crate) project_path: PathBuf,
    pub(crate) start_frame: usize,
    pub(crate) outcome: IdeRecordingAbortOutcome,
}

impl RunningDesktopIdeApplication {
    pub(crate) fn prepare_recording_target(
        &self,
        channel_index: usize,
    ) -> Result<IdeRecordingTarget, IdeRecordingPreparationError> {
        let snapshot = self
            .snapshot()
            .ok_or(IdeRecordingPreparationError::ProjectSnapshotUnavailable)?;
        let monitoring = snapshot
            .monitoring
            .ok_or(IdeRecordingPreparationError::InputCueUnavailable)?;
        if monitoring.input_state != StreamState::Running {
            return Err(IdeRecordingPreparationError::InputCueNotRunning(
                monitoring.input_state,
            ));
        }
        if channel_index >= snapshot.mixer_channel_count {
            return Err(IdeRecordingPreparationError::TrackUnavailable);
        }
        if snapshot.mixer_channel_kinds[channel_index] != IdeMixerChannelKind::Audio {
            return Err(IdeRecordingPreparationError::TrackNotAudio);
        }
        Ok(IdeRecordingTarget {
            project_path: snapshot.project_path.to_path_buf(),
            project_id: try_owned(snapshot.project_id)?,
            project_name: try_owned(snapshot.project_name)?,
            channel_index,
            track_id: try_owned(snapshot.mixer_channel_ids[channel_index])?,
            track_name: try_owned(snapshot.mixer_channel_names[channel_index])?,
            monitor_gain: monitoring.gain,
            monitor_muted: monitoring.muted,
            observed_frame: snapshot.recovery.position_frame(),
        })
    }
}

impl IdeRecordingTarget {
    #[must_use]
    pub(crate) fn project_path(&self) -> &Path {
        &self.project_path
    }

    #[must_use]
    pub(crate) const fn observed_frame(&self) -> usize {
        self.observed_frame
    }
}

impl IdeRecordingPlan {
    pub(crate) fn prepare(
        target: IdeRecordingTarget,
        recovery: &hyper_desktop_audio::DesktopPlaybackRecovery,
    ) -> Result<Self, IdeRecordingPreparationError> {
        let project = recovery
            .prepared_project()
            .ok_or(IdeRecordingPreparationError::ProjectSnapshotUnavailable)?;
        let (loaded, mixer, _, _) = project.session().parts();
        if project.package().manifest().project_id() != target.project_id {
            return Err(IdeRecordingPreparationError::ProjectChanged);
        }
        let slot = mixer
            .slots()
            .get(target.channel_index)
            .ok_or(IdeRecordingPreparationError::TrackUnavailable)?;
        if slot.track_id() != target.track_id {
            return Err(IdeRecordingPreparationError::ProjectChanged);
        }
        if slot.kind() != hyper_playback_json::CompiledMixerTrackKind::Audio
            || !loaded.is_audio_track(slot.track_id())
        {
            return Err(IdeRecordingPreparationError::TrackNotAudio);
        }
        let start_frame = recovery.checkpoint().position_frame();
        let timeline = FrameTimeline::try_new(loaded.sample_rate_hz(), loaded.tempo_events())
            .map_err(IdeRecordingPreparationError::Timeline)?;
        // Version-1 recording metadata stores integer ticks. Anchoring to the
        // first tick at or after the sample frame never claims an earlier start.
        let start_tick = timeline
            .position_at_frame(start_frame)
            .and_then(|position| position.quantize_forward(1))
            .map_err(IdeRecordingPreparationError::Timeline)?;
        Ok(Self {
            project_path: project.package().root().to_path_buf(),
            project_id: target.project_id,
            project_name: target.project_name,
            track_id: target.track_id,
            track_name: target.track_name,
            sample_rate_hz: loaded.sample_rate_hz(),
            start_frame,
            start_tick,
            monitor_gain: target.monitor_gain,
            monitor_muted: target.monitor_muted,
        })
    }
}

impl DesktopIdeApplication {
    pub(crate) fn start_recording(
        self,
        plan: IdeRecordingPlan,
    ) -> Result<RecordingDesktopIdeApplication, Box<IdeRecordingStartFailure>> {
        let project_path = plan.project_path.clone();
        let start_frame = plan.start_frame;
        match start_recording(self, plan) {
            Ok(recording) => Ok(recording),
            Err(failure) => {
                let (application, reason) = *failure;
                Err(Box::new(IdeRecordingStartFailure {
                    application,
                    project_path,
                    start_frame,
                    reason,
                }))
            }
        }
    }
}

impl RecordingDesktopIdeApplication {
    #[must_use]
    pub(crate) fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub(crate) fn project_name(&self) -> &str {
        &self.project_name
    }

    #[must_use]
    pub(crate) fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub(crate) fn track_name(&self) -> &str {
        &self.track_name
    }

    #[must_use]
    pub(crate) const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub(crate) const fn start_frame(&self) -> usize {
        self.start_frame
    }

    #[must_use]
    pub(crate) const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    #[must_use]
    pub(crate) fn journal_path(&self) -> &Path {
        self.session.journal_path()
    }

    #[must_use]
    pub(crate) fn observation(&mut self) -> IdeRecordingObservation {
        let recording = self.session.recording_snapshot();
        let control = self.session.control();
        let input_meter = self
            .application
            .observe_input_meter(Some(self.session.input_meter_receiver()));
        IdeRecordingObservation {
            accepted_frames: recording.accepted_frame_count,
            rejected_frames: recording.rejected_frame_count,
            monitor_gain: control.gain_linear(),
            monitor_muted: control.muted(),
            input_state: self.session.input_status().state(),
            input: self.session.input_counters().snapshot(),
            output_state: self.session.output_status().state(),
            output: self.session.output_counters().snapshot(),
            monitoring: self.session.monitoring_snapshot(),
            input_meter,
        }
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "the integer cue domain is bounded below the exact f32 integer range"
    )]
    pub(crate) fn apply_monitoring(
        &mut self,
        command: IdeMonitoringCommand,
    ) -> Result<(), IdeMonitoringCommandError> {
        let control = self.session.control();
        match command {
            IdeMonitoringCommand::Gain { gain_millionths } => {
                if gain_millionths > MONITOR_GAIN_MILLIONTHS_MAXIMUM {
                    return Err(IdeMonitoringCommandError::ExcessiveGain);
                }
                control
                    .set_gain_linear(
                        gain_millionths as f32 / MONITOR_GAIN_MILLIONTHS_MAXIMUM as f32,
                    )
                    .map_err(IdeMonitoringCommandError::Backend)
            }
            IdeMonitoringCommand::Mute { muted } => {
                control.set_muted(muted);
                Ok(())
            }
        }
    }

    pub(crate) fn finalize(
        self,
    ) -> Result<IdeRecordingFinalized, Box<IdeRecordingFinalizeFailure>> {
        let Self {
            application,
            session,
            context,
            project_path,
            start_frame,
            ..
        } = self;
        let journal_path = session.journal_path().to_path_buf();
        let finalized = match session.finalize(context) {
            Ok(finalized) => finalized,
            Err(reason) => {
                return Err(Box::new(IdeRecordingFinalizeFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingFinalizeError::Backend(Box::new(reason)),
                }));
            }
        };
        let (recording, monitoring) = finalized.into_parts();
        let frames = recording.publication().frames();
        let handoff = match recording.to_handoff_json() {
            Ok(handoff) => handoff,
            Err(source) => {
                return Err(Box::new(IdeRecordingFinalizeFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingFinalizeError::Handoff {
                        journal_path,
                        source,
                    },
                }));
            }
        };
        let imported = match import_finalized_recording_handoff(&project_path, &handoff) {
            Ok(imported) => imported,
            Err(source) => {
                return Err(Box::new(IdeRecordingFinalizeFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingFinalizeError::Import {
                        journal_path,
                        source,
                    },
                }));
            }
        };
        Ok(IdeRecordingFinalized {
            application,
            project_path,
            start_frame,
            outcome: imported_outcome(
                &imported,
                frames,
                journal_path,
                monitoring.into_output_error(),
            ),
        })
    }

    pub(crate) fn abort(self) -> Result<IdeRecordingAborted, Box<IdeRecordingAbortFailure>> {
        let Self {
            application,
            session,
            project_path,
            start_frame,
            ..
        } = self;
        let aborted = match session.abort() {
            Ok(aborted) => aborted,
            Err(reason) => {
                return Err(Box::new(IdeRecordingAbortFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingAbortError::Backend(Box::new(reason)),
                }));
            }
        };
        let (completion, monitoring) = aborted.into_parts();
        let outcome = match completion {
            RecordingWorkerCompletion::Aborted {
                frames,
                journal_path,
            } => IdeRecordingAbortOutcome {
                frames,
                journal_path,
                monitor_output_error: monitoring.into_output_error(),
            },
            RecordingWorkerCompletion::Failed {
                frames,
                journal_path,
                error,
            } => {
                return Err(Box::new(IdeRecordingAbortFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingAbortError::Writer {
                        frames,
                        journal_path,
                        source: error,
                    },
                }));
            }
            RecordingWorkerCompletion::Published(_) => {
                return Err(Box::new(IdeRecordingAbortFailure {
                    application,
                    project_path,
                    start_frame,
                    reason: IdeRecordingAbortError::UnexpectedPublication,
                }));
            }
        };
        Ok(IdeRecordingAborted {
            application,
            project_path,
            start_frame,
            outcome,
        })
    }
}

fn start_recording(
    mut application: DesktopIdeApplication,
    plan: IdeRecordingPlan,
) -> Result<RecordingDesktopIdeApplication, Box<(DesktopIdeApplication, IdeRecordingStartError)>> {
    let output = match application.output_config() {
        Ok(output) => output,
        Err(source) => {
            return Err(Box::new((
                application,
                IdeRecordingStartError::AudioConfiguration(Box::new(source)),
            )));
        }
    };
    let input = match application.input_config() {
        Ok(Some(input)) => input,
        Ok(None) => {
            return Err(Box::new((
                application,
                IdeRecordingStartError::InputCueUnavailable,
            )));
        }
        Err(source) => {
            return Err(Box::new((
                application,
                IdeRecordingStartError::AudioConfiguration(Box::new(source)),
            )));
        }
    };
    let recording_id = Uuid::new_v4().hyphenated().to_string();
    let asset_id = Uuid::new_v4().hyphenated().to_string();
    let context = RecordingMetadataContext {
        id: recording_id.clone(),
        project_id: plan.project_id.clone(),
        asset_id,
        track_id: plan.track_id.clone(),
        capture_kind: RecordingCaptureKind::New,
        captured_at_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        timeline_start_tick: plan.start_tick,
        source_input_indices: vec![0, 1],
    };
    let workspace = match create_recording_workspace(&plan.project_path, &recording_id) {
        Ok(workspace) => workspace,
        Err(source) => return Err(Box::new((application, source))),
    };
    let recording = DesktopRecordingSessionConfig {
        input,
        transport_capacity_frames: DESKTOP_RECORDING_TRANSPORT_FRAMES,
        writer: RecordingWorkerConfig {
            journal_path: workspace.journal_path.clone(),
            final_wav_path: workspace.wav_path,
            journal: JournalConfig {
                sample_rate: plan.sample_rate_hz,
                max_chunk_frames: 4_096,
                max_total_frames: DESKTOP_RECORDING_MAXIMUM_FRAMES,
            },
            drain_frames: DESKTOP_RECORDING_DRAIN_FRAMES,
        },
        completion_timeout: DESKTOP_RECORDING_COMPLETION_TIMEOUT,
    };
    let config = match DesktopMonitoredRecordingConfig::try_new(
        recording,
        output,
        DESKTOP_MONITORING_TRANSPORT_FRAMES,
        DESKTOP_MONITORING_TARGET_FRAMES,
        DESKTOP_MONITORING_CHUNK_FRAMES,
    ) {
        Ok(config) => config,
        Err(source) => {
            remove_empty_workspace(&workspace.directory);
            return Err(Box::new((
                application,
                IdeRecordingStartError::MonitoringConfiguration(source),
            )));
        }
    };
    let session = match DesktopMonitoredRecordingSession::start_with_monitor_state(
        config,
        plan.monitor_gain,
        plan.monitor_muted,
    ) {
        Ok(session) => session,
        Err(source) => {
            remove_empty_workspace(&workspace.directory);
            return Err(Box::new((
                application,
                IdeRecordingStartError::Backend(Box::new(source)),
            )));
        }
    };
    application.reset_input_meter_observer();
    Ok(RecordingDesktopIdeApplication {
        application,
        session,
        context,
        project_path: plan.project_path,
        project_id: plan.project_id,
        project_name: plan.project_name,
        track_id: plan.track_id,
        track_name: plan.track_name,
        sample_rate_hz: plan.sample_rate_hz,
        start_frame: plan.start_frame,
        start_tick: plan.start_tick,
    })
}

fn imported_outcome(
    imported: &RecordingImportOutcome,
    frames: u64,
    journal_path: PathBuf,
    monitor_output_error: Option<DesktopOutputStopError>,
) -> IdeRecordingImportOutcome {
    IdeRecordingImportOutcome {
        project_revision: imported.project_revision(),
        frames,
        recording_path: imported.recording_path().to_owned(),
        metadata_path: imported.metadata_path().to_owned(),
        journal_path,
        monitor_output_error,
    }
}

struct RecordingWorkspace {
    directory: PathBuf,
    journal_path: PathBuf,
    wav_path: PathBuf,
}

fn create_recording_workspace(
    project_path: &Path,
    recording_id: &str,
) -> Result<RecordingWorkspace, IdeRecordingStartError> {
    let project = canonical_directory(project_path, "open recording project")?;
    let backups = direct_child_directory(&project, BACKUPS_DIRECTORY)?;
    let captures = direct_child_directory(&backups, DESKTOP_RECORDING_DIRECTORY)?;
    let directory = captures.join(recording_id);
    fs::create_dir(&directory).map_err(|source| IdeRecordingStartError::Workspace {
        operation: "create recording workspace",
        path: directory.clone(),
        source,
    })?;
    let canonical = canonical_directory(&directory, "verify recording workspace")?;
    if canonical.parent() != Some(captures.as_path()) {
        return Err(IdeRecordingStartError::UnsafeWorkspace(canonical));
    }
    Ok(RecordingWorkspace {
        journal_path: canonical.join(RECORDING_JOURNAL_FILE),
        wav_path: canonical.join(RECORDING_WAV_FILE),
        directory: canonical,
    })
}

fn direct_child_directory(parent: &Path, name: &str) -> Result<PathBuf, IdeRecordingStartError> {
    let path = parent.join(name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(IdeRecordingStartError::UnsafeWorkspace(path));
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(&path).map_err(|source| IdeRecordingStartError::Workspace {
                operation: "create recording directory",
                path: path.clone(),
                source,
            })?;
        }
        Err(source) => {
            return Err(IdeRecordingStartError::Workspace {
                operation: "inspect recording directory",
                path,
                source,
            });
        }
    }
    let canonical = canonical_directory(&path, "verify recording directory")?;
    if canonical.parent() != Some(parent) {
        return Err(IdeRecordingStartError::UnsafeWorkspace(canonical));
    }
    Ok(canonical)
}

fn canonical_directory(
    path: &Path,
    operation: &'static str,
) -> Result<PathBuf, IdeRecordingStartError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| IdeRecordingStartError::Workspace {
            operation,
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(IdeRecordingStartError::UnsafeWorkspace(path.to_path_buf()));
    }
    fs::canonicalize(path).map_err(|source| IdeRecordingStartError::Workspace {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

fn remove_empty_workspace(path: &Path) {
    let _ = fs::remove_dir(path);
}

fn try_owned(value: &str) -> Result<String, IdeRecordingPreparationError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| IdeRecordingPreparationError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn workspace_is_created_only_under_project_backups() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let project = std::env::temp_dir().join(format!(
            "hyper-ide-recording-workspace-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&project).unwrap();
        let workspace =
            create_recording_workspace(&project, "018f47a2-6b2d-7c11-8a44-001122334499").unwrap();

        assert_eq!(
            workspace.directory.parent().unwrap().file_name().unwrap(),
            DESKTOP_RECORDING_DIRECTORY
        );
        assert_eq!(
            workspace
                .directory
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            BACKUPS_DIRECTORY
        );
        assert_eq!(
            workspace.journal_path.file_name().unwrap(),
            RECORDING_JOURNAL_FILE
        );
        assert_eq!(workspace.wav_path.file_name().unwrap(), RECORDING_WAV_FILE);

        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn workspace_rejects_a_non_directory_backup_boundary() {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let project = std::env::temp_dir().join(format!(
            "hyper-ide-recording-unsafe-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&project).unwrap();
        fs::write(project.join(BACKUPS_DIRECTORY), b"not a directory").unwrap();

        assert!(matches!(
            create_recording_workspace(&project, "018f47a2-6b2d-7c11-8a44-001122334499"),
            Err(IdeRecordingStartError::UnsafeWorkspace(_))
        ));

        fs::remove_dir_all(project).unwrap();
    }
}
