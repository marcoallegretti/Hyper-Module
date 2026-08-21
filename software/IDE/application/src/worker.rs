use core::error::Error;
use core::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{
    Receiver, RecvError, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hyper_desktop_audio::{DesktopAudioPreviewSnapshot, DesktopAudioPreviewState};
use hyper_fs_authority::path_is_proven_absent;
use hyper_mixer_runtime::{LatestOutputMeterSnapshot, PlaybackRecoverySnapshot, TransportCommand};
use hyper_playback_json::SceneTrackAction;
use hyper_project_history::{ProjectHistory, ProjectHistoryError, RecentProject};
use hyper_recording_metadata::RecordingCaptureKind;
use hyper_runtime_bundle_builder::{
    DEFAULT_MAXIMUM_RUNTIME_BUNDLE_BYTES, PublishedRuntimeBundle, RuntimeBundleBuildError,
    build_project_runtime_bundle,
};
use hyper_storage_engine::{
    ProjectCreationError, ProjectCreationRequest, create_project as create_project_package,
};
use hyper_thread_supervisor::{ThreadPermit, ThreadSupervisor, ThreadSupervisorSnapshot};

use super::recording::{IdeRecordingPlan, IdeRecordingTarget, RecordingDesktopIdeApplication};
use super::{
    ControlBinding, ControlScope, ControlSource, ControlTarget, ControlTransform,
    DesktopIdeApplication, DesktopIdePendingClose, DesktopIdePendingSaveClose,
    DesktopPlaybackRecovery, Edge, IdeApplicationConfig, IdeArrangementLocateError,
    IdeArrangementPlacementEditError, IdeAudioClipActivationEditError,
    IdeAudioClipDuplicationError, IdeAudioClipFadeEditError, IdeAudioClipGainEditError,
    IdeAudioClipRemovalError,
    IdeAudioClipLoopEditError, IdeAudioClipRenameEditError, IdeAudioClipReverseEditError,
    IdeAudioClipSourceSlipEditError, IdeAudioImportBatchFailure, IdeAudioImportBatchOutcome,
    IdeAudioImportError, IdeAudioInputInventory, IdeAudioOutputInventory,
    IdeAudioPreviewControlCommand, IdeAudioPreviewError, IdeAudioTrackAdditionError,
    IdeAudioTrackEditError, IdeAudioTrackRemovalError, IdeAutomationTakeCreationError,
    IdeAutomationTakeEditError, IdeAutomationTakeRequest, IdeAutomationTarget, IdeCloseOutcome,
    IdeControlMappingEdit, IdeControlMappingEditError, IdeInputDiscoveryError, IdeInputSelection,
    IdeInputSelectionError, IdeLoopRangeFrames, IdeLoudnessCommand, IdeLoudnessCommandError,
    IdeMixerChannelKind, IdeMixerChannelState, IdeMixerCommand, IdeMixerCommandError,
    IdeMixerMainState, IdeMonitoringCommand, IdeMonitoringCommandError, IdeMonitoringState,
    IdeNativeCloseFailure, IdeOutputDiscoveryError, IdeOutputSelection, IdeOutputSelectionError,
    IdeOutputTestError, IdeOutputTestSnapshot, IdeOutputTestStartOutcome, IdeOutputTestStopOutcome,
    IdePendingCloseWait, IdePendingOutputTestShutdown, IdePendingOutputTestShutdownWait,
    IdePendingSaveCloseWait, IdeProjectExportError, IdeProjectOpenError, IdeProjectSaveError,
    IdeRecordingAbortError, IdeRecordingAbortOutcome, IdeRecordingClipCreationError,
    IdeRecordingError, IdeRecordingImportOutcome, IdeRecordingObservation, IdeResourceRestoreError,
    IdeSaveCloseOutcome, IdeSceneAvailability, IdeSceneCreationError, IdeSceneDuplicationError,
    IdeSceneEditError, IdeSceneLaunchQuantization, IdeSceneRenameError, IdeSceneSlotEdit,
    IdeSceneSlotEditError, IdeTempoEditError, IdeTransportSeekError, IdeUnusedSampleCleanupError,
    IdeUnusedSampleCleanupSummary, InputInventoryError,
    LocateState, OutputInventoryError, OutputStreamSnapshot, OutputStreamState,
    OutputTestingDesktopIdeApplication, RunningDesktopIdeApplication, RunningIdeSnapshot,
};

#[cfg(test)]
use super::IdeAudioClipRemovalCommittedWarning;

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
    StartOutputTest,
    StopOutputTest,
    CreateProject,
    OpenProject,
    Transport,
    Seek,
    LocatePlacement,
    LocateSceneLaunch,
    Loudness,
    Mixer,
    Monitoring,
    SaveProject,
    PrepareRuntime,
    AddAudioTrack,
    RemoveAudioTrack,
    RenameAudioTrack,
    SetAudioTrackActive,
    ImportAudio,
    CreateAudioClipFromTake,
    DuplicateAudioClip,
    RemoveAudioClip,
    CleanUnusedSamples,
    AddArrangementPlacement,
    AddArrangementSceneLaunch,
    MoveArrangementSceneLaunch,
    SetArrangementLoopRange,
    SetTempo,
    SetMeter,
    SetArrangementSceneLaunchScene,
    RemoveArrangementSceneLaunch,
    DuplicateArrangementPlacement,
    SplitArrangementPlacement,
    SetArrangementPlacementActive,
    MoveArrangementPlacement,
    SetArrangementPlacementDuration,
    TrimArrangementPlacementFront,
    RemoveArrangementPlacement,
    RenameAudioClip,
    SetAudioClipActive,
    SetAudioClipGain,
    SetAudioClipFades,
    SetAudioClipLoop,
    SetAudioClipReverse,
    SetAudioClipSourceStart,
    AddScene,
    DuplicateScene,
    SetSceneSlot,
    RenameScene,
    SetSceneActive,
    AddAutomationTake,
    SetAutomationTakeActive,
    EditControlMapping,
    ExportProject,
    Undo,
    Redo,
    StartRecording,
    StopRecording,
    AbortRecording,
    StartAudioPreview,
    StopAudioPreview,
    AudioPreviewControl,
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
    StartOutputTest,
    StopOutputTest,
    CreateProject {
        parent: PathBuf,
        directory_name: String,
        project_name: String,
    },
    OpenProject(PathBuf),
    OpenRecentProject(PathBuf),
    Transport(TransportCommand),
    Seek {
        frame: usize,
    },
    LocatePlacement {
        project_id: String,
        index: usize,
        placement_id: String,
    },
    LocateSceneLaunch {
        project_id: String,
        index: usize,
        launch_id: String,
        state: LocateState,
    },
    Loudness(IdeLoudnessCommand),
    Mixer(IdeMixerCommand),
    Monitoring(IdeMonitoringCommand),
    SaveProject,
    PrepareRuntime {
        output_path: PathBuf,
    },
    AddAudioTrack {
        name: String,
    },
    RemoveAudioTrack {
        track_id: String,
    },
    RenameAudioTrack {
        track_id: String,
        name: String,
    },
    SetAudioTrackActive {
        track_id: String,
        active: bool,
    },
    ImportAudio {
        paths: Vec<PathBuf>,
        track_id: String,
    },
    CreateAudioClipFromTake {
        recording_id: String,
        name: String,
    },
    DuplicateAudioClip {
        clip_id: String,
        name: String,
    },
    RemoveAudioClip {
        clip_id: String,
    },
    CleanUnusedSamples {
        confirmation: IdeUnusedSampleCleanupSummary,
    },
    AddArrangementPlacement {
        clip_id: String,
        start_tick: u64,
    },
    AddArrangementSceneLaunch {
        scene_id: String,
        tick: u64,
    },
    MoveArrangementSceneLaunch {
        launch_id: String,
        tick: u64,
    },
    SetMeter {
        numerator: u16,
        denominator: u16,
    },
    SetTempo {
        microseconds_per_quarter: u32,
    },
    SetArrangementLoopRange {
        range: Option<IdeLoopRangeFrames>,
    },
    SetArrangementSceneLaunchScene {
        launch_id: String,
        scene_id: String,
    },
    RemoveArrangementSceneLaunch {
        launch_id: String,
    },
    DuplicateArrangementPlacement {
        placement_id: String,
        start_tick: u64,
    },
    SplitArrangementPlacement {
        placement_id: String,
        split_tick: u64,
    },
    SetArrangementPlacementActive {
        placement_id: String,
        active: bool,
    },
    MoveArrangementPlacement {
        placement_id: String,
        start_tick: u64,
    },
    SetArrangementPlacementDuration {
        placement_id: String,
        duration_ticks: u64,
    },
    TrimArrangementPlacementFront {
        placement_id: String,
        start_tick: u64,
    },
    RemoveArrangementPlacement {
        placement_id: String,
    },
    RenameAudioClip {
        clip_id: String,
        name: String,
    },
    SetAudioClipActive {
        clip_id: String,
        active: bool,
    },
    SetAudioClipGain {
        clip_id: String,
        gain_decibels: f64,
    },
    SetAudioClipFades {
        clip_id: String,
        fade_in_ticks: u64,
        fade_out_ticks: u64,
    },
    SetAudioClipLoop {
        clip_id: String,
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
    },
    SetAudioClipReverse {
        clip_id: String,
        reverse: bool,
    },
    SetAudioClipSourceStart {
        clip_id: String,
        source_start_frame: u64,
    },
    AddScene {
        name: String,
        quantization: IdeSceneLaunchQuantization,
    },
    AddAutomationTake {
        request: IdeAutomationTakeRequest,
    },
    SetAutomationTakeActive {
        take_id: String,
        enabled: bool,
    },
    EditControlMapping {
        edit: IdeControlMappingEdit,
    },
    ExportProject {
        wav_path: std::path::PathBuf,
        block_frames: usize,
    },
    Undo,
    Redo,
    DuplicateScene {
        scene_id: String,
        name: String,
    },
    SetSceneSlot {
        scene_id: String,
        track_id: String,
        edit: IdeSceneSlotEdit,
    },
    RenameScene {
        scene_id: String,
        name: String,
    },
    SetSceneActive {
        scene_id: String,
        active: bool,
    },
    StartRecording {
        channel_index: usize,
    },
    StopRecording,
    AbortRecording,
    StartAudioPreview {
        asset_id: String,
    },
    StopAudioPreview,
    AudioPreviewControl(IdeAudioPreviewControlCommand),
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
            Self::StartOutputTest => IdeWorkerCommandKind::StartOutputTest,
            Self::StopOutputTest => IdeWorkerCommandKind::StopOutputTest,
            Self::CreateProject { .. } => IdeWorkerCommandKind::CreateProject,
            Self::OpenProject(_) | Self::OpenRecentProject(_) => IdeWorkerCommandKind::OpenProject,
            Self::Transport(_) => IdeWorkerCommandKind::Transport,
            Self::Seek { .. } => IdeWorkerCommandKind::Seek,
            Self::LocatePlacement { .. } => IdeWorkerCommandKind::LocatePlacement,
            Self::LocateSceneLaunch { .. } => IdeWorkerCommandKind::LocateSceneLaunch,
            Self::Loudness(_) => IdeWorkerCommandKind::Loudness,
            Self::Mixer(_) => IdeWorkerCommandKind::Mixer,
            Self::Monitoring(_) => IdeWorkerCommandKind::Monitoring,
            Self::SaveProject => IdeWorkerCommandKind::SaveProject,
            Self::PrepareRuntime { .. } => IdeWorkerCommandKind::PrepareRuntime,
            Self::AddAudioTrack { .. } => IdeWorkerCommandKind::AddAudioTrack,
            Self::RemoveAudioTrack { .. } => IdeWorkerCommandKind::RemoveAudioTrack,
            Self::RenameAudioTrack { .. } => IdeWorkerCommandKind::RenameAudioTrack,
            Self::SetAudioTrackActive { .. } => IdeWorkerCommandKind::SetAudioTrackActive,
            Self::ImportAudio { .. } => IdeWorkerCommandKind::ImportAudio,
            Self::CreateAudioClipFromTake { .. } => IdeWorkerCommandKind::CreateAudioClipFromTake,
            Self::DuplicateAudioClip { .. } => IdeWorkerCommandKind::DuplicateAudioClip,
            Self::RemoveAudioClip { .. } => IdeWorkerCommandKind::RemoveAudioClip,
            Self::CleanUnusedSamples { .. } => IdeWorkerCommandKind::CleanUnusedSamples,
            Self::AddArrangementPlacement { .. } => IdeWorkerCommandKind::AddArrangementPlacement,
            Self::AddArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::AddArrangementSceneLaunch
            }
            Self::MoveArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::MoveArrangementSceneLaunch
            }
            Self::SetArrangementLoopRange { .. } => IdeWorkerCommandKind::SetArrangementLoopRange,
            Self::SetTempo { .. } => IdeWorkerCommandKind::SetTempo,
            Self::SetMeter { .. } => IdeWorkerCommandKind::SetMeter,
            Self::SetArrangementSceneLaunchScene { .. } => {
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene
            }
            Self::RemoveArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch
            }
            Self::DuplicateArrangementPlacement { .. } => {
                IdeWorkerCommandKind::DuplicateArrangementPlacement
            }
            Self::SplitArrangementPlacement { .. } => {
                IdeWorkerCommandKind::SplitArrangementPlacement
            }
            Self::SetArrangementPlacementActive { .. } => {
                IdeWorkerCommandKind::SetArrangementPlacementActive
            }
            Self::MoveArrangementPlacement { .. } => IdeWorkerCommandKind::MoveArrangementPlacement,
            Self::SetArrangementPlacementDuration { .. } => {
                IdeWorkerCommandKind::SetArrangementPlacementDuration
            }
            Self::TrimArrangementPlacementFront { .. } => {
                IdeWorkerCommandKind::TrimArrangementPlacementFront
            }
            Self::RemoveArrangementPlacement { .. } => {
                IdeWorkerCommandKind::RemoveArrangementPlacement
            }
            Self::RenameAudioClip { .. } => IdeWorkerCommandKind::RenameAudioClip,
            Self::SetAudioClipActive { .. } => IdeWorkerCommandKind::SetAudioClipActive,
            Self::SetAudioClipGain { .. } => IdeWorkerCommandKind::SetAudioClipGain,
            Self::SetAudioClipFades { .. } => IdeWorkerCommandKind::SetAudioClipFades,
            Self::SetAudioClipLoop { .. } => IdeWorkerCommandKind::SetAudioClipLoop,
            Self::SetAudioClipReverse { .. } => IdeWorkerCommandKind::SetAudioClipReverse,
            Self::SetAudioClipSourceStart { .. } => IdeWorkerCommandKind::SetAudioClipSourceStart,
            Self::AddScene { .. } => IdeWorkerCommandKind::AddScene,
            Self::AddAutomationTake { .. } => IdeWorkerCommandKind::AddAutomationTake,
            Self::SetAutomationTakeActive { .. } => IdeWorkerCommandKind::SetAutomationTakeActive,
            Self::EditControlMapping { .. } => IdeWorkerCommandKind::EditControlMapping,
            Self::ExportProject { .. } => IdeWorkerCommandKind::ExportProject,
            Self::Undo => IdeWorkerCommandKind::Undo,
            Self::Redo => IdeWorkerCommandKind::Redo,
            Self::DuplicateScene { .. } => IdeWorkerCommandKind::DuplicateScene,
            Self::SetSceneSlot { .. } => IdeWorkerCommandKind::SetSceneSlot,
            Self::RenameScene { .. } => IdeWorkerCommandKind::RenameScene,
            Self::SetSceneActive { .. } => IdeWorkerCommandKind::SetSceneActive,
            Self::StartRecording { .. } => IdeWorkerCommandKind::StartRecording,
            Self::StopRecording => IdeWorkerCommandKind::StopRecording,
            Self::AbortRecording => IdeWorkerCommandKind::AbortRecording,
            Self::StartAudioPreview { .. } => IdeWorkerCommandKind::StartAudioPreview,
            Self::StopAudioPreview => IdeWorkerCommandKind::StopAudioPreview,
            Self::AudioPreviewControl(_) => IdeWorkerCommandKind::AudioPreviewControl,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeRecordingCaptureKind {
    New,
    Overdub,
}

impl From<RecordingCaptureKind> for IdeRecordingCaptureKind {
    fn from(value: RecordingCaptureKind) -> Self {
        match value {
            RecordingCaptureKind::New => Self::New,
            RecordingCaptureKind::Overdub => Self::Overdub,
        }
    }
}

#[derive(Debug)]
pub struct IdeRecordingTake {
    id: String,
    asset_id: String,
    track_id: String,
    track_name: String,
    capture_kind: IdeRecordingCaptureKind,
    captured_at_utc: String,
    timeline_start_tick: u64,
    sample_rate_hz: u32,
    channels: u8,
    frame_count: u64,
}

impl IdeRecordingTake {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
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
    pub const fn capture_kind(&self) -> IdeRecordingCaptureKind {
        self.capture_kind
    }

    #[must_use]
    pub fn captured_at_utc(&self) -> &str {
        &self.captured_at_utc
    }

    #[must_use]
    pub const fn timeline_start_tick(&self) -> u64 {
        self.timeline_start_tick
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn channels(&self) -> u8 {
        self.channels
    }

    #[must_use]
    pub const fn frame_count(&self) -> u64 {
        self.frame_count
    }
}

#[derive(Debug)]
pub struct IdeAudioPreviewObservation {
    snapshot: DesktopAudioPreviewSnapshot,
    asset_id: Option<String>,
}

impl IdeAudioPreviewObservation {
    #[must_use]
    pub const fn state(&self) -> DesktopAudioPreviewState {
        self.snapshot.state()
    }

    #[must_use]
    pub fn asset_id(&self) -> Option<&str> {
        self.asset_id.as_deref()
    }

    #[must_use]
    pub const fn played_frames(&self) -> u64 {
        self.snapshot.played_frames()
    }

    #[must_use]
    pub const fn total_frames(&self) -> u64 {
        self.snapshot.total_frames()
    }

    #[must_use]
    pub const fn gain(&self) -> f32 {
        self.snapshot.gain()
    }

    #[must_use]
    pub const fn muted(&self) -> bool {
        self.snapshot.muted()
    }
}

#[derive(Debug)]
pub struct IdeRunningObservation {
    playback_session_generation: u64,
    project_id: String,
    project_name: String,
    sample_rate_hz: u32,
    microseconds_per_quarter: u32,
    meter_events: Vec<hyper_playback_compiler::MeterEvent>,
    tempo_events: Vec<hyper_playback_compiler::TempoEvent>,
    playback_extent: super::IdePlaybackExtent,
    arrangement: Arc<super::IdePlaybackArrangement>,
    automation: Arc<super::IdePlaybackAutomation>,
    control_bindings: Vec<IdeControlBinding>,
    audio_clips: Vec<IdeAudioClip>,
    recording_takes: Vec<IdeRecordingTake>,
    unused_sample_cleanup: IdeUnusedSampleCleanupSummary,
    preview: IdeAudioPreviewObservation,
    scenes: Vec<IdeScene>,
    track_count: usize,
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
    id: String,
    name: String,
    active: bool,
    availability: IdeSceneAvailability,
    track_slots: Vec<IdeSceneTrackSlot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeSceneTrackSlot {
    track_id: String,
    track_name: String,
    action: IdeSceneTrackAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeSceneTrackAction {
    Continue,
    Stop,
    Play { clip_id: String },
}

#[derive(Clone, Debug, PartialEq)]
/// Owned UI-thread observation of one validated reusable audio clip.
pub struct IdeAudioClip {
    id: String,
    name: String,
    track_id: String,
    track_name: String,
    duration_ticks: u64,
    source_start_frame: usize,
    source_frame_count: usize,
    active: bool,
    gain_decibels: f64,
    loop_enabled: bool,
    loop_start_tick: u64,
    loop_end_tick: u64,
    reverse: bool,
    fade_in_ticks: u64,
    fade_out_ticks: u64,
}

impl IdeAudioClip {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
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
    pub const fn duration_ticks(&self) -> u64 {
        self.duration_ticks
    }

    #[must_use]
    pub const fn source_start_frame(&self) -> usize {
        self.source_start_frame
    }

    #[must_use]
    pub const fn source_frame_count(&self) -> usize {
        self.source_frame_count
    }

    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    #[must_use]
    pub const fn gain_decibels(&self) -> f64 {
        self.gain_decibels
    }

    #[must_use]
    pub const fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    #[must_use]
    pub const fn loop_start_tick(&self) -> u64 {
        self.loop_start_tick
    }

    #[must_use]
    pub const fn loop_end_tick(&self) -> u64 {
        self.loop_end_tick
    }

    #[must_use]
    pub const fn reverse(&self) -> bool {
        self.reverse
    }

    #[must_use]
    pub const fn fade_in_ticks(&self) -> u64 {
        self.fade_in_ticks
    }

    #[must_use]
    pub const fn fade_out_ticks(&self) -> u64 {
        self.fade_out_ticks
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdeMixerChannel {
    id: String,
    name: String,
    kind: IdeMixerChannelKind,
    active: bool,
    state: Option<IdeMixerChannelState>,
    sends: Vec<IdeMixerSend>,
}

/// One persisted control binding with the labels its identities resolve to in
/// the current project.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeControlBinding {
    id: String,
    enabled: bool,
    source: String,
    scope: String,
    target: String,
    transform: String,
}

impl IdeControlBinding {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }

    #[must_use]
    pub fn transform(&self) -> &str {
        &self.transform
    }
}

const fn control_edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Rising => "rising",
        Edge::Falling => "falling",
    }
}

fn control_source_label(source: &ControlSource) -> String {
    match source {
        ControlSource::CvInput { input_index } => format!("CV {}", input_index + 1),
        ControlSource::ClockInput { edge } => {
            format!("Clock {}", control_edge_name(*edge))
        }
        ControlSource::ResetInput { edge } => {
            format!("Reset {}", control_edge_name(*edge))
        }
    }
}

fn control_scope_label(scope: &ControlScope, snapshot: &RunningIdeSnapshot<'_>) -> String {
    match scope {
        ControlScope::Global => "Global".to_owned(),
        ControlScope::MixerPage => "Mixer page".to_owned(),
        ControlScope::Track { track_id } => {
            format!("Track {}", control_track_label(track_id, snapshot))
        }
        ControlScope::Scene { scene_id } => {
            format!("Scene {}", control_scene_label(scene_id, snapshot))
        }
    }
}

fn control_track_label(track_id: &str, snapshot: &RunningIdeSnapshot<'_>) -> String {
    snapshot
        .loaded
        .track_name(track_id)
        .unwrap_or(track_id)
        .to_owned()
}

fn control_scene_label(scene_id: &str, snapshot: &RunningIdeSnapshot<'_>) -> String {
    snapshot.scene_ids[..snapshot.scene_count]
        .iter()
        .position(|id| *id == scene_id)
        .map_or_else(
            || scene_id.to_owned(),
            |index| snapshot.scene_names[index].to_owned(),
        )
}

fn control_send_label(send_id: &str, snapshot: &RunningIdeSnapshot<'_>) -> String {
    for channel_index in 0..snapshot.mixer_channel_count {
        for send_index in 0..snapshot.mixer_send_counts[channel_index] {
            if snapshot.mixer_send_ids[channel_index][send_index] == send_id {
                return format!(
                    "{} to {}",
                    snapshot.mixer_channel_names[channel_index],
                    snapshot.mixer_send_target_names[channel_index][send_index]
                );
            }
        }
    }
    send_id.to_owned()
}

fn control_target_label(target: &ControlTarget, snapshot: &RunningIdeSnapshot<'_>) -> String {
    match target {
        ControlTarget::MixerChannel {
            track_id,
            parameter,
        } => format!(
            "{} {}",
            control_track_label(track_id, snapshot),
            format!("{parameter:?}").to_lowercase()
        ),
        ControlTarget::MixerSend { send_id, parameter } => format!(
            "{} send {}",
            control_send_label(send_id, snapshot),
            format!("{parameter:?}").to_lowercase()
        ),
        ControlTarget::MixerMain { parameter } => {
            format!("Main {}", format!("{parameter:?}").to_lowercase())
        }
        ControlTarget::MixerCue { parameter } => {
            format!("Cue {}", format!("{parameter:?}").to_lowercase())
        }
        ControlTarget::Scene { scene_id, action } => format!(
            "Scene {} {}",
            control_scene_label(scene_id, snapshot),
            format!("{action:?}").to_lowercase()
        ),
        ControlTarget::Transport { action } => {
            format!("Transport {}", format!("{action:?}").to_lowercase())
        }
    }
}

fn control_transform_label(transform: ControlTransform) -> String {
    match transform {
        ControlTransform::Linear {
            input_min,
            input_max,
            output_min,
            output_max,
        } => format!("Linear {input_min}..{input_max} to {output_min}..{output_max}"),
        ControlTransform::Trigger { behavior } => format!("{behavior:?}"),
    }
}

fn ide_control_binding(
    binding: &ControlBinding,
    snapshot: &RunningIdeSnapshot<'_>,
) -> Result<IdeControlBinding, IdeWorkerCommandFailure> {
    Ok(IdeControlBinding {
        id: try_project_owned(binding.id())?,
        enabled: binding.enabled(),
        source: control_source_label(binding.source()),
        scope: control_scope_label(binding.scope(), snapshot),
        target: control_target_label(binding.target(), snapshot),
        transform: control_transform_label(binding.transform()),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdeMixerSend {
    id: String,
    target_name: String,
    tap: super::IdeMixerSendTap,
    state: Option<super::IdeMixerSendState>,
}

impl IdeMixerChannel {
    #[cfg(any(test, feature = "ui-test-support"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str, kind: IdeMixerChannelKind) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
            active: true,
            state: None,
            sends: Vec::new(),
        }
    }

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
    pub const fn active(&self) -> bool {
        self.active
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
    pub fn id(&self) -> &str {
        &self.id
    }

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
    /// Returns the validated project-supplied scene identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the validated project-supplied scene name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the validated persisted scene activation state.
    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    /// Returns the session's prepared launch status for this scene.
    #[must_use]
    pub const fn availability(&self) -> IdeSceneAvailability {
        self.availability
    }

    #[cfg(any(test, feature = "ui-test-support"))]
    #[must_use]
    pub fn for_test(id: &str, name: &str, track_slots: Vec<IdeSceneTrackSlot>) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            active: true,
            availability: IdeSceneAvailability::Ready,
            track_slots,
        }
    }

    #[must_use]
    pub fn track_slots(&self) -> &[IdeSceneTrackSlot] {
        &self.track_slots
    }
}

impl IdeSceneTrackSlot {
    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub fn track_name(&self) -> &str {
        &self.track_name
    }

    #[cfg(any(test, feature = "ui-test-support"))]
    #[must_use]
    pub fn for_test(track_id: &str, track_name: &str, action: IdeSceneTrackAction) -> Self {
        Self {
            track_id: track_id.to_owned(),
            track_name: track_name.to_owned(),
            action,
        }
    }

    #[must_use]
    pub const fn action(&self) -> &IdeSceneTrackAction {
        &self.action
    }
}

impl IdeRunningObservation {
    /// Identifies the playback session that produced this observation.
    ///
    /// The value changes whenever project playback is closed and reopened,
    /// including durable edits that rebuild the runtime.
    #[must_use]
    pub const fn playback_session_generation(&self) -> u64 {
        self.playback_session_generation
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    /// The exact stored tempo at the project origin.
    #[must_use]
    pub const fn microseconds_per_quarter(&self) -> u32 {
        self.microseconds_per_quarter
    }

    /// The step meter map that groups the tempo's pulse into bars.
    #[must_use]
    pub fn meter_events(&self) -> &[hyper_playback_compiler::MeterEvent] {
        &self.meter_events
    }

    /// The step tempo map that places a frame on a tick.
    #[must_use]
    pub fn tempo_events(&self) -> &[hyper_playback_compiler::TempoEvent] {
        &self.tempo_events
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn playback_extent(&self) -> super::IdePlaybackExtent {
        self.playback_extent
    }

    #[must_use]
    pub fn arrangement(&self) -> &Arc<super::IdePlaybackArrangement> {
        &self.arrangement
    }

    #[must_use]
    pub fn automation(&self) -> &Arc<super::IdePlaybackAutomation> {
        &self.automation
    }

    #[must_use]
    pub fn control_bindings(&self) -> &[IdeControlBinding] {
        &self.control_bindings
    }

    #[must_use]
    pub fn audio_clips(&self) -> &[IdeAudioClip] {
        &self.audio_clips
    }

    #[must_use]
    pub fn recording_takes(&self) -> &[IdeRecordingTake] {
        &self.recording_takes
    }

    #[must_use]
    pub const fn unused_sample_cleanup(&self) -> IdeUnusedSampleCleanupSummary {
        self.unused_sample_cleanup
    }

    #[must_use]
    pub const fn preview(&self) -> &IdeAudioPreviewObservation {
        &self.preview
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
    pub const fn track_count(&self) -> usize {
        self.track_count
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

    /// Returns scene-launch state from the newest committed audio cycle, when
    /// this observation received one.
    #[must_use]
    pub fn scene_launch_state(&self) -> Option<super::IdeSceneLaunchState> {
        self.meter
            .snapshot()
            .map(|snapshot| snapshot.cycle().scene_launch_state())
    }

    /// Derives one display-ready loudness snapshot from the same newest meter
    /// observation used by the sample-peak UI.
    #[must_use]
    pub fn loudness(
        &self,
        config: super::IdeLoudnessPresentationConfig,
    ) -> Option<super::IdeLoudnessPresentationSnapshot> {
        self.meter
            .snapshot()
            .map(|snapshot| snapshot.present_loudness(config))
    }
}

#[derive(Debug)]
pub enum IdeProjectCreationError {
    Creation(ProjectCreationError),
    CreatedButNotOpened {
        project_path: PathBuf,
        source: IdeProjectOpenError,
    },
}

impl IdeProjectCreationError {
    #[must_use]
    pub fn created_project(&self) -> Option<&std::path::Path> {
        match self {
            Self::Creation(_) => None,
            Self::CreatedButNotOpened { project_path, .. } => Some(project_path),
        }
    }
}

impl fmt::Display for IdeProjectCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Creation(source) => write!(formatter, "project creation failed: {source}"),
            Self::CreatedButNotOpened {
                project_path,
                source,
            } => write!(
                formatter,
                "project was safely created at {} but could not be opened: {source}",
                project_path.display()
            ),
        }
    }
}

impl Error for IdeProjectCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Creation(source) => Some(source),
            Self::CreatedButNotOpened { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeRuntimePreparationError {
    Save(IdeProjectSaveError),
    Bundle(RuntimeBundleBuildError),
    ReopenAfterPublication {
        bundle: Box<PublishedRuntimeBundle>,
        source: Box<IdeProjectOpenError>,
    },
    ReopenAfterFailure {
        preparation: Box<IdeRuntimePreparationError>,
        source: Box<IdeProjectOpenError>,
    },
}

impl IdeRuntimePreparationError {
    /// Returns a verified Runtime artifact when preparation reached that point
    /// before a later cleanup, durability, or project-reopen failure.
    #[must_use]
    pub fn published(&self) -> Option<&PublishedRuntimeBundle> {
        match self {
            Self::Save(_) => None,
            Self::Bundle(source) => source.published(),
            Self::ReopenAfterPublication { bundle, .. } => Some(bundle),
            Self::ReopenAfterFailure { preparation, .. } => preparation.published(),
        }
    }
}

impl fmt::Display for IdeRuntimePreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Save(source) => {
                write!(
                    formatter,
                    "project could not be saved for Runtime: {source}"
                )
            }
            Self::Bundle(source) => write!(
                formatter,
                "project state was saved but Runtime bundle construction failed: {source}"
            ),
            Self::ReopenAfterPublication { bundle, source } => write!(
                formatter,
                "Runtime bundle was published at {} but the project could not be reopened: {source}",
                bundle.output_path().display()
            ),
            Self::ReopenAfterFailure {
                preparation,
                source,
            } => write!(
                formatter,
                "Runtime preparation failed ({preparation}) and the project could not be reopened ({source})"
            ),
        }
    }
}

impl Error for IdeRuntimePreparationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Save(source) => Some(source),
            Self::Bundle(source) => Some(source),
            Self::ReopenAfterPublication { source, .. }
            | Self::ReopenAfterFailure { source, .. } => Some(source.as_ref()),
        }
    }
}

#[derive(Debug)]
pub enum IdeWorkerCommandFailure {
    Discovery(IdeOutputDiscoveryError),
    InputDiscovery(IdeInputDiscoveryError),
    OutputSelection(IdeOutputSelectionError),
    InputSelection(IdeInputSelectionError),
    ProjectCreation(Box<IdeProjectCreationError>),
    ProjectOpen(IdeProjectOpenError),
    RecentProjectMissing(PathBuf),
    Transport(super::DesktopDirectControlError),
    Seek(IdeTransportSeekError),
    ArrangementLocate(IdeArrangementLocateError),
    Loudness(IdeLoudnessCommandError),
    OutputTest(IdeOutputTestError),
    Mixer(IdeMixerCommandError),
    Monitoring(IdeMonitoringCommandError),
    ProjectSave(IdeProjectSaveError),
    RuntimePreparation(Box<IdeRuntimePreparationError>),
    AudioTrackAddition(Box<super::IdeAudioTrackAdditionError>),
    AudioTrackRemoval(Box<super::IdeAudioTrackRemovalError>),
    AudioTrackRename(Box<super::IdeAudioTrackEditError>),
    AudioTrackActivation(Box<super::IdeAudioTrackEditError>),
    AudioImport(Box<super::IdeAudioImportBatchFailure>),
    AudioImportReopen {
        outcome: IdeAudioImportBatchOutcome,
        source: Box<IdeProjectOpenError>,
    },
    AudioImportAndReopen {
        import: Box<IdeAudioImportBatchFailure>,
        reopen: Box<IdeProjectOpenError>,
    },
    RecordingClipCreation(Box<IdeRecordingClipCreationError>),
    AudioClipDuplication(Box<IdeAudioClipDuplicationError>),
    AudioClipRemoval(Box<IdeAudioClipRemovalError>),
    UnusedSampleCleanup(Box<IdeUnusedSampleCleanupError>),
    ArrangementPlacementAddition(Box<IdeArrangementPlacementEditError>),
    ArrangementSceneLaunchAddition(Box<IdeArrangementPlacementEditError>),
    ArrangementSceneLaunchMove(Box<IdeArrangementPlacementEditError>),
    ArrangementLoopRange(Box<IdeArrangementPlacementEditError>),
    Tempo(Box<IdeTempoEditError>),
    ArrangementSceneLaunchSceneEdit(Box<IdeArrangementPlacementEditError>),
    ArrangementSceneLaunchRemoval(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementDuplication(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementSplit(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementActivation(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementMove(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementDuration(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementFrontTrim(Box<IdeArrangementPlacementEditError>),
    ArrangementPlacementRemoval(Box<IdeArrangementPlacementEditError>),
    AudioClipRename(Box<IdeAudioClipRenameEditError>),
    AudioClipActivation(Box<IdeAudioClipActivationEditError>),
    AudioClipGain(Box<IdeAudioClipGainEditError>),
    AudioClipFades(Box<IdeAudioClipFadeEditError>),
    AudioClipLoop(Box<IdeAudioClipLoopEditError>),
    AudioClipReverse(Box<IdeAudioClipReverseEditError>),
    AudioClipSourceSlip(Box<IdeAudioClipSourceSlipEditError>),
    SceneCreation(Box<IdeSceneCreationError>),
    AutomationTakeCreation(Box<IdeAutomationTakeCreationError>),
    AutomationTakeActivation(Box<IdeAutomationTakeEditError>),
    ControlMappingEdit(Box<IdeControlMappingEditError>),
    ProjectExport(Box<IdeProjectExportError>),
    EditRestore(Box<IdeResourceRestoreError>),
    NothingToRestore,
    SceneDuplication(Box<IdeSceneDuplicationError>),
    SceneSlotEdit(Box<IdeSceneSlotEditError>),
    SceneRename(Box<IdeSceneRenameError>),
    SceneActivation(Box<IdeSceneEditError>),
    Recording(IdeRecordingError),
    AudioPreview(IdeAudioPreviewError),
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
            Self::Tempo(source) => source.fmt(formatter),
            Self::InputDiscovery(source) => source.fmt(formatter),
            Self::OutputSelection(source) => source.fmt(formatter),
            Self::InputSelection(source) => source.fmt(formatter),
            Self::ProjectCreation(source) => source.fmt(formatter),
            Self::ProjectOpen(source) => source.fmt(formatter),
            Self::RecentProjectMissing(path) => {
                write!(
                    formatter,
                    "recent project no longer exists at {}",
                    path.display()
                )
            }
            Self::Transport(source) => source.fmt(formatter),
            Self::Seek(source) => source.fmt(formatter),
            Self::ArrangementLocate(source) => source.fmt(formatter),
            Self::Loudness(source) => source.fmt(formatter),
            Self::OutputTest(source) => source.fmt(formatter),
            Self::Mixer(source) => source.fmt(formatter),
            Self::Monitoring(source) => source.fmt(formatter),
            Self::ProjectSave(source) => source.fmt(formatter),
            Self::RuntimePreparation(source) => source.fmt(formatter),
            Self::AudioTrackAddition(source) => source.fmt(formatter),
            Self::AudioTrackRemoval(source) => source.fmt(formatter),
            Self::AudioTrackRename(source) | Self::AudioTrackActivation(source) => {
                source.fmt(formatter)
            }
            Self::AudioImport(source) => source.fmt(formatter),
            Self::AudioImportReopen { outcome, source } => write!(
                formatter,
                "{} audio files were imported through project revision {}, but the project could not be reopened: {source}",
                outcome.imported(),
                outcome.final_revision()
            ),
            Self::AudioImportAndReopen { import, reopen } => write!(
                formatter,
                "audio import failed ({import}) and the project could not be reopened ({reopen})"
            ),
            Self::RecordingClipCreation(source) => source.fmt(formatter),
            Self::AudioClipDuplication(source) => source.fmt(formatter),
            Self::AudioClipRemoval(source) => source.fmt(formatter),
            Self::UnusedSampleCleanup(source) => source.fmt(formatter),
            Self::ArrangementPlacementAddition(source)
            | Self::ArrangementSceneLaunchAddition(source)
            | Self::ArrangementSceneLaunchMove(source)
            | Self::ArrangementLoopRange(source)
            | Self::ArrangementSceneLaunchSceneEdit(source)
            | Self::ArrangementSceneLaunchRemoval(source)
            | Self::ArrangementPlacementDuplication(source)
            | Self::ArrangementPlacementSplit(source)
            | Self::ArrangementPlacementActivation(source)
            | Self::ArrangementPlacementMove(source)
            | Self::ArrangementPlacementDuration(source)
            | Self::ArrangementPlacementFrontTrim(source)
            | Self::ArrangementPlacementRemoval(source) => source.fmt(formatter),
            Self::AudioClipRename(source) => source.fmt(formatter),
            Self::AudioClipActivation(source) => source.fmt(formatter),
            Self::AudioClipGain(source) => source.fmt(formatter),
            Self::AudioClipFades(source) => source.fmt(formatter),
            Self::AudioClipLoop(source) => source.fmt(formatter),
            Self::AudioClipReverse(source) => source.fmt(formatter),
            Self::AudioClipSourceSlip(source) => source.fmt(formatter),
            Self::SceneCreation(source) => source.fmt(formatter),
            Self::AutomationTakeCreation(source) => source.fmt(formatter),
            Self::AutomationTakeActivation(source) => source.fmt(formatter),
            Self::ControlMappingEdit(source) => source.fmt(formatter),
            Self::ProjectExport(source) => source.fmt(formatter),
            Self::EditRestore(source) => source.fmt(formatter),
            Self::NothingToRestore => formatter.write_str("there is no edit to restore"),
            Self::SceneDuplication(source) => source.fmt(formatter),
            Self::SceneSlotEdit(source) => source.fmt(formatter),
            Self::SceneRename(source) | Self::SceneActivation(source) => source.fmt(formatter),
            Self::Recording(source) => source.fmt(formatter),
            Self::AudioPreview(source) => source.fmt(formatter),
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
            Self::Tempo(source) => Some(source),
            Self::InputDiscovery(source) => Some(source),
            Self::OutputSelection(source) => Some(source),
            Self::InputSelection(source) => Some(source),
            Self::ProjectCreation(source) => Some(source),
            Self::ProjectOpen(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Seek(source) => Some(source),
            Self::ArrangementLocate(source) => Some(source),
            Self::Loudness(source) => Some(source),
            Self::OutputTest(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::Monitoring(source) => Some(source),
            Self::ProjectSave(source) => Some(source),
            Self::RuntimePreparation(source) => Some(source.as_ref()),
            Self::AudioTrackAddition(source) => Some(source),
            Self::AudioTrackRemoval(source) => Some(source),
            Self::AudioTrackRename(source) | Self::AudioTrackActivation(source) => Some(source),
            Self::AudioImport(source) => Some(source),
            Self::AudioImportReopen { source, .. } => Some(source.as_ref()),
            Self::AudioImportAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::RecordingClipCreation(source) => Some(source),
            Self::AudioClipDuplication(source) => Some(source),
            Self::AudioClipRemoval(source) => Some(source),
            Self::UnusedSampleCleanup(source) => Some(source),
            Self::ArrangementPlacementAddition(source)
            | Self::ArrangementSceneLaunchAddition(source)
            | Self::ArrangementSceneLaunchMove(source)
            | Self::ArrangementLoopRange(source)
            | Self::ArrangementSceneLaunchSceneEdit(source)
            | Self::ArrangementSceneLaunchRemoval(source)
            | Self::ArrangementPlacementDuplication(source)
            | Self::ArrangementPlacementSplit(source)
            | Self::ArrangementPlacementActivation(source)
            | Self::ArrangementPlacementMove(source)
            | Self::ArrangementPlacementDuration(source)
            | Self::ArrangementPlacementFrontTrim(source)
            | Self::ArrangementPlacementRemoval(source) => Some(source),
            Self::AudioClipRename(source) => Some(source),
            Self::AudioClipActivation(source) => Some(source),
            Self::AudioClipGain(source) => Some(source),
            Self::AudioClipFades(source) => Some(source),
            Self::AudioClipLoop(source) => Some(source),
            Self::AudioClipReverse(source) => Some(source),
            Self::AudioClipSourceSlip(source) => Some(source),
            Self::SceneCreation(source) => Some(source),
            Self::AutomationTakeCreation(source) => Some(source),
            Self::AutomationTakeActivation(source) => Some(source),
            Self::ControlMappingEdit(source) => Some(source),
            Self::ProjectExport(source) => Some(source),
            Self::EditRestore(source) => Some(source),
            Self::SceneDuplication(source) => Some(source),
            Self::SceneSlotEdit(source) => Some(source),
            Self::SceneRename(source) | Self::SceneActivation(source) => Some(source),
            Self::Recording(source) => Some(source),
            Self::AudioPreview(source) => Some(source),
            Self::Publication(source) => Some(source),
            Self::InputPublication(source) => Some(source),
            Self::InvalidState
            | Self::RecentProjectMissing(_)
            | Self::ProjectPublicationCapacityUnavailable
            | Self::ProjectSnapshotUnavailable
            | Self::NothingToRestore => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipEditCommittedWarning {
    Rename(Box<IdeAudioClipRenameEditError>),
    Activation(Box<IdeAudioClipActivationEditError>),
    Gain(Box<IdeAudioClipGainEditError>),
    Fades(Box<IdeAudioClipFadeEditError>),
    Loop(Box<IdeAudioClipLoopEditError>),
    Reverse(Box<IdeAudioClipReverseEditError>),
    SourceSlip(Box<IdeAudioClipSourceSlipEditError>),
}

impl fmt::Display for IdeAudioClipEditCommittedWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rename(source) => source.fmt(formatter),
            Self::Activation(source) => source.fmt(formatter),
            Self::Gain(source) => source.fmt(formatter),
            Self::Fades(source) => source.fmt(formatter),
            Self::Loop(source) => source.fmt(formatter),
            Self::Reverse(source) => source.fmt(formatter),
            Self::SourceSlip(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeAudioClipEditCommittedWarning {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rename(source) => Some(source),
            Self::Activation(source) => Some(source),
            Self::Gain(source) => Some(source),
            Self::Fades(source) => Some(source),
            Self::Loop(source) => Some(source),
            Self::Reverse(source) => Some(source),
            Self::SourceSlip(source) => Some(source),
        }
    }
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "events transfer fixed-size owned observations without an additional infallible allocation"
)]
pub enum IdeWorkerEvent {
    RecentProjectsChanged {
        projects: Vec<RecentProject>,
    },
    ProjectHistoryWarning(ProjectHistoryError),
    Closed {
        snapshot: IdeClosedSnapshot,
        native_failure: Option<IdeNativeCloseFailure>,
        input_failure: Option<IdeInputDiscoveryError>,
    },
    Running(IdeRunningObservation),
    OutputTest(IdeOutputTestSnapshot),
    OutputTestStopping,
    OutputTestStopped,
    TransportQueued(TransportCommand),
    LoudnessQueued(IdeLoudnessCommand),
    MixerQueued(IdeMixerCommand),
    MonitoringApplied(IdeMonitoringCommand),
    ProjectSaved {
        revision: u64,
        changed: bool,
    },
    RuntimePreparing,
    RuntimePrepared {
        bundle: PublishedRuntimeBundle,
    },
    AudioTrackAdded {
        revision: u64,
    },
    AudioTrackRemoved {
        revision: u64,
    },
    AudioTrackRenamed {
        revision: u64,
        changed: bool,
    },
    AudioTrackActivationChanged {
        revision: u64,
        changed: bool,
        active: bool,
    },
    AudioImported(IdeAudioImportBatchOutcome),
    AudioClipCreatedFromTake {
        revision: u64,
        clip_id: String,
    },
    AudioClipDuplicated {
        revision: u64,
        clip_id: String,
        name: String,
    },
    AudioClipRemoved {
        revision: u64,
        clip_id: String,
    },
    AudioClipRemovalCommittedWithWarning {
        warning: Box<IdeAudioClipRemovalError>,
    },
    UnusedSampleCleanupStarting {
        assets: usize,
        bytes: u64,
    },
    UnusedSamplesCleaned {
        revision: u64,
        assets: usize,
        bytes: u64,
    },
    UnusedSampleCleanupCommittedWithWarning {
        warning: Box<IdeUnusedSampleCleanupError>,
    },
    AudioClipDuplicationCommittedWithWarning {
        name: String,
        warning: Box<IdeAudioClipDuplicationError>,
    },
    AudioClipEditCommittedWithWarning {
        command: IdeWorkerCommandKind,
        warning: IdeAudioClipEditCommittedWarning,
    },
    ArrangementPlacementAdded {
        revision: u64,
        start_tick: u64,
    },
    ArrangementSceneLaunchAdded {
        revision: u64,
        scene_id: String,
        tick: u64,
    },
    ArrangementSceneLaunchMoved {
        revision: u64,
        launch_id: String,
        tick: u64,
    },
    ArrangementSceneLaunchSceneChanged {
        revision: u64,
        launch_id: String,
        scene_id: String,
    },
    ArrangementSceneLaunchRemoved {
        revision: u64,
        launch_id: String,
    },
    ArrangementPlacementDuplicated {
        revision: u64,
        placement_id: String,
        start_tick: u64,
    },
    ArrangementPlacementSplit {
        revision: u64,
        placement_id: String,
        split_tick: u64,
    },
    ArrangementPlacementEditCommittedWithWarning {
        command: IdeWorkerCommandKind,
        warning: Box<IdeArrangementPlacementEditError>,
    },
    ArrangementPlacementActivationChanged {
        revision: u64,
        changed: bool,
        active: bool,
    },
    ArrangementPlacementMoved {
        revision: u64,
        changed: bool,
        start_tick: u64,
    },
    ArrangementPlacementDurationChanged {
        revision: u64,
        changed: bool,
        duration_ticks: u64,
    },
    ArrangementPlacementFrontTrimmed {
        revision: u64,
        changed: bool,
        start_tick: u64,
    },
    ArrangementPlacementRemoved {
        revision: u64,
    },
    AudioClipRenamed {
        revision: u64,
        changed: bool,
        name: String,
    },
    AudioClipActivationChanged {
        revision: u64,
        changed: bool,
        active: bool,
    },
    AudioClipGainChanged {
        revision: u64,
        changed: bool,
        gain_decibels: f64,
    },
    AudioClipFadesChanged {
        revision: u64,
        changed: bool,
        fade_in_ticks: u64,
        fade_out_ticks: u64,
    },
    AudioClipLoopChanged {
        revision: u64,
        changed: bool,
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
    },
    AudioClipReverseChanged {
        revision: u64,
        changed: bool,
        reverse: bool,
    },
    AudioClipSourceStartChanged {
        revision: u64,
        changed: bool,
        source_start_frame: u64,
    },
    SceneAdded {
        revision: u64,
        scene_id: String,
        name: String,
    },
    SceneCreationCommittedWithWarning {
        name: String,
        warning: Box<IdeSceneCreationError>,
    },
    AutomationTakeAdded {
        revision: u64,
        take_id: String,
        name: String,
    },
    AutomationTakeCreationCommittedWithWarning {
        name: String,
        warning: Box<IdeAutomationTakeCreationError>,
    },
    AutomationTakeActivationChanged {
        revision: u64,
        changed: bool,
        enabled: bool,
    },
    AutomationTakeActivationCommittedWithWarning {
        warning: Box<IdeAutomationTakeEditError>,
    },
    ControlMappingEdited {
        revision: u64,
        edit: IdeControlMappingEdit,
    },
    ControlMappingEditCommittedWithWarning {
        warning: Box<IdeControlMappingEditError>,
    },
    ProjectExporting,
    ProjectExported {
        report: hyper_desktop_audio::OfflineExportReport,
    },
    UndoStateChanged {
        undo_label: Option<&'static str>,
        redo_label: Option<&'static str>,
    },
    EditRestored {
        revision: u64,
        redo: bool,
        label: &'static str,
    },
    EditRestoring {
        redo: bool,
        label: &'static str,
    },
    EditRestoreCommittedWithWarning {
        redo: bool,
        label: &'static str,
        warning: Box<IdeResourceRestoreError>,
    },
    SceneDuplicated {
        revision: u64,
        scene_id: String,
        name: String,
    },
    SceneDuplicationCommittedWithWarning {
        name: String,
        warning: Box<IdeSceneDuplicationError>,
    },
    SceneSlotChanged {
        revision: u64,
        changed: bool,
        edit: IdeSceneSlotEdit,
    },
    SceneSlotEditCommittedWithWarning {
        edit: IdeSceneSlotEdit,
        warning: Box<IdeSceneSlotEditError>,
    },
    SceneRenamed {
        revision: u64,
        changed: bool,
        name: String,
    },
    SceneRenameCommittedWithWarning {
        name: String,
        warning: Box<IdeSceneRenameError>,
    },
    SceneActivationChanged {
        revision: u64,
        changed: bool,
        active: bool,
    },
    SceneActivationCommittedWithWarning {
        active: bool,
        warning: Box<IdeSceneEditError>,
    },
    RecordingPreparing,
    Recording(IdeRecordingState),
    RecordingFinalizing,
    RecordingImported(IdeRecordingImportOutcome),
    RecordingAborted(IdeRecordingAbortOutcome),
    AudioPreviewPreparing,
    AudioPreviewStopRequested,
    AudioPreviewControlApplied(IdeAudioPreviewControlCommand),
    Saving,
    AudioTrackAdding,
    AudioTrackRemoving,
    AudioTrackRenaming,
    AudioTrackActivationChanging {
        active: bool,
    },
    AudioImporting {
        files: usize,
    },
    AudioClipCreatingFromTake,
    AudioClipDuplicating,
    AudioClipRemoving,
    ArrangementPlacementAdding {
        start_tick: u64,
    },
    ArrangementSceneLaunchAdding {
        tick: u64,
    },
    ArrangementSceneLaunchMoving {
        tick: u64,
    },
    ArrangementSceneLaunchSceneChanging,
    ArrangementSceneLaunchRemoving,
    ArrangementLoopRangeChanging,
    TempoChanging,
    MeterChanging,
    MeterChanged {
        revision: u64,
        numerator: u16,
        denominator: u16,
    },
    MeterCommittedWithWarning {
        warning: Box<IdeTempoEditError>,
    },
    TempoChanged {
        revision: u64,
        microseconds_per_quarter: u32,
    },
    TempoCommittedWithWarning {
        warning: Box<IdeTempoEditError>,
    },
    ArrangementLoopRangeChanged {
        revision: u64,
    },
    ArrangementLoopRangeCommittedWithWarning {
        warning: Box<IdeArrangementPlacementEditError>,
    },
    ArrangementPlacementDuplicating {
        start_tick: u64,
    },
    ArrangementPlacementSplitting {
        split_tick: u64,
    },
    ArrangementPlacementActivationChanging {
        active: bool,
    },
    ArrangementPlacementMoving {
        start_tick: u64,
    },
    ArrangementPlacementDurationChanging {
        duration_ticks: u64,
    },
    ArrangementPlacementFrontTrimming {
        start_tick: u64,
    },
    ArrangementPlacementRemoving,
    AudioClipRenaming,
    AudioClipActivationChanging {
        active: bool,
    },
    AudioClipGainChanging {
        gain_decibels: f64,
    },
    AudioClipFadesChanging {
        fade_in_ticks: u64,
        fade_out_ticks: u64,
    },
    AudioClipLoopChanging {
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
    },
    AudioClipReverseChanging {
        reverse: bool,
    },
    AudioClipSourceStartChanging {
        source_start_frame: u64,
    },
    SceneAdding,
    AutomationTakeAdding,
    AutomationTakeActivationChanging {
        enabled: bool,
    },
    ControlMappingEditing {
        edit: IdeControlMappingEdit,
    },
    SceneDuplicating,
    SceneSlotChanging {
        edit: IdeSceneSlotEdit,
    },
    SceneRenaming,
    SceneActivationChanging {
        active: bool,
    },
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
        Self::start_state_with_history(
            WorkerState::Undiscovered(application),
            None,
            command_capacity,
            event_capacity,
        )
    }

    /// Starts the sole blocking application owner with a persistent recent-project authority.
    ///
    /// # Errors
    ///
    /// Returns validation, supervisor-capacity, or thread-spawn failure.
    pub fn start_with_project_history(
        application: IdeApplicationConfig,
        project_history: ProjectHistory,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, IdeWorkerStartError> {
        Self::start_state_with_history(
            WorkerState::Undiscovered(application),
            Some(project_history),
            command_capacity,
            event_capacity,
        )
    }

    #[cfg(test)]
    fn start_state(
        state: WorkerState,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, IdeWorkerStartError> {
        Self::start_state_with_history(state, None, command_capacity, event_capacity)
    }

    fn start_state_with_history(
        state: WorkerState,
        project_history: Option<ProjectHistory>,
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
                run_worker(
                    state,
                    project_history.as_ref(),
                    &command_receiver,
                    &event_sender,
                )
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
    OutputTesting(OutputTestingDesktopIdeApplication),
    OutputTestStopping {
        pending: IdePendingOutputTestShutdown,
        command: IdeWorkerCommandKind,
    },
    Running(RunningDesktopIdeApplication),
    SaveFence {
        application: RunningDesktopIdeApplication,
        operation: ProjectMutation,
        sequence: u64,
        deadline: Instant,
    },
    RecordingFence {
        application: RunningDesktopIdeApplication,
        target: IdeRecordingTarget,
        sequence: u64,
        deadline: Instant,
    },
    AudioPreviewFence {
        application: RunningDesktopIdeApplication,
        asset_id: String,
        sequence: u64,
        deadline: Instant,
    },
    RecordingClosing {
        pending: DesktopIdePendingSaveClose,
        target: IdeRecordingTarget,
    },
    Recording(RecordingDesktopIdeApplication),
    Saving {
        pending: DesktopIdePendingSaveClose,
        operation: ProjectMutation,
    },
    Closing(DesktopIdePendingClose),
    Empty,
}

#[derive(Debug)]
enum ProjectMutation {
    SaveMixer,
    PrepareRuntime {
        output_path: PathBuf,
    },
    AddAudioTrack {
        name: String,
    },
    RemoveAudioTrack {
        track_id: String,
    },
    RenameAudioTrack {
        track_id: String,
        name: String,
    },
    SetAudioTrackActive {
        track_id: String,
        active: bool,
    },
    ImportAudio {
        paths: Vec<PathBuf>,
        track_id: String,
    },
    CreateAudioClipFromTake {
        recording_id: String,
        name: String,
    },
    DuplicateAudioClip {
        clip_id: String,
        name: String,
    },
    RemoveAudioClip {
        clip_id: String,
    },
    CleanUnusedSamples {
        confirmation: IdeUnusedSampleCleanupSummary,
    },
    AddArrangementPlacement {
        clip_id: String,
        start_tick: u64,
    },
    AddArrangementSceneLaunch {
        scene_id: String,
        tick: u64,
    },
    MoveArrangementSceneLaunch {
        launch_id: String,
        tick: u64,
    },
    SetMeter {
        numerator: u16,
        denominator: u16,
    },
    SetTempo {
        microseconds_per_quarter: u32,
    },
    SetArrangementLoopRange {
        range: Option<IdeLoopRangeFrames>,
    },
    SetArrangementSceneLaunchScene {
        launch_id: String,
        scene_id: String,
    },
    RemoveArrangementSceneLaunch {
        launch_id: String,
    },
    DuplicateArrangementPlacement {
        placement_id: String,
        start_tick: u64,
    },
    SplitArrangementPlacement {
        placement_id: String,
        split_tick: u64,
    },
    SetArrangementPlacementActive {
        placement_id: String,
        active: bool,
    },
    MoveArrangementPlacement {
        placement_id: String,
        start_tick: u64,
    },
    SetArrangementPlacementDuration {
        placement_id: String,
        duration_ticks: u64,
    },
    TrimArrangementPlacementFront {
        placement_id: String,
        start_tick: u64,
    },
    RemoveArrangementPlacement {
        placement_id: String,
    },
    RenameAudioClip {
        clip_id: String,
        name: String,
    },
    SetAudioClipActive {
        clip_id: String,
        active: bool,
    },
    SetAudioClipGain {
        clip_id: String,
        gain_decibels: f64,
    },
    SetAudioClipFades {
        clip_id: String,
        fade_in_ticks: u64,
        fade_out_ticks: u64,
    },
    SetAudioClipLoop {
        clip_id: String,
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
    },
    SetAudioClipReverse {
        clip_id: String,
        reverse: bool,
    },
    SetAudioClipSourceStart {
        clip_id: String,
        source_start_frame: u64,
    },
    AddScene {
        name: String,
        quantization: IdeSceneLaunchQuantization,
    },
    AddAutomationTake {
        request: IdeAutomationTakeRequest,
    },
    SetAutomationTakeActive {
        take_id: String,
        enabled: bool,
    },
    EditControlMapping {
        edit: IdeControlMappingEdit,
    },
    RestoreResource {
        entry: UndoEntry,
        redo: bool,
    },
    DuplicateScene {
        scene_id: String,
        name: String,
    },
    SetSceneSlot {
        scene_id: String,
        track_id: String,
        edit: IdeSceneSlotEdit,
    },
    RenameScene {
        scene_id: String,
        name: String,
    },
    SetSceneActive {
        scene_id: String,
        active: bool,
    },
}

impl ProjectMutation {
    const fn command(&self) -> IdeWorkerCommandKind {
        match self {
            Self::SetArrangementLoopRange { .. } => IdeWorkerCommandKind::SetArrangementLoopRange,
            Self::SetTempo { .. } => IdeWorkerCommandKind::SetTempo,
            Self::SetMeter { .. } => IdeWorkerCommandKind::SetMeter,
            Self::SaveMixer => IdeWorkerCommandKind::SaveProject,
            Self::PrepareRuntime { .. } => IdeWorkerCommandKind::PrepareRuntime,
            Self::AddAudioTrack { .. } => IdeWorkerCommandKind::AddAudioTrack,
            Self::RemoveAudioTrack { .. } => IdeWorkerCommandKind::RemoveAudioTrack,
            Self::RenameAudioTrack { .. } => IdeWorkerCommandKind::RenameAudioTrack,
            Self::SetAudioTrackActive { .. } => IdeWorkerCommandKind::SetAudioTrackActive,
            Self::ImportAudio { .. } => IdeWorkerCommandKind::ImportAudio,
            Self::CreateAudioClipFromTake { .. } => IdeWorkerCommandKind::CreateAudioClipFromTake,
            Self::DuplicateAudioClip { .. } => IdeWorkerCommandKind::DuplicateAudioClip,
            Self::RemoveAudioClip { .. } => IdeWorkerCommandKind::RemoveAudioClip,
            Self::CleanUnusedSamples { .. } => IdeWorkerCommandKind::CleanUnusedSamples,
            Self::AddArrangementPlacement { .. } => IdeWorkerCommandKind::AddArrangementPlacement,
            Self::AddArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::AddArrangementSceneLaunch
            }
            Self::MoveArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::MoveArrangementSceneLaunch
            }
            Self::SetArrangementSceneLaunchScene { .. } => {
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene
            }
            Self::RemoveArrangementSceneLaunch { .. } => {
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch
            }
            Self::DuplicateArrangementPlacement { .. } => {
                IdeWorkerCommandKind::DuplicateArrangementPlacement
            }
            Self::SplitArrangementPlacement { .. } => {
                IdeWorkerCommandKind::SplitArrangementPlacement
            }
            Self::SetArrangementPlacementActive { .. } => {
                IdeWorkerCommandKind::SetArrangementPlacementActive
            }
            Self::MoveArrangementPlacement { .. } => IdeWorkerCommandKind::MoveArrangementPlacement,
            Self::SetArrangementPlacementDuration { .. } => {
                IdeWorkerCommandKind::SetArrangementPlacementDuration
            }
            Self::TrimArrangementPlacementFront { .. } => {
                IdeWorkerCommandKind::TrimArrangementPlacementFront
            }
            Self::RemoveArrangementPlacement { .. } => {
                IdeWorkerCommandKind::RemoveArrangementPlacement
            }
            Self::RenameAudioClip { .. } => IdeWorkerCommandKind::RenameAudioClip,
            Self::SetAudioClipActive { .. } => IdeWorkerCommandKind::SetAudioClipActive,
            Self::SetAudioClipGain { .. } => IdeWorkerCommandKind::SetAudioClipGain,
            Self::SetAudioClipFades { .. } => IdeWorkerCommandKind::SetAudioClipFades,
            Self::SetAudioClipLoop { .. } => IdeWorkerCommandKind::SetAudioClipLoop,
            Self::SetAudioClipReverse { .. } => IdeWorkerCommandKind::SetAudioClipReverse,
            Self::SetAudioClipSourceStart { .. } => IdeWorkerCommandKind::SetAudioClipSourceStart,
            Self::AddScene { .. } => IdeWorkerCommandKind::AddScene,
            Self::AddAutomationTake { .. } => IdeWorkerCommandKind::AddAutomationTake,
            Self::SetAutomationTakeActive { .. } => IdeWorkerCommandKind::SetAutomationTakeActive,
            Self::EditControlMapping { .. } => IdeWorkerCommandKind::EditControlMapping,
            Self::RestoreResource { redo: false, .. } => IdeWorkerCommandKind::Undo,
            Self::RestoreResource { redo: true, .. } => IdeWorkerCommandKind::Redo,
            Self::DuplicateScene { .. } => IdeWorkerCommandKind::DuplicateScene,
            Self::SetSceneSlot { .. } => IdeWorkerCommandKind::SetSceneSlot,
            Self::RenameScene { .. } => IdeWorkerCommandKind::RenameScene,
            Self::SetSceneActive { .. } => IdeWorkerCommandKind::SetSceneActive,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive mapping keeps every pending project mutation observable"
    )]
    fn pending_event(&self) -> IdeWorkerEvent {
        match self {
            Self::SaveMixer => IdeWorkerEvent::Saving,
            Self::PrepareRuntime { .. } => IdeWorkerEvent::RuntimePreparing,
            Self::AddAudioTrack { .. } => IdeWorkerEvent::AudioTrackAdding,
            Self::RemoveAudioTrack { .. } => IdeWorkerEvent::AudioTrackRemoving,
            Self::RenameAudioTrack { .. } => IdeWorkerEvent::AudioTrackRenaming,
            Self::SetAudioTrackActive { active, .. } => {
                IdeWorkerEvent::AudioTrackActivationChanging { active: *active }
            }
            Self::ImportAudio { paths, .. } => {
                IdeWorkerEvent::AudioImporting { files: paths.len() }
            }
            Self::CreateAudioClipFromTake { .. } => IdeWorkerEvent::AudioClipCreatingFromTake,
            Self::DuplicateAudioClip { .. } => IdeWorkerEvent::AudioClipDuplicating,
            Self::RemoveAudioClip { .. } => IdeWorkerEvent::AudioClipRemoving,
            Self::CleanUnusedSamples { confirmation } => {
                IdeWorkerEvent::UnusedSampleCleanupStarting {
                    assets: confirmation.assets(),
                    bytes: confirmation.bytes(),
                }
            }
            Self::AddArrangementPlacement { start_tick, .. } => {
                IdeWorkerEvent::ArrangementPlacementAdding {
                    start_tick: *start_tick,
                }
            }
            Self::AddArrangementSceneLaunch { tick, .. } => {
                IdeWorkerEvent::ArrangementSceneLaunchAdding { tick: *tick }
            }
            Self::MoveArrangementSceneLaunch { tick, .. } => {
                IdeWorkerEvent::ArrangementSceneLaunchMoving { tick: *tick }
            }
            Self::SetArrangementSceneLaunchScene { .. } => {
                IdeWorkerEvent::ArrangementSceneLaunchSceneChanging
            }
            Self::SetArrangementLoopRange { .. } => IdeWorkerEvent::ArrangementLoopRangeChanging,
            Self::SetTempo { .. } => IdeWorkerEvent::TempoChanging,
            Self::SetMeter { .. } => IdeWorkerEvent::MeterChanging,
            Self::RemoveArrangementSceneLaunch { .. } => {
                IdeWorkerEvent::ArrangementSceneLaunchRemoving
            }
            Self::DuplicateArrangementPlacement { start_tick, .. } => {
                IdeWorkerEvent::ArrangementPlacementDuplicating {
                    start_tick: *start_tick,
                }
            }
            Self::SplitArrangementPlacement { split_tick, .. } => {
                IdeWorkerEvent::ArrangementPlacementSplitting {
                    split_tick: *split_tick,
                }
            }
            Self::SetArrangementPlacementActive { active, .. } => {
                IdeWorkerEvent::ArrangementPlacementActivationChanging { active: *active }
            }
            Self::MoveArrangementPlacement { start_tick, .. } => {
                IdeWorkerEvent::ArrangementPlacementMoving {
                    start_tick: *start_tick,
                }
            }
            Self::SetArrangementPlacementDuration { duration_ticks, .. } => {
                IdeWorkerEvent::ArrangementPlacementDurationChanging {
                    duration_ticks: *duration_ticks,
                }
            }
            Self::TrimArrangementPlacementFront { start_tick, .. } => {
                IdeWorkerEvent::ArrangementPlacementFrontTrimming {
                    start_tick: *start_tick,
                }
            }
            Self::RemoveArrangementPlacement { .. } => IdeWorkerEvent::ArrangementPlacementRemoving,
            Self::RenameAudioClip { .. } => IdeWorkerEvent::AudioClipRenaming,
            Self::SetAudioClipActive { active, .. } => {
                IdeWorkerEvent::AudioClipActivationChanging { active: *active }
            }
            Self::SetAudioClipGain { gain_decibels, .. } => IdeWorkerEvent::AudioClipGainChanging {
                gain_decibels: *gain_decibels,
            },
            Self::SetAudioClipFades {
                fade_in_ticks,
                fade_out_ticks,
                ..
            } => IdeWorkerEvent::AudioClipFadesChanging {
                fade_in_ticks: *fade_in_ticks,
                fade_out_ticks: *fade_out_ticks,
            },
            Self::SetAudioClipLoop {
                enabled,
                start_tick,
                end_tick,
                ..
            } => IdeWorkerEvent::AudioClipLoopChanging {
                enabled: *enabled,
                start_tick: *start_tick,
                end_tick: *end_tick,
            },
            Self::SetAudioClipReverse { reverse, .. } => {
                IdeWorkerEvent::AudioClipReverseChanging { reverse: *reverse }
            }
            Self::SetAudioClipSourceStart {
                source_start_frame, ..
            } => IdeWorkerEvent::AudioClipSourceStartChanging {
                source_start_frame: *source_start_frame,
            },
            Self::AddScene { .. } => IdeWorkerEvent::SceneAdding,
            Self::AddAutomationTake { .. } => IdeWorkerEvent::AutomationTakeAdding,
            Self::SetAutomationTakeActive { enabled, .. } => {
                IdeWorkerEvent::AutomationTakeActivationChanging { enabled: *enabled }
            }
            Self::EditControlMapping { edit } => {
                IdeWorkerEvent::ControlMappingEditing { edit: edit.clone() }
            }
            Self::RestoreResource { entry, redo } => IdeWorkerEvent::EditRestoring {
                redo: *redo,
                label: entry.label,
            },
            Self::DuplicateScene { .. } => IdeWorkerEvent::SceneDuplicating,
            Self::SetSceneSlot { edit, .. } => {
                IdeWorkerEvent::SceneSlotChanging { edit: edit.clone() }
            }
            Self::RenameScene { .. } => IdeWorkerEvent::SceneRenaming,
            Self::SetSceneActive { active, .. } => {
                IdeWorkerEvent::SceneActivationChanging { active: *active }
            }
        }
    }
}

const MAX_UNDO_DEPTH: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
struct UndoEntry {
    resource_id: String,
    bytes: Vec<u8>,
    label: &'static str,
}

/// Ardour's two bounded lists, holding complete previous documents rather than
/// inverse operations. History is per-session and dies with the project.
#[derive(Debug, Default)]
struct UndoHistory {
    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
}

impl UndoHistory {
    fn record_edit(&mut self, entry: UndoEntry) {
        self.redo.clear();
        push_bounded(&mut self.undo, entry);
    }

    fn record_restore(&mut self, redo: bool, entry: UndoEntry) {
        if redo {
            push_bounded(&mut self.undo, entry);
        } else {
            push_bounded(&mut self.redo, entry);
        }
    }

    fn peek(&self, redo: bool) -> Option<&UndoEntry> {
        if redo {
            self.redo.last()
        } else {
            self.undo.last()
        }
    }

    /// Consumes the entry a committed restore just applied and records the
    /// state it displaced on the opposite stack.
    fn complete_restore(&mut self, redo: bool, displaced: Option<UndoEntry>) {
        if redo {
            self.redo.pop();
        } else {
            self.undo.pop();
        }
        if let Some(entry) = displaced {
            self.record_restore(redo, entry);
        }
    }

    fn clear(&mut self) -> bool {
        let had_entries = !self.undo.is_empty() || !self.redo.is_empty();
        self.undo.clear();
        self.redo.clear();
        had_entries
    }

    fn state_event(&self) -> IdeWorkerEvent {
        IdeWorkerEvent::UndoStateChanged {
            undo_label: self.undo.last().map(|entry| entry.label),
            redo_label: self.redo.last().map(|entry| entry.label),
        }
    }
}

fn push_bounded(stack: &mut Vec<UndoEntry>, entry: UndoEntry) {
    if stack.len() >= MAX_UNDO_DEPTH {
        stack.remove(0);
    }
    stack.push(entry);
}

/// The user-facing name of the durable edit a history entry reverses.
const fn undo_label(command: IdeWorkerCommandKind) -> &'static str {
    match command {
        IdeWorkerCommandKind::SaveProject => "mixer save",
        IdeWorkerCommandKind::PrepareRuntime => "Runtime preparation save",
        IdeWorkerCommandKind::RenameAudioTrack => "track rename",
        IdeWorkerCommandKind::SetAudioTrackActive => "track activation",
        IdeWorkerCommandKind::MoveArrangementPlacement => "placement move",
        IdeWorkerCommandKind::SetArrangementPlacementDuration => "placement duration",
        IdeWorkerCommandKind::TrimArrangementPlacementFront => "placement trim",
        IdeWorkerCommandKind::RemoveArrangementPlacement => "placement removal",
        IdeWorkerCommandKind::SetArrangementPlacementActive => "placement activation",
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => "scene marker move",
        IdeWorkerCommandKind::SetArrangementLoopRange => "loop range",
        IdeWorkerCommandKind::SetTempo => "tempo",
        IdeWorkerCommandKind::SetMeter => "time signature",
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene => "scene marker reassignment",
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch => "scene marker removal",
        IdeWorkerCommandKind::RenameAudioClip => "clip rename",
        IdeWorkerCommandKind::SetAudioClipActive => "clip activation",
        IdeWorkerCommandKind::SetAudioClipGain => "clip gain",
        IdeWorkerCommandKind::SetAudioClipFades => "clip fades",
        IdeWorkerCommandKind::SetAudioClipLoop => "clip loop",
        IdeWorkerCommandKind::SetAudioClipReverse => "clip direction",
        IdeWorkerCommandKind::SetAudioClipSourceStart => "clip source slip",
        IdeWorkerCommandKind::RenameScene => "scene rename",
        IdeWorkerCommandKind::SetSceneSlot => "scene slot edit",
        IdeWorkerCommandKind::SetSceneActive => "scene activation",
        IdeWorkerCommandKind::SetAutomationTakeActive => "automation activation",
        IdeWorkerCommandKind::AddArrangementPlacement => "placement",
        IdeWorkerCommandKind::AddArrangementSceneLaunch => "scene marker",
        IdeWorkerCommandKind::DuplicateArrangementPlacement => "placement duplication",
        IdeWorkerCommandKind::SplitArrangementPlacement => "placement split",
        IdeWorkerCommandKind::AddScene => "scene",
        IdeWorkerCommandKind::DuplicateScene => "scene duplication",
        IdeWorkerCommandKind::AddAutomationTake => "automation take",
        IdeWorkerCommandKind::Initialize
        | IdeWorkerCommandKind::RetryDiscovery
        | IdeWorkerCommandKind::RefreshOutputs
        | IdeWorkerCommandKind::RefreshInputs
        | IdeWorkerCommandKind::SelectSystemDefault
        | IdeWorkerCommandKind::SelectOutput
        | IdeWorkerCommandKind::DisableInputMonitoring
        | IdeWorkerCommandKind::SelectSystemDefaultInput
        | IdeWorkerCommandKind::SelectInput
        | IdeWorkerCommandKind::StartOutputTest
        | IdeWorkerCommandKind::StopOutputTest
        | IdeWorkerCommandKind::CreateProject
        | IdeWorkerCommandKind::OpenProject
        | IdeWorkerCommandKind::Transport
        | IdeWorkerCommandKind::Seek
        | IdeWorkerCommandKind::LocatePlacement
        | IdeWorkerCommandKind::LocateSceneLaunch
        | IdeWorkerCommandKind::Loudness
        | IdeWorkerCommandKind::Mixer
        | IdeWorkerCommandKind::Monitoring
        | IdeWorkerCommandKind::AddAudioTrack
        | IdeWorkerCommandKind::RemoveAudioTrack
        | IdeWorkerCommandKind::ImportAudio
        | IdeWorkerCommandKind::CreateAudioClipFromTake
        | IdeWorkerCommandKind::DuplicateAudioClip
        | IdeWorkerCommandKind::RemoveAudioClip
        | IdeWorkerCommandKind::CleanUnusedSamples
        | IdeWorkerCommandKind::EditControlMapping
        | IdeWorkerCommandKind::ExportProject
        | IdeWorkerCommandKind::Undo
        | IdeWorkerCommandKind::Redo
        | IdeWorkerCommandKind::StartRecording
        | IdeWorkerCommandKind::StopRecording
        | IdeWorkerCommandKind::AbortRecording
        | IdeWorkerCommandKind::StartAudioPreview
        | IdeWorkerCommandKind::StopAudioPreview
        | IdeWorkerCommandKind::AudioPreviewControl
        | IdeWorkerCommandKind::Observe
        | IdeWorkerCommandKind::CloseProject
        | IdeWorkerCommandKind::Shutdown => "edit",
    }
}

fn run_worker(
    mut state: WorkerState,
    project_history: Option<&ProjectHistory>,
    commands: &Receiver<IdeWorkerCommand>,
    events: &SyncSender<IdeWorkerEvent>,
) -> IdeWorkerShutdownReport {
    let mut unpublished_native_failure = None;
    let mut history = UndoHistory::default();
    if !publish_project_history(project_history, events) {
        return finish_worker(state, unpublished_native_failure);
    }
    if !initialize_state(&mut state, events, &mut unpublished_native_failure) {
        return finish_worker(state, unpublished_native_failure);
    }
    loop {
        if !poll_pending_state(
            &mut state,
            events,
            &mut unpublished_native_failure,
            &mut history,
        ) {
            return finish_worker(state, unpublished_native_failure);
        }
        let command = if matches!(
            state,
            WorkerState::SaveFence { .. }
                | WorkerState::RecordingFence { .. }
                | WorkerState::AudioPreviewFence { .. }
                | WorkerState::RecordingClosing { .. }
                | WorkerState::OutputTestStopping { .. }
                | WorkerState::Closing(_)
                | WorkerState::Saving { .. }
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
        if !process_command(
            &mut state,
            command,
            events,
            &mut unpublished_native_failure,
            &mut history,
            project_history,
        ) {
            break;
        }
    }
    finish_worker(state, unpublished_native_failure)
}

fn publish_project_history(
    project_history: Option<&ProjectHistory>,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let Some(project_history) = project_history else {
        return true;
    };
    match project_history.load() {
        Ok(projects) => send_event(events, IdeWorkerEvent::RecentProjectsChanged { projects }),
        Err(reason) => send_event(events, IdeWorkerEvent::ProjectHistoryWarning(reason)),
    }
}

fn remember_running_project(
    project_history: Option<&ProjectHistory>,
    application: &RunningDesktopIdeApplication,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let (Some(project_history), Some(snapshot)) = (project_history, application.snapshot()) else {
        return true;
    };
    match project_history.remember(snapshot.project_name, snapshot.project_path) {
        Ok(projects) => send_event(events, IdeWorkerEvent::RecentProjectsChanged { projects }),
        Err(reason) => send_event(events, IdeWorkerEvent::ProjectHistoryWarning(reason)),
    }
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

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive top-level command router must remain compiler-checked"
)]
fn process_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    history: &mut UndoHistory,
    project_history: Option<&ProjectHistory>,
) -> bool {
    let kind = command.kind();
    if matches!(
        command,
        IdeWorkerCommand::CloseProject
            | IdeWorkerCommand::OpenProject(_)
            | IdeWorkerCommand::OpenRecentProject(_)
            | IdeWorkerCommand::CreateProject { .. }
    ) && history.clear()
        && !send_event(events, history.state_event())
    {
        return false;
    }
    match command {
        command @ (IdeWorkerCommand::RetryDiscovery
        | IdeWorkerCommand::RefreshOutputs
        | IdeWorkerCommand::SelectSystemDefault
        | IdeWorkerCommand::SelectOutput(_)) => {
            process_output_setup_command(state, command, events, unpublished_native_failure)
        }
        command @ (IdeWorkerCommand::RefreshInputs
        | IdeWorkerCommand::DisableInputMonitoring
        | IdeWorkerCommand::SelectSystemDefaultInput
        | IdeWorkerCommand::SelectInput(_)) => {
            process_input_command(state, command, events, unpublished_native_failure)
        }
        IdeWorkerCommand::StartOutputTest => start_output_test(state, events),
        IdeWorkerCommand::StopOutputTest => {
            stop_output_test(state, events, unpublished_native_failure)
        }
        IdeWorkerCommand::CreateProject {
            parent,
            directory_name,
            project_name,
        } => create_project(
            state,
            &parent,
            &directory_name,
            &project_name,
            events,
            unpublished_native_failure,
            project_history,
        ),
        IdeWorkerCommand::OpenProject(path) => open_project(
            state,
            path,
            events,
            unpublished_native_failure,
            project_history,
        ),
        IdeWorkerCommand::OpenRecentProject(path) => open_recent_project(
            state,
            path,
            events,
            unpublished_native_failure,
            project_history,
        ),
        IdeWorkerCommand::Transport(command) => process_transport_command(state, command, events),
        IdeWorkerCommand::Seek { frame } => process_seek_command(state, frame, events),
        IdeWorkerCommand::LocatePlacement {
            project_id,
            index,
            placement_id,
        } => process_arrangement_locate_command(state, &project_id, index, &placement_id, events),
        IdeWorkerCommand::LocateSceneLaunch {
            project_id,
            index,
            launch_id,
            state: locate_state,
        } => process_arrangement_scene_launch_locate_command(
            state,
            &project_id,
            index,
            &launch_id,
            locate_state,
            events,
        ),
        IdeWorkerCommand::Loudness(command) => process_loudness_command(state, command, events),
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
        command @ (IdeWorkerCommand::SaveProject
        | IdeWorkerCommand::PrepareRuntime { .. }
        | IdeWorkerCommand::AddAudioTrack { .. }
        | IdeWorkerCommand::RemoveAudioTrack { .. }
        | IdeWorkerCommand::RenameAudioTrack { .. }
        | IdeWorkerCommand::SetAudioTrackActive { .. }
        | IdeWorkerCommand::ImportAudio { .. }
        | IdeWorkerCommand::CreateAudioClipFromTake { .. }
        | IdeWorkerCommand::DuplicateAudioClip { .. }
        | IdeWorkerCommand::RemoveAudioClip { .. }
        | IdeWorkerCommand::CleanUnusedSamples { .. }
        | IdeWorkerCommand::AddArrangementPlacement { .. }
        | IdeWorkerCommand::AddArrangementSceneLaunch { .. }
        | IdeWorkerCommand::MoveArrangementSceneLaunch { .. }
        | IdeWorkerCommand::SetArrangementLoopRange { .. }
        | IdeWorkerCommand::SetTempo { .. }
        | IdeWorkerCommand::SetMeter { .. }
        | IdeWorkerCommand::SetArrangementSceneLaunchScene { .. }
        | IdeWorkerCommand::RemoveArrangementSceneLaunch { .. }
        | IdeWorkerCommand::DuplicateArrangementPlacement { .. }
        | IdeWorkerCommand::SplitArrangementPlacement { .. }
        | IdeWorkerCommand::SetArrangementPlacementActive { .. }
        | IdeWorkerCommand::MoveArrangementPlacement { .. }
        | IdeWorkerCommand::SetArrangementPlacementDuration { .. }
        | IdeWorkerCommand::TrimArrangementPlacementFront { .. }
        | IdeWorkerCommand::RemoveArrangementPlacement { .. }
        | IdeWorkerCommand::RenameAudioClip { .. }
        | IdeWorkerCommand::SetAudioClipActive { .. }
        | IdeWorkerCommand::SetAudioClipGain { .. }
        | IdeWorkerCommand::SetAudioClipFades { .. }
        | IdeWorkerCommand::SetAudioClipLoop { .. }
        | IdeWorkerCommand::SetAudioClipReverse { .. }
        | IdeWorkerCommand::SetAudioClipSourceStart { .. }
        | IdeWorkerCommand::AddScene { .. }
        | IdeWorkerCommand::AddAutomationTake { .. }
        | IdeWorkerCommand::SetAutomationTakeActive { .. }
        | IdeWorkerCommand::EditControlMapping { .. }
        | IdeWorkerCommand::Undo
        | IdeWorkerCommand::Redo
        | IdeWorkerCommand::DuplicateScene { .. }
        | IdeWorkerCommand::SetSceneSlot { .. }
        | IdeWorkerCommand::RenameScene { .. }
        | IdeWorkerCommand::SetSceneActive { .. }) => {
            process_project_mutation_command(state, command, events, history)
        }
        IdeWorkerCommand::StartRecording { channel_index } => {
            start_recording(state, channel_index, events)
        }
        IdeWorkerCommand::StopRecording => finalize_recording(state, events),
        IdeWorkerCommand::AbortRecording => abort_recording(state, events),
        command @ (IdeWorkerCommand::StartAudioPreview { .. }
        | IdeWorkerCommand::StopAudioPreview
        | IdeWorkerCommand::AudioPreviewControl(_)) => {
            process_audio_preview_command(state, command, events)
        }
        IdeWorkerCommand::ExportProject {
            wav_path,
            block_frames,
        } => export_project(state, events, &wav_path, block_frames),
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

fn process_output_setup_command(
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
        _ => unreachable!("only output-setup commands are routed to this handler"),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive command router keeps every project mutation on one lifecycle path"
)]
fn process_project_mutation_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
    history: &mut UndoHistory,
) -> bool {
    match command {
        IdeWorkerCommand::SaveProject => save_project(state, events),
        IdeWorkerCommand::PrepareRuntime { output_path } => begin_project_mutation_request(
            state,
            events,
            ProjectMutation::PrepareRuntime { output_path },
        ),
        IdeWorkerCommand::AddAudioTrack { name } => add_audio_track(state, events, &name),
        IdeWorkerCommand::RemoveAudioTrack { track_id } => {
            remove_audio_track(state, events, &track_id)
        }
        IdeWorkerCommand::RenameAudioTrack { track_id, name } => {
            rename_audio_track(state, events, &track_id, &name)
        }
        IdeWorkerCommand::SetAudioTrackActive { track_id, active } => {
            set_audio_track_active(state, events, &track_id, active)
        }
        IdeWorkerCommand::ImportAudio { paths, track_id } => {
            import_audio(state, events, paths, track_id)
        }
        IdeWorkerCommand::CreateAudioClipFromTake { recording_id, name } => {
            create_audio_clip_from_take(state, events, &recording_id, &name)
        }
        IdeWorkerCommand::DuplicateAudioClip { clip_id, name } => {
            duplicate_audio_clip(state, events, &clip_id, &name)
        }
        IdeWorkerCommand::RemoveAudioClip { clip_id } => remove_audio_clip(state, events, &clip_id),
        IdeWorkerCommand::CleanUnusedSamples { confirmation } => begin_project_mutation_request(
            state,
            events,
            ProjectMutation::CleanUnusedSamples { confirmation },
        ),
        command @ (IdeWorkerCommand::AddArrangementPlacement { .. }
        | IdeWorkerCommand::AddArrangementSceneLaunch { .. }
        | IdeWorkerCommand::MoveArrangementSceneLaunch { .. }
        | IdeWorkerCommand::SetArrangementLoopRange { .. }
        | IdeWorkerCommand::SetTempo { .. }
        | IdeWorkerCommand::SetMeter { .. }
        | IdeWorkerCommand::SetArrangementSceneLaunchScene { .. }
        | IdeWorkerCommand::RemoveArrangementSceneLaunch { .. }
        | IdeWorkerCommand::DuplicateArrangementPlacement { .. }
        | IdeWorkerCommand::SplitArrangementPlacement { .. }
        | IdeWorkerCommand::SetArrangementPlacementActive { .. }
        | IdeWorkerCommand::MoveArrangementPlacement { .. }
        | IdeWorkerCommand::SetArrangementPlacementDuration { .. }
        | IdeWorkerCommand::TrimArrangementPlacementFront { .. }
        | IdeWorkerCommand::RemoveArrangementPlacement { .. }) => {
            process_arrangement_mutation_command(state, command, events)
        }
        IdeWorkerCommand::RenameAudioClip { clip_id, name } => {
            rename_audio_clip(state, events, &clip_id, &name)
        }
        IdeWorkerCommand::SetAudioClipActive { clip_id, active } => {
            set_audio_clip_active(state, events, &clip_id, active)
        }
        IdeWorkerCommand::SetAudioClipGain {
            clip_id,
            gain_decibels,
        } => set_audio_clip_gain(state, events, &clip_id, gain_decibels),
        IdeWorkerCommand::SetAudioClipFades {
            clip_id,
            fade_in_ticks,
            fade_out_ticks,
        } => set_audio_clip_fades(state, events, &clip_id, fade_in_ticks, fade_out_ticks),
        IdeWorkerCommand::SetAudioClipLoop {
            clip_id,
            enabled,
            start_tick,
            end_tick,
        } => set_audio_clip_loop(state, events, &clip_id, enabled, start_tick, end_tick),
        IdeWorkerCommand::SetAudioClipReverse { clip_id, reverse } => {
            set_audio_clip_reverse(state, events, &clip_id, reverse)
        }
        IdeWorkerCommand::SetAudioClipSourceStart {
            clip_id,
            source_start_frame,
        } => set_audio_clip_source_start(state, events, &clip_id, source_start_frame),
        IdeWorkerCommand::AddScene { name, quantization } => {
            add_scene(state, events, &name, quantization)
        }
        IdeWorkerCommand::AddAutomationTake { request } => {
            add_automation_take(state, events, request)
        }
        IdeWorkerCommand::SetAutomationTakeActive { take_id, enabled } => {
            set_automation_take_active(state, events, &take_id, enabled)
        }
        IdeWorkerCommand::EditControlMapping { edit } => edit_control_mapping(state, events, edit),
        IdeWorkerCommand::Undo => restore_edit(state, events, history, false),
        IdeWorkerCommand::Redo => restore_edit(state, events, history, true),
        IdeWorkerCommand::DuplicateScene { scene_id, name } => {
            duplicate_scene(state, events, &scene_id, &name)
        }
        IdeWorkerCommand::SetSceneSlot {
            scene_id,
            track_id,
            edit,
        } => set_scene_slot(state, events, &scene_id, &track_id, edit),
        IdeWorkerCommand::RenameScene { scene_id, name } => {
            rename_scene(state, events, &scene_id, &name)
        }
        IdeWorkerCommand::SetSceneActive { scene_id, active } => {
            set_scene_active(state, events, &scene_id, active)
        }
        _ => unreachable!("only project-mutation commands are routed to this handler"),
    }
}

fn import_audio(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    paths: Vec<PathBuf>,
    track_id: String,
) -> bool {
    let invalid = if paths.is_empty() {
        Some(IdeAudioImportError::EmptySelection)
    } else if track_id.is_empty() {
        Some(IdeAudioImportError::InvalidDestinationTrack)
    } else {
        None
    };
    if let Some(source) = invalid {
        return publish_failure(
            events,
            IdeWorkerCommandKind::ImportAudio,
            IdeWorkerCommandFailure::AudioImport(Box::new(IdeAudioImportBatchFailure {
                completed: 0,
                total: paths.len(),
                path: paths.first().cloned().unwrap_or_default(),
                source: Box::new(source),
            })),
        );
    }
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::ImportAudio { paths, track_id },
    )
}

fn process_arrangement_mutation_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match command {
        IdeWorkerCommand::AddArrangementPlacement {
            clip_id,
            start_tick,
        } => add_arrangement_placement(state, events, &clip_id, start_tick),
        IdeWorkerCommand::AddArrangementSceneLaunch { scene_id, tick } => {
            add_arrangement_scene_launch(state, events, &scene_id, tick)
        }
        IdeWorkerCommand::SetArrangementLoopRange { range } => begin_project_mutation_request(
            state,
            events,
            ProjectMutation::SetArrangementLoopRange { range },
        ),
        IdeWorkerCommand::SetTempo {
            microseconds_per_quarter,
        } => begin_project_mutation_request(
            state,
            events,
            ProjectMutation::SetTempo {
                microseconds_per_quarter,
            },
        ),
        IdeWorkerCommand::SetMeter {
            numerator,
            denominator,
        } => begin_project_mutation_request(
            state,
            events,
            ProjectMutation::SetMeter {
                numerator,
                denominator,
            },
        ),
        IdeWorkerCommand::MoveArrangementSceneLaunch { launch_id, tick } => {
            move_arrangement_scene_launch(state, events, &launch_id, tick)
        }
        IdeWorkerCommand::SetArrangementSceneLaunchScene {
            launch_id,
            scene_id,
        } => set_arrangement_scene_launch_scene(state, events, &launch_id, &scene_id),
        IdeWorkerCommand::RemoveArrangementSceneLaunch { launch_id } => {
            remove_arrangement_scene_launch(state, events, &launch_id)
        }
        IdeWorkerCommand::DuplicateArrangementPlacement {
            placement_id,
            start_tick,
        } => duplicate_arrangement_placement(state, events, &placement_id, start_tick),
        IdeWorkerCommand::SplitArrangementPlacement {
            placement_id,
            split_tick,
        } => split_arrangement_placement(state, events, &placement_id, split_tick),
        IdeWorkerCommand::SetArrangementPlacementActive {
            placement_id,
            active,
        } => set_arrangement_placement_active(state, events, &placement_id, active),
        IdeWorkerCommand::MoveArrangementPlacement {
            placement_id,
            start_tick,
        } => move_arrangement_placement(state, events, &placement_id, start_tick),
        IdeWorkerCommand::SetArrangementPlacementDuration {
            placement_id,
            duration_ticks,
        } => set_arrangement_placement_duration(state, events, &placement_id, duration_ticks),
        IdeWorkerCommand::TrimArrangementPlacementFront {
            placement_id,
            start_tick,
        } => trim_arrangement_placement_front(state, events, &placement_id, start_tick),
        IdeWorkerCommand::RemoveArrangementPlacement { placement_id } => {
            remove_arrangement_placement(state, events, &placement_id)
        }
        _ => unreachable!("only arrangement mutations are routed to this handler"),
    }
}

fn create_project(
    state: &mut WorkerState,
    parent: &Path,
    directory_name: &str,
    project_name: &str,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    project_history: Option<&ProjectHistory>,
) -> bool {
    if !matches!(state, WorkerState::Closed(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::CreateProject,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    let WorkerState::Closed(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("closed state was checked before project-creation ownership transfer");
    };
    let creation = match create_project_package(
        parent,
        ProjectCreationRequest {
            directory_name,
            project_name,
        },
    ) {
        Ok(creation) => creation,
        Err(source) => {
            *state = WorkerState::Closed(application);
            return publish_failure(
                events,
                IdeWorkerCommandKind::CreateProject,
                IdeWorkerCommandFailure::ProjectCreation(Box::new(
                    IdeProjectCreationError::Creation(source),
                )),
            );
        }
    };
    let project_path = creation.project().root().to_path_buf();
    drop(creation);
    match application.open_project(&project_path) {
        Ok(running) => {
            let history_published = remember_running_project(project_history, &running, events);
            *state = WorkerState::Running(running);
            history_published && publish_state(state, events, unpublished_native_failure)
        }
        Err(failure) => {
            let (application, source) = failure.into_parts();
            *state = WorkerState::Closed(application);
            publish_failure(
                events,
                IdeWorkerCommandKind::CreateProject,
                IdeWorkerCommandFailure::ProjectCreation(Box::new(
                    IdeProjectCreationError::CreatedButNotOpened {
                        project_path,
                        source,
                    },
                )),
            )
        }
    }
}

fn open_project(
    state: &mut WorkerState,
    path: PathBuf,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    project_history: Option<&ProjectHistory>,
) -> bool {
    if !matches!(state, WorkerState::Closed(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::OpenProject,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    let WorkerState::Closed(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("closed state was checked before ownership transfer");
    };
    match application.open_project(path) {
        Ok(running) => {
            let history_published = remember_running_project(project_history, &running, events);
            *state = WorkerState::Running(running);
            history_published && publish_state(state, events, unpublished_native_failure)
        }
        Err(failure) => {
            let (application, reason) = failure.into_parts();
            *state = WorkerState::Closed(application);
            publish_failure(
                events,
                IdeWorkerCommandKind::OpenProject,
                IdeWorkerCommandFailure::ProjectOpen(reason),
            )
        }
    }
}

fn open_recent_project(
    state: &mut WorkerState,
    path: PathBuf,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    project_history: Option<&ProjectHistory>,
) -> bool {
    match path_is_proven_absent(&path) {
        Ok(true) => {
            if let Some(project_history) = project_history {
                let history_event = match project_history.forget(&path) {
                    Ok(projects) => IdeWorkerEvent::RecentProjectsChanged { projects },
                    Err(reason) => IdeWorkerEvent::ProjectHistoryWarning(reason),
                };
                if !send_event(events, history_event) {
                    return false;
                }
            }
            publish_failure(
                events,
                IdeWorkerCommandKind::OpenProject,
                IdeWorkerCommandFailure::RecentProjectMissing(path),
            )
        }
        Ok(false) | Err(_) => open_project(
            state,
            path,
            events,
            unpublished_native_failure,
            project_history,
        ),
    }
}

fn start_output_test(state: &mut WorkerState, events: &SyncSender<IdeWorkerEvent>) -> bool {
    if !matches!(state, WorkerState::Closed(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartOutputTest,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    let WorkerState::Closed(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("closed state was checked before output-test ownership transfer");
    };
    match application.start_output_test() {
        IdeOutputTestStartOutcome::Started(application) => {
            *state = WorkerState::OutputTesting(application);
            let mut no_native_failure = None;
            publish_state(state, events, &mut no_native_failure)
        }
        IdeOutputTestStartOutcome::Failed {
            application,
            reason,
        } => {
            *state = WorkerState::Closed(application);
            publish_failure(
                events,
                IdeWorkerCommandKind::StartOutputTest,
                IdeWorkerCommandFailure::OutputTest(reason),
            )
        }
        IdeOutputTestStartOutcome::Pending(pending) => {
            *state = WorkerState::OutputTestStopping {
                pending,
                command: IdeWorkerCommandKind::StartOutputTest,
            };
            send_event(events, IdeWorkerEvent::OutputTestStopping)
        }
    }
}

fn stop_output_test(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    if !matches!(state, WorkerState::OutputTesting(_)) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StopOutputTest,
            IdeWorkerCommandFailure::InvalidState,
        );
    }
    let WorkerState::OutputTesting(application) = std::mem::replace(state, WorkerState::Empty)
    else {
        unreachable!("output-test state was checked before ownership transfer");
    };
    match application.stop() {
        IdeOutputTestStopOutcome::Closed {
            application,
            failure,
        } => complete_output_test_stop(
            state,
            application,
            failure,
            IdeWorkerCommandKind::StopOutputTest,
            events,
            unpublished_native_failure,
        ),
        IdeOutputTestStopOutcome::Pending(pending) => {
            *state = WorkerState::OutputTestStopping {
                pending,
                command: IdeWorkerCommandKind::StopOutputTest,
            };
            send_event(events, IdeWorkerEvent::OutputTestStopping)
        }
    }
}

fn complete_output_test_stop(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    failure: Option<IdeOutputTestError>,
    command: IdeWorkerCommandKind,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    *state = WorkerState::Closed(application);
    if !publish_state(state, events, unpublished_native_failure) {
        return false;
    }
    match failure {
        Some(reason) => {
            publish_failure(events, command, IdeWorkerCommandFailure::OutputTest(reason))
        }
        None => send_event(events, IdeWorkerEvent::OutputTestStopped),
    }
}

fn process_transport_command(
    state: &mut WorkerState,
    command: TransportCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match state {
        WorkerState::Running(application) if application.audio_preview_is_active() => {
            publish_failure(
                events,
                IdeWorkerCommandKind::Transport,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                    hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
                )),
            )
        }
        WorkerState::Running(application) => match application.try_send_transport(command) {
            Ok(()) => send_event(events, IdeWorkerEvent::TransportQueued(command)),
            Err(reason) => publish_failure(
                events,
                IdeWorkerCommandKind::Transport,
                IdeWorkerCommandFailure::Transport(reason),
            ),
        },
        _ => publish_failure(
            events,
            IdeWorkerCommandKind::Transport,
            IdeWorkerCommandFailure::InvalidState,
        ),
    }
}

fn process_seek_command(
    state: &mut WorkerState,
    frame: usize,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match state {
        WorkerState::Running(application) if application.audio_preview_is_active() => {
            publish_failure(
                events,
                IdeWorkerCommandKind::Seek,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                    hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
                )),
            )
        }
        WorkerState::Running(application) => match application.try_seek(frame) {
            Ok(()) => send_event(
                events,
                IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                    frame,
                    state: LocateState::Preserve,
                }),
            ),
            Err(reason) => publish_failure(
                events,
                IdeWorkerCommandKind::Seek,
                IdeWorkerCommandFailure::Seek(reason),
            ),
        },
        _ => publish_failure(
            events,
            IdeWorkerCommandKind::Seek,
            IdeWorkerCommandFailure::InvalidState,
        ),
    }
}

fn process_arrangement_locate_command(
    state: &mut WorkerState,
    project_id: &str,
    index: usize,
    placement_id: &str,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match state {
        WorkerState::Running(application) if application.audio_preview_is_active() => {
            publish_failure(
                events,
                IdeWorkerCommandKind::LocatePlacement,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                    hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
                )),
            )
        }
        WorkerState::Running(application) => {
            match application.try_locate_arrangement_placement(project_id, index, placement_id) {
                Ok(frame) => send_event(
                    events,
                    IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                        frame,
                        state: LocateState::Preserve,
                    }),
                ),
                Err(reason) => publish_failure(
                    events,
                    IdeWorkerCommandKind::LocatePlacement,
                    IdeWorkerCommandFailure::ArrangementLocate(reason),
                ),
            }
        }
        _ => publish_failure(
            events,
            IdeWorkerCommandKind::LocatePlacement,
            IdeWorkerCommandFailure::InvalidState,
        ),
    }
}

fn process_arrangement_scene_launch_locate_command(
    state: &mut WorkerState,
    project_id: &str,
    index: usize,
    launch_id: &str,
    locate_state: LocateState,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match state {
        WorkerState::Running(application) if application.audio_preview_is_active() => {
            publish_failure(
                events,
                IdeWorkerCommandKind::LocateSceneLaunch,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                    hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
                )),
            )
        }
        WorkerState::Running(application) => {
            match application.try_locate_arrangement_scene_launch(
                project_id,
                index,
                launch_id,
                locate_state,
            ) {
                Ok(frame) => send_event(
                    events,
                    IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                        frame,
                        state: locate_state,
                    }),
                ),
                Err(reason) => publish_failure(
                    events,
                    IdeWorkerCommandKind::LocateSceneLaunch,
                    IdeWorkerCommandFailure::ArrangementLocate(reason),
                ),
            }
        }
        _ => publish_failure(
            events,
            IdeWorkerCommandKind::LocateSceneLaunch,
            IdeWorkerCommandFailure::InvalidState,
        ),
    }
}

fn process_loudness_command(
    state: &mut WorkerState,
    command: IdeLoudnessCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match state {
        WorkerState::Running(application) if application.audio_preview_is_active() => {
            publish_failure(
                events,
                IdeWorkerCommandKind::Loudness,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                    hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
                )),
            )
        }
        WorkerState::Running(application) => match application.try_send_loudness(command) {
            Ok(()) => send_event(events, IdeWorkerEvent::LoudnessQueued(command)),
            Err(reason) => publish_failure(
                events,
                IdeWorkerCommandKind::Loudness,
                IdeWorkerCommandFailure::Loudness(reason),
            ),
        },
        _ => publish_failure(
            events,
            IdeWorkerCommandKind::Loudness,
            IdeWorkerCommandFailure::InvalidState,
        ),
    }
}

fn process_audio_preview_command(
    state: &mut WorkerState,
    command: IdeWorkerCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    match command {
        IdeWorkerCommand::StartAudioPreview { asset_id } => {
            start_audio_preview(state, asset_id, events)
        }
        IdeWorkerCommand::StopAudioPreview => stop_audio_preview(state, events),
        IdeWorkerCommand::AudioPreviewControl(command) => {
            process_audio_preview_control(state, command, events)
        }
        _ => unreachable!("only audio-preview commands are routed to this handler"),
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

fn start_audio_preview(
    state: &mut WorkerState,
    asset_id: String,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let WorkerState::Running(application) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartAudioPreview,
            IdeWorkerCommandFailure::InvalidState,
        );
    };
    if let Err(reason) = application.validate_audio_preview_asset(&asset_id) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartAudioPreview,
            IdeWorkerCommandFailure::AudioPreview(reason),
        );
    }
    if let Err(reason) = application.try_send_transport(TransportCommand::Stop) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartAudioPreview,
            IdeWorkerCommandFailure::Transport(reason),
        );
    }
    let sequence = match application.try_send_mixer_fence() {
        Ok(Some(sequence)) => sequence,
        Ok(None) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::StartAudioPreview,
                IdeWorkerCommandFailure::AudioPreview(
                    IdeAudioPreviewError::MixerFenceSequenceUnavailable,
                ),
            );
        }
        Err(reason) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::StartAudioPreview,
                IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::MixerCommit(reason)),
            );
        }
    };
    let WorkerState::Running(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("running state was checked before audio preview ownership transfer");
    };
    *state = WorkerState::AudioPreviewFence {
        application,
        asset_id,
        sequence,
        deadline: Instant::now() + MIXER_SAVE_COMMIT_TIMEOUT,
    };
    send_event(events, IdeWorkerEvent::AudioPreviewPreparing)
}

fn stop_audio_preview(state: &mut WorkerState, events: &SyncSender<IdeWorkerEvent>) -> bool {
    let WorkerState::Running(application) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StopAudioPreview,
            IdeWorkerCommandFailure::InvalidState,
        );
    };
    match application.try_stop_audio_preview() {
        Ok(()) => send_event(events, IdeWorkerEvent::AudioPreviewStopRequested),
        Err(reason) => publish_failure(
            events,
            IdeWorkerCommandKind::StopAudioPreview,
            IdeWorkerCommandFailure::AudioPreview(reason),
        ),
    }
}

fn process_audio_preview_control(
    state: &mut WorkerState,
    command: IdeAudioPreviewControlCommand,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    let WorkerState::Running(application) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AudioPreviewControl,
            IdeWorkerCommandFailure::InvalidState,
        );
    };
    match application.apply_audio_preview_control(command) {
        Ok(()) => send_event(events, IdeWorkerEvent::AudioPreviewControlApplied(command)),
        Err(reason) => publish_failure(
            events,
            IdeWorkerCommandKind::AudioPreviewControl,
            IdeWorkerCommandFailure::AudioPreview(reason),
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
    if application.audio_preview_is_active() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::StartRecording,
            IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
            )),
        );
    }
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
    begin_project_mutation_request(state, events, ProjectMutation::SaveMixer)
}

fn add_audio_track(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    name: &str,
) -> bool {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddAudioTrack,
            IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
                IdeAudioTrackAdditionError::InvalidName,
            )),
        );
    }
    let mut normalized = String::new();
    if normalized.try_reserve_exact(name.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddAudioTrack,
            IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
                IdeAudioTrackAdditionError::CapacityUnavailable,
            )),
        );
    }
    normalized.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::AddAudioTrack { name: normalized },
    )
}

fn remove_audio_track(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    track_id: &str,
) -> bool {
    if track_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveAudioTrack,
            IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
                IdeAudioTrackRemovalError::InvalidTrackIdentity,
            )),
        );
    }
    let mut owned_track_id = String::new();
    if owned_track_id.try_reserve_exact(track_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveAudioTrack,
            IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
                IdeAudioTrackRemovalError::CapacityUnavailable,
            )),
        );
    }
    owned_track_id.push_str(track_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RemoveAudioTrack {
            track_id: owned_track_id,
        },
    )
}

fn rename_audio_track(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    track_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if track_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioTrack,
            IdeWorkerCommandFailure::AudioTrackRename(Box::new(
                IdeAudioTrackEditError::TrackResourceMissing,
            )),
        );
    }
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioTrack,
            IdeWorkerCommandFailure::AudioTrackRename(Box::new(
                IdeAudioTrackEditError::InvalidName,
            )),
        );
    }
    let mut owned_track_id = String::new();
    let mut normalized_name = String::new();
    if owned_track_id.try_reserve_exact(track_id.len()).is_err()
        || normalized_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioTrack,
            IdeWorkerCommandFailure::AudioTrackRename(Box::new(
                IdeAudioTrackEditError::CapacityUnavailable,
            )),
        );
    }
    owned_track_id.push_str(track_id);
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RenameAudioTrack {
            track_id: owned_track_id,
            name: normalized_name,
        },
    )
}

fn set_audio_track_active(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    track_id: &str,
    active: bool,
) -> bool {
    if track_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioTrackActive,
            IdeWorkerCommandFailure::AudioTrackActivation(Box::new(
                IdeAudioTrackEditError::TrackResourceMissing,
            )),
        );
    }
    let mut owned_track_id = String::new();
    if owned_track_id.try_reserve_exact(track_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioTrackActive,
            IdeWorkerCommandFailure::AudioTrackActivation(Box::new(
                IdeAudioTrackEditError::CapacityUnavailable,
            )),
        );
    }
    owned_track_id.push_str(track_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioTrackActive {
            track_id: owned_track_id,
            active,
        },
    )
}

fn set_arrangement_placement_active(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    active: bool,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementPlacementActive,
            IdeWorkerCommandFailure::ArrangementPlacementActivation(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementPlacementActive,
            IdeWorkerCommandFailure::ArrangementPlacementActivation(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetArrangementPlacementActive {
            placement_id: owned_placement_id,
            active,
        },
    )
}

fn create_audio_clip_from_take(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    recording_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if recording_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::CreateAudioClipFromTake,
            IdeWorkerCommandFailure::RecordingClipCreation(Box::new(
                IdeRecordingClipCreationError::InvalidRecordingIdentity,
            )),
        );
    }
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::CreateAudioClipFromTake,
            IdeWorkerCommandFailure::RecordingClipCreation(Box::new(
                IdeRecordingClipCreationError::InvalidClipName,
            )),
        );
    }
    let mut owned_recording_id = String::new();
    let mut normalized_name = String::new();
    if owned_recording_id
        .try_reserve_exact(recording_id.len())
        .is_err()
        || normalized_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::CreateAudioClipFromTake,
            IdeWorkerCommandFailure::RecordingClipCreation(Box::new(
                IdeRecordingClipCreationError::CapacityUnavailable,
            )),
        );
    }
    owned_recording_id.push_str(recording_id);
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::CreateAudioClipFromTake {
            recording_id: owned_recording_id,
            name: normalized_name,
        },
    )
}

fn duplicate_audio_clip(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateAudioClip,
            IdeWorkerCommandFailure::AudioClipDuplication(Box::new(
                IdeAudioClipDuplicationError::InvalidSourceClipIdentity,
            )),
        );
    }
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateAudioClip,
            IdeWorkerCommandFailure::AudioClipDuplication(Box::new(
                IdeAudioClipDuplicationError::InvalidClipName,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    let mut normalized_name = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err()
        || normalized_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateAudioClip,
            IdeWorkerCommandFailure::AudioClipDuplication(Box::new(
                IdeAudioClipDuplicationError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::DuplicateAudioClip {
            clip_id: owned_clip_id,
            name: normalized_name,
        },
    )
}

fn remove_audio_clip(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveAudioClip,
            IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
                IdeAudioClipRemovalError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveAudioClip,
            IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
                IdeAudioClipRemovalError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RemoveAudioClip {
            clip_id: owned_clip_id,
        },
    )
}

fn add_arrangement_placement(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    start_tick: u64,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementAddition(Box::new(
                IdeArrangementPlacementEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementAddition(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::AddArrangementPlacement {
            clip_id: owned_clip_id,
            start_tick,
        },
    )
}

fn add_arrangement_scene_launch(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    scene_id: &str,
    tick: u64,
) -> bool {
    if scene_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddArrangementSceneLaunch,
            IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(Box::new(
                IdeArrangementPlacementEditError::InvalidSceneIdentity,
            )),
        );
    }
    let mut owned_scene_id = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddArrangementSceneLaunch,
            IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::AddArrangementSceneLaunch {
            scene_id: owned_scene_id,
            tick,
        },
    )
}

fn move_arrangement_scene_launch(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    launch_id: &str,
    tick: u64,
) -> bool {
    let launch_id = match own_arrangement_scene_launch_id(launch_id) {
        Ok(launch_id) => launch_id,
        Err(reason) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                IdeWorkerCommandFailure::ArrangementSceneLaunchMove(Box::new(reason)),
            );
        }
    };
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::MoveArrangementSceneLaunch { launch_id, tick },
    )
}

fn set_arrangement_scene_launch_scene(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    launch_id: &str,
    scene_id: &str,
) -> bool {
    let launch_id = match own_arrangement_scene_launch_id(launch_id) {
        Ok(launch_id) => launch_id,
        Err(reason) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(Box::new(reason)),
            );
        }
    };
    if scene_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
            IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(Box::new(
                IdeArrangementPlacementEditError::InvalidSceneIdentity,
            )),
        );
    }
    let mut owned_scene_id = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
            IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetArrangementSceneLaunchScene {
            launch_id,
            scene_id: owned_scene_id,
        },
    )
}

fn remove_arrangement_scene_launch(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    launch_id: &str,
) -> bool {
    let launch_id = match own_arrangement_scene_launch_id(launch_id) {
        Ok(launch_id) => launch_id,
        Err(reason) => {
            return publish_failure(
                events,
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval(Box::new(reason)),
            );
        }
    };
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RemoveArrangementSceneLaunch { launch_id },
    )
}

fn own_arrangement_scene_launch_id(
    launch_id: &str,
) -> Result<String, IdeArrangementPlacementEditError> {
    if launch_id.is_empty() {
        return Err(IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity);
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(launch_id.len())
        .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
    owned.push_str(launch_id);
    Ok(owned)
}

fn duplicate_arrangement_placement(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    start_tick: u64,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementDuplication(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementDuplication(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::DuplicateArrangementPlacement {
            placement_id: owned_placement_id,
            start_tick,
        },
    )
}

fn split_arrangement_placement(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    split_tick: u64,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SplitArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementSplit(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SplitArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementSplit(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SplitArrangementPlacement {
            placement_id: owned_placement_id,
            split_tick,
        },
    )
}

fn move_arrangement_placement(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    start_tick: u64,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::MoveArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementMove(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::MoveArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementMove(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::MoveArrangementPlacement {
            placement_id: owned_placement_id,
            start_tick,
        },
    )
}

fn set_arrangement_placement_duration(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    duration_ticks: u64,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementPlacementDuration,
            IdeWorkerCommandFailure::ArrangementPlacementDuration(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetArrangementPlacementDuration,
            IdeWorkerCommandFailure::ArrangementPlacementDuration(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetArrangementPlacementDuration {
            placement_id: owned_placement_id,
            duration_ticks,
        },
    )
}

fn trim_arrangement_placement_front(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
    start_tick: u64,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::TrimArrangementPlacementFront,
            IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::TrimArrangementPlacementFront,
            IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::TrimArrangementPlacementFront {
            placement_id: owned_placement_id,
            start_tick,
        },
    )
}

fn remove_arrangement_placement(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    placement_id: &str,
) -> bool {
    if placement_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementRemoval(Box::new(
                IdeArrangementPlacementEditError::InvalidPlacementIdentity,
            )),
        );
    }
    let mut owned_placement_id = String::new();
    if owned_placement_id
        .try_reserve_exact(placement_id.len())
        .is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RemoveArrangementPlacement,
            IdeWorkerCommandFailure::ArrangementPlacementRemoval(Box::new(
                IdeArrangementPlacementEditError::CapacityUnavailable,
            )),
        );
    }
    owned_placement_id.push_str(placement_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RemoveArrangementPlacement {
            placement_id: owned_placement_id,
        },
    )
}

fn rename_audio_clip(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioClip,
            IdeWorkerCommandFailure::AudioClipRename(Box::new(
                IdeAudioClipRenameEditError::InvalidClipIdentity,
            )),
        );
    }
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioClip,
            IdeWorkerCommandFailure::AudioClipRename(Box::new(
                IdeAudioClipRenameEditError::InvalidName,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    let mut normalized_name = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err()
        || normalized_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameAudioClip,
            IdeWorkerCommandFailure::AudioClipRename(Box::new(
                IdeAudioClipRenameEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RenameAudioClip {
            clip_id: owned_clip_id,
            name: normalized_name,
        },
    )
}

fn set_audio_clip_active(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    active: bool,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipActive,
            IdeWorkerCommandFailure::AudioClipActivation(Box::new(
                IdeAudioClipActivationEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipActive,
            IdeWorkerCommandFailure::AudioClipActivation(Box::new(
                IdeAudioClipActivationEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipActive {
            clip_id: owned_clip_id,
            active,
        },
    )
}

fn add_scene(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    name: &str,
    quantization: IdeSceneLaunchQuantization,
) -> bool {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddScene,
            IdeWorkerCommandFailure::SceneCreation(Box::new(IdeSceneCreationError::InvalidName)),
        );
    }
    if matches!(
        quantization,
        IdeSceneLaunchQuantization::Grid { quantum_ticks: 0 }
    ) {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddScene,
            IdeWorkerCommandFailure::SceneCreation(Box::new(
                IdeSceneCreationError::InvalidQuantization,
            )),
        );
    }
    let mut normalized_name = String::new();
    if normalized_name.try_reserve_exact(name.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddScene,
            IdeWorkerCommandFailure::SceneCreation(Box::new(
                IdeSceneCreationError::CapacityUnavailable,
            )),
        );
    }
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::AddScene {
            name: normalized_name,
            quantization,
        },
    )
}

fn add_automation_take(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    mut request: IdeAutomationTakeRequest,
) -> bool {
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddAutomationTake,
            IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(
                IdeAutomationTakeCreationError::InvalidName,
            )),
        );
    }
    let empty_target = match &request.target {
        IdeAutomationTarget::Channel { track_id, .. } => track_id.is_empty(),
        IdeAutomationTarget::Send { send_id, .. } => send_id.is_empty(),
        IdeAutomationTarget::Main { .. } => false,
    };
    if empty_target {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddAutomationTake,
            IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(
                IdeAutomationTakeCreationError::InvalidTargetIdentity,
            )),
        );
    }
    let mut normalized_name = String::new();
    if normalized_name.try_reserve_exact(name.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::AddAutomationTake,
            IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(
                IdeAutomationTakeCreationError::CapacityUnavailable,
            )),
        );
    }
    normalized_name.push_str(name);
    request.name = normalized_name;
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::AddAutomationTake { request },
    )
}

fn export_project(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    wav_path: &std::path::Path,
    block_frames: usize,
) -> bool {
    let WorkerState::Running(running) = state else {
        return publish_failure(
            events,
            IdeWorkerCommandKind::ExportProject,
            IdeWorkerCommandFailure::ProjectExport(Box::new(
                IdeProjectExportError::ProjectSnapshotUnavailable,
            )),
        );
    };
    if !send_event(events, IdeWorkerEvent::ProjectExporting) {
        return false;
    }
    match running.export_project(wav_path, block_frames) {
        Ok(report) => send_event(events, IdeWorkerEvent::ProjectExported { report }),
        Err(reason) => publish_failure(
            events,
            IdeWorkerCommandKind::ExportProject,
            IdeWorkerCommandFailure::ProjectExport(Box::new(reason)),
        ),
    }
}

/// A restore that failed still has to leave history agreeing with disk. A
/// committed publication consumes its entry exactly as a clean one would; a
/// refusal that can never succeed is discarded so it cannot wedge the entries
/// behind it; anything retryable is left in place.
fn restore_failure_event(
    history: &mut UndoHistory,
    entry: &UndoEntry,
    redo: bool,
    reason: IdeResourceRestoreError,
) -> (IdeWorkerEvent, bool) {
    if reason.publication_committed() {
        history.complete_restore(redo, None);
        return (
            IdeWorkerEvent::EditRestoreCommittedWithWarning {
                redo,
                label: entry.label,
                warning: Box::new(reason),
            },
            true,
        );
    }
    let consumed = reason.is_permanent_refusal();
    if consumed {
        history.complete_restore(redo, None);
    }
    (
        IdeWorkerEvent::CommandFailed {
            command: if redo {
                IdeWorkerCommandKind::Redo
            } else {
                IdeWorkerCommandKind::Undo
            },
            reason: IdeWorkerCommandFailure::EditRestore(Box::new(reason)),
        },
        consumed,
    )
}

/// The single capture point. A restore's own outcome feeds the opposite stack,
/// so undoing an undo is redo. A structural change no stored document can
/// express drops the history rather than leaving entries that would silently
/// revert later work.
fn observe_history(
    history: &mut UndoHistory,
    attempt: &ProjectMutationAttempt,
    command: IdeWorkerCommandKind,
    events: &SyncSender<IdeWorkerEvent>,
) {
    let changed = match attempt {
        ProjectMutationAttempt::RestoreResource {
            entry,
            redo,
            result: Ok(outcome),
        } => {
            history.complete_restore(
                *redo,
                outcome.previous_resource().map(|previous| UndoEntry {
                    resource_id: entry.resource_id.clone(),
                    bytes: previous.to_vec(),
                    label: entry.label,
                }),
            );
            true
        }
        other => match history_effect(other) {
            HistoryEffect::Replacement(outcome) => match outcome.previous_resource() {
                Some(previous) => {
                    history.record_edit(UndoEntry {
                        resource_id: outcome.resource_id().to_owned(),
                        bytes: previous.to_vec(),
                        label: undo_label(command),
                    });
                    true
                }
                None => false,
            },
            HistoryEffect::Invalidate => history.clear(),
            HistoryEffect::None => false,
        },
    };
    if changed {
        let _ = send_event(events, history.state_event());
    }
}

/// What a completed mutation does to the undo history.
enum HistoryEffect<'a> {
    /// The mutation replaced one resource; the storage layer decides whether
    /// anything was actually displaced.
    Replacement(&'a hyper_storage_engine::ProjectResourcePublicationOutcome),
    /// The mutation changed the project's structure in a way a stored document
    /// cannot express, so every retained entry may no longer fit.
    Invalidate,
    /// The mutation failed, so the project is unchanged.
    None,
}

/// Exhaustive by construction: a new mutation variant will not compile until
/// its history effect is stated. The previous hand-maintained list silently
/// omitted `AddArrangementPlacement`, `DuplicateArrangementPlacement`, and
/// `SplitArrangementPlacement`, which replace the same arrangement document as
/// the operations it did list, so an undo reverted work it never captured.
#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive mutation table keeps every success, partial commit, and refusal tied to its undo-history effect"
)]
fn history_effect(attempt: &ProjectMutationAttempt) -> HistoryEffect<'_> {
    match attempt {
        ProjectMutationAttempt::PrepareRuntime(attempt) => attempt
            .save
            .as_ref()
            .map_or(HistoryEffect::None, HistoryEffect::Replacement),
        ProjectMutationAttempt::Save(Ok(outcome))
        | ProjectMutationAttempt::RenameAudioTrack(Ok(outcome))
        | ProjectMutationAttempt::RemoveArrangementPlacement(Ok(outcome))
        | ProjectMutationAttempt::SetAudioTrackActive {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::AddArrangementSceneLaunch {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::MoveArrangementSceneLaunch {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetArrangementLoopRange {
            result: Ok(outcome),
        }
        | ProjectMutationAttempt::SetTempo {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetMeter {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetArrangementSceneLaunchScene {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::RemoveArrangementSceneLaunch {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetArrangementPlacementActive {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetSceneSlot {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::MoveArrangementPlacement {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetArrangementPlacementDuration {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::TrimArrangementPlacementFront {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::RenameAudioClip {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipActive {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipGain {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipFades {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipLoop {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipReverse {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAudioClipSourceStart {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::AddScene {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::AddAutomationTake {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetAutomationTakeActive {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::DuplicateScene {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::RenameScene {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::SetSceneActive {
            result: Ok(outcome),
            ..
        }
        | ProjectMutationAttempt::RestoreResource {
            result: Ok(outcome),
            ..
        } => HistoryEffect::Replacement(outcome),
        ProjectMutationAttempt::AddArrangementPlacement {
            result: Ok(publication),
            ..
        }
        | ProjectMutationAttempt::DuplicateArrangementPlacement {
            result: Ok(publication),
            ..
        }
        | ProjectMutationAttempt::SplitArrangementPlacement {
            result: Ok(publication),
            ..
        } => HistoryEffect::Replacement(&publication.outcome),
        ProjectMutationAttempt::AddAudioTrack(Ok(_))
        | ProjectMutationAttempt::RemoveAudioTrack(Ok(_))
        | ProjectMutationAttempt::ImportAudio(
            Ok(_)
            | Err(IdeAudioImportBatchFailure {
                completed: 1.., ..
            }),
        )
        | ProjectMutationAttempt::CreateAudioClipFromTake(Ok(_))
        | ProjectMutationAttempt::DuplicateAudioClip { result: Ok(_), .. }
        | ProjectMutationAttempt::RemoveAudioClip(Ok(_))
        | ProjectMutationAttempt::CleanUnusedSamples(Ok(_))
        | ProjectMutationAttempt::EditControlMapping { result: Ok(_), .. } => {
            HistoryEffect::Invalidate
        }
        ProjectMutationAttempt::RemoveAudioClip(Err(reason))
            if reason.publication_committed() =>
        {
            HistoryEffect::Invalidate
        }
        ProjectMutationAttempt::CleanUnusedSamples(Err(reason))
            if reason.publication_committed() =>
        {
            HistoryEffect::Invalidate
        }
        ProjectMutationAttempt::Save(Err(_))
        | ProjectMutationAttempt::AddAudioTrack(Err(_))
        | ProjectMutationAttempt::RemoveAudioTrack(Err(_))
        | ProjectMutationAttempt::ImportAudio(Err(IdeAudioImportBatchFailure {
            completed: 0,
            ..
        }))
        | ProjectMutationAttempt::RenameAudioTrack(Err(_))
        | ProjectMutationAttempt::CreateAudioClipFromTake(Err(_))
        | ProjectMutationAttempt::RemoveArrangementPlacement(Err(_))
        | ProjectMutationAttempt::SetAudioTrackActive { result: Err(_), .. }
        | ProjectMutationAttempt::DuplicateAudioClip { result: Err(_), .. }
        | ProjectMutationAttempt::RemoveAudioClip(Err(_))
        | ProjectMutationAttempt::CleanUnusedSamples(Err(_))
        | ProjectMutationAttempt::AddArrangementPlacement { result: Err(_), .. }
        | ProjectMutationAttempt::AddArrangementSceneLaunch { result: Err(_), .. }
        | ProjectMutationAttempt::MoveArrangementSceneLaunch { result: Err(_), .. }
        | ProjectMutationAttempt::SetArrangementLoopRange { result: Err(_) }
        | ProjectMutationAttempt::SetTempo { result: Err(_), .. }
        | ProjectMutationAttempt::SetMeter { result: Err(_), .. }
        | ProjectMutationAttempt::SetArrangementSceneLaunchScene { result: Err(_), .. }
        | ProjectMutationAttempt::RemoveArrangementSceneLaunch { result: Err(_), .. }
        | ProjectMutationAttempt::DuplicateArrangementPlacement { result: Err(_), .. }
        | ProjectMutationAttempt::SplitArrangementPlacement { result: Err(_), .. }
        | ProjectMutationAttempt::SetArrangementPlacementActive { result: Err(_), .. }
        | ProjectMutationAttempt::SetSceneSlot { result: Err(_), .. }
        | ProjectMutationAttempt::MoveArrangementPlacement { result: Err(_), .. }
        | ProjectMutationAttempt::SetArrangementPlacementDuration { result: Err(_), .. }
        | ProjectMutationAttempt::TrimArrangementPlacementFront { result: Err(_), .. }
        | ProjectMutationAttempt::RenameAudioClip { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipActive { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipGain { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipFades { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipLoop { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipReverse { result: Err(_), .. }
        | ProjectMutationAttempt::SetAudioClipSourceStart { result: Err(_), .. }
        | ProjectMutationAttempt::AddScene { result: Err(_), .. }
        | ProjectMutationAttempt::AddAutomationTake { result: Err(_), .. }
        | ProjectMutationAttempt::SetAutomationTakeActive { result: Err(_), .. }
        | ProjectMutationAttempt::EditControlMapping { result: Err(_), .. }
        | ProjectMutationAttempt::DuplicateScene { result: Err(_), .. }
        | ProjectMutationAttempt::RenameScene { result: Err(_), .. }
        | ProjectMutationAttempt::SetSceneActive { result: Err(_), .. }
        | ProjectMutationAttempt::RestoreResource { result: Err(_), .. } => HistoryEffect::None,
    }
}

fn restore_edit(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    history: &mut UndoHistory,
    redo: bool,
) -> bool {
    let command = if redo {
        IdeWorkerCommandKind::Redo
    } else {
        IdeWorkerCommandKind::Undo
    };
    let Some(entry) = history.peek(redo).cloned() else {
        return publish_failure(events, command, IdeWorkerCommandFailure::NothingToRestore);
    };
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RestoreResource { entry, redo },
    )
}

fn edit_control_mapping(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    edit: IdeControlMappingEdit,
) -> bool {
    let empty_identity = match &edit {
        IdeControlMappingEdit::Insert(_) => false,
        IdeControlMappingEdit::SetEnabled { binding_id, .. }
        | IdeControlMappingEdit::Remove { binding_id } => binding_id.is_empty(),
    };
    if empty_identity {
        return publish_failure(
            events,
            IdeWorkerCommandKind::EditControlMapping,
            IdeWorkerCommandFailure::ControlMappingEdit(Box::new(
                IdeControlMappingEditError::InvalidBindingIdentity,
            )),
        );
    }
    begin_project_mutation_request(state, events, ProjectMutation::EditControlMapping { edit })
}

fn control_mapping_edit_failure_event(reason: IdeControlMappingEditError) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::ControlMappingEditCommittedWithWarning {
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::EditControlMapping,
            reason: IdeWorkerCommandFailure::ControlMappingEdit(Box::new(reason)),
        }
    }
}

fn attempt_control_mapping_edit(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    edit: &IdeControlMappingEdit,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<u64, IdeControlMappingEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeControlMappingEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_control_mapping_edit(application, recovery, edit)
        .and_then(|plan| {
            plan.publish()
                .map_err(|source| IdeControlMappingEditError::Publication(Box::new(source)))
        })
}

fn control_mapping_edit_reopen_failure(
    result: Result<u64, IdeControlMappingEditError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(revision) => IdeControlMappingEditError::ReopenAfterPublication {
            revision,
            source: Box::new(reopen),
        },
        Err(IdeControlMappingEditError::Publication(publication)) => {
            IdeControlMappingEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeControlMappingEditError::NativeClose(close)) => {
            IdeControlMappingEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeControlMappingEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::ControlMappingEdit(Box::new(reason))
}

fn set_automation_take_active(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    take_id: &str,
    enabled: bool,
) -> bool {
    let mut owned_take_id = String::new();
    if take_id.is_empty() || owned_take_id.try_reserve_exact(take_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAutomationTakeActive,
            IdeWorkerCommandFailure::AutomationTakeActivation(Box::new(if take_id.is_empty() {
                IdeAutomationTakeEditError::InvalidTakeIdentity
            } else {
                IdeAutomationTakeEditError::CapacityUnavailable
            })),
        );
    }
    owned_take_id.push_str(take_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAutomationTakeActive {
            take_id: owned_take_id,
            enabled,
        },
    )
}

fn duplicate_scene(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    scene_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    let invalid = if scene_id.is_empty() {
        Some(IdeSceneDuplicationError::InvalidSourceSceneIdentity)
    } else if name.is_empty() || name.chars().count() > 120 {
        Some(IdeSceneDuplicationError::InvalidName)
    } else {
        None
    };
    if let Some(reason) = invalid {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateScene,
            IdeWorkerCommandFailure::SceneDuplication(Box::new(reason)),
        );
    }
    let mut owned_scene_id = String::new();
    let mut owned_name = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err()
        || owned_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::DuplicateScene,
            IdeWorkerCommandFailure::SceneDuplication(Box::new(
                IdeSceneDuplicationError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    owned_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::DuplicateScene {
            scene_id: owned_scene_id,
            name: owned_name,
        },
    )
}

fn set_scene_slot(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    scene_id: &str,
    track_id: &str,
    edit: IdeSceneSlotEdit,
) -> bool {
    let invalid = if scene_id.is_empty() {
        Some(IdeSceneSlotEditError::InvalidSceneIdentity)
    } else if track_id.is_empty() {
        Some(IdeSceneSlotEditError::InvalidTrackIdentity)
    } else if matches!(&edit, IdeSceneSlotEdit::Play { clip_id } if clip_id.is_empty()) {
        Some(IdeSceneSlotEditError::InvalidClipIdentity)
    } else {
        None
    };
    if let Some(reason) = invalid {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetSceneSlot,
            IdeWorkerCommandFailure::SceneSlotEdit(Box::new(reason)),
        );
    }
    let mut owned_scene_id = String::new();
    let mut owned_track_id = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err()
        || owned_track_id.try_reserve_exact(track_id.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetSceneSlot,
            IdeWorkerCommandFailure::SceneSlotEdit(Box::new(
                IdeSceneSlotEditError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    owned_track_id.push_str(track_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetSceneSlot {
            scene_id: owned_scene_id,
            track_id: owned_track_id,
            edit,
        },
    )
}

fn rename_scene(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    scene_id: &str,
    name: &str,
) -> bool {
    let name = name.trim();
    if scene_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameScene,
            IdeWorkerCommandFailure::SceneRename(Box::new(
                IdeSceneRenameError::InvalidSceneIdentity,
            )),
        );
    }
    if name.is_empty() || name.chars().count() > 120 {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameScene,
            IdeWorkerCommandFailure::SceneRename(Box::new(IdeSceneRenameError::InvalidName)),
        );
    }
    let mut owned_scene_id = String::new();
    let mut normalized_name = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err()
        || normalized_name.try_reserve_exact(name.len()).is_err()
    {
        return publish_failure(
            events,
            IdeWorkerCommandKind::RenameScene,
            IdeWorkerCommandFailure::SceneRename(Box::new(
                IdeSceneRenameError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    normalized_name.push_str(name);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::RenameScene {
            scene_id: owned_scene_id,
            name: normalized_name,
        },
    )
}

fn set_scene_active(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    scene_id: &str,
    active: bool,
) -> bool {
    if scene_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetSceneActive,
            IdeWorkerCommandFailure::SceneActivation(Box::new(
                IdeSceneEditError::InvalidSceneIdentity,
            )),
        );
    }
    let mut owned_scene_id = String::new();
    if owned_scene_id.try_reserve_exact(scene_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetSceneActive,
            IdeWorkerCommandFailure::SceneActivation(Box::new(
                IdeSceneEditError::CapacityUnavailable,
            )),
        );
    }
    owned_scene_id.push_str(scene_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetSceneActive {
            scene_id: owned_scene_id,
            active,
        },
    )
}

fn set_audio_clip_gain(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    gain_decibels: f64,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipGain,
            IdeWorkerCommandFailure::AudioClipGain(Box::new(
                IdeAudioClipGainEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipGain,
            IdeWorkerCommandFailure::AudioClipGain(Box::new(
                IdeAudioClipGainEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipGain {
            clip_id: owned_clip_id,
            gain_decibels,
        },
    )
}

fn set_audio_clip_fades(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    fade_in_ticks: u64,
    fade_out_ticks: u64,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipFades,
            IdeWorkerCommandFailure::AudioClipFades(Box::new(
                IdeAudioClipFadeEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipFades,
            IdeWorkerCommandFailure::AudioClipFades(Box::new(
                IdeAudioClipFadeEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipFades {
            clip_id: owned_clip_id,
            fade_in_ticks,
            fade_out_ticks,
        },
    )
}

fn set_audio_clip_loop(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipLoop,
            IdeWorkerCommandFailure::AudioClipLoop(Box::new(
                IdeAudioClipLoopEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipLoop,
            IdeWorkerCommandFailure::AudioClipLoop(Box::new(
                IdeAudioClipLoopEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipLoop {
            clip_id: owned_clip_id,
            enabled,
            start_tick,
            end_tick,
        },
    )
}

fn set_audio_clip_reverse(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    reverse: bool,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipReverse,
            IdeWorkerCommandFailure::AudioClipReverse(Box::new(
                IdeAudioClipReverseEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipReverse,
            IdeWorkerCommandFailure::AudioClipReverse(Box::new(
                IdeAudioClipReverseEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipReverse {
            clip_id: owned_clip_id,
            reverse,
        },
    )
}

fn set_audio_clip_source_start(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    clip_id: &str,
    source_start_frame: u64,
) -> bool {
    if clip_id.is_empty() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipSourceStart,
            IdeWorkerCommandFailure::AudioClipSourceSlip(Box::new(
                IdeAudioClipSourceSlipEditError::InvalidClipIdentity,
            )),
        );
    }
    let mut owned_clip_id = String::new();
    if owned_clip_id.try_reserve_exact(clip_id.len()).is_err() {
        return publish_failure(
            events,
            IdeWorkerCommandKind::SetAudioClipSourceStart,
            IdeWorkerCommandFailure::AudioClipSourceSlip(Box::new(
                IdeAudioClipSourceSlipEditError::CapacityUnavailable,
            )),
        );
    }
    owned_clip_id.push_str(clip_id);
    begin_project_mutation_request(
        state,
        events,
        ProjectMutation::SetAudioClipSourceStart {
            clip_id: owned_clip_id,
            source_start_frame,
        },
    )
}

fn begin_project_mutation_request(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    operation: ProjectMutation,
) -> bool {
    let command = operation.command();
    let WorkerState::Running(application) = state else {
        return publish_failure(events, command, IdeWorkerCommandFailure::InvalidState);
    };
    if application.audio_preview_is_active() {
        return publish_failure(
            events,
            command,
            IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::Command(
                hyper_desktop_audio::DesktopAudioPreviewCommandError::PreviewBusy,
            )),
        );
    }
    if !application.can_save_mixer() {
        return publish_failure(
            events,
            command,
            project_mutation_save_failure(
                &operation,
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ),
        );
    }
    let sequence = match application.try_send_mixer_fence() {
        Ok(Some(sequence)) => sequence,
        Ok(None) => {
            return publish_failure(
                events,
                command,
                project_mutation_save_failure(
                    &operation,
                    IdeProjectSaveError::MixerFenceSequenceUnavailable,
                ),
            );
        }
        Err(source) => {
            return publish_failure(
                events,
                command,
                project_mutation_save_failure(&operation, IdeProjectSaveError::MixerCommit(source)),
            );
        }
    };
    let WorkerState::Running(application) = std::mem::replace(state, WorkerState::Empty) else {
        unreachable!("running state was checked before save ownership transfer");
    };
    *state = WorkerState::SaveFence {
        application,
        operation,
        sequence,
        deadline: Instant::now() + MIXER_SAVE_COMMIT_TIMEOUT,
    };
    let WorkerState::SaveFence { operation, .. } = state else {
        unreachable!();
    };
    send_event(events, operation.pending_event())
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive mutation-to-typed-save-error mapping remains auditable in one match"
)]
fn project_mutation_save_failure(
    operation: &ProjectMutation,
    reason: IdeProjectSaveError,
) -> IdeWorkerCommandFailure {
    match operation {
        ProjectMutation::SaveMixer => IdeWorkerCommandFailure::ProjectSave(reason),
        ProjectMutation::PrepareRuntime { .. } => IdeWorkerCommandFailure::RuntimePreparation(
            Box::new(IdeRuntimePreparationError::Save(reason)),
        ),
        ProjectMutation::AddAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackAddition(
            Box::new(IdeAudioTrackAdditionError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::RemoveAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackRemoval(
            Box::new(IdeAudioTrackRemovalError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::RenameAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackRename(
            Box::new(IdeAudioTrackEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioTrackActive { .. } => {
            IdeWorkerCommandFailure::AudioTrackActivation(Box::new(
                IdeAudioTrackEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::ImportAudio { paths, .. } => IdeWorkerCommandFailure::AudioImport(
            Box::new(IdeAudioImportBatchFailure {
                completed: 0,
                total: paths.len(),
                path: paths.first().cloned().unwrap_or_default(),
                source: Box::new(IdeAudioImportError::MixerSave(Box::new(reason))),
            }),
        ),
        ProjectMutation::CreateAudioClipFromTake { .. } => {
            IdeWorkerCommandFailure::RecordingClipCreation(Box::new(
                IdeRecordingClipCreationError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::DuplicateAudioClip { .. } => {
            IdeWorkerCommandFailure::AudioClipDuplication(Box::new(
                IdeAudioClipDuplicationError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::RemoveAudioClip { .. } => IdeWorkerCommandFailure::AudioClipRemoval(
            Box::new(IdeAudioClipRemovalError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::CleanUnusedSamples { .. } => {
            IdeWorkerCommandFailure::UnusedSampleCleanup(Box::new(
                IdeUnusedSampleCleanupError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::AddArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementAddition(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::AddArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::MoveArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchMove(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SetArrangementLoopRange { .. } => {
            IdeWorkerCommandFailure::ArrangementLoopRange(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SetTempo { .. } | ProjectMutation::SetMeter { .. } => {
            IdeWorkerCommandFailure::Tempo(Box::new(IdeTempoEditError::MixerSave(Box::new(reason))))
        }
        ProjectMutation::SetArrangementSceneLaunchScene { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::RemoveArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::DuplicateArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementDuplication(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SplitArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementSplit(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SetArrangementPlacementActive { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementActivation(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::MoveArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementMove(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SetArrangementPlacementDuration { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementDuration(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::TrimArrangementPlacementFront { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::RemoveArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementRemoval(Box::new(
                IdeArrangementPlacementEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::RenameAudioClip { .. } => IdeWorkerCommandFailure::AudioClipRename(
            Box::new(IdeAudioClipRenameEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipActive { .. } => IdeWorkerCommandFailure::AudioClipActivation(
            Box::new(IdeAudioClipActivationEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipGain { .. } => IdeWorkerCommandFailure::AudioClipGain(
            Box::new(IdeAudioClipGainEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipFades { .. } => IdeWorkerCommandFailure::AudioClipFades(
            Box::new(IdeAudioClipFadeEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipLoop { .. } => IdeWorkerCommandFailure::AudioClipLoop(
            Box::new(IdeAudioClipLoopEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipReverse { .. } => IdeWorkerCommandFailure::AudioClipReverse(
            Box::new(IdeAudioClipReverseEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetAudioClipSourceStart { .. } => {
            IdeWorkerCommandFailure::AudioClipSourceSlip(Box::new(
                IdeAudioClipSourceSlipEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::AddScene { .. } => IdeWorkerCommandFailure::SceneCreation(Box::new(
            IdeSceneCreationError::MixerSave(Box::new(reason)),
        )),
        ProjectMutation::AddAutomationTake { .. } => {
            IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(
                IdeAutomationTakeCreationError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::SetAutomationTakeActive { .. } => {
            IdeWorkerCommandFailure::AutomationTakeActivation(Box::new(
                IdeAutomationTakeEditError::MixerSave(Box::new(reason)),
            ))
        }
        ProjectMutation::EditControlMapping { .. } => IdeWorkerCommandFailure::ControlMappingEdit(
            Box::new(IdeControlMappingEditError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::RestoreResource { .. } => IdeWorkerCommandFailure::EditRestore(Box::new(
            IdeResourceRestoreError::MixerSave(Box::new(reason)),
        )),
        ProjectMutation::DuplicateScene { .. } => IdeWorkerCommandFailure::SceneDuplication(
            Box::new(IdeSceneDuplicationError::MixerSave(Box::new(reason))),
        ),
        ProjectMutation::SetSceneSlot { .. } => IdeWorkerCommandFailure::SceneSlotEdit(Box::new(
            IdeSceneSlotEditError::MixerSave(Box::new(reason)),
        )),
        ProjectMutation::RenameScene { .. } => IdeWorkerCommandFailure::SceneRename(Box::new(
            IdeSceneRenameError::MixerSave(Box::new(reason)),
        )),
        ProjectMutation::SetSceneActive { .. } => IdeWorkerCommandFailure::SceneActivation(
            Box::new(IdeSceneEditError::MixerSave(Box::new(reason))),
        ),
    }
}

fn begin_save_close(
    state: &mut WorkerState,
    application: RunningDesktopIdeApplication,
    operation: ProjectMutation,
    events: &SyncSender<IdeWorkerEvent>,
    history: &mut UndoHistory,
) -> bool {
    match application.close_for_save() {
        IdeSaveCloseOutcome::Closed {
            application,
            recovery,
            failure,
        } => complete_project_mutation(
            state,
            application,
            recovery,
            operation,
            failure,
            events,
            history,
        ),
        IdeSaveCloseOutcome::Pending(pending) => {
            let event = operation.pending_event();
            *state = WorkerState::Saving { pending, operation };
            send_event(events, event)
        }
        IdeSaveCloseOutcome::Unavailable {
            application,
            failure,
        } => {
            let reason = failure.map_or_else(
                || {
                    project_mutation_save_failure(
                        &operation,
                        IdeProjectSaveError::ProjectSnapshotUnavailable,
                    )
                },
                |failure| project_mutation_native_close_failure(&operation, failure),
            );
            publish_closed_project_mutation_failure(
                state,
                application,
                events,
                operation.command(),
                reason,
            )
        }
    }
}

fn publish_closed_project_mutation_failure(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
    reason: IdeWorkerCommandFailure,
) -> bool {
    *state = WorkerState::Closed(application);
    let mut no_native_failure = None;
    if !publish_state(state, events, &mut no_native_failure) {
        return false;
    }
    publish_failure(events, command, reason)
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive mutation-to-typed-close-error mapping remains auditable in one match"
)]
fn project_mutation_native_close_failure(
    operation: &ProjectMutation,
    failure: IdeNativeCloseFailure,
) -> IdeWorkerCommandFailure {
    match operation {
        ProjectMutation::SaveMixer => {
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::NativeClose(failure))
        }
        ProjectMutation::PrepareRuntime { .. } => {
            IdeWorkerCommandFailure::RuntimePreparation(Box::new(IdeRuntimePreparationError::Save(
                IdeProjectSaveError::NativeClose(failure),
            )))
        }
        ProjectMutation::AddAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackAddition(
            Box::new(IdeAudioTrackAdditionError::NativeClose(failure)),
        ),
        ProjectMutation::RemoveAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackRemoval(
            Box::new(IdeAudioTrackRemovalError::NativeClose(failure)),
        ),
        ProjectMutation::RenameAudioTrack { .. } => IdeWorkerCommandFailure::AudioTrackRename(
            Box::new(IdeAudioTrackEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioTrackActive { .. } => {
            IdeWorkerCommandFailure::AudioTrackActivation(Box::new(
                IdeAudioTrackEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::ImportAudio { paths, .. } => IdeWorkerCommandFailure::AudioImport(
            Box::new(IdeAudioImportBatchFailure {
                completed: 0,
                total: paths.len(),
                path: paths.first().cloned().unwrap_or_default(),
                source: Box::new(IdeAudioImportError::NativeClose(failure)),
            }),
        ),
        ProjectMutation::CreateAudioClipFromTake { .. } => {
            IdeWorkerCommandFailure::RecordingClipCreation(Box::new(
                IdeRecordingClipCreationError::NativeClose(failure),
            ))
        }
        ProjectMutation::DuplicateAudioClip { .. } => {
            IdeWorkerCommandFailure::AudioClipDuplication(Box::new(
                IdeAudioClipDuplicationError::NativeClose(failure),
            ))
        }
        ProjectMutation::RemoveAudioClip { .. } => IdeWorkerCommandFailure::AudioClipRemoval(
            Box::new(IdeAudioClipRemovalError::NativeClose(failure)),
        ),
        ProjectMutation::CleanUnusedSamples { .. } => {
            IdeWorkerCommandFailure::UnusedSampleCleanup(Box::new(
                IdeUnusedSampleCleanupError::NativeClose(failure),
            ))
        }
        ProjectMutation::AddArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementAddition(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::AddArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::MoveArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchMove(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::SetArrangementLoopRange { .. } => {
            IdeWorkerCommandFailure::ArrangementLoopRange(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::SetTempo { .. } | ProjectMutation::SetMeter { .. } => {
            IdeWorkerCommandFailure::Tempo(Box::new(IdeTempoEditError::NativeClose(failure)))
        }
        ProjectMutation::SetArrangementSceneLaunchScene { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::RemoveArrangementSceneLaunch { .. } => {
            IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::DuplicateArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementDuplication(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::SplitArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementSplit(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::SetArrangementPlacementActive { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementActivation(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::MoveArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementMove(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::SetArrangementPlacementDuration { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementDuration(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::TrimArrangementPlacementFront { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::RemoveArrangementPlacement { .. } => {
            IdeWorkerCommandFailure::ArrangementPlacementRemoval(Box::new(
                IdeArrangementPlacementEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::RenameAudioClip { .. } => IdeWorkerCommandFailure::AudioClipRename(
            Box::new(IdeAudioClipRenameEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipActive { .. } => IdeWorkerCommandFailure::AudioClipActivation(
            Box::new(IdeAudioClipActivationEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipGain { .. } => IdeWorkerCommandFailure::AudioClipGain(
            Box::new(IdeAudioClipGainEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipFades { .. } => IdeWorkerCommandFailure::AudioClipFades(
            Box::new(IdeAudioClipFadeEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipLoop { .. } => IdeWorkerCommandFailure::AudioClipLoop(
            Box::new(IdeAudioClipLoopEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipReverse { .. } => IdeWorkerCommandFailure::AudioClipReverse(
            Box::new(IdeAudioClipReverseEditError::NativeClose(failure)),
        ),
        ProjectMutation::SetAudioClipSourceStart { .. } => {
            IdeWorkerCommandFailure::AudioClipSourceSlip(Box::new(
                IdeAudioClipSourceSlipEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::AddScene { .. } => IdeWorkerCommandFailure::SceneCreation(Box::new(
            IdeSceneCreationError::NativeClose(failure),
        )),
        ProjectMutation::AddAutomationTake { .. } => {
            IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(
                IdeAutomationTakeCreationError::NativeClose(failure),
            ))
        }
        ProjectMutation::SetAutomationTakeActive { .. } => {
            IdeWorkerCommandFailure::AutomationTakeActivation(Box::new(
                IdeAutomationTakeEditError::NativeClose(failure),
            ))
        }
        ProjectMutation::EditControlMapping { .. } => IdeWorkerCommandFailure::ControlMappingEdit(
            Box::new(IdeControlMappingEditError::NativeClose(failure)),
        ),
        ProjectMutation::RestoreResource { .. } => IdeWorkerCommandFailure::EditRestore(Box::new(
            IdeResourceRestoreError::NativeClose(failure),
        )),
        ProjectMutation::DuplicateScene { .. } => IdeWorkerCommandFailure::SceneDuplication(
            Box::new(IdeSceneDuplicationError::NativeClose(failure)),
        ),
        ProjectMutation::SetSceneSlot { .. } => IdeWorkerCommandFailure::SceneSlotEdit(Box::new(
            IdeSceneSlotEditError::NativeClose(failure),
        )),
        ProjectMutation::RenameScene { .. } => IdeWorkerCommandFailure::SceneRename(Box::new(
            IdeSceneRenameError::NativeClose(failure),
        )),
        ProjectMutation::SetSceneActive { .. } => IdeWorkerCommandFailure::SceneActivation(
            Box::new(IdeSceneEditError::NativeClose(failure)),
        ),
    }
}

pub(super) struct ArrangementPlacementCreationPublication {
    pub(super) outcome: hyper_storage_engine::ProjectResourcePublicationOutcome,
    pub(super) placement_id: String,
}

struct RuntimePreparationAttempt {
    save: Option<hyper_storage_engine::ProjectResourcePublicationOutcome>,
    result: Result<PublishedRuntimeBundle, IdeRuntimePreparationError>,
}

enum ProjectMutationAttempt {
    Save(Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeProjectSaveError>),
    PrepareRuntime(RuntimePreparationAttempt),
    AddAudioTrack(
        Result<hyper_storage_engine::AudioTrackPublicationOutcome, IdeAudioTrackAdditionError>,
    ),
    RemoveAudioTrack(
        Result<hyper_storage_engine::AudioTrackRemovalOutcome, IdeAudioTrackRemovalError>,
    ),
    RenameAudioTrack(
        Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioTrackEditError>,
    ),
    SetAudioTrackActive {
        active: bool,
        result:
            Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioTrackEditError>,
    },
    ImportAudio(Result<IdeAudioImportBatchOutcome, IdeAudioImportBatchFailure>),
    CreateAudioClipFromTake(
        Result<hyper_storage_engine::AudioClipPublicationOutcome, IdeRecordingClipCreationError>,
    ),
    DuplicateAudioClip {
        name: String,
        result:
            Result<hyper_storage_engine::AudioClipPublicationOutcome, IdeAudioClipDuplicationError>,
    },
    RemoveAudioClip(
        Result<hyper_storage_engine::AudioClipRemovalOutcome, IdeAudioClipRemovalError>,
    ),
    CleanUnusedSamples(
        Result<hyper_storage_engine::SampleCleanupOutcome, IdeUnusedSampleCleanupError>,
    ),
    AddArrangementPlacement {
        start_tick: u64,
        result: Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError>,
    },
    AddArrangementSceneLaunch {
        scene_id: String,
        tick: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    SetMeter {
        numerator: u16,
        denominator: u16,
        result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeTempoEditError>,
    },
    SetTempo {
        microseconds_per_quarter: u32,
        result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeTempoEditError>,
    },
    SetArrangementLoopRange {
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    MoveArrangementSceneLaunch {
        launch_id: String,
        tick: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    SetArrangementSceneLaunchScene {
        launch_id: String,
        scene_id: String,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    RemoveArrangementSceneLaunch {
        launch_id: String,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    DuplicateArrangementPlacement {
        start_tick: u64,
        result: Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError>,
    },
    SplitArrangementPlacement {
        split_tick: u64,
        result: Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError>,
    },
    SetArrangementPlacementActive {
        active: bool,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    SetSceneSlot {
        edit: IdeSceneSlotEdit,
        result:
            Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneSlotEditError>,
    },
    MoveArrangementPlacement {
        start_tick: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    SetArrangementPlacementDuration {
        duration_ticks: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    TrimArrangementPlacementFront {
        start_tick: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    },
    RemoveArrangementPlacement(
        Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeArrangementPlacementEditError,
        >,
    ),
    RenameAudioClip {
        name: String,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipRenameEditError,
        >,
    },
    SetAudioClipActive {
        active: bool,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipActivationEditError,
        >,
    },
    SetAudioClipGain {
        gain_decibels: f64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipGainEditError,
        >,
    },
    SetAudioClipFades {
        fade_in_ticks: u64,
        fade_out_ticks: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipFadeEditError,
        >,
    },
    SetAudioClipLoop {
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipLoopEditError,
        >,
    },
    SetAudioClipReverse {
        reverse: bool,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipReverseEditError,
        >,
    },
    SetAudioClipSourceStart {
        source_start_frame: u64,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAudioClipSourceSlipEditError,
        >,
    },
    AddScene {
        name: String,
        result:
            Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneCreationError>,
    },
    AddAutomationTake {
        name: String,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAutomationTakeCreationError,
        >,
    },
    SetAutomationTakeActive {
        enabled: bool,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeAutomationTakeEditError,
        >,
    },
    EditControlMapping {
        edit: IdeControlMappingEdit,
        result: Result<u64, IdeControlMappingEditError>,
    },
    RestoreResource {
        entry: UndoEntry,
        redo: bool,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeResourceRestoreError,
        >,
    },
    DuplicateScene {
        name: String,
        result: Result<
            hyper_storage_engine::ProjectResourcePublicationOutcome,
            IdeSceneDuplicationError,
        >,
    },
    RenameScene {
        name: String,
        result:
            Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneRenameError>,
    },
    SetSceneActive {
        active: bool,
        result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneEditError>,
    },
}

fn publish_arrangement_edit_failure(
    events: &SyncSender<IdeWorkerEvent>,
    command: IdeWorkerCommandKind,
    reason: IdeArrangementPlacementEditError,
    failure: fn(Box<IdeArrangementPlacementEditError>) -> IdeWorkerCommandFailure,
) -> bool {
    send_event(
        events,
        arrangement_edit_failure_event(command, reason, failure),
    )
}

fn audio_clip_edit_failure_event<E>(
    command: IdeWorkerCommandKind,
    reason: E,
    publication_committed: fn(&E) -> bool,
    warning: fn(Box<E>) -> IdeAudioClipEditCommittedWarning,
    failure: fn(Box<E>) -> IdeWorkerCommandFailure,
) -> IdeWorkerEvent {
    if publication_committed(&reason) {
        IdeWorkerEvent::AudioClipEditCommittedWithWarning {
            command,
            warning: warning(Box::new(reason)),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command,
            reason: failure(Box::new(reason)),
        }
    }
}

pub(super) fn audio_clip_removal_failure_event(
    reason: IdeAudioClipRemovalError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::AudioClipRemovalCommittedWithWarning {
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::RemoveAudioClip,
            reason: IdeWorkerCommandFailure::AudioClipRemoval(Box::new(reason)),
        }
    }
}

pub(super) fn audio_clip_rename_edit_failure_event(
    reason: IdeAudioClipRenameEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::RenameAudioClip,
        reason,
        IdeAudioClipRenameEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Rename,
        IdeWorkerCommandFailure::AudioClipRename,
    )
}

pub(super) fn audio_clip_activation_edit_failure_event(
    reason: IdeAudioClipActivationEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipActive,
        reason,
        IdeAudioClipActivationEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Activation,
        IdeWorkerCommandFailure::AudioClipActivation,
    )
}

pub(super) fn audio_clip_gain_edit_failure_event(
    reason: IdeAudioClipGainEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipGain,
        reason,
        IdeAudioClipGainEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Gain,
        IdeWorkerCommandFailure::AudioClipGain,
    )
}

pub(super) fn audio_clip_fade_edit_failure_event(
    reason: IdeAudioClipFadeEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipFades,
        reason,
        IdeAudioClipFadeEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Fades,
        IdeWorkerCommandFailure::AudioClipFades,
    )
}

pub(super) fn audio_clip_loop_edit_failure_event(
    reason: IdeAudioClipLoopEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipLoop,
        reason,
        IdeAudioClipLoopEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Loop,
        IdeWorkerCommandFailure::AudioClipLoop,
    )
}

pub(super) fn audio_clip_reverse_edit_failure_event(
    reason: IdeAudioClipReverseEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipReverse,
        reason,
        IdeAudioClipReverseEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::Reverse,
        IdeWorkerCommandFailure::AudioClipReverse,
    )
}

pub(super) fn audio_clip_source_slip_edit_failure_event(
    reason: IdeAudioClipSourceSlipEditError,
) -> IdeWorkerEvent {
    audio_clip_edit_failure_event(
        IdeWorkerCommandKind::SetAudioClipSourceStart,
        reason,
        IdeAudioClipSourceSlipEditError::publication_committed,
        IdeAudioClipEditCommittedWarning::SourceSlip,
        IdeWorkerCommandFailure::AudioClipSourceSlip,
    )
}

pub(super) fn scene_rename_failure_event(
    name: String,
    reason: IdeSceneRenameError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::SceneRenameCommittedWithWarning {
            name,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::RenameScene,
            reason: IdeWorkerCommandFailure::SceneRename(Box::new(reason)),
        }
    }
}

pub(super) fn scene_creation_failure_event(
    name: String,
    reason: IdeSceneCreationError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::SceneCreationCommittedWithWarning {
            name,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::AddScene,
            reason: IdeWorkerCommandFailure::SceneCreation(Box::new(reason)),
        }
    }
}

fn automation_take_activation_failure_event(reason: IdeAutomationTakeEditError) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::AutomationTakeActivationCommittedWithWarning {
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::SetAutomationTakeActive,
            reason: IdeWorkerCommandFailure::AutomationTakeActivation(Box::new(reason)),
        }
    }
}

fn automation_take_creation_failure_event(
    name: String,
    reason: IdeAutomationTakeCreationError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::AutomationTakeCreationCommittedWithWarning {
            name,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::AddAutomationTake,
            reason: IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(reason)),
        }
    }
}

pub(super) fn scene_duplication_failure_event(
    name: String,
    reason: IdeSceneDuplicationError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::SceneDuplicationCommittedWithWarning {
            name,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::DuplicateScene,
            reason: IdeWorkerCommandFailure::SceneDuplication(Box::new(reason)),
        }
    }
}

fn scene_slot_edit_failure_event(
    edit: IdeSceneSlotEdit,
    reason: IdeSceneSlotEditError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::SceneSlotEditCommittedWithWarning {
            edit,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::SetSceneSlot,
            reason: IdeWorkerCommandFailure::SceneSlotEdit(Box::new(reason)),
        }
    }
}

pub(super) fn scene_activation_failure_event(
    active: bool,
    reason: IdeSceneEditError,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::SceneActivationCommittedWithWarning {
            active,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::SetSceneActive,
            reason: IdeWorkerCommandFailure::SceneActivation(Box::new(reason)),
        }
    }
}

pub(super) fn arrangement_edit_failure_event(
    command: IdeWorkerCommandKind,
    reason: IdeArrangementPlacementEditError,
    failure: fn(Box<IdeArrangementPlacementEditError>) -> IdeWorkerCommandFailure,
) -> IdeWorkerEvent {
    if reason.publication_committed() {
        IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
            command,
            warning: Box::new(reason),
        }
    } else {
        IdeWorkerEvent::CommandFailed {
            command,
            reason: failure(Box::new(reason)),
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "all mutation completion events remain exhaustively paired with reopen publication"
)]
fn complete_project_mutation(
    state: &mut WorkerState,
    application: DesktopIdeApplication,
    recovery: DesktopPlaybackRecovery,
    operation: ProjectMutation,
    native_failure: Option<IdeNativeCloseFailure>,
    events: &SyncSender<IdeWorkerEvent>,
    history: &mut UndoHistory,
) -> bool {
    let command = operation.command();
    let Some(project_path) = recovery
        .prepared_project()
        .map(|project| project.package().root().to_path_buf())
    else {
        return publish_closed_project_mutation_failure(
            state,
            application,
            events,
            command,
            project_mutation_save_failure(
                &operation,
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ),
        );
    };
    let attempt = attempt_project_mutation(&application, &recovery, operation, native_failure);
    drop(recovery);
    observe_history(history, &attempt, command, events);
    match application.open_project(&project_path) {
        Ok(running) => {
            *state = WorkerState::Running(running);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            match attempt {
                ProjectMutationAttempt::Save(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::ProjectSaved {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                    },
                ),
                ProjectMutationAttempt::Save(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::SaveProject,
                    IdeWorkerCommandFailure::ProjectSave(reason),
                ),
                ProjectMutationAttempt::PrepareRuntime(attempt) => match attempt.result {
                    Ok(bundle) => send_event(events, IdeWorkerEvent::RuntimePrepared { bundle }),
                    Err(reason) => publish_failure(
                        events,
                        IdeWorkerCommandKind::PrepareRuntime,
                        IdeWorkerCommandFailure::RuntimePreparation(Box::new(reason)),
                    ),
                },
                ProjectMutationAttempt::AddAudioTrack(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::AudioTrackAdded {
                        revision: outcome.project_revision(),
                    },
                ),
                ProjectMutationAttempt::AddAudioTrack(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::AddAudioTrack,
                    IdeWorkerCommandFailure::AudioTrackAddition(Box::new(reason)),
                ),
                ProjectMutationAttempt::RemoveAudioTrack(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::AudioTrackRemoved {
                        revision: outcome.project_revision(),
                    },
                ),
                ProjectMutationAttempt::RemoveAudioTrack(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::RemoveAudioTrack,
                    IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(reason)),
                ),
                ProjectMutationAttempt::RenameAudioTrack(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::AudioTrackRenamed {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                    },
                ),
                ProjectMutationAttempt::RenameAudioTrack(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::RenameAudioTrack,
                    IdeWorkerCommandFailure::AudioTrackRename(Box::new(reason)),
                ),
                ProjectMutationAttempt::SetAudioTrackActive {
                    active,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioTrackActivationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        active,
                    },
                ),
                ProjectMutationAttempt::SetAudioTrackActive {
                    result: Err(reason),
                    ..
                } => publish_failure(
                    events,
                    IdeWorkerCommandKind::SetAudioTrackActive,
                    IdeWorkerCommandFailure::AudioTrackActivation(Box::new(reason)),
                ),
                ProjectMutationAttempt::ImportAudio(Ok(outcome)) => {
                    send_event(events, IdeWorkerEvent::AudioImported(outcome))
                }
                ProjectMutationAttempt::ImportAudio(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::ImportAudio,
                    IdeWorkerCommandFailure::AudioImport(Box::new(reason)),
                ),
                ProjectMutationAttempt::CreateAudioClipFromTake(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::AudioClipCreatedFromTake {
                        revision: outcome.project_revision(),
                        clip_id: outcome.clip_id().to_owned(),
                    },
                ),
                ProjectMutationAttempt::CreateAudioClipFromTake(Err(reason)) => publish_failure(
                    events,
                    IdeWorkerCommandKind::CreateAudioClipFromTake,
                    IdeWorkerCommandFailure::RecordingClipCreation(Box::new(reason)),
                ),
                ProjectMutationAttempt::DuplicateAudioClip {
                    name,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipDuplicated {
                        revision: outcome.project_revision(),
                        clip_id: outcome.clip_id().to_owned(),
                        name,
                    },
                ),
                ProjectMutationAttempt::DuplicateAudioClip {
                    name,
                    result: Err(reason),
                } if reason.committed_clip_id().is_some() => send_event(
                    events,
                    IdeWorkerEvent::AudioClipDuplicationCommittedWithWarning {
                        name,
                        warning: Box::new(reason),
                    },
                ),
                ProjectMutationAttempt::DuplicateAudioClip {
                    result: Err(reason),
                    ..
                } => publish_failure(
                    events,
                    IdeWorkerCommandKind::DuplicateAudioClip,
                    IdeWorkerCommandFailure::AudioClipDuplication(Box::new(reason)),
                ),
                ProjectMutationAttempt::RemoveAudioClip(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::AudioClipRemoved {
                        revision: outcome.project_revision(),
                        clip_id: outcome.clip_id().to_owned(),
                    },
                ),
                ProjectMutationAttempt::RemoveAudioClip(Err(reason)) => {
                    send_event(events, audio_clip_removal_failure_event(reason))
                }
                ProjectMutationAttempt::CleanUnusedSamples(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::UnusedSamplesCleaned {
                        revision: outcome.project_revision(),
                        assets: outcome.removed_assets(),
                        bytes: outcome.removed_bytes(),
                    },
                ),
                ProjectMutationAttempt::CleanUnusedSamples(Err(reason)) => {
                    if reason.publication_committed() {
                        send_event(
                            events,
                            IdeWorkerEvent::UnusedSampleCleanupCommittedWithWarning {
                                warning: Box::new(reason),
                            },
                        )
                    } else {
                        publish_failure(
                            events,
                            IdeWorkerCommandKind::CleanUnusedSamples,
                            IdeWorkerCommandFailure::UnusedSampleCleanup(Box::new(reason)),
                        )
                    }
                }
                ProjectMutationAttempt::AddArrangementPlacement {
                    start_tick,
                    result: Ok(publication),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementAdded {
                        revision: publication.outcome.project_revision(),
                        start_tick,
                    },
                ),
                ProjectMutationAttempt::AddArrangementPlacement {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::AddArrangementPlacement,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementAddition,
                ),
                ProjectMutationAttempt::AddArrangementSceneLaunch {
                    scene_id,
                    tick,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementSceneLaunchAdded {
                        revision: outcome.project_revision(),
                        scene_id,
                        tick,
                    },
                ),
                ProjectMutationAttempt::AddArrangementSceneLaunch {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::AddArrangementSceneLaunch,
                    reason,
                    IdeWorkerCommandFailure::ArrangementSceneLaunchAddition,
                ),
                ProjectMutationAttempt::SetMeter {
                    numerator,
                    denominator,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::MeterChanged {
                        revision: outcome.project_revision(),
                        numerator,
                        denominator,
                    },
                ),
                ProjectMutationAttempt::SetMeter {
                    result: Err(reason),
                    ..
                } => send_event(
                    events,
                    if reason.publication_committed() {
                        IdeWorkerEvent::MeterCommittedWithWarning {
                            warning: Box::new(reason),
                        }
                    } else {
                        IdeWorkerEvent::CommandFailed {
                            command: IdeWorkerCommandKind::SetMeter,
                            reason: IdeWorkerCommandFailure::Tempo(Box::new(reason)),
                        }
                    },
                ),
                ProjectMutationAttempt::SetTempo {
                    microseconds_per_quarter,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::TempoChanged {
                        revision: outcome.project_revision(),
                        microseconds_per_quarter,
                    },
                ),
                ProjectMutationAttempt::SetTempo {
                    result: Err(reason),
                    ..
                } => send_event(
                    events,
                    if reason.publication_committed() {
                        IdeWorkerEvent::TempoCommittedWithWarning {
                            warning: Box::new(reason),
                        }
                    } else {
                        IdeWorkerEvent::CommandFailed {
                            command: IdeWorkerCommandKind::SetTempo,
                            reason: IdeWorkerCommandFailure::Tempo(Box::new(reason)),
                        }
                    },
                ),
                ProjectMutationAttempt::SetArrangementLoopRange {
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementLoopRangeChanged {
                        revision: outcome.project_revision(),
                    },
                ),
                ProjectMutationAttempt::SetArrangementLoopRange {
                    result: Err(reason),
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::SetArrangementLoopRange,
                    reason,
                    IdeWorkerCommandFailure::ArrangementLoopRange,
                ),
                ProjectMutationAttempt::MoveArrangementSceneLaunch {
                    launch_id,
                    tick,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementSceneLaunchMoved {
                        revision: outcome.project_revision(),
                        launch_id,
                        tick,
                    },
                ),
                ProjectMutationAttempt::MoveArrangementSceneLaunch {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                    reason,
                    IdeWorkerCommandFailure::ArrangementSceneLaunchMove,
                ),
                ProjectMutationAttempt::SetArrangementSceneLaunchScene {
                    launch_id,
                    scene_id,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementSceneLaunchSceneChanged {
                        revision: outcome.project_revision(),
                        launch_id,
                        scene_id,
                    },
                ),
                ProjectMutationAttempt::SetArrangementSceneLaunchScene {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                    reason,
                    IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit,
                ),
                ProjectMutationAttempt::RemoveArrangementSceneLaunch {
                    launch_id,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementSceneLaunchRemoved {
                        revision: outcome.project_revision(),
                        launch_id,
                    },
                ),
                ProjectMutationAttempt::RemoveArrangementSceneLaunch {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                    reason,
                    IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval,
                ),
                ProjectMutationAttempt::DuplicateArrangementPlacement {
                    start_tick,
                    result: Ok(publication),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementDuplicated {
                        revision: publication.outcome.project_revision(),
                        placement_id: publication.placement_id,
                        start_tick,
                    },
                ),
                ProjectMutationAttempt::DuplicateArrangementPlacement {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::DuplicateArrangementPlacement,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementDuplication,
                ),
                ProjectMutationAttempt::SplitArrangementPlacement {
                    split_tick,
                    result: Ok(publication),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementSplit {
                        revision: publication.outcome.project_revision(),
                        placement_id: publication.placement_id,
                        split_tick,
                    },
                ),
                ProjectMutationAttempt::SplitArrangementPlacement {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::SplitArrangementPlacement,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementSplit,
                ),
                ProjectMutationAttempt::SetArrangementPlacementActive {
                    active,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementActivationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        active,
                    },
                ),
                ProjectMutationAttempt::SetArrangementPlacementActive {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::SetArrangementPlacementActive,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementActivation,
                ),
                ProjectMutationAttempt::MoveArrangementPlacement {
                    start_tick,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementMoved {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        start_tick,
                    },
                ),
                ProjectMutationAttempt::MoveArrangementPlacement {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::MoveArrangementPlacement,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementMove,
                ),
                ProjectMutationAttempt::SetArrangementPlacementDuration {
                    duration_ticks,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementDurationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        duration_ticks,
                    },
                ),
                ProjectMutationAttempt::SetArrangementPlacementDuration {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::SetArrangementPlacementDuration,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementDuration,
                ),
                ProjectMutationAttempt::TrimArrangementPlacementFront {
                    start_tick,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementFrontTrimmed {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        start_tick,
                    },
                ),
                ProjectMutationAttempt::TrimArrangementPlacementFront {
                    result: Err(reason),
                    ..
                } => publish_arrangement_edit_failure(
                    events,
                    IdeWorkerCommandKind::TrimArrangementPlacementFront,
                    reason,
                    IdeWorkerCommandFailure::ArrangementPlacementFrontTrim,
                ),
                ProjectMutationAttempt::RemoveArrangementPlacement(Ok(outcome)) => send_event(
                    events,
                    IdeWorkerEvent::ArrangementPlacementRemoved {
                        revision: outcome.project_revision(),
                    },
                ),
                ProjectMutationAttempt::RemoveArrangementPlacement(Err(reason)) => {
                    publish_arrangement_edit_failure(
                        events,
                        IdeWorkerCommandKind::RemoveArrangementPlacement,
                        reason,
                        IdeWorkerCommandFailure::ArrangementPlacementRemoval,
                    )
                }
                ProjectMutationAttempt::RenameAudioClip {
                    name,
                    result: Ok(outcome),
                } => send_event(events, audio_clip_rename_success_event(name, &outcome)),
                ProjectMutationAttempt::RenameAudioClip {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_rename_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipActive {
                    active,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipActivationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        active,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipActive {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_activation_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipGain {
                    gain_decibels,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipGainChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        gain_decibels,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipGain {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_gain_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipFades {
                    fade_in_ticks,
                    fade_out_ticks,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipFadesChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        fade_in_ticks,
                        fade_out_ticks,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipFades {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_fade_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipLoop {
                    enabled,
                    start_tick,
                    end_tick,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipLoopChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        enabled,
                        start_tick,
                        end_tick,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipLoop {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_loop_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipReverse {
                    reverse,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipReverseChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        reverse,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipReverse {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_reverse_edit_failure_event(reason)),
                ProjectMutationAttempt::SetAudioClipSourceStart {
                    source_start_frame,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AudioClipSourceStartChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        source_start_frame,
                    },
                ),
                ProjectMutationAttempt::SetAudioClipSourceStart {
                    result: Err(reason),
                    ..
                } => send_event(events, audio_clip_source_slip_edit_failure_event(reason)),
                ProjectMutationAttempt::AddScene {
                    name,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::SceneAdded {
                        revision: outcome.project_revision(),
                        scene_id: outcome.resource_id().to_owned(),
                        name,
                    },
                ),
                ProjectMutationAttempt::AddScene {
                    name,
                    result: Err(reason),
                } => send_event(events, scene_creation_failure_event(name, reason)),
                ProjectMutationAttempt::AddAutomationTake {
                    name,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AutomationTakeAdded {
                        revision: outcome.project_revision(),
                        take_id: outcome.resource_id().to_owned(),
                        name,
                    },
                ),
                ProjectMutationAttempt::AddAutomationTake {
                    name,
                    result: Err(reason),
                } => send_event(events, automation_take_creation_failure_event(name, reason)),
                ProjectMutationAttempt::SetAutomationTakeActive {
                    enabled,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::AutomationTakeActivationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        enabled,
                    },
                ),
                ProjectMutationAttempt::SetAutomationTakeActive {
                    result: Err(reason),
                    ..
                } => send_event(events, automation_take_activation_failure_event(reason)),
                ProjectMutationAttempt::EditControlMapping {
                    edit,
                    result: Ok(revision),
                } => send_event(
                    events,
                    IdeWorkerEvent::ControlMappingEdited { revision, edit },
                ),
                ProjectMutationAttempt::EditControlMapping {
                    result: Err(reason),
                    ..
                } => send_event(events, control_mapping_edit_failure_event(reason)),
                ProjectMutationAttempt::RestoreResource {
                    entry,
                    redo,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::EditRestored {
                        revision: outcome.project_revision(),
                        redo,
                        label: entry.label,
                    },
                ),
                ProjectMutationAttempt::RestoreResource {
                    entry,
                    redo,
                    result: Err(reason),
                } => {
                    let (event, consumed) =
                        restore_failure_event(&mut *history, &entry, redo, reason);
                    (!consumed || send_event(events, history.state_event()))
                        && send_event(events, event)
                }
                ProjectMutationAttempt::DuplicateScene {
                    name,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::SceneDuplicated {
                        revision: outcome.project_revision(),
                        scene_id: outcome.resource_id().to_owned(),
                        name,
                    },
                ),
                ProjectMutationAttempt::DuplicateScene {
                    name,
                    result: Err(reason),
                } => send_event(events, scene_duplication_failure_event(name, reason)),
                ProjectMutationAttempt::SetSceneSlot {
                    edit,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::SceneSlotChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        edit,
                    },
                ),
                ProjectMutationAttempt::SetSceneSlot {
                    edit,
                    result: Err(reason),
                } => send_event(events, scene_slot_edit_failure_event(edit, reason)),
                ProjectMutationAttempt::RenameScene {
                    name,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::SceneRenamed {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        name,
                    },
                ),
                ProjectMutationAttempt::RenameScene {
                    name,
                    result: Err(reason),
                } => send_event(events, scene_rename_failure_event(name, reason)),
                ProjectMutationAttempt::SetSceneActive {
                    active,
                    result: Ok(outcome),
                } => send_event(
                    events,
                    IdeWorkerEvent::SceneActivationChanged {
                        revision: outcome.project_revision(),
                        changed: outcome.changed(),
                        active,
                    },
                ),
                ProjectMutationAttempt::SetSceneActive {
                    active,
                    result: Err(reason),
                } => send_event(events, scene_activation_failure_event(active, reason)),
            }
        }
        Err(failure) => {
            let (application, reopen) = failure.into_parts();
            *state = WorkerState::Closed(application);
            let mut no_native_failure = None;
            if !publish_state(state, events, &mut no_native_failure) {
                return false;
            }
            let reason = project_mutation_reopen_failure(attempt, reopen);
            publish_failure(events, command, reason)
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive dispatcher keeps every mutation paired with its typed preparation and publication outcome"
)]
fn attempt_project_mutation(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    operation: ProjectMutation,
    native_failure: Option<IdeNativeCloseFailure>,
) -> ProjectMutationAttempt {
    match operation {
        ProjectMutation::SaveMixer => {
            ProjectMutationAttempt::Save(if let Some(failure) = native_failure {
                Err(IdeProjectSaveError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_mixer_save(application, recovery).and_then(
                    |plan| {
                        plan.publish()
                            .map_err(|source| IdeProjectSaveError::Publication(Box::new(source)))
                    },
                )
            })
        }
        ProjectMutation::PrepareRuntime { output_path } => {
            if let Some(failure) = native_failure {
                return ProjectMutationAttempt::PrepareRuntime(RuntimePreparationAttempt {
                    save: None,
                    result: Err(IdeRuntimePreparationError::Save(
                        IdeProjectSaveError::NativeClose(failure),
                    )),
                });
            }
            let save = RunningDesktopIdeApplication::prepare_mixer_save(application, recovery)
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeProjectSaveError::Publication(Box::new(source)))
                });
            let save = match save {
                Ok(save) => save,
                Err(source) => {
                    return ProjectMutationAttempt::PrepareRuntime(RuntimePreparationAttempt {
                        save: None,
                        result: Err(IdeRuntimePreparationError::Save(source)),
                    });
                }
            };
            let result = recovery
                .prepared_project()
                .ok_or(IdeProjectSaveError::ProjectSnapshotUnavailable)
                .map_err(IdeRuntimePreparationError::Save)
                .and_then(|project| {
                    build_project_runtime_bundle(
                        project.package().root(),
                        output_path,
                        env!("CARGO_PKG_VERSION"),
                        DEFAULT_MAXIMUM_RUNTIME_BUNDLE_BYTES,
                        application.config.project_limits(),
                    )
                    .map_err(IdeRuntimePreparationError::Bundle)
                });
            ProjectMutationAttempt::PrepareRuntime(RuntimePreparationAttempt {
                save: Some(save),
                result,
            })
        }
        ProjectMutation::AddAudioTrack { name } => {
            ProjectMutationAttempt::AddAudioTrack(if let Some(failure) = native_failure {
                Err(IdeAudioTrackAdditionError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_audio_track_addition(
                    application,
                    recovery,
                    &name,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeAudioTrackAdditionError::Publication(Box::new(source)))
                })
            })
        }
        ProjectMutation::RemoveAudioTrack { track_id } => {
            ProjectMutationAttempt::RemoveAudioTrack(if let Some(failure) = native_failure {
                Err(IdeAudioTrackRemovalError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_audio_track_removal(
                    application,
                    recovery,
                    &track_id,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeAudioTrackRemovalError::Publication(Box::new(source)))
                })
            })
        }
        ProjectMutation::RenameAudioTrack { track_id, name } => {
            ProjectMutationAttempt::RenameAudioTrack(if let Some(failure) = native_failure {
                Err(IdeAudioTrackEditError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_audio_track_rename(
                    application,
                    recovery,
                    &track_id,
                    &name,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeAudioTrackEditError::Publication(Box::new(source)))
                })
            })
        }
        ProjectMutation::SetAudioTrackActive { track_id, active } => {
            ProjectMutationAttempt::SetAudioTrackActive {
                active,
                result: if let Some(failure) = native_failure {
                    Err(IdeAudioTrackEditError::NativeClose(failure))
                } else {
                    RunningDesktopIdeApplication::prepare_audio_track_activation(
                        application,
                        recovery,
                        &track_id,
                        active,
                    )
                    .and_then(|plan| {
                        plan.publish()
                            .map_err(|source| IdeAudioTrackEditError::Publication(Box::new(source)))
                    })
                },
            }
        }
        ProjectMutation::ImportAudio { paths, track_id } => {
            ProjectMutationAttempt::ImportAudio(if let Some(failure) = native_failure {
                let total = paths.len();
                Err(IdeAudioImportBatchFailure {
                    completed: 0,
                    total,
                    path: paths.first().cloned().unwrap_or_default(),
                    source: Box::new(IdeAudioImportError::NativeClose(failure)),
                })
            } else {
                RunningDesktopIdeApplication::import_audio_files(
                    application,
                    recovery,
                    &paths,
                    &track_id,
                )
            })
        }
        ProjectMutation::CreateAudioClipFromTake { recording_id, name } => {
            ProjectMutationAttempt::CreateAudioClipFromTake(if let Some(failure) = native_failure {
                Err(IdeRecordingClipCreationError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_recording_clip_creation(
                    application,
                    recovery,
                    &recording_id,
                    &name,
                )
                .and_then(|plan| {
                    plan.publish().map_err(|source| {
                        IdeRecordingClipCreationError::Publication(Box::new(source))
                    })
                })
            })
        }
        ProjectMutation::DuplicateAudioClip { clip_id, name } => {
            ProjectMutationAttempt::DuplicateAudioClip {
                name: name.clone(),
                result: if let Some(failure) = native_failure {
                    Err(IdeAudioClipDuplicationError::NativeClose(failure))
                } else {
                    RunningDesktopIdeApplication::prepare_audio_clip_duplication(
                        application,
                        recovery,
                        &clip_id,
                        &name,
                    )
                    .and_then(publish_audio_clip_duplication_plan)
                },
            }
        }
        ProjectMutation::RemoveAudioClip { clip_id } => {
            ProjectMutationAttempt::RemoveAudioClip(if let Some(failure) = native_failure {
                Err(IdeAudioClipRemovalError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_audio_clip_removal(
                    application,
                    recovery,
                    &clip_id,
                )
                .and_then(|plan| plan.publish())
            })
        }
        ProjectMutation::CleanUnusedSamples { confirmation } => {
            ProjectMutationAttempt::CleanUnusedSamples(if let Some(failure) = native_failure {
                Err(IdeUnusedSampleCleanupError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_unused_sample_cleanup(
                    application,
                    recovery,
                    confirmation,
                )
                .and_then(|plan| plan.publish())
            })
        }
        ProjectMutation::AddArrangementPlacement {
            clip_id,
            start_tick,
        } => ProjectMutationAttempt::AddArrangementPlacement {
            start_tick,
            result: attempt_arrangement_placement_addition(
                application,
                recovery,
                &clip_id,
                start_tick,
                native_failure,
            ),
        },
        ProjectMutation::AddArrangementSceneLaunch { scene_id, tick } => {
            ProjectMutationAttempt::AddArrangementSceneLaunch {
                scene_id: scene_id.clone(),
                tick,
                result: attempt_arrangement_scene_launch_addition(
                    application,
                    recovery,
                    &scene_id,
                    tick,
                    native_failure,
                ),
            }
        }
        ProjectMutation::SetMeter {
            numerator,
            denominator,
        } => ProjectMutationAttempt::SetMeter {
            numerator,
            denominator,
            result: if let Some(failure) = native_failure {
                Err(IdeTempoEditError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_timeline_meter(
                    application,
                    recovery,
                    numerator,
                    denominator,
                )
                .and_then(|plan| plan.publish())
            },
        },
        ProjectMutation::SetTempo {
            microseconds_per_quarter,
        } => ProjectMutationAttempt::SetTempo {
            microseconds_per_quarter,
            result: if let Some(failure) = native_failure {
                Err(IdeTempoEditError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_timeline_tempo(
                    application,
                    recovery,
                    microseconds_per_quarter,
                )
                .and_then(|plan| plan.publish())
            },
        },
        ProjectMutation::SetArrangementLoopRange { range } => {
            ProjectMutationAttempt::SetArrangementLoopRange {
                result: attempt_arrangement_loop_range(
                    application,
                    recovery,
                    range,
                    native_failure,
                ),
            }
        }
        ProjectMutation::MoveArrangementSceneLaunch { launch_id, tick } => {
            ProjectMutationAttempt::MoveArrangementSceneLaunch {
                launch_id: launch_id.clone(),
                tick,
                result: attempt_arrangement_scene_launch_move(
                    application,
                    recovery,
                    &launch_id,
                    tick,
                    native_failure,
                ),
            }
        }
        ProjectMutation::SetArrangementSceneLaunchScene {
            launch_id,
            scene_id,
        } => ProjectMutationAttempt::SetArrangementSceneLaunchScene {
            launch_id: launch_id.clone(),
            scene_id: scene_id.clone(),
            result: attempt_arrangement_scene_launch_scene_edit(
                application,
                recovery,
                &launch_id,
                &scene_id,
                native_failure,
            ),
        },
        ProjectMutation::RemoveArrangementSceneLaunch { launch_id } => {
            ProjectMutationAttempt::RemoveArrangementSceneLaunch {
                launch_id: launch_id.clone(),
                result: attempt_arrangement_scene_launch_removal(
                    application,
                    recovery,
                    &launch_id,
                    native_failure,
                ),
            }
        }
        ProjectMutation::DuplicateArrangementPlacement {
            placement_id,
            start_tick,
        } => ProjectMutationAttempt::DuplicateArrangementPlacement {
            start_tick,
            result: attempt_arrangement_placement_duplication(
                application,
                recovery,
                &placement_id,
                start_tick,
                native_failure,
            ),
        },
        ProjectMutation::SplitArrangementPlacement {
            placement_id,
            split_tick,
        } => ProjectMutationAttempt::SplitArrangementPlacement {
            split_tick,
            result: attempt_arrangement_placement_split(
                application,
                recovery,
                &placement_id,
                split_tick,
                native_failure,
            ),
        },
        ProjectMutation::SetArrangementPlacementActive {
            placement_id,
            active,
        } => ProjectMutationAttempt::SetArrangementPlacementActive {
            active,
            result: attempt_arrangement_placement_activation(
                application,
                recovery,
                &placement_id,
                active,
                native_failure,
            ),
        },
        ProjectMutation::MoveArrangementPlacement {
            placement_id,
            start_tick,
        } => ProjectMutationAttempt::MoveArrangementPlacement {
            start_tick,
            result: attempt_arrangement_placement_move(
                application,
                recovery,
                &placement_id,
                start_tick,
                native_failure,
            ),
        },
        ProjectMutation::SetArrangementPlacementDuration {
            placement_id,
            duration_ticks,
        } => ProjectMutationAttempt::SetArrangementPlacementDuration {
            duration_ticks,
            result: attempt_arrangement_placement_duration(
                application,
                recovery,
                &placement_id,
                duration_ticks,
                native_failure,
            ),
        },
        ProjectMutation::TrimArrangementPlacementFront {
            placement_id,
            start_tick,
        } => ProjectMutationAttempt::TrimArrangementPlacementFront {
            start_tick,
            result: attempt_arrangement_placement_front_trim(
                application,
                recovery,
                &placement_id,
                start_tick,
                native_failure,
            ),
        },
        ProjectMutation::RemoveArrangementPlacement { placement_id } => {
            ProjectMutationAttempt::RemoveArrangementPlacement(
                attempt_arrangement_placement_removal(
                    application,
                    recovery,
                    &placement_id,
                    native_failure,
                ),
            )
        }
        ProjectMutation::RenameAudioClip { clip_id, name } => {
            ProjectMutationAttempt::RenameAudioClip {
                name: name.clone(),
                result: attempt_audio_clip_rename(
                    application,
                    recovery,
                    &clip_id,
                    &name,
                    native_failure,
                ),
            }
        }
        ProjectMutation::SetAudioClipActive { clip_id, active } => {
            ProjectMutationAttempt::SetAudioClipActive {
                active,
                result: attempt_audio_clip_activation(
                    application,
                    recovery,
                    &clip_id,
                    active,
                    native_failure,
                ),
            }
        }
        ProjectMutation::SetAudioClipGain {
            clip_id,
            gain_decibels,
        } => ProjectMutationAttempt::SetAudioClipGain {
            gain_decibels,
            result: attempt_audio_clip_gain(
                application,
                recovery,
                &clip_id,
                gain_decibels,
                native_failure,
            ),
        },
        ProjectMutation::SetAudioClipFades {
            clip_id,
            fade_in_ticks,
            fade_out_ticks,
        } => ProjectMutationAttempt::SetAudioClipFades {
            fade_in_ticks,
            fade_out_ticks,
            result: attempt_audio_clip_fades(
                application,
                recovery,
                &clip_id,
                fade_in_ticks,
                fade_out_ticks,
                native_failure,
            ),
        },
        ProjectMutation::SetAudioClipLoop {
            clip_id,
            enabled,
            start_tick,
            end_tick,
        } => ProjectMutationAttempt::SetAudioClipLoop {
            enabled,
            start_tick,
            end_tick,
            result: attempt_audio_clip_loop(
                application,
                recovery,
                &clip_id,
                enabled,
                start_tick,
                end_tick,
                native_failure,
            ),
        },
        ProjectMutation::SetAudioClipReverse { clip_id, reverse } => {
            ProjectMutationAttempt::SetAudioClipReverse {
                reverse,
                result: attempt_audio_clip_reverse(
                    application,
                    recovery,
                    &clip_id,
                    reverse,
                    native_failure,
                ),
            }
        }
        ProjectMutation::SetAudioClipSourceStart {
            clip_id,
            source_start_frame,
        } => ProjectMutationAttempt::SetAudioClipSourceStart {
            source_start_frame,
            result: attempt_audio_clip_source_slip(
                application,
                recovery,
                &clip_id,
                source_start_frame,
                native_failure,
            ),
        },
        ProjectMutation::EditControlMapping { edit } => {
            ProjectMutationAttempt::EditControlMapping {
                edit: edit.clone(),
                result: attempt_control_mapping_edit(application, recovery, &edit, native_failure),
            }
        }
        ProjectMutation::RestoreResource { entry, redo } => {
            let result = if let Some(failure) = native_failure {
                Err(IdeResourceRestoreError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_resource_restore(
                    application,
                    recovery,
                    &entry.resource_id,
                    &entry.bytes,
                )
                .and_then(|plan| plan.publish())
            };
            ProjectMutationAttempt::RestoreResource {
                entry,
                redo,
                result,
            }
        }
        ProjectMutation::SetAutomationTakeActive { take_id, enabled } => {
            ProjectMutationAttempt::SetAutomationTakeActive {
                enabled,
                result: attempt_automation_take_activation(
                    application,
                    recovery,
                    &take_id,
                    enabled,
                    native_failure,
                ),
            }
        }
        ProjectMutation::AddAutomationTake { request } => {
            ProjectMutationAttempt::AddAutomationTake {
                name: request.name.clone(),
                result: attempt_automation_take_creation(
                    application,
                    recovery,
                    &request,
                    native_failure,
                ),
            }
        }
        ProjectMutation::AddScene { name, quantization } => ProjectMutationAttempt::AddScene {
            name: name.clone(),
            result: if let Some(failure) = native_failure {
                Err(IdeSceneCreationError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_scene_creation(
                    application,
                    recovery,
                    &name,
                    quantization,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeSceneCreationError::Publication(Box::new(source)))
                })
            },
        },
        ProjectMutation::DuplicateScene { scene_id, name } => {
            ProjectMutationAttempt::DuplicateScene {
                name: name.clone(),
                result: if let Some(failure) = native_failure {
                    Err(IdeSceneDuplicationError::NativeClose(failure))
                } else {
                    RunningDesktopIdeApplication::prepare_scene_duplication(
                        application,
                        recovery,
                        &scene_id,
                        &name,
                    )
                    .and_then(|plan| {
                        plan.publish().map_err(|source| {
                            IdeSceneDuplicationError::Publication(Box::new(source))
                        })
                    })
                },
            }
        }
        ProjectMutation::SetSceneSlot {
            scene_id,
            track_id,
            edit,
        } => ProjectMutationAttempt::SetSceneSlot {
            edit: edit.clone(),
            result: if let Some(failure) = native_failure {
                Err(IdeSceneSlotEditError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_scene_slot_edit(
                    application,
                    recovery,
                    &scene_id,
                    &track_id,
                    &edit,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeSceneSlotEditError::Publication(Box::new(source)))
                })
            },
        },
        ProjectMutation::RenameScene { scene_id, name } => ProjectMutationAttempt::RenameScene {
            name: name.clone(),
            result: if let Some(failure) = native_failure {
                Err(IdeSceneRenameError::NativeClose(failure))
            } else {
                RunningDesktopIdeApplication::prepare_scene_rename(
                    application,
                    recovery,
                    &scene_id,
                    &name,
                )
                .and_then(|plan| {
                    plan.publish()
                        .map_err(|source| IdeSceneRenameError::Publication(Box::new(source)))
                })
            },
        },
        ProjectMutation::SetSceneActive { scene_id, active } => {
            ProjectMutationAttempt::SetSceneActive {
                active,
                result: if let Some(failure) = native_failure {
                    Err(IdeSceneEditError::NativeClose(failure))
                } else {
                    RunningDesktopIdeApplication::prepare_scene_activation(
                        application,
                        recovery,
                        &scene_id,
                        active,
                    )
                    .and_then(|plan| {
                        plan.publish()
                            .map_err(|source| IdeSceneEditError::Publication(Box::new(source)))
                    })
                },
            }
        }
    }
}

fn publish_audio_clip_duplication_plan(
    plan: super::IdeAudioClipCreationPlan,
) -> Result<hyper_storage_engine::AudioClipPublicationOutcome, IdeAudioClipDuplicationError> {
    match plan.publish() {
        Ok(outcome) => Ok(outcome),
        Err(source) => Err(IdeAudioClipDuplicationError::Publication {
            clip_id: plan.clip_id,
            source: Box::new(source),
        }),
    }
}

fn attempt_audio_clip_rename(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    name: &str,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipRenameEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipRenameEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_rename(application, recovery, clip_id, name)
        .and_then(|plan| {
            plan.publish()
                .map_err(|source| IdeAudioClipRenameEditError::Publication(Box::new(source)))
        })
}

fn attempt_audio_clip_activation(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    active: bool,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipActivationEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipActivationEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_activation(
        application,
        recovery,
        clip_id,
        active,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipActivationEditError::Publication(Box::new(source)))
    })
}

fn attempt_arrangement_placement_activation(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    active: bool,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_activation(
        application,
        recovery,
        placement_id,
        active,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_placement_addition(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    start_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_addition(
        application,
        recovery,
        clip_id,
        start_tick,
    )
    .and_then(publish_arrangement_placement_creation_plan)
}

fn attempt_automation_take_creation(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    request: &IdeAutomationTakeRequest,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAutomationTakeCreationError>
{
    if let Some(failure) = native_failure {
        return Err(IdeAutomationTakeCreationError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_automation_take_creation(application, recovery, request)
        .and_then(|plan| {
            plan.publish()
                .map_err(|source| IdeAutomationTakeCreationError::Publication(Box::new(source)))
        })
}

fn attempt_arrangement_scene_launch_addition(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    scene_id: &str,
    tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_scene_launch_addition(
        application,
        recovery,
        scene_id,
        tick,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_loop_range(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    range: Option<IdeLoopRangeFrames>,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_loop_range(application, recovery, range)
        .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_scene_launch_move(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    launch_id: &str,
    tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_scene_launch_move(
        application,
        recovery,
        launch_id,
        tick,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_scene_launch_scene_edit(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    launch_id: &str,
    scene_id: &str,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_scene_launch_scene_edit(
        application,
        recovery,
        launch_id,
        scene_id,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_scene_launch_removal(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    launch_id: &str,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_scene_launch_removal(
        application,
        recovery,
        launch_id,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_placement_duplication(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    start_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_duplication(
        application,
        recovery,
        placement_id,
        start_tick,
    )
    .and_then(publish_arrangement_placement_creation_plan)
}

fn attempt_arrangement_placement_split(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    split_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_split(
        application,
        recovery,
        placement_id,
        split_tick,
    )
    .and_then(publish_arrangement_placement_creation_plan)
}

fn attempt_arrangement_placement_move(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    start_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_move(
        application,
        recovery,
        placement_id,
        start_tick,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_placement_duration(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    duration_ticks: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_duration(
        application,
        recovery,
        placement_id,
        duration_ticks,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_placement_front_trim(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    start_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_front_trim(
        application,
        recovery,
        placement_id,
        start_tick,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn attempt_arrangement_placement_removal(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    placement_id: &str,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeArrangementPlacementEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_arrangement_placement_removal(
        application,
        recovery,
        placement_id,
    )
    .and_then(publish_arrangement_placement_edit_plan)
}

fn publish_arrangement_placement_edit_plan(
    plan: super::IdeArrangementPlacementEditPlan,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeArrangementPlacementEditError>
{
    let publication = plan.publish();
    let created_placement_id = plan.created_placement_id.map(String::into_boxed_str);
    publication.map_err(|source| IdeArrangementPlacementEditError::Publication {
        created_placement_id,
        source: Box::new(source),
    })
}

fn publish_arrangement_placement_creation_plan(
    plan: super::IdeArrangementPlacementEditPlan,
) -> Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError> {
    let publication = plan.publish();
    let created_placement_id = plan
        .created_placement_id
        .expect("arrangement placement creation plan must retain its identity");
    match publication {
        Ok(outcome) => Ok(ArrangementPlacementCreationPublication {
            outcome,
            placement_id: created_placement_id,
        }),
        Err(source) => Err(IdeArrangementPlacementEditError::Publication {
            created_placement_id: Some(created_placement_id.into_boxed_str()),
            source: Box::new(source),
        }),
    }
}

fn attempt_audio_clip_gain(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    gain_decibels: f64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipGainEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipGainEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_gain(
        application,
        recovery,
        clip_id,
        gain_decibels,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipGainEditError::Publication(Box::new(source)))
    })
}

fn attempt_audio_clip_fades(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    fade_in_ticks: u64,
    fade_out_ticks: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipFadeEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipFadeEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_fades(
        application,
        recovery,
        clip_id,
        fade_in_ticks,
        fade_out_ticks,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipFadeEditError::Publication(Box::new(source)))
    })
}

fn attempt_audio_clip_loop(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipLoopEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipLoopEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_loop(
        application,
        recovery,
        clip_id,
        enabled,
        start_tick,
        end_tick,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipLoopEditError::Publication(Box::new(source)))
    })
}

fn attempt_audio_clip_reverse(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    reverse: bool,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipReverseEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipReverseEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_reverse(
        application,
        recovery,
        clip_id,
        reverse,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipReverseEditError::Publication(Box::new(source)))
    })
}

fn attempt_audio_clip_source_slip(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    clip_id: &str,
    source_start_frame: u64,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioClipSourceSlipEditError>
{
    if let Some(failure) = native_failure {
        return Err(IdeAudioClipSourceSlipEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_audio_clip_source_slip(
        application,
        recovery,
        clip_id,
        source_start_frame,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAudioClipSourceSlipEditError::Publication(Box::new(source)))
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "every mutation outcome retains its command-specific recovery evidence"
)]
fn project_mutation_reopen_failure(
    attempt: ProjectMutationAttempt,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    match attempt {
        ProjectMutationAttempt::Save(Ok(outcome)) => {
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::ReopenAfterPublication {
                outcome: Box::new(outcome),
                source: Box::new(reopen),
            })
        }
        ProjectMutationAttempt::Save(Err(IdeProjectSaveError::Publication(publication))) => {
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            })
        }
        ProjectMutationAttempt::Save(Err(IdeProjectSaveError::NativeClose(close))) => {
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            })
        }
        ProjectMutationAttempt::Save(Err(preparation)) => {
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::ReopenAfterPreparation {
                preparation: Box::new(preparation),
                reopen: Box::new(reopen),
            })
        }
        ProjectMutationAttempt::PrepareRuntime(attempt) => {
            let reason = match attempt.result {
                Ok(bundle) => IdeRuntimePreparationError::ReopenAfterPublication {
                    bundle: Box::new(bundle),
                    source: Box::new(reopen),
                },
                Err(preparation) => IdeRuntimePreparationError::ReopenAfterFailure {
                    preparation: Box::new(preparation),
                    source: Box::new(reopen),
                },
            };
            IdeWorkerCommandFailure::RuntimePreparation(Box::new(reason))
        }
        ProjectMutationAttempt::AddAudioTrack(Ok(outcome)) => {
            IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
                IdeAudioTrackAdditionError::ReopenAfterPublication {
                    outcome,
                    source: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::AddAudioTrack(Err(IdeAudioTrackAdditionError::Publication(
            publication,
        ))) => IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
            IdeAudioTrackAdditionError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::AddAudioTrack(Err(IdeAudioTrackAdditionError::NativeClose(
            close,
        ))) => IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
            IdeAudioTrackAdditionError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::AddAudioTrack(Err(preparation)) => {
            IdeWorkerCommandFailure::AudioTrackAddition(Box::new(
                IdeAudioTrackAdditionError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::RemoveAudioTrack(Ok(outcome)) => {
            IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
                IdeAudioTrackRemovalError::ReopenAfterPublication {
                    outcome,
                    source: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::RemoveAudioTrack(Err(IdeAudioTrackRemovalError::Publication(
            publication,
        ))) => IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
            IdeAudioTrackRemovalError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::RemoveAudioTrack(Err(IdeAudioTrackRemovalError::NativeClose(
            close,
        ))) => IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
            IdeAudioTrackRemovalError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::RemoveAudioTrack(Err(preparation)) => {
            IdeWorkerCommandFailure::AudioTrackRemoval(Box::new(
                IdeAudioTrackRemovalError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::RenameAudioTrack(result) => audio_track_edit_reopen_failure(
            result,
            reopen,
            IdeWorkerCommandFailure::AudioTrackRename,
        ),
        ProjectMutationAttempt::SetAudioTrackActive { result, .. } => {
            audio_track_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::AudioTrackActivation,
            )
        }
        ProjectMutationAttempt::ImportAudio(Ok(outcome)) => {
            IdeWorkerCommandFailure::AudioImportReopen {
                outcome,
                source: Box::new(reopen),
            }
        }
        ProjectMutationAttempt::ImportAudio(Err(import)) => {
            IdeWorkerCommandFailure::AudioImportAndReopen {
                import: Box::new(import),
                reopen: Box::new(reopen),
            }
        }
        ProjectMutationAttempt::CreateAudioClipFromTake(result) => {
            recording_clip_creation_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::DuplicateAudioClip { result, .. } => {
            audio_clip_duplication_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::RemoveAudioClip(Ok(outcome)) => {
            IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
                IdeAudioClipRemovalError::ReopenAfterPublication {
                    outcome,
                    source: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::RemoveAudioClip(Err(
            IdeAudioClipRemovalError::PublicationCommittedWithWarning(warning),
        )) => IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
            IdeAudioClipRemovalError::PublicationCommittedWithWarningAndReopen {
                warning,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::RemoveAudioClip(Err(IdeAudioClipRemovalError::Publication(
            publication,
        ))) => IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
            IdeAudioClipRemovalError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::RemoveAudioClip(Err(IdeAudioClipRemovalError::NativeClose(
            close,
        ))) => IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
            IdeAudioClipRemovalError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            },
        )),
        ProjectMutationAttempt::RemoveAudioClip(Err(preparation)) => {
            IdeWorkerCommandFailure::AudioClipRemoval(Box::new(
                IdeAudioClipRemovalError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::CleanUnusedSamples(Ok(outcome)) => {
            IdeWorkerCommandFailure::UnusedSampleCleanup(Box::new(
                IdeUnusedSampleCleanupError::ReopenAfterPublication {
                    outcome,
                    source: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::CleanUnusedSamples(Err(cleanup)) => {
            IdeWorkerCommandFailure::UnusedSampleCleanup(Box::new(
                IdeUnusedSampleCleanupError::ReopenAfterFailure {
                    cleanup: Box::new(cleanup),
                    source: Box::new(reopen),
                },
            ))
        }
        ProjectMutationAttempt::AddArrangementPlacement { result, .. } => {
            arrangement_placement_creation_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementAddition,
            )
        }
        ProjectMutationAttempt::AddArrangementSceneLaunch { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementSceneLaunchAddition,
            )
        }
        ProjectMutationAttempt::SetMeter { result, .. }
        | ProjectMutationAttempt::SetTempo { result, .. } => {
            let reason = match result {
                Ok(outcome) => IdeTempoEditError::ReopenAfterPublication {
                    outcome: Box::new(outcome),
                    source: Box::new(reopen),
                },
                Err(preparation) => IdeTempoEditError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            };
            IdeWorkerCommandFailure::Tempo(Box::new(reason))
        }
        ProjectMutationAttempt::SetArrangementLoopRange { result } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementLoopRange,
            )
        }
        ProjectMutationAttempt::MoveArrangementSceneLaunch { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementSceneLaunchMove,
            )
        }
        ProjectMutationAttempt::SetArrangementSceneLaunchScene { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit,
            )
        }
        ProjectMutationAttempt::RemoveArrangementSceneLaunch { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval,
            )
        }
        ProjectMutationAttempt::DuplicateArrangementPlacement { result, .. } => {
            arrangement_placement_creation_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementDuplication,
            )
        }
        ProjectMutationAttempt::SplitArrangementPlacement { result, .. } => {
            arrangement_placement_creation_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementSplit,
            )
        }
        ProjectMutationAttempt::SetArrangementPlacementActive { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementActivation,
            )
        }
        ProjectMutationAttempt::MoveArrangementPlacement { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementMove,
            )
        }
        ProjectMutationAttempt::SetArrangementPlacementDuration { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementDuration,
            )
        }
        ProjectMutationAttempt::TrimArrangementPlacementFront { result, .. } => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementFrontTrim,
            )
        }
        ProjectMutationAttempt::RemoveArrangementPlacement(result) => {
            arrangement_placement_edit_reopen_failure(
                result,
                reopen,
                IdeWorkerCommandFailure::ArrangementPlacementRemoval,
            )
        }
        ProjectMutationAttempt::RenameAudioClip { result, .. } => {
            audio_clip_rename_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipActive { result, .. } => {
            audio_clip_activation_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipGain { result, .. } => {
            audio_clip_gain_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipFades { result, .. } => {
            audio_clip_fade_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipLoop { result, .. } => {
            audio_clip_loop_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipReverse { result, .. } => {
            audio_clip_reverse_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAudioClipSourceStart { result, .. } => {
            audio_clip_source_slip_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::AddScene { result, .. } => {
            scene_creation_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::AddAutomationTake { result, .. } => {
            automation_take_creation_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetAutomationTakeActive { result, .. } => {
            automation_take_activation_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::EditControlMapping { result, .. } => {
            control_mapping_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::RestoreResource { result, .. } => {
            let reason = match result {
                Ok(outcome) => IdeResourceRestoreError::ReopenAfterPublication {
                    outcome: Box::new(outcome),
                    source: Box::new(reopen),
                },
                Err(IdeResourceRestoreError::NativeClose(close)) => {
                    IdeResourceRestoreError::NativeCloseAndReopen {
                        close,
                        reopen: Box::new(reopen),
                    }
                }
                Err(preparation) => IdeResourceRestoreError::ReopenAfterPreparation {
                    preparation: Box::new(preparation),
                    reopen: Box::new(reopen),
                },
            };
            IdeWorkerCommandFailure::EditRestore(Box::new(reason))
        }
        ProjectMutationAttempt::DuplicateScene { result, .. } => {
            scene_duplication_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetSceneSlot { result, .. } => {
            scene_slot_edit_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::RenameScene { result, .. } => {
            scene_rename_reopen_failure(result, reopen)
        }
        ProjectMutationAttempt::SetSceneActive { result, .. } => {
            scene_activation_reopen_failure(result, reopen)
        }
    }
}

fn attempt_automation_take_activation(
    application: &DesktopIdeApplication,
    recovery: &DesktopPlaybackRecovery,
    take_id: &str,
    enabled: bool,
    native_failure: Option<IdeNativeCloseFailure>,
) -> Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAutomationTakeEditError> {
    if let Some(failure) = native_failure {
        return Err(IdeAutomationTakeEditError::NativeClose(failure));
    }
    RunningDesktopIdeApplication::prepare_automation_take_activation(
        application,
        recovery,
        take_id,
        enabled,
    )
    .and_then(|plan| {
        plan.publish()
            .map_err(|source| IdeAutomationTakeEditError::Publication(Box::new(source)))
    })
}

fn automation_take_activation_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAutomationTakeEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAutomationTakeEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAutomationTakeEditError::Publication(publication)) => {
            IdeAutomationTakeEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAutomationTakeEditError::NativeClose(close)) => {
            IdeAutomationTakeEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAutomationTakeEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AutomationTakeActivation(Box::new(reason))
}

fn automation_take_creation_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAutomationTakeCreationError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAutomationTakeCreationError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAutomationTakeCreationError::Publication(publication)) => {
            IdeAutomationTakeCreationError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAutomationTakeCreationError::NativeClose(close)) => {
            IdeAutomationTakeCreationError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAutomationTakeCreationError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AutomationTakeCreation(Box::new(reason))
}

fn scene_creation_reopen_failure(
    result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneCreationError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeSceneCreationError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeSceneCreationError::Publication(publication)) => {
            IdeSceneCreationError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeSceneCreationError::NativeClose(close)) => {
            IdeSceneCreationError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeSceneCreationError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::SceneCreation(Box::new(reason))
}

pub(super) fn scene_duplication_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeSceneDuplicationError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeSceneDuplicationError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeSceneDuplicationError::Publication(publication)) => {
            IdeSceneDuplicationError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeSceneDuplicationError::NativeClose(close)) => {
            IdeSceneDuplicationError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeSceneDuplicationError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::SceneDuplication(Box::new(reason))
}

fn scene_slot_edit_reopen_failure(
    result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneSlotEditError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeSceneSlotEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeSceneSlotEditError::Publication(publication)) => {
            IdeSceneSlotEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeSceneSlotEditError::NativeClose(close)) => {
            IdeSceneSlotEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeSceneSlotEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::SceneSlotEdit(Box::new(reason))
}

fn recording_clip_creation_reopen_failure(
    result: Result<
        hyper_storage_engine::AudioClipPublicationOutcome,
        IdeRecordingClipCreationError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeRecordingClipCreationError::ReopenAfterPublication {
            outcome,
            source: Box::new(reopen),
        },
        Err(IdeRecordingClipCreationError::Publication(publication)) => {
            IdeRecordingClipCreationError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeRecordingClipCreationError::NativeClose(close)) => {
            IdeRecordingClipCreationError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeRecordingClipCreationError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::RecordingClipCreation(Box::new(reason))
}

fn audio_clip_duplication_reopen_failure(
    result: Result<hyper_storage_engine::AudioClipPublicationOutcome, IdeAudioClipDuplicationError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipDuplicationError::ReopenAfterPublication {
            outcome,
            source: Box::new(reopen),
        },
        Err(IdeAudioClipDuplicationError::Publication { clip_id, source }) => {
            IdeAudioClipDuplicationError::PublicationAndReopen {
                clip_id,
                publication: source,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipDuplicationError::NativeClose(close)) => {
            IdeAudioClipDuplicationError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipDuplicationError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipDuplication(Box::new(reason))
}

pub(super) fn audio_clip_rename_success_event(
    name: String,
    outcome: &hyper_storage_engine::ProjectResourcePublicationOutcome,
) -> IdeWorkerEvent {
    IdeWorkerEvent::AudioClipRenamed {
        revision: outcome.project_revision(),
        changed: outcome.changed(),
        name,
    }
}

pub(super) fn audio_clip_rename_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipRenameEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipRenameEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipRenameEditError::Publication(publication)) => {
            IdeAudioClipRenameEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipRenameEditError::NativeClose(close)) => {
            IdeAudioClipRenameEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipRenameEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipRename(Box::new(reason))
}

pub(super) fn scene_rename_reopen_failure(
    result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneRenameError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeSceneRenameError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeSceneRenameError::Publication(publication)) => {
            IdeSceneRenameError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeSceneRenameError::NativeClose(close)) => IdeSceneRenameError::NativeCloseAndReopen {
            close,
            reopen: Box::new(reopen),
        },
        Err(preparation) => IdeSceneRenameError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::SceneRename(Box::new(reason))
}

pub(super) fn scene_activation_reopen_failure(
    result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeSceneEditError>,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeSceneEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeSceneEditError::Publication(publication)) => {
            IdeSceneEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeSceneEditError::NativeClose(close)) => IdeSceneEditError::NativeCloseAndReopen {
            close,
            reopen: Box::new(reopen),
        },
        Err(preparation) => IdeSceneEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::SceneActivation(Box::new(reason))
}

fn audio_clip_activation_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipActivationEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipActivationEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipActivationEditError::Publication(publication)) => {
            IdeAudioClipActivationEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipActivationEditError::NativeClose(close)) => {
            IdeAudioClipActivationEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipActivationEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipActivation(Box::new(reason))
}

fn audio_clip_gain_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipGainEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipGainEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipGainEditError::Publication(publication)) => {
            IdeAudioClipGainEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipGainEditError::NativeClose(close)) => {
            IdeAudioClipGainEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipGainEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipGain(Box::new(reason))
}

fn audio_clip_fade_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipFadeEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipFadeEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipFadeEditError::Publication(publication)) => {
            IdeAudioClipFadeEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipFadeEditError::NativeClose(close)) => {
            IdeAudioClipFadeEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipFadeEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipFades(Box::new(reason))
}

fn audio_clip_loop_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipLoopEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipLoopEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipLoopEditError::Publication(publication)) => {
            IdeAudioClipLoopEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipLoopEditError::NativeClose(close)) => {
            IdeAudioClipLoopEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipLoopEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipLoop(Box::new(reason))
}

fn audio_clip_reverse_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipReverseEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipReverseEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipReverseEditError::Publication(publication)) => {
            IdeAudioClipReverseEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipReverseEditError::NativeClose(close)) => {
            IdeAudioClipReverseEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipReverseEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipReverse(Box::new(reason))
}

fn audio_clip_source_slip_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeAudioClipSourceSlipEditError,
    >,
    reopen: IdeProjectOpenError,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioClipSourceSlipEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioClipSourceSlipEditError::Publication(publication)) => {
            IdeAudioClipSourceSlipEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioClipSourceSlipEditError::NativeClose(close)) => {
            IdeAudioClipSourceSlipEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioClipSourceSlipEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    IdeWorkerCommandFailure::AudioClipSourceSlip(Box::new(reason))
}

fn arrangement_placement_edit_reopen_failure(
    result: Result<
        hyper_storage_engine::ProjectResourcePublicationOutcome,
        IdeArrangementPlacementEditError,
    >,
    reopen: IdeProjectOpenError,
    wrap: fn(Box<IdeArrangementPlacementEditError>) -> IdeWorkerCommandFailure,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeArrangementPlacementEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            created_placement_id: None,
            source: Box::new(reopen),
        },
        Err(IdeArrangementPlacementEditError::Publication {
            created_placement_id,
            source,
        }) => IdeArrangementPlacementEditError::PublicationAndReopen {
            created_placement_id,
            publication: source,
            reopen: Box::new(reopen),
        },
        Err(IdeArrangementPlacementEditError::NativeClose(close)) => {
            IdeArrangementPlacementEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeArrangementPlacementEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    wrap(Box::new(reason))
}

pub(super) fn arrangement_placement_creation_reopen_failure(
    result: Result<ArrangementPlacementCreationPublication, IdeArrangementPlacementEditError>,
    reopen: IdeProjectOpenError,
    wrap: fn(Box<IdeArrangementPlacementEditError>) -> IdeWorkerCommandFailure,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(publication) => IdeArrangementPlacementEditError::ReopenAfterPublication {
            outcome: Box::new(publication.outcome),
            created_placement_id: Some(publication.placement_id.into_boxed_str()),
            source: Box::new(reopen),
        },
        Err(IdeArrangementPlacementEditError::Publication {
            created_placement_id,
            source,
        }) => IdeArrangementPlacementEditError::PublicationAndReopen {
            created_placement_id,
            publication: source,
            reopen: Box::new(reopen),
        },
        Err(IdeArrangementPlacementEditError::NativeClose(close)) => {
            IdeArrangementPlacementEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeArrangementPlacementEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    wrap(Box::new(reason))
}

fn audio_track_edit_reopen_failure(
    result: Result<hyper_storage_engine::ProjectResourcePublicationOutcome, IdeAudioTrackEditError>,
    reopen: IdeProjectOpenError,
    wrap: fn(Box<IdeAudioTrackEditError>) -> IdeWorkerCommandFailure,
) -> IdeWorkerCommandFailure {
    let reason = match result {
        Ok(outcome) => IdeAudioTrackEditError::ReopenAfterPublication {
            outcome: Box::new(outcome),
            source: Box::new(reopen),
        },
        Err(IdeAudioTrackEditError::Publication(publication)) => {
            IdeAudioTrackEditError::PublicationAndReopen {
                publication,
                reopen: Box::new(reopen),
            }
        }
        Err(IdeAudioTrackEditError::NativeClose(close)) => {
            IdeAudioTrackEditError::NativeCloseAndReopen {
                close,
                reopen: Box::new(reopen),
            }
        }
        Err(preparation) => IdeAudioTrackEditError::ReopenAfterPreparation {
            preparation: Box::new(preparation),
            reopen: Box::new(reopen),
        },
    };
    wrap(Box::new(reason))
}

#[allow(
    clippy::too_many_lines,
    reason = "one state-machine boundary retains the sole application owner across every pending native transition"
)]
fn poll_pending_state(
    state: &mut WorkerState,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
    history: &mut UndoHistory,
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
        WorkerState::Saving { pending, operation } => match pending.wait(Duration::ZERO) {
            IdePendingSaveCloseWait::Closed {
                application,
                recovery,
                failure,
            } => complete_project_mutation(
                state,
                application,
                recovery,
                operation,
                failure,
                events,
                history,
            ),
            IdePendingSaveCloseWait::TimedOut(pending) => {
                *state = WorkerState::Saving { pending, operation };
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
            operation,
            sequence,
            deadline,
        } => {
            if application.committed_mixer_fence() >= sequence {
                begin_save_close(state, application, operation, events, history)
            } else if Instant::now() >= deadline {
                let command = operation.command();
                let reason = project_mutation_save_failure(
                    &operation,
                    IdeProjectSaveError::MixerCommitTimeout,
                );
                *state = WorkerState::Running(application);
                publish_failure(events, command, reason)
            } else {
                *state = WorkerState::SaveFence {
                    application,
                    operation,
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
        WorkerState::AudioPreviewFence {
            application,
            asset_id,
            sequence,
            deadline,
        } => poll_audio_preview_fence(state, application, asset_id, sequence, deadline, events),
        WorkerState::OutputTestStopping { pending, command } => {
            poll_output_test_shutdown(state, pending, command, events, unpublished_native_failure)
        }
        current => {
            *state = current;
            true
        }
    }
}

fn poll_output_test_shutdown(
    state: &mut WorkerState,
    pending: IdePendingOutputTestShutdown,
    command: IdeWorkerCommandKind,
    events: &SyncSender<IdeWorkerEvent>,
    unpublished_native_failure: &mut Option<IdeNativeCloseFailure>,
) -> bool {
    match pending.wait(Duration::ZERO) {
        IdePendingOutputTestShutdownWait::Closed {
            application,
            failure,
        } => complete_output_test_stop(
            state,
            application,
            failure,
            command,
            events,
            unpublished_native_failure,
        ),
        IdePendingOutputTestShutdownWait::TimedOut(pending) => {
            *state = WorkerState::OutputTestStopping { pending, command };
            true
        }
    }
}

fn poll_audio_preview_fence(
    state: &mut WorkerState,
    application: RunningDesktopIdeApplication,
    asset_id: String,
    sequence: u64,
    deadline: Instant,
    events: &SyncSender<IdeWorkerEvent>,
) -> bool {
    if application.committed_mixer_fence() >= sequence {
        let attempt = application.try_start_audio_preview(&asset_id);
        *state = WorkerState::Running(application);
        let mut no_native_failure = None;
        if !publish_state(state, events, &mut no_native_failure) {
            return false;
        }
        match attempt {
            Ok(()) => true,
            Err(reason) => publish_failure(
                events,
                IdeWorkerCommandKind::StartAudioPreview,
                IdeWorkerCommandFailure::AudioPreview(reason),
            ),
        }
    } else if Instant::now() >= deadline {
        *state = WorkerState::Running(application);
        publish_failure(
            events,
            IdeWorkerCommandKind::StartAudioPreview,
            IdeWorkerCommandFailure::AudioPreview(IdeAudioPreviewError::MixerCommitTimeout),
        )
    } else {
        *state = WorkerState::AudioPreviewFence {
            application,
            asset_id,
            sequence,
            deadline,
        };
        true
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
        WorkerState::OutputTesting(application) => {
            IdeWorkerEvent::OutputTest(application.snapshot())
        }
        WorkerState::Recording(application) => match recording_observation(application) {
            Ok(observation) => IdeWorkerEvent::Recording(observation),
            Err(reason) => {
                return publish_failure(events, IdeWorkerCommandKind::Observe, reason);
            }
        },
        WorkerState::Closing(_) => IdeWorkerEvent::Closing,
        WorkerState::SaveFence { operation, .. } | WorkerState::Saving { operation, .. } => {
            operation.pending_event()
        }
        WorkerState::RecordingFence { .. } | WorkerState::RecordingClosing { .. } => {
            IdeWorkerEvent::RecordingPreparing
        }
        WorkerState::AudioPreviewFence { .. } => IdeWorkerEvent::AudioPreviewPreparing,
        WorkerState::OutputTestStopping { .. } => IdeWorkerEvent::OutputTestStopping,
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

#[allow(
    clippy::too_many_lines,
    reason = "one conversion publishes a coherent bounded snapshot from a single runtime observation"
)]
fn running_observation(
    application: &mut RunningDesktopIdeApplication,
) -> Result<IdeRunningObservation, IdeWorkerCommandFailure> {
    let snapshot = application
        .snapshot()
        .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
    let project_id = try_project_owned(snapshot.project_id)?;
    let project_name = try_project_owned(snapshot.project_name)?;
    let mut audio_clips = Vec::new();
    audio_clips
        .try_reserve_exact(snapshot.loaded.clip_count())
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for clip_index in 0..snapshot.loaded.clip_count() {
        let clip_id = snapshot
            .loaded
            .clip_id_at(clip_index)
            .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
        let track_id = snapshot
            .loaded
            .clip_track_id_at(clip_index)
            .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
        let track_name = snapshot
            .loaded
            .track_name(track_id)
            .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
        audio_clips.push(IdeAudioClip {
            id: try_project_owned(clip_id)?,
            name: try_project_owned(
                snapshot
                    .loaded
                    .clip_name_at(clip_index)
                    .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            )?,
            track_id: try_project_owned(track_id)?,
            track_name: try_project_owned(track_name)?,
            duration_ticks: snapshot
                .loaded
                .clip_duration_ticks_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            source_start_frame: snapshot
                .loaded
                .clip_source_start_frame_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            source_frame_count: snapshot
                .loaded
                .clip_source_frame_count_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            active: snapshot
                .loaded
                .clip_active_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            gain_decibels: snapshot
                .loaded
                .clip_gain_decibels_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            loop_enabled: snapshot
                .loaded
                .clip_loop_enabled_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            loop_start_tick: snapshot
                .loaded
                .clip_loop_start_tick_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            loop_end_tick: snapshot
                .loaded
                .clip_loop_end_tick_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            reverse: snapshot
                .loaded
                .clip_reverse_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            fade_in_ticks: snapshot
                .loaded
                .clip_fade_in_ticks_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
            fade_out_ticks: snapshot
                .loaded
                .clip_fade_out_ticks_at(clip_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?,
        });
    }
    let preview_asset_id = snapshot
        .preview
        .recording_index()
        .map(|recording_index| {
            snapshot
                .recording_metadata
                .get(recording_index)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)
                .and_then(|recording| try_project_owned(recording.asset_id()))
        })
        .transpose()?;
    let mut recording_takes = Vec::new();
    recording_takes
        .try_reserve_exact(snapshot.recording_metadata.len())
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for recording in snapshot.recording_metadata {
        let track_name = snapshot
            .loaded
            .track_name(recording.track_id())
            .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?;
        recording_takes.push(IdeRecordingTake {
            id: try_project_owned(recording.id())?,
            asset_id: try_project_owned(recording.asset_id())?,
            track_id: try_project_owned(recording.track_id())?,
            track_name: try_project_owned(track_name)?,
            capture_kind: recording.capture_kind().into(),
            captured_at_utc: try_project_owned(recording.captured_at_utc())?,
            timeline_start_tick: recording.timeline_start_tick(),
            sample_rate_hz: recording.sample_rate_hz(),
            channels: recording.channels(),
            frame_count: recording.frame_count(),
        });
    }
    recording_takes.sort_unstable_by(|left, right| {
        right
            .captured_at_utc
            .cmp(&left.captured_at_utc)
            .then_with(|| right.id.cmp(&left.id))
    });
    let mut scenes = Vec::new();
    scenes
        .try_reserve_exact(snapshot.scene_count)
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for scene_index in 0..snapshot.scene_count {
        let mut track_slots = Vec::new();
        track_slots
            .try_reserve_exact(snapshot.mixer_channel_count)
            .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
        for channel_index in 0..snapshot.mixer_channel_count {
            let track_id = snapshot.mixer_channel_ids[channel_index];
            let action = match snapshot
                .loaded
                .scene_track_action(snapshot.scene_ids[scene_index], track_id)
                .ok_or(IdeWorkerCommandFailure::ProjectSnapshotUnavailable)?
            {
                SceneTrackAction::Continue => IdeSceneTrackAction::Continue,
                SceneTrackAction::Stop => IdeSceneTrackAction::Stop,
                SceneTrackAction::Play { clip_id } => IdeSceneTrackAction::Play {
                    clip_id: try_project_owned(clip_id)?,
                },
            };
            track_slots.push(IdeSceneTrackSlot {
                track_id: try_project_owned(track_id)?,
                track_name: try_project_owned(snapshot.mixer_channel_names[channel_index])?,
                action,
            });
        }
        scenes.push(IdeScene {
            id: try_project_owned(snapshot.scene_ids[scene_index])?,
            name: try_project_owned(snapshot.scene_names[scene_index])?,
            active: snapshot.scene_active[scene_index],
            availability: snapshot.scene_availability[scene_index],
            track_slots,
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
                id: try_project_owned(snapshot.mixer_send_ids[channel_index][send_index])?,
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
            active: snapshot.mixer_channel_active[channel_index],
            state: snapshot.mixer_channel_states[channel_index],
            sends,
        });
    }
    let mut control_bindings = Vec::new();
    control_bindings
        .try_reserve_exact(snapshot.control_bindings.len())
        .map_err(|_| IdeWorkerCommandFailure::ProjectPublicationCapacityUnavailable)?;
    for binding in snapshot.control_bindings {
        control_bindings.push(ide_control_binding(binding, &snapshot)?);
    }
    Ok(IdeRunningObservation {
        playback_session_generation: snapshot.playback_session_generation,
        project_id,
        project_name,
        sample_rate_hz: snapshot.sample_rate_hz,
        microseconds_per_quarter: snapshot
            .loaded
            .tempo_events()
            .first()
            .map_or(500_000, |event| event.microseconds_per_quarter),
        meter_events: snapshot.loaded.meter_events().to_vec(),
        tempo_events: snapshot.loaded.tempo_events().to_vec(),
        playback_extent: snapshot.playback_extent,
        arrangement: snapshot.arrangement,
        automation: snapshot.automation,
        control_bindings,
        audio_clips,
        recording_takes,
        unused_sample_cleanup: snapshot.unused_sample_cleanup,
        preview: IdeAudioPreviewObservation {
            snapshot: snapshot.preview,
            asset_id: preview_asset_id,
        },
        scenes,
        track_count: snapshot.track_count,
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
            WorkerState::OutputTesting(application) => match application.stop() {
                IdeOutputTestStopOutcome::Closed { failure, .. } => {
                    return (failure.as_ref().and_then(output_test_native_failure), None);
                }
                IdeOutputTestStopOutcome::Pending(pending) => WorkerState::OutputTestStopping {
                    pending,
                    command: IdeWorkerCommandKind::StopOutputTest,
                },
            },
            WorkerState::OutputTestStopping { pending, command } => {
                match pending.wait(WORKER_SHUTDOWN_POLL) {
                    IdePendingOutputTestShutdownWait::Closed { failure, .. } => {
                        return (failure.as_ref().and_then(output_test_native_failure), None);
                    }
                    IdePendingOutputTestShutdownWait::TimedOut(pending) => {
                        WorkerState::OutputTestStopping { pending, command }
                    }
                }
            }
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
            WorkerState::AudioPreviewFence { application, .. } => match application.close() {
                IdeCloseOutcome::Closed { failure, .. } => return (failure, None),
                IdeCloseOutcome::Pending(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Closing(pending) => match pending.wait(WORKER_SHUTDOWN_POLL) {
                IdePendingCloseWait::Closed { failure, .. } => return (failure, None),
                IdePendingCloseWait::TimedOut(pending) => WorkerState::Closing(pending),
            },
            WorkerState::Saving { pending, operation } => {
                match pending.wait(WORKER_SHUTDOWN_POLL) {
                    IdePendingSaveCloseWait::Closed { failure, .. } => return (failure, None),
                    IdePendingSaveCloseWait::TimedOut(pending) => {
                        WorkerState::Saving { pending, operation }
                    }
                }
            }
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

fn output_test_native_failure(error: &IdeOutputTestError) -> Option<IdeNativeCloseFailure> {
    match error {
        IdeOutputTestError::OutputStop(
            hyper_desktop_audio::DesktopOutputStopError::RuntimeFailed,
        )
        | IdeOutputTestError::PendingRuntimeFailed => Some(IdeNativeCloseFailure::RuntimeFailed),
        IdeOutputTestError::OutputStop(
            hyper_desktop_audio::DesktopOutputStopError::ThreadPanicked,
        )
        | IdeOutputTestError::PendingThreadPanicked => Some(IdeNativeCloseFailure::ThreadPanicked),
        IdeOutputTestError::SelectedOutputUnavailable
        | IdeOutputTestError::OutputConfig(_)
        | IdeOutputTestError::ToneConfig(_)
        | IdeOutputTestError::OutputStart(_)
        | IdeOutputTestError::OutputStop(
            hyper_desktop_audio::DesktopOutputStopError::ShutdownPending { .. },
        )
        | IdeOutputTestError::StartupCancellationCompleted => None,
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
    use crate::{
        IdeAudioOutputInventory, IdeOutputTestSessionOwner, IdeOutputTestSnapshot,
        IdeOutputTestState, IdePlaybackExtent, IdePlaybackSessionOwner,
        OutputTestingDesktopIdeApplication, TestPlaybackSession,
    };
    use hyper_audio_source::{AudioContainer, DecodeLimits, decode_bytes};
    use hyper_media_cache::CacheLimits;
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;
    use hyper_project_loader::ProjectPlaybackLimits;
    use hyper_storage_engine::{ProjectCreationErrorKind, verify_project_package};

    const WAIT: Duration = Duration::from_secs(2);
    const AUDIO_IMPORT_FLAC_FIXTURE_BASE64: &str =
        include_str!("../../../../crates/audio-source/tests/fixtures/mono-44100.flac.b64");

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

    fn audio_import_application() -> DesktopIdeApplication {
        let config = IdeApplicationConfig::try_new(
            256,
            ProjectPlaybackLimits {
                max_total_resource_bytes: 2_097_152,
                audio: DecodeLimits::new(2_097_152, 300_000, 2_400_000).unwrap(),
                session: PlaybackSessionAssemblyLimits {
                    documents: LoaderLimits::VERSION_1,
                    media_cache: CacheLimits {
                        max_assets: 8,
                        max_frames_per_asset: 300_000,
                        max_decoded_sample_bytes: 4_800_000,
                    },
                },
            },
        )
        .unwrap();
        DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap(),
            IdeAudioInputInventory::try_new(&[("input-a", "Input A")]).unwrap(),
        )
    }

    fn mono_pcm16_wav(sample_rate_hz: u32, samples: &[i16]) -> Vec<u8> {
        pcm16_wav(1, sample_rate_hz, samples)
    }

    fn pcm16_wav(channels: u16, sample_rate_hz: u32, samples: &[i16]) -> Vec<u8> {
        assert_eq!(samples.len() % usize::from(channels), 0);
        let data_bytes = u32::try_from(std::mem::size_of_val(samples)).unwrap();
        let block_align = channels * 2;
        let byte_rate = sample_rate_hz * u32::from(block_align);
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
        bytes.extend_from_slice(&byte_rate.to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn decode_base64_fixture(encoded: &str) -> Vec<u8> {
        let mut output = Vec::with_capacity(encoded.len() * 3 / 4);
        let mut accumulator = 0_u32;
        let mut bits = 0_u8;
        for byte in encoded.bytes().filter(|byte| !byte.is_ascii_whitespace() && *byte != b'=') {
            let value = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => panic!("invalid base64 fixture"),
            };
            accumulator = (accumulator << 6) | u32::from(value);
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                output.push(u8::try_from((accumulator >> bits) & 0xff).unwrap());
                accumulator &= (1_u32 << bits) - 1;
            }
        }
        output
    }

    fn assert_imported_audio_assets(project: &Path, expected_revision: u64) {
        let verified = verify_project_package(project).unwrap();
        assert_eq!(verified.manifest().revision(), expected_revision);
        let mut decoded_assets = verified
            .manifest()
            .assets()
            .iter()
            .map(|asset| {
                decode_bytes(
                    verified.read_asset(asset.id(), 2_097_152).unwrap(),
                    AudioContainer::Wav,
                    48_000,
                    DecodeLimits::new(2_097_152, 300_000, 2_400_000).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        decoded_assets.sort_by_key(hyper_audio_source::DecodedAudio::frame_count);
        assert_eq!(decoded_assets[0].frame_count(), 480);
        assert_eq!(decoded_assets[0].channels(), 2);
        assert_ne!(
            decoded_assets[0].plane(0).unwrap(),
            decoded_assets[0].plane(1).unwrap()
        );
        assert_eq!(decoded_assets[1].frame_count(), 240_000);
        assert_eq!(decoded_assets[1].channels(), 1);
    }

    fn assert_worker_clip_removal_lifecycle(
        worker: &DesktopIdeWorker,
        project: &Path,
        initial_revision: u64,
        import: &IdeAudioImportBatchOutcome,
    ) {
        let retained_clip_id = import.clip_ids()[0].clone();
        let removed_clip_id = import.clip_ids()[1].clone();
        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: retained_clip_id.clone(),
                name: "Stereo retained".to_owned(),
            })
            .unwrap();
        assert!(matches!(recv(worker), IdeWorkerEvent::AudioClipRenaming));
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: Some(_),
                redo_label: None,
            }
        ));
        let IdeWorkerEvent::Running(renamed) = recv(worker) else {
            panic!("the renamed project must reopen before completion");
        };
        assert!(renamed
            .audio_clips()
            .iter()
            .any(|clip| clip.id() == retained_clip_id && clip.name() == "Stereo retained"));
        let renamed_generation = renamed.playback_session_generation();
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::AudioClipRenamed { changed: true, .. }
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveAudioClip {
                clip_id: removed_clip_id.clone(),
            })
            .unwrap();
        assert!(matches!(recv(worker), IdeWorkerEvent::AudioClipRemoving));
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: None,
                redo_label: None,
            }
        ));
        let IdeWorkerEvent::Running(after_removal) = recv(worker) else {
            panic!("the removed project must reopen before completion");
        };
        assert_eq!(after_removal.audio_clips().len(), 1);
        assert_eq!(after_removal.audio_clips()[0].id(), retained_clip_id);
        assert_ne!(
            after_removal.playback_session_generation(),
            renamed_generation,
            "removal must reopen under a new native playback owner"
        );
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::AudioClipRemoved { revision, clip_id }
                if revision == initial_revision + 4 && clip_id == removed_clip_id
        ));
        assert_imported_audio_assets(project, initial_revision + 4);

        assert_worker_unused_sample_cleanup_lifecycle(
            worker,
            project,
            initial_revision,
            &retained_clip_id,
            &after_removal,
        );
    }

    fn assert_worker_unused_sample_cleanup_lifecycle(
        worker: &DesktopIdeWorker,
        project: &Path,
        initial_revision: u64,
        retained_clip_id: &str,
        after_removal: &IdeRunningObservation,
    ) {
        let cleanup_before_edit = after_removal.unused_sample_cleanup();
        assert_eq!(cleanup_before_edit.assets(), 1);
        assert!(cleanup_before_edit.bytes() > 0);

        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: retained_clip_id.to_owned(),
                name: "Stereo retained before cleanup".to_owned(),
            })
            .unwrap();
        assert!(matches!(recv(worker), IdeWorkerEvent::AudioClipRenaming));
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: Some(_),
                redo_label: None,
            }
        ));
        let IdeWorkerEvent::Running(before_cleanup) = recv(worker) else {
            panic!("the edited project must reopen before cleanup");
        };
        let cleanup = before_cleanup.unused_sample_cleanup();
        assert_eq!(cleanup.assets(), 1);
        assert_eq!(cleanup.bytes(), cleanup_before_edit.bytes());
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::AudioClipRenamed { changed: true, .. }
        ));

        worker
            .try_send(IdeWorkerCommand::CleanUnusedSamples {
                confirmation: cleanup,
            })
            .unwrap();
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UnusedSampleCleanupStarting { assets: 1, bytes }
                if bytes == cleanup.bytes()
        ));
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: None,
                redo_label: None,
            }
        ));
        let IdeWorkerEvent::Running(cleaned) = recv(worker) else {
            panic!("the cleaned project must reopen before completion");
        };
        assert!(cleaned.unused_sample_cleanup().is_empty());
        assert_eq!(cleaned.audio_clips().len(), 1);
        assert!(matches!(
            recv(worker),
            IdeWorkerEvent::UnusedSamplesCleaned { revision, assets: 1, bytes }
                if revision == initial_revision + 6 && bytes == cleanup.bytes()
        ));
        let verified = verify_project_package(project).unwrap();
        assert_eq!(verified.manifest().revision(), initial_revision + 6);
        assert_eq!(verified.manifest().assets().len(), 1);
    }

    fn output_testing_application(
        stop_failure: Option<IdeNativeCloseFailure>,
    ) -> OutputTestingDesktopIdeApplication {
        OutputTestingDesktopIdeApplication {
            application: closed_application(),
            session: IdeOutputTestSessionOwner::Test {
                snapshot: IdeOutputTestSnapshot {
                    frequency_hz: 1_000.0,
                    level_dbfs: -18.0,
                    state: IdeOutputTestState::Running,
                    output: OutputStreamSnapshot {
                        callback_count: 7,
                        rendered_callback_count: 7,
                        rendered_frame_count: 1_024,
                        ..OutputStreamSnapshot::default()
                    },
                },
                stop_failure,
            },
        }
    }

    fn recv(worker: &DesktopIdeWorker) -> IdeWorkerEvent {
        worker.events.as_ref().unwrap().recv_timeout(WAIT).unwrap()
    }

    fn recv_native_mutation(worker: &DesktopIdeWorker) -> IdeWorkerEvent {
        worker
            .events
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("the native project mutation must publish its next event")
    }

    fn save_changed_mixer(worker: &DesktopIdeWorker) {
        worker.try_send(IdeWorkerCommand::SaveProject).unwrap();
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::Saving
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: Some("mixer save"),
                redo_label: None,
            }
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::Running(_)
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::ProjectSaved { changed: true, .. }
        ));
    }

    fn refuse_audio_import(
        worker: &DesktopIdeWorker,
        unsupported: &Path,
        track_id: String,
    ) {
        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: vec![unsupported.to_path_buf()],
                track_id,
            })
            .unwrap();
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::AudioImporting { files: 1 }
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::Running(_)
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImport(reason),
            } if reason.completed() == 0
                && reason.path() == unsupported
                && matches!(reason.reason(), IdeAudioImportError::UnsupportedContainer)
        ));
    }

    fn undo_mixer_save(worker: &DesktopIdeWorker) {
        worker.try_send(IdeWorkerCommand::Undo).unwrap();
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::EditRestoring {
                redo: false,
                label: "mixer save",
            }
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: None,
                redo_label: Some("mixer save"),
            }
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::Running(_)
        ));
        assert!(matches!(
            recv_native_mutation(worker),
            IdeWorkerEvent::EditRestored {
                redo: false,
                label: "mixer save",
                ..
            }
        ));
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
    fn recent_project_history_is_loaded_and_proven_missing_entry_is_forgotten() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-history-{}",
            uuid::Uuid::new_v4()
        ));
        let config = fixture.join("config");
        let project = fixture.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let history = ProjectHistory::at(&config);
        history.remember("Daily Session", &project).unwrap();
        let canonical_project = std::fs::canonicalize(&project).unwrap();

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            3,
            4,
        )
        .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::RecentProjectsChanged { projects }
                if projects.len() == 1
                    && projects[0].name() == "Daily Session"
                    && projects[0].path() == canonical_project
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));

        worker
            .try_send(IdeWorkerCommand::OpenRecentProject(
                canonical_project.clone(),
            ))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::OpenProject,
                reason: IdeWorkerCommandFailure::ProjectOpen(_),
            }
        ));
        assert_eq!(history.load().unwrap().len(), 1);

        std::fs::remove_dir(&project).unwrap();
        worker
            .try_send(IdeWorkerCommand::OpenRecentProject(
                canonical_project.clone(),
            ))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::RecentProjectsChanged { projects } if projects.is_empty()
        ));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::OpenProject,
                reason: IdeWorkerCommandFailure::RecentProjectMissing(path),
            } if path == canonical_project
        ));
        assert!(history.load().unwrap().is_empty());
        assert_eq!(shutdown_report(worker).native_failure(), None);

        std::fs::remove_file(history.document_path()).unwrap();
        std::fs::remove_file(config.join("recent-projects.lock")).unwrap();
        std::fs::remove_dir(config).unwrap();
        std::fs::remove_dir(fixture).unwrap();
    }

    #[test]
    fn recent_project_with_a_dangling_parent_link_is_retained() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-linked-history-{}",
            uuid::Uuid::new_v4()
        ));
        let config = fixture.join("config");
        let parent = fixture.join("parent");
        let project = parent.join("project");
        let missing_target = fixture.join("missing-target");
        std::fs::create_dir_all(&project).unwrap();
        let history = ProjectHistory::at(&config);
        history.remember("Linked Session", &project).unwrap();
        let canonical_project = std::fs::canonicalize(&project).unwrap();
        std::fs::remove_dir_all(&parent).unwrap();
        create_directory_link(&missing_target, &parent);

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            2,
            3,
        )
        .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::RecentProjectsChanged { .. }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenRecentProject(
                canonical_project.clone(),
            ))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::OpenProject,
                reason: IdeWorkerCommandFailure::ProjectOpen(_),
            }
        ));
        assert_eq!(history.load().unwrap()[0].path(), canonical_project);
        assert_eq!(shutdown_report(worker).native_failure(), None);

        remove_directory_link(&parent);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn successful_open_and_creation_publish_recent_projects_before_running() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-history-success-{}",
            uuid::Uuid::new_v4()
        ));
        let config = fixture.join("config");
        let projects = fixture.join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let existing = create_project_package(
            &projects,
            ProjectCreationRequest {
                directory_name: "Existing",
                project_name: "Existing",
            },
        )
        .unwrap()
        .project()
        .root()
        .to_path_buf();
        let manifest_path = existing.join("project.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest["name"] = serde_json::Value::String(" Existing\n".to_owned());
        std::fs::write(
            &manifest_path,
            serde_json_canonicalizer::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let history = ProjectHistory::at(&config);

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            2,
            4,
        )
        .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::RecentProjectsChanged { projects } if projects.is_empty())
        );
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(existing.clone()))
            .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::RecentProjectsChanged { projects } if projects.len() == 1 && projects[0].name() == " Existing\n")
        );
        assert!(matches!(recv(&worker), IdeWorkerEvent::Running(_)));
        assert_eq!(history.load().unwrap()[0].path(), existing);
        assert_eq!(shutdown_report(worker).native_failure(), None);

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            2,
            4,
        )
        .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::RecentProjectsChanged { projects } if projects.len() == 1)
        );
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::CreateProject {
                parent: projects.clone(),
                directory_name: "Created".to_owned(),
                project_name: "Created".to_owned(),
            })
            .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::RecentProjectsChanged { projects } if projects.len() == 2 && projects[0].name() == "Created")
        );
        assert!(matches!(recv(&worker), IdeWorkerEvent::Running(_)));
        assert_eq!(history.load().unwrap()[0].name(), "Created");
        assert_eq!(shutdown_report(worker).native_failure(), None);

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn running_worker_imports_each_audio_file_and_reopens_once_with_the_committed_batch() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-audio-import-success-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&fixture).unwrap();
        let created = create_project_package(
            &fixture,
            ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project = created.project().root().to_path_buf();
        let initial_revision = created.project().manifest().revision();
        drop(created);
        let first = fixture.join("Stereo.wav");
        let second = fixture.join("Voice.flac");
        let mut stereo = Vec::with_capacity(441 * 2);
        for frame in 0..441 {
            stereo.push(if frame == 100 { 24_000 } else { 0 });
            stereo.push(if frame == 300 { -16_000 } else { 0 });
        }
        std::fs::write(&first, pcm16_wav(2, 44_100, &stereo)).unwrap();
        std::fs::write(
            &second,
            decode_base64_fixture(AUDIO_IMPORT_FLAC_FIXTURE_BASE64),
        )
        .unwrap();

        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(audio_import_application()), 2, 5)
                .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(project.clone()))
            .unwrap();
        let IdeWorkerEvent::Running(opened) = recv(&worker) else {
            panic!("the verified project must open before import");
        };
        let original_generation = opened.playback_session_generation();
        let track_id = opened
            .mixer_channels()
            .iter()
            .find(|channel| channel.kind() == IdeMixerChannelKind::Audio)
            .unwrap()
            .id()
            .to_owned();

        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: vec![first, second],
                track_id: track_id.clone(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioImporting { files: 2 }
        ));
        let reopened_event = worker
            .events
            .as_ref()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("the audio-import worker must publish a terminal state");
        let IdeWorkerEvent::Running(reopened) = reopened_event else {
            panic!("the worker must publish the reloaded committed project first");
        };
        let completion = recv(&worker);
        assert_ne!(
            reopened.playback_session_generation(),
            original_generation,
            "the native playback owner is replaced exactly at the worker fence"
        );
        let IdeWorkerEvent::AudioImported(outcome) = completion else {
            panic!("the successful batch must be reported after the reloaded state: {completion:?}");
        };
        assert_eq!(reopened.audio_clips().len(), 2);
        assert_eq!(
            reopened
                .audio_clips()
                .iter()
                .map(IdeAudioClip::name)
                .collect::<Vec<_>>(),
            ["Stereo", "Voice"]
        );
        assert!(reopened.audio_clips().iter().all(|clip| clip.track_id() == track_id));
        assert_eq!(
            reopened
                .audio_clips()
                .iter()
                .map(IdeAudioClip::source_frame_count)
                .collect::<Vec<_>>(),
            [480, 240_000]
        );
        assert_eq!(outcome.imported(), 2);
        assert_eq!(outcome.clip_ids().len(), 2);
        assert_eq!(outcome.final_revision(), initial_revision + 2);
        assert_imported_audio_assets(&project, initial_revision + 2);

        assert_worker_clip_removal_lifecycle(&worker, &project, initial_revision, &outcome);
        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn running_worker_stops_audio_batch_at_the_first_failure_and_keeps_prior_commits() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-audio-import-partial-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&fixture).unwrap();
        let created = create_project_package(
            &fixture,
            ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project = created.project().root().to_path_buf();
        let initial_revision = created.project().manifest().revision();
        drop(created);
        let valid = fixture.join("Bass.wav");
        let unsupported = fixture.join("Lead.mp3");
        std::fs::write(&valid, mono_pcm16_wav(48_000, &[500; 48])).unwrap();
        std::fs::write(&unsupported, b"not an accepted container").unwrap();

        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 2, 5)
                .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(project.clone()))
            .unwrap();
        let IdeWorkerEvent::Running(opened) = recv(&worker) else {
            panic!("the project must open");
        };
        let track_id = opened
            .mixer_channels()
            .iter()
            .find(|channel| channel.kind() == IdeMixerChannelKind::Audio)
            .unwrap()
            .id()
            .to_owned();
        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: vec![valid, unsupported.clone()],
                track_id,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioImporting { files: 2 }
        ));
        let reopened = recv(&worker);
        let completion = recv(&worker);
        assert!(matches!(
            reopened,
            IdeWorkerEvent::Running(ref observation)
                if observation.audio_clips().len() == 1
                    && observation.audio_clips()[0].name() == "Bass"
        ), "unexpected reopened state: {reopened:?}; completion: {completion:?}");
        assert!(matches!(
            completion,
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImport(reason),
            } if reason.completed() == 1
                && reason.total() == 2
                && reason.path() == unsupported
                && matches!(reason.reason(), IdeAudioImportError::UnsupportedContainer)
        ));
        assert_eq!(
            verify_project_package(&project).unwrap().manifest().revision(),
            initial_revision + 1,
            "the first independent publication survives the later refusal"
        );
        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn completed_audio_import_closes_before_reporting_a_forced_reopen_failure() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-audio-import-reopen-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&fixture).unwrap();
        let created = create_project_package(
            &fixture,
            ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project = created.project().root().to_path_buf();
        let initial_revision = created.project().manifest().revision();
        drop(created);
        let source = fixture.join("Bass.wav");
        std::fs::write(&source, mono_pcm16_wav(48_000, &[500; 48])).unwrap();

        let mut running = audio_import_application()
            .open_project(&project)
            .expect("the verified project must open before forcing the final reopen failure");
        running.application.playback_session_generation = u64::MAX;
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 6).unwrap();
        let IdeWorkerEvent::Running(opened) = recv(&worker) else {
            panic!("the worker must publish the running project");
        };
        let track_id = opened
            .mixer_channels()
            .iter()
            .find(|channel| channel.kind() == IdeMixerChannelKind::Audio)
            .unwrap()
            .id()
            .to_owned();

        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: vec![source],
                track_id,
            })
            .unwrap();
        assert!(matches!(
            recv_native_mutation(&worker),
            IdeWorkerEvent::AudioImporting { files: 1 }
        ));
        assert!(
            matches!(recv_native_mutation(&worker), IdeWorkerEvent::Closed { .. }),
            "closed ownership must be visible before the composite failure"
        );
        assert!(matches!(
            recv_native_mutation(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImportReopen { outcome, source },
            } if outcome.imported() == 1
                && outcome.final_revision() == initial_revision + 1
                && matches!(source.as_ref(), IdeProjectOpenError::PlaybackSessionGenerationExhausted)
        ));
        assert_eq!(
            verify_project_package(&project).unwrap().manifest().revision(),
            initial_revision + 1,
            "the committed import remains truthful when only the reload fails"
        );
        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn partial_audio_import_closes_before_reporting_both_failures() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-audio-import-partial-reopen-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&fixture).unwrap();
        let created = create_project_package(
            &fixture,
            ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project = created.project().root().to_path_buf();
        let initial_revision = created.project().manifest().revision();
        drop(created);
        let valid = fixture.join("Bass.wav");
        let unsupported = fixture.join("Lead.mp3");
        std::fs::write(&valid, mono_pcm16_wav(48_000, &[500; 48])).unwrap();
        std::fs::write(&unsupported, b"not an accepted container").unwrap();

        let mut running = audio_import_application()
            .open_project(&project)
            .expect("the verified project must open before forcing the final reopen failure");
        running.application.playback_session_generation = u64::MAX;
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 6).unwrap();
        let IdeWorkerEvent::Running(opened) = recv(&worker) else {
            panic!("the worker must publish the running project");
        };
        let track_id = opened
            .mixer_channels()
            .iter()
            .find(|channel| channel.kind() == IdeMixerChannelKind::Audio)
            .unwrap()
            .id()
            .to_owned();

        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: vec![valid, unsupported.clone()],
                track_id,
            })
            .unwrap();
        assert!(matches!(
            recv_native_mutation(&worker),
            IdeWorkerEvent::AudioImporting { files: 2 }
        ));
        assert!(
            matches!(recv_native_mutation(&worker), IdeWorkerEvent::Closed { .. }),
            "closed ownership must be visible before the composite failure"
        );
        assert!(matches!(
            recv_native_mutation(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImportAndReopen { import, reopen },
            } if import.completed() == 1
                && import.total() == 2
                && import.path() == unsupported
                && matches!(import.reason(), IdeAudioImportError::UnsupportedContainer)
                && matches!(reopen.as_ref(), IdeProjectOpenError::PlaybackSessionGenerationExhausted)
        ));
        assert_eq!(
            verify_project_package(&project).unwrap().manifest().revision(),
            initial_revision + 1,
            "the first publication remains committed despite both later failures"
        );
        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn zero_clip_import_keeps_a_real_mixer_save_undo_valid() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-audio-import-undo-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&fixture).unwrap();
        let created = create_project_package(
            &fixture,
            ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project = created.project().root().to_path_buf();
        drop(created);
        let package = verify_project_package(&project).unwrap();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|resource| resource.role() == hyper_project_model::ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let original_mixer = package.read_resource(&mixer_id).unwrap();
        drop(package);
        let unsupported = fixture.join("Lead.mp3");
        std::fs::write(&unsupported, b"not an accepted container").unwrap();

        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 2, 8)
                .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(project.clone()))
            .unwrap();
        let IdeWorkerEvent::Running(opened) = recv_native_mutation(&worker) else {
            panic!("the verified project must open");
        };
        let track_id = opened
            .mixer_channels()
            .iter()
            .find(|channel| channel.kind() == IdeMixerChannelKind::Audio)
            .unwrap()
            .id()
            .to_owned();

        worker
            .try_send(IdeWorkerCommand::Mixer(IdeMixerCommand::MainMute {
                muted: true,
            }))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::MixerQueued(IdeMixerCommand::MainMute { muted: true })
        ));
        save_changed_mixer(&worker);
        let first_saved_mixer = verify_project_package(&project)
            .unwrap()
            .read_resource(&mixer_id)
            .unwrap();
        assert_ne!(first_saved_mixer, original_mixer);

        worker
            .try_send(IdeWorkerCommand::Mixer(IdeMixerCommand::MainGain {
                gain_millionths: 500_000,
            }))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::MixerQueued(IdeMixerCommand::MainGain {
                gain_millionths: 500_000,
            })
        ));
        refuse_audio_import(&worker, &unsupported, track_id);
        let second_saved_mixer = verify_project_package(&project)
            .unwrap()
            .read_resource(&mixer_id)
            .unwrap();
        assert_ne!(second_saved_mixer, first_saved_mixer);

        undo_mixer_save(&worker);
        assert_eq!(
            verify_project_package(&project)
                .unwrap()
                .read_resource(&mixer_id)
                .unwrap(),
            original_mixer,
            "the retained undo still targets the current project authority"
        );

        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn invalid_audio_import_request_is_rejected_without_closing_playback() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 2).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                ..
            }
        ));
        worker
            .try_send(IdeWorkerCommand::ImportAudio {
                paths: Vec::new(),
                track_id: "track".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImport(reason),
            } if reason.completed() == 0
                && reason.total() == 0
                && matches!(reason.reason(), IdeAudioImportError::EmptySelection)
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn failed_open_and_created_but_not_opened_do_not_mutate_history() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-history-failure-{}",
            uuid::Uuid::new_v4()
        ));
        let config = fixture.join("config");
        let projects = fixture.join("projects");
        let seeded = projects.join("seeded");
        std::fs::create_dir_all(&seeded).unwrap();
        let history = ProjectHistory::at(&config);
        history.remember("Seeded", &seeded).unwrap();
        let before = std::fs::read(history.document_path()).unwrap();

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            2,
            3,
        )
        .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::RecentProjectsChanged { .. }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(fixture.join("missing")))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::OpenProject,
                reason: IdeWorkerCommandFailure::ProjectOpen(_),
            }
        ));
        assert_eq!(std::fs::read(history.document_path()).unwrap(), before);
        assert_eq!(shutdown_report(worker).native_failure(), None);

        let mut application = closed_application();
        application.output_selection = IdeOutputSelection::Routed("missing-route".to_owned());
        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(application),
            Some(history.clone()),
            2,
            3,
        )
        .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::RecentProjectsChanged { .. }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::CreateProject {
                parent: projects,
                directory_name: "Preserved".to_owned(),
                project_name: "Preserved".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::CreateProject,
                reason: IdeWorkerCommandFailure::ProjectCreation(_),
            }
        ));
        assert_eq!(std::fs::read(history.document_path()).unwrap(), before);
        assert_eq!(shutdown_report(worker).native_failure(), None);

        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn history_lock_failure_warns_without_hiding_successful_open() {
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-history-warning-{}",
            uuid::Uuid::new_v4()
        ));
        let config = fixture.join("config");
        let projects = fixture.join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        let project = create_project_package(
            &projects,
            ProjectCreationRequest {
                directory_name: "Openable",
                project_name: "Openable",
            },
        )
        .unwrap()
        .project()
        .root()
        .to_path_buf();
        let history = ProjectHistory::at(&config);
        std::fs::create_dir_all(&config).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(config.join("recent-projects.lock"))
            .unwrap();
        lock.try_lock().unwrap();

        let worker = DesktopIdeWorker::start_state_with_history(
            WorkerState::Closed(closed_application()),
            Some(history.clone()),
            2,
            4,
        )
        .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::RecentProjectsChanged { projects } if projects.is_empty())
        );
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        worker
            .try_send(IdeWorkerCommand::OpenProject(project))
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ProjectHistoryWarning(reason)
                if reason.kind() == hyper_project_history::ProjectHistoryErrorKind::Locked
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Running(_)));
        assert!(!history.document_path().exists());
        assert_eq!(shutdown_report(worker).native_failure(), None);

        drop(lock);
        std::fs::remove_dir_all(fixture).unwrap();
    }

    #[cfg(unix)]
    fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn create_directory_link(target: &std::path::Path, link: &std::path::Path) {
        std::fs::create_dir(target).unwrap();
        junction::create(target, link).unwrap();
        std::fs::remove_dir(target).unwrap();
    }

    #[cfg(unix)]
    fn remove_directory_link(link: &std::path::Path) {
        std::fs::remove_file(link).unwrap();
    }

    #[cfg(windows)]
    fn remove_directory_link(link: &std::path::Path) {
        junction::delete(link).unwrap();
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
    fn closed_worker_rejects_invalid_project_creation_without_losing_ownership() {
        let parent = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-worker-invalid-creation-{}",
            std::process::id()
        ));
        std::fs::create_dir(&parent).unwrap();
        let worker =
            DesktopIdeWorker::start_state(WorkerState::Closed(closed_application()), 3, 3).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));

        worker
            .try_send(IdeWorkerCommand::CreateProject {
                parent: parent.clone(),
                directory_name: "..".to_owned(),
                project_name: "Daily Session".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::CreateProject,
                reason: IdeWorkerCommandFailure::ProjectCreation(reason),
            } if matches!(reason.as_ref(), IdeProjectCreationError::Creation(source)
                if source.kind() == ProjectCreationErrorKind::InvalidRequest)
        ));
        worker.try_send(IdeWorkerCommand::Observe).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert_eq!(shutdown_report(worker).native_failure(), None);
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        std::fs::remove_dir(parent).unwrap();
    }

    #[test]
    fn project_created_before_open_failure_is_preserved_and_reported() {
        let parent = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-worker-created-project-{}",
            std::process::id()
        ));
        std::fs::create_dir(&parent).unwrap();
        let project_path = parent.join("Daily Session");
        let mut application = closed_application();
        application.output_selection = IdeOutputSelection::Routed("missing-route".to_owned());
        let worker = DesktopIdeWorker::start_state(WorkerState::Closed(application), 3, 3).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));

        worker
            .try_send(IdeWorkerCommand::CreateProject {
                parent: parent.clone(),
                directory_name: "Daily Session".to_owned(),
                project_name: "Daily Session".to_owned(),
            })
            .unwrap();
        let IdeWorkerEvent::CommandFailed {
            command: IdeWorkerCommandKind::CreateProject,
            reason: IdeWorkerCommandFailure::ProjectCreation(reason),
        } = recv(&worker)
        else {
            panic!("creation must report a preserved project after open failure");
        };
        let IdeProjectCreationError::CreatedButNotOpened {
            project_path: failed_path,
            source: IdeProjectOpenError::SelectedOutputUnavailable,
        } = *reason
        else {
            panic!("creation must retain the project path and open failure");
        };
        assert_eq!(
            failed_path,
            std::fs::canonicalize(&project_path).unwrap(),
            "the reported project must be the verified created directory"
        );
        verify_project_package(&project_path).unwrap();
        worker.try_send(IdeWorkerCommand::Observe).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert_eq!(shutdown_report(worker).native_failure(), None);
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn output_diagnostic_observation_and_stop_retain_the_closed_application() {
        let worker = DesktopIdeWorker::start_state(
            WorkerState::OutputTesting(output_testing_application(None)),
            2,
            3,
        )
        .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::OutputTest(snapshot)
                if snapshot.state() == IdeOutputTestState::Running
                    && snapshot.output().rendered_frame_count == 1_024
        ));

        worker.try_send(IdeWorkerCommand::StopOutputTest).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(recv(&worker), IdeWorkerEvent::OutputTestStopped));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn output_diagnostic_start_failure_retains_the_closed_application() {
        let mut application = closed_application();
        application.output_selection = IdeOutputSelection::Routed("missing-route".to_owned());
        let worker = DesktopIdeWorker::start_state(WorkerState::Closed(application), 2, 3).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));

        worker.try_send(IdeWorkerCommand::StartOutputTest).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::StartOutputTest,
                reason: IdeWorkerCommandFailure::OutputTest(
                    IdeOutputTestError::SelectedOutputUnavailable
                ),
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SelectSystemDefault)
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn output_diagnostic_stop_failure_closes_before_reporting_the_reason() {
        let worker = DesktopIdeWorker::start_state(
            WorkerState::OutputTesting(output_testing_application(Some(
                IdeNativeCloseFailure::RuntimeFailed,
            ))),
            2,
            3,
        )
        .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::OutputTest(_)));

        worker.try_send(IdeWorkerCommand::StopOutputTest).unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::StopOutputTest,
                reason: IdeWorkerCommandFailure::OutputTest(IdeOutputTestError::OutputStop(
                    hyper_desktop_audio::DesktopOutputStopError::RuntimeFailed
                )),
            }
        ));
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
    fn running_worker_queues_only_bounded_seek_positions() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                extent: IdePlaybackExtent::Bounded { end_frame: 2_000 },
                ..TestPlaybackSession::default()
            }),
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
            .try_send(IdeWorkerCommand::Seek { frame: 1_500 })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                frame: 1_500,
                state: LocateState::Preserve,
            })
        ));
        worker
            .try_send(IdeWorkerCommand::Seek { frame: 2_001 })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Seek,
                reason: IdeWorkerCommandFailure::Seek(IdeTransportSeekError::OutOfRange {
                    requested: 2_001,
                    end_frame: 2_000,
                }),
            }
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn running_worker_locates_only_the_current_arrangement_placement_identity() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                extent: IdePlaybackExtent::Unbounded,
                arrangement_project_id: "project-a".to_owned(),
                arrangement_placements: vec![("placement-a".to_owned(), 1_250)],
                ..TestPlaybackSession::default()
            }),
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
            .try_send(IdeWorkerCommand::LocatePlacement {
                project_id: "project-b".to_owned(),
                index: 0,
                placement_id: "placement-a".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::LocatePlacement,
                reason: IdeWorkerCommandFailure::ArrangementLocate(
                    IdeArrangementLocateError::StaleProject
                ),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::LocatePlacement {
                project_id: "project-a".to_owned(),
                index: 0,
                placement_id: "stale".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::LocatePlacement,
                reason: IdeWorkerCommandFailure::ArrangementLocate(
                    IdeArrangementLocateError::StalePlacement { requested_index: 0 }
                ),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::LocatePlacement {
                project_id: "project-a".to_owned(),
                index: 0,
                placement_id: "placement-a".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                frame: 1_250,
                state: LocateState::Preserve,
            })
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn running_worker_locates_and_plays_only_the_current_scene_marker_identity() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                extent: IdePlaybackExtent::Unbounded,
                arrangement_project_id: "project-a".to_owned(),
                arrangement_scene_launches: vec![("marker-a".to_owned(), 2_500)],
                ..TestPlaybackSession::default()
            }),
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
            .try_send(IdeWorkerCommand::LocateSceneLaunch {
                project_id: "project-a".to_owned(),
                index: 0,
                launch_id: "stale".to_owned(),
                state: LocateState::Preserve,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::LocateSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementLocate(
                    IdeArrangementLocateError::StaleSceneLaunch { requested_index: 0 }
                ),
            }
        ));
        worker
            .try_send(IdeWorkerCommand::LocateSceneLaunch {
                project_id: "project-a".to_owned(),
                index: 0,
                launch_id: "marker-a".to_owned(),
                state: LocateState::Rolling,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::TransportQueued(TransportCommand::Locate {
                frame: 2_500,
                state: LocateState::Rolling,
            })
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn running_worker_queues_typed_loudness_controls_in_order() {
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

        for command in [
            IdeLoudnessCommand::Standby,
            IdeLoudnessCommand::Reset,
            IdeLoudnessCommand::Run,
        ] {
            worker
                .try_send(IdeWorkerCommand::Loudness(command))
                .unwrap();
            assert!(matches!(
                recv(&worker),
                IdeWorkerEvent::LoudnessQueued(actual) if actual == command
            ));
        }
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
    fn runtime_preparation_saves_before_building_and_reports_the_typed_failure() {
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

        worker
            .try_send(IdeWorkerCommand::PrepareRuntime {
                output_path: std::env::temp_dir().join("unreachable.runtime"),
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::RuntimePreparing));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::PrepareRuntime,
                reason: IdeWorkerCommandFailure::RuntimePreparation(reason),
            } if matches!(reason.as_ref(), IdeRuntimePreparationError::Save(
                IdeProjectSaveError::ProjectSnapshotUnavailable
            ))
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
    fn audio_track_addition_validates_name_before_changing_running_state() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 3).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::AddAudioTrack {
                name: "   ".to_owned(),
            })
            .unwrap();

        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackAddition(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackAdditionError::InvalidName)
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
    fn audio_track_addition_waits_for_the_mixer_fence_before_closing() {
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

        worker
            .try_send(IdeWorkerCommand::AddAudioTrack {
                name: " Vocal ".to_owned(),
            })
            .unwrap();

        assert!(matches!(recv(&worker), IdeWorkerEvent::AudioTrackAdding));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackAddition(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackAdditionError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_track_removal_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));
        worker
            .try_send(IdeWorkerCommand::RemoveAudioTrack {
                track_id: String::new(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackRemoval(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackRemovalError::InvalidTrackIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveAudioTrack {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
            })
            .unwrap();

        assert!(matches!(recv(&worker), IdeWorkerEvent::AudioTrackRemoving));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackRemoval(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackRemovalError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_track_rename_validates_name_before_changing_running_state() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 3).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RenameAudioTrack {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                name: "   ".to_owned(),
            })
            .unwrap();

        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackRename(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackEditError::InvalidName)
        ));
        worker
            .try_send(IdeWorkerCommand::RenameAudioTrack {
                track_id: String::new(),
                name: "Vocal".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackRename(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackEditError::TrackResourceMissing)
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
    fn audio_track_rename_waits_for_the_mixer_fence_before_closing() {
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

        worker
            .try_send(IdeWorkerCommand::RenameAudioTrack {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                name: "Vocal".to_owned(),
            })
            .unwrap();

        assert!(matches!(recv(&worker), IdeWorkerEvent::AudioTrackRenaming));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioTrack,
                reason: IdeWorkerCommandFailure::AudioTrackRename(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_track_activation_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioTrackActive {
                track_id: String::new(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioTrackActive,
                reason: IdeWorkerCommandFailure::AudioTrackActivation(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackEditError::TrackResourceMissing)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioTrackActive {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioTrackActivationChanging { active: false }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioTrackActive,
                reason: IdeWorkerCommandFailure::AudioTrackActivation(reason),
            } if matches!(reason.as_ref(), IdeAudioTrackEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn recorded_take_clip_creation_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 7).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));
        worker
            .try_send(IdeWorkerCommand::CreateAudioClipFromTake {
                recording_id: String::new(),
                name: "Take 1".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::CreateAudioClipFromTake,
                reason: IdeWorkerCommandFailure::RecordingClipCreation(reason),
            } if matches!(
                reason.as_ref(),
                IdeRecordingClipCreationError::InvalidRecordingIdentity
            )
        ));
        worker
            .try_send(IdeWorkerCommand::CreateAudioClipFromTake {
                recording_id: "018f47a2-6b2d-7c11-8a44-001122334477".to_owned(),
                name: "   ".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::CreateAudioClipFromTake,
                reason: IdeWorkerCommandFailure::RecordingClipCreation(reason),
            } if matches!(reason.as_ref(), IdeRecordingClipCreationError::InvalidClipName)
        ));
        worker
            .try_send(IdeWorkerCommand::CreateAudioClipFromTake {
                recording_id: "018f47a2-6b2d-7c11-8a44-001122334477".to_owned(),
                name: " Vocal take ".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipCreatingFromTake
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::CreateAudioClipFromTake,
                reason: IdeWorkerCommandFailure::RecordingClipCreation(reason),
            } if matches!(reason.as_ref(), IdeRecordingClipCreationError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_activation_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementPlacementActive {
                placement_id: String::new(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementPlacementActive,
                reason: IdeWorkerCommandFailure::ArrangementPlacementActivation(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementPlacementActive {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementActivationChanging { active: false }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementPlacementActive,
                reason: IdeWorkerCommandFailure::ArrangementPlacementActivation(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_move_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::MoveArrangementPlacement {
                placement_id: String::new(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::MoveArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementMove(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::MoveArrangementPlacement {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementMoving { start_tick: 1_920 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::MoveArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementMove(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_addition_validates_clip_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::AddArrangementPlacement {
                clip_id: String::new(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementAddition(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidClipIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::AddArrangementPlacement {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementAdding { start_tick: 1_920 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementAddition(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_duplication_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::DuplicateArrangementPlacement {
                placement_id: String::new(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementDuplication(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::DuplicateArrangementPlacement {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                start_tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementDuplicating { start_tick: 1_920 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementDuplication(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_scene_launch_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::AddArrangementSceneLaunch {
                scene_id: String::new(),
                tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidSceneIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::AddArrangementSceneLaunch {
                scene_id: "018f47a2-6b2d-7c11-8a44-001122334461".to_owned(),
                tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementSceneLaunchAdding { tick: 1_920 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchAddition(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_scene_launch_move_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::MoveArrangementSceneLaunch {
                launch_id: String::new(),
                tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchMove(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::MoveArrangementSceneLaunch {
                launch_id: "018f47a2-6b2d-7c11-8a44-001122334465".to_owned(),
                tick: 1_920,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementSceneLaunchMoving { tick: 1_920 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchMove(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_scene_launch_scene_edit_validates_identities_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementSceneLaunchScene {
                launch_id: String::new(),
                scene_id: "scene-a".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity
            )
        ));
        worker
            .try_send(IdeWorkerCommand::SetArrangementSceneLaunchScene {
                launch_id: "marker-a".to_owned(),
                scene_id: String::new(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidSceneIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementSceneLaunchScene {
                launch_id: "marker-a".to_owned(),
                scene_id: "scene-a".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementSceneLaunchSceneChanging
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchSceneEdit(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_scene_launch_removal_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveArrangementSceneLaunch {
                launch_id: String::new(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveArrangementSceneLaunch {
                launch_id: "018f47a2-6b2d-7c11-8a44-001122334465".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementSceneLaunchRemoving
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                reason: IdeWorkerCommandFailure::ArrangementSceneLaunchRemoval(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_split_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SplitArrangementPlacement {
                placement_id: String::new(),
                split_tick: 960,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SplitArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementSplit(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::SplitArrangementPlacement {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                split_tick: 960,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementSplitting { split_tick: 960 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SplitArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementSplit(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_duplication_publication_and_reopen_errors_retain_the_planned_identity() {
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122334465";
        let plan = crate::IdeArrangementPlacementEditPlan {
            mixer: crate::IdeMixerSavePlan {
                project_path: std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()),
                expected_project_sha256: [0; 32],
                resource_id: "mixer".to_owned(),
                resource_bytes: Vec::new(),
            },
            arrangement_id: "arrangement".to_owned(),
            arrangement_bytes: Vec::new(),
            addition: false,
            created_placement_id: Some(placement_id.to_owned()),
        };
        let Err(publication) = publish_arrangement_placement_creation_plan(plan) else {
            panic!("publication against a missing project must fail");
        };
        assert!(matches!(
            &publication,
            IdeArrangementPlacementEditError::Publication {
                created_placement_id: Some(retained),
                ..
            } if retained.as_ref() == placement_id
        ));

        assert!(matches!(
            arrangement_placement_creation_reopen_failure(
                Err(publication),
                IdeProjectOpenError::SelectedOutputUnavailable,
                IdeWorkerCommandFailure::ArrangementPlacementDuplication,
            ),
            IdeWorkerCommandFailure::ArrangementPlacementDuplication(reason)
                if matches!(reason.as_ref(),
                    IdeArrangementPlacementEditError::PublicationAndReopen {
                        created_placement_id: Some(retained),
                        reopen,
                        ..
                    } if retained.as_ref() == placement_id
                        && matches!(reopen.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable))
        ));
    }

    #[test]
    fn arrangement_duration_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementPlacementDuration {
                placement_id: String::new(),
                duration_ticks: 960,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementPlacementDuration,
                reason: IdeWorkerCommandFailure::ArrangementPlacementDuration(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::SetArrangementPlacementDuration {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                duration_ticks: 960,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementDurationChanging {
                duration_ticks: 960
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetArrangementPlacementDuration,
                reason: IdeWorkerCommandFailure::ArrangementPlacementDuration(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_front_trim_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::TrimArrangementPlacementFront {
                placement_id: String::new(),
                start_tick: 480,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::TrimArrangementPlacementFront,
                reason: IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::TrimArrangementPlacementFront {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
                start_tick: 480,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementFrontTrimming { start_tick: 480 }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::TrimArrangementPlacementFront,
                reason: IdeWorkerCommandFailure::ArrangementPlacementFrontTrim(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn arrangement_removal_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveArrangementPlacement {
                placement_id: String::new(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementRemoval(reason),
            } if matches!(
                reason.as_ref(),
                IdeArrangementPlacementEditError::InvalidPlacementIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveArrangementPlacement {
                placement_id: "018f47a2-6b2d-7c11-8a44-001122334464".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ArrangementPlacementRemoving
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveArrangementPlacement,
                reason: IdeWorkerCommandFailure::ArrangementPlacementRemoval(reason),
            } if matches!(reason.as_ref(), IdeArrangementPlacementEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_rename_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 3, 6).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: "   ".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRename(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRenameEditError::InvalidName)
        ));
        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: String::new(),
                name: "Lead vocal".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRename(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRenameEditError::InvalidClipIdentity)
        ));
        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: "\u{00e9}".repeat(121),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRename(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRenameEditError::InvalidName)
        ));

        worker
            .try_send(IdeWorkerCommand::RenameAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: format!(" {} ", "\u{00e9}".repeat(120)),
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::AudioClipRenaming));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRename(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRenameEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn automation_take_creation_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 3, 6).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        let request = |name: &str, target: IdeAutomationTarget| IdeAutomationTakeRequest {
            name: name.to_owned(),
            start_tick: 0,
            end_tick: 1_920,
            target,
            interpolation: crate::AutomationInterpolation::Linear,
            points: vec![crate::AutomationPoint::new(
                0,
                crate::AutomationValue::Integer(1_000_000),
            )],
        };
        let main_target = IdeAutomationTarget::Main {
            parameter: crate::AutomationMainParameter::Gain,
        };

        worker
            .try_send(IdeWorkerCommand::AddAutomationTake {
                request: request("   ", main_target.clone()),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddAutomationTake,
                reason: IdeWorkerCommandFailure::AutomationTakeCreation(reason),
            } if matches!(reason.as_ref(), IdeAutomationTakeCreationError::InvalidName)
        ));

        worker
            .try_send(IdeWorkerCommand::AddAutomationTake {
                request: request(
                    "Fade out",
                    IdeAutomationTarget::Channel {
                        track_id: String::new(),
                        parameter: crate::AutomationChannelParameter::Gain,
                    },
                ),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddAutomationTake,
                reason: IdeWorkerCommandFailure::AutomationTakeCreation(reason),
            } if matches!(reason.as_ref(), IdeAutomationTakeCreationError::InvalidTargetIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::AddAutomationTake {
                request: request("Fade out", main_target),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AutomationTakeAdding
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddAutomationTake,
                reason: IdeWorkerCommandFailure::AutomationTakeCreation(reason),
            } if matches!(reason.as_ref(), IdeAutomationTakeCreationError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    fn labelled(id: &str, label: &'static str) -> UndoEntry {
        UndoEntry {
            resource_id: id.to_owned(),
            bytes: id.as_bytes().to_vec(),
            label,
        }
    }

    fn entry(id: &str) -> UndoEntry {
        labelled(id, "clip gain")
    }

    #[test]
    fn undo_history_bounds_depth_and_clears_redo_on_a_new_edit() {
        let mut history = UndoHistory::default();
        for index in 0..MAX_UNDO_DEPTH + 5 {
            history.record_edit(entry(&format!("edit-{index}")));
        }
        assert_eq!(history.undo.len(), MAX_UNDO_DEPTH, "depth is bounded");
        assert_eq!(
            history.undo.first().unwrap().resource_id,
            "edit-5",
            "the oldest entries are dropped first"
        );

        let peeked = history.peek(false).unwrap().clone();
        assert_eq!(
            peeked.resource_id,
            format!("edit-{}", MAX_UNDO_DEPTH + 4),
            "undo targets the newest entry"
        );
        assert_eq!(
            history.undo.len(),
            MAX_UNDO_DEPTH,
            "peeking leaves the entry in place until the restore commits"
        );
        history.complete_restore(false, Some(labelled("inverse", "scene rename")));
        assert_eq!(
            history.undo.len(),
            MAX_UNDO_DEPTH - 1,
            "a committed restore consumes its entry"
        );
        assert_eq!(history.redo.len(), 1, "an undo feeds the redo stack");
        assert!(
            matches!(
                history.state_event(),
                IdeWorkerEvent::UndoStateChanged {
                    undo_label: Some("clip gain"),
                    redo_label: Some("scene rename"),
                }
            ),
            "each stack reports its own newest label"
        );

        history.record_edit(entry("new-edit"));
        assert!(history.redo.is_empty(), "a new edit invalidates redo");

        history.clear();
        assert!(matches!(
            history.state_event(),
            IdeWorkerEvent::UndoStateChanged {
                undo_label: None,
                redo_label: None,
            }
        ));
    }

    struct CapturedEdit {
        parent: std::path::PathBuf,
        root: std::path::PathBuf,
        mixer_id: String,
        original: Vec<u8>,
        replacement: String,
        outcome: hyper_storage_engine::ProjectResourcePublicationOutcome,
    }

    impl Drop for CapturedEdit {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.parent);
        }
    }

    /// A real project whose mixer has been replaced once, so the outcome under
    /// test is the one production publishes rather than a hand-built value.
    fn captured_mixer_edit(name: &str) -> CapturedEdit {
        let parent = std::env::temp_dir().join(format!("hyper-undo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let created = hyper_storage_engine::create_project(
            &parent,
            hyper_storage_engine::ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let root = created.project().root().to_path_buf();
        let package = hyper_storage_engine::verify_project_package(&root).unwrap();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == hyper_project_model::ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let original = package.read_resource(&mixer_id).unwrap();
        let replacement = String::from_utf8(original.clone())
            .unwrap()
            .replace("\"gain_millionths\":1000000", "\"gain_millionths\":500000");
        assert_ne!(replacement.as_bytes(), original.as_slice());
        let outcome = hyper_storage_engine::publish_project_resource(
            &root,
            hyper_storage_engine::ProjectResourcePublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: &mixer_id,
                expected_role: hyper_project_model::ResourceRole::Mixer,
                resource_bytes: replacement.as_bytes(),
            },
        )
        .unwrap();
        CapturedEdit {
            parent,
            root,
            mixer_id,
            original,
            replacement,
            outcome,
        }
    }

    #[test]
    fn history_captures_a_changed_replacement_and_names_the_command() {
        let edit = captured_mixer_edit("capture");
        let (mixer_id, original, outcome) = (&edit.mixer_id, &edit.original, &edit.outcome);
        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut history = UndoHistory::default();
        observe_history(
            &mut history,
            &ProjectMutationAttempt::Save(Ok(outcome.clone())),
            IdeWorkerCommandKind::SaveProject,
            &events,
        );
        assert_eq!(history.undo.len(), 1, "a changed replacement is captured");
        assert_eq!(&history.undo[0].resource_id, mixer_id);
        assert_eq!(&history.undo[0].bytes, original);
        assert_eq!(history.undo[0].label, "mixer save");
        assert!(matches!(
            receiver.try_recv(),
            Ok(IdeWorkerEvent::UndoStateChanged {
                undo_label: Some("mixer save"),
                ..
            })
        ));

        let entry = history.peek(false).unwrap().clone();
        observe_history(
            &mut history,
            &ProjectMutationAttempt::RestoreResource {
                entry: entry.clone(),
                redo: false,
                result: Ok(outcome.clone()),
            },
            IdeWorkerCommandKind::Undo,
            &events,
        );
        assert!(
            history.undo.is_empty(),
            "the applied entry is consumed only once the restore commits"
        );
        assert_eq!(
            history.redo.len(),
            1,
            "an undo's own previous bytes become the redo entry"
        );
        assert_eq!(
            &history.redo[0].resource_id, mixer_id,
            "aimed at the same resource, so the redo can actually be applied"
        );
        assert_eq!(&history.redo[0].bytes, original);
        assert_eq!(history.redo[0].label, "mixer save");
    }

    #[test]
    fn history_captures_nothing_from_an_unchanged_republication() {
        let edit = captured_mixer_edit("unchanged");
        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut history = UndoHistory::default();
        let root = &edit.root;
        let mixer_id = &edit.mixer_id;
        let replacement = &edit.replacement;
        let identical = hyper_storage_engine::publish_project_resource(
            root,
            hyper_storage_engine::ProjectResourcePublicationRequest {
                expected_project_sha256: *hyper_storage_engine::verify_project_package(root)
                    .unwrap()
                    .manifest_sha256(),
                resource_id: mixer_id,
                expected_role: hyper_project_model::ResourceRole::Mixer,
                resource_bytes: replacement.as_bytes(),
            },
        )
        .unwrap();
        observe_history(
            &mut history,
            &ProjectMutationAttempt::Save(Ok(identical)),
            IdeWorkerCommandKind::SaveProject,
            &events,
        );
        assert!(
            history.undo.is_empty(),
            "an unchanged publication captures nothing"
        );
        assert!(
            receiver.try_recv().is_err(),
            "and announces no history change"
        );
    }

    #[test]
    fn a_refused_undo_keeps_its_entry_for_a_later_attempt() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: false,
                ..TestPlaybackSession::default()
            }),
        };
        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut state = WorkerState::Running(running);
        let mut history = UndoHistory::default();
        history.record_edit(entry("scene"));
        while receiver.try_recv().is_ok() {}

        assert!(restore_edit(&mut state, &events, &mut history, false));
        assert!(
            matches!(
                receiver.try_recv(),
                Ok(IdeWorkerEvent::CommandFailed {
                    command: IdeWorkerCommandKind::Undo,
                    ..
                })
            ),
            "an undo that never reaches the project is reported as a failure"
        );
        assert_eq!(
            history.undo.len(),
            1,
            "and the entry survives so the user can retry"
        );
        assert_eq!(history.peek(false).unwrap().resource_id, "scene");
    }

    #[test]
    fn closing_a_project_drops_its_history_through_the_dispatcher() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let (events, receiver) = std::sync::mpsc::sync_channel(16);
        let mut state = WorkerState::Running(running);
        let mut history = UndoHistory::default();
        let mut native = None;
        history.record_edit(labelled("scene", "scene rename"));
        while receiver.try_recv().is_ok() {}

        assert!(process_command(
            &mut state,
            IdeWorkerCommand::CloseProject,
            &events,
            &mut native,
            &mut history,
            None,
        ));
        assert!(
            history.undo.is_empty() && history.redo.is_empty(),
            "entries from the closed project cannot target the next one"
        );
        assert!(
            matches!(
                receiver.try_recv(),
                Ok(IdeWorkerEvent::UndoStateChanged {
                    undo_label: None,
                    redo_label: None,
                })
            ),
            "and the controls are told before anything else happens"
        );
    }

    /// The rule the previous hand-maintained list broke: adding, duplicating,
    /// or splitting a placement rewrites the same arrangement document as
    /// moving one, so it must be captured or an undo reverts work it never saw.
    #[test]
    fn placement_creation_is_captured_because_it_replaces_the_arrangement() {
        let edit = captured_mixer_edit("placement-capture");
        let publication = || ArrangementPlacementCreationPublication {
            outcome: edit.outcome.clone(),
            placement_id: "placement".to_owned(),
        };
        for attempt in [
            ProjectMutationAttempt::AddArrangementPlacement {
                start_tick: 0,
                result: Ok(publication()),
            },
            ProjectMutationAttempt::DuplicateArrangementPlacement {
                start_tick: 480,
                result: Ok(publication()),
            },
            ProjectMutationAttempt::SplitArrangementPlacement {
                split_tick: 240,
                result: Ok(publication()),
            },
        ] {
            assert!(
                matches!(history_effect(&attempt), HistoryEffect::Replacement(_)),
                "creating a placement replaces the arrangement document"
            );
        }

        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut history = UndoHistory::default();
        observe_history(
            &mut history,
            &ProjectMutationAttempt::AddArrangementPlacement {
                start_tick: 0,
                result: Ok(publication()),
            },
            IdeWorkerCommandKind::AddArrangementPlacement,
            &events,
        );
        assert_eq!(history.undo.len(), 1, "and reaches the history");
        assert_eq!(history.undo[0].bytes, edit.original);
        assert_eq!(history.undo[0].label, "placement");
        assert!(receiver.try_recv().is_ok());
    }

    /// A change no stored document can express must drop the history rather
    /// than leave entries that would silently revert it.
    /// Exhaustiveness proves every mutation is listed, not that it is classified
    /// correctly. These three were listed and never checked.
    #[test]
    fn the_timeline_and_loop_edits_are_classified_as_undoable_replacements() {
        let edit = captured_mixer_edit("classified");
        let outcome = edit.outcome.clone();
        for (attempt, label, kind) in [
            (
                ProjectMutationAttempt::SetTempo {
                    microseconds_per_quarter: 250_000,
                    result: Ok(outcome.clone()),
                },
                "tempo",
                IdeWorkerCommandKind::SetTempo,
            ),
            (
                ProjectMutationAttempt::SetMeter {
                    numerator: 3,
                    denominator: 4,
                    result: Ok(outcome.clone()),
                },
                "time signature",
                IdeWorkerCommandKind::SetMeter,
            ),
            (
                ProjectMutationAttempt::SetArrangementLoopRange {
                    result: Ok(outcome.clone()),
                },
                "loop range",
                IdeWorkerCommandKind::SetArrangementLoopRange,
            ),
        ] {
            assert!(
                matches!(history_effect(&attempt), HistoryEffect::Replacement(_)),
                "{label} replaces a resource, so it must be undoable"
            );
            assert_eq!(
                undo_label(kind),
                label,
                "and the button must name the edit it reverses"
            );

            let (events, receiver) = std::sync::mpsc::sync_channel(8);
            let mut history = UndoHistory::default();
            observe_history(&mut history, &attempt, kind, &events);
            assert_eq!(
                history.undo.len(),
                1,
                "{label} reaches the history through observe_history"
            );
            assert_eq!(history.undo[0].label, label);
            assert!(receiver.try_recv().is_ok());
        }

        for failed in [
            ProjectMutationAttempt::SetTempo {
                microseconds_per_quarter: 250_000,
                result: Err(IdeTempoEditError::TimelineResourceMissing),
            },
            ProjectMutationAttempt::SetArrangementLoopRange {
                result: Err(IdeArrangementPlacementEditError::ArrangementResourceMissing),
            },
        ] {
            assert!(
                matches!(history_effect(&failed), HistoryEffect::None),
                "a failed edit changed nothing, so history stands"
            );
        }
    }

    #[test]
    fn a_structural_change_invalidates_the_retained_history() {
        assert!(matches!(
            history_effect(&ProjectMutationAttempt::EditControlMapping {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: "binding".to_owned()
                },
                result: Ok(7),
            }),
            HistoryEffect::Invalidate
        ));

        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut history = UndoHistory::default();
        history.record_edit(labelled("scene", "scene rename"));
        history.record_restore(false, labelled("undone", "clip gain"));
        while receiver.try_recv().is_ok() {}

        observe_history(
            &mut history,
            &ProjectMutationAttempt::EditControlMapping {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: "binding".to_owned(),
                },
                result: Ok(7),
            },
            IdeWorkerCommandKind::EditControlMapping,
            &events,
        );
        assert!(
            history.undo.is_empty() && history.redo.is_empty(),
            "both stacks are dropped, not just the undo side"
        );
        assert!(
            matches!(
                receiver.try_recv(),
                Ok(IdeWorkerEvent::UndoStateChanged {
                    undo_label: None,
                    redo_label: None,
                })
            ),
            "and the controls stop offering an undo that no longer fits"
        );

        let mut untouched = UndoHistory::default();
        untouched.record_edit(labelled("scene", "scene rename"));
        observe_history(
            &mut untouched,
            &ProjectMutationAttempt::EditControlMapping {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: "binding".to_owned(),
                },
                result: Err(IdeControlMappingEditError::InvalidBindingIdentity),
            },
            IdeWorkerCommandKind::EditControlMapping,
            &events,
        );
        assert_eq!(
            untouched.undo.len(),
            1,
            "a failed mutation changed nothing, so history stands"
        );
    }

    #[test]
    fn committed_audio_clip_removal_warning_invalidates_history_and_stays_truthful() {
        let committed_warning = || {
            IdeAudioClipRemovalError::PublicationCommittedWithWarning(
                IdeAudioClipRemovalCommittedWarning::new(std::io::Error::other(
                    "simulated cleanup warning",
                )),
            )
        };
        let attempt = ProjectMutationAttempt::RemoveAudioClip(Err(committed_warning()));
        assert!(matches!(history_effect(&attempt), HistoryEffect::Invalidate));

        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut history = UndoHistory::default();
        history.record_edit(labelled("scene", "scene rename"));
        history.record_restore(false, labelled("undone", "clip gain"));
        observe_history(
            &mut history,
            &attempt,
            IdeWorkerCommandKind::RemoveAudioClip,
            &events,
        );
        assert!(history.undo.is_empty() && history.redo.is_empty());
        assert!(matches!(
            receiver.try_recv(),
            Ok(IdeWorkerEvent::UndoStateChanged {
                undo_label: None,
                redo_label: None,
            })
        ));

        assert!(matches!(
            audio_clip_removal_failure_event(committed_warning()),
            IdeWorkerEvent::AudioClipRemovalCommittedWithWarning { warning }
                if warning.publication_committed()
                    && warning.to_string().contains("audio clip was removed")
        ));
        let reopen = project_mutation_reopen_failure(
            ProjectMutationAttempt::RemoveAudioClip(Err(committed_warning())),
            IdeProjectOpenError::SelectedOutputUnavailable,
        );
        assert!(matches!(
            reopen,
            IdeWorkerCommandFailure::AudioClipRemoval(reason)
                if reason.publication_committed()
                    && reason.to_string().contains("was removed")
                    && reason.to_string().contains("could not be reopened")
        ));

        let unpublished = ProjectMutationAttempt::RemoveAudioClip(Err(
            IdeAudioClipRemovalError::InvalidClipIdentity,
        ));
        assert!(matches!(history_effect(&unpublished), HistoryEffect::None));
        assert!(matches!(
            audio_clip_removal_failure_event(IdeAudioClipRemovalError::InvalidClipIdentity),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveAudioClip,
                ..
            }
        ));
    }

    #[test]
    fn audio_import_history_effect_distinguishes_zero_partial_and_complete_batches() {
        let completed = ProjectMutationAttempt::ImportAudio(Ok(IdeAudioImportBatchOutcome {
            imported: 2,
            final_revision: 3,
            clip_ids: vec!["clip-a".to_owned(), "clip-b".to_owned()],
        }));
        let partial = ProjectMutationAttempt::ImportAudio(Err(IdeAudioImportBatchFailure {
            completed: 1,
            total: 2,
            path: PathBuf::from("second.flac"),
            source: Box::new(IdeAudioImportError::UnsupportedContainer),
        }));
        for attempt in [&completed, &partial] {
            let (events, receiver) = std::sync::mpsc::sync_channel(4);
            let mut history = UndoHistory::default();
            history.record_edit(labelled("undo", "clip gain"));
            history.record_restore(false, labelled("redo", "scene rename"));

            observe_history(
                &mut history,
                attempt,
                IdeWorkerCommandKind::ImportAudio,
                &events,
            );

            assert!(history.undo.is_empty() && history.redo.is_empty());
            assert!(matches!(
                receiver.try_recv(),
                Ok(IdeWorkerEvent::UndoStateChanged {
                    undo_label: None,
                    redo_label: None,
                })
            ));
        }

        let zero = ProjectMutationAttempt::ImportAudio(Err(IdeAudioImportBatchFailure {
            completed: 0,
            total: 1,
            path: PathBuf::from("refused.wav"),
            source: Box::new(IdeAudioImportError::UnsupportedContainer),
        }));
        let (events, receiver) = std::sync::mpsc::sync_channel(4);
        let mut history = UndoHistory::default();
        history.record_edit(labelled("undo", "clip gain"));
        history.record_restore(false, labelled("redo", "scene rename"));

        observe_history(
            &mut history,
            &zero,
            IdeWorkerCommandKind::ImportAudio,
            &events,
        );

        assert_eq!(history.undo.len(), 1);
        assert_eq!(history.redo.len(), 1);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn audio_import_reopen_failures_retain_complete_and_partial_outcomes() {
        let completed = project_mutation_reopen_failure(
            ProjectMutationAttempt::ImportAudio(Ok(IdeAudioImportBatchOutcome {
                imported: 2,
                final_revision: 3,
                clip_ids: vec!["clip-a".to_owned(), "clip-b".to_owned()],
            })),
            IdeProjectOpenError::SelectedOutputUnavailable,
        );
        assert!(matches!(
            completed,
            IdeWorkerCommandFailure::AudioImportReopen { outcome, source }
                if outcome.imported() == 2
                    && outcome.final_revision() == 3
                    && matches!(source.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable)
        ));

        let partial = project_mutation_reopen_failure(
            ProjectMutationAttempt::ImportAudio(Err(IdeAudioImportBatchFailure {
                completed: 1,
                total: 2,
                path: PathBuf::from("second.flac"),
                source: Box::new(IdeAudioImportError::UnsupportedContainer),
            })),
            IdeProjectOpenError::SelectedOutputUnavailable,
        );
        assert!(matches!(
            partial,
            IdeWorkerCommandFailure::AudioImportAndReopen { import, reopen }
                if import.completed() == 1
                    && import.total() == 2
                    && matches!(import.reason(), IdeAudioImportError::UnsupportedContainer)
                    && matches!(reopen.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable)
        ));
    }

    #[test]
    fn the_controls_report_the_newest_entry_on_each_stack() {
        let mut history = UndoHistory::default();
        history.record_edit(labelled("first", "scene rename"));
        history.record_edit(labelled("second", "clip gain"));
        history.record_restore(false, labelled("older-undone", "track rename"));
        history.record_restore(false, labelled("newest-undone", "clip loop"));

        assert!(
            matches!(
                history.state_event(),
                IdeWorkerEvent::UndoStateChanged {
                    undo_label: Some("clip gain"),
                    redo_label: Some("clip loop"),
                }
            ),
            "each button names the entry it would apply, not the oldest one              still retained: {:?}",
            history.state_event()
        );
    }

    #[test]
    fn the_depth_bound_is_twenty() {
        assert_eq!(MAX_UNDO_DEPTH, 20);
        let mut history = UndoHistory::default();
        for index in 0..25 {
            history.record_edit(entry(&format!("edit-{index}")));
        }
        assert_eq!(history.undo.len(), 20);
        assert_eq!(history.undo.first().unwrap().resource_id, "edit-5");

        let mut redo_side = UndoHistory::default();
        for index in 0..25 {
            redo_side.record_restore(false, entry(&format!("undone-{index}")));
        }
        assert_eq!(
            redo_side.redo.len(),
            20,
            "the bound holds on the redo stack too"
        );
        assert_eq!(redo_side.redo.first().unwrap().resource_id, "undone-5");
    }

    #[test]
    fn the_redo_direction_reads_and_feeds_the_opposite_stacks() {
        let mut history = UndoHistory::default();
        history.record_restore(false, labelled("undone", "scene rename"));
        history.record_edit(labelled("edited", "clip gain"));
        assert!(
            history.redo.is_empty(),
            "the edit cleared redo, so only the undo stack is populated"
        );

        history.record_restore(false, labelled("displaced", "scene rename"));
        assert_eq!(history.peek(false).unwrap().resource_id, "edited");
        assert_eq!(
            history.peek(true).unwrap().resource_id,
            "displaced",
            "redo reads the redo stack, not the undo stack"
        );

        history.complete_restore(true, Some(labelled("back", "scene rename")));
        assert!(
            history.redo.is_empty(),
            "a committed redo consumes from the redo stack"
        );
        assert_eq!(
            history.undo.len(),
            2,
            "and feeds the undo stack with what it displaced"
        );
        assert_eq!(history.peek(false).unwrap().resource_id, "back");
    }

    #[test]
    fn a_redo_reports_itself_as_a_redo() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: false,
                ..TestPlaybackSession::default()
            }),
        };
        let (events, receiver) = std::sync::mpsc::sync_channel(8);
        let mut state = WorkerState::Running(running);
        let mut history = UndoHistory::default();
        history.record_restore(false, labelled("scene", "scene rename"));
        while receiver.try_recv().is_ok() {}

        assert!(restore_edit(&mut state, &events, &mut history, true));
        assert!(
            matches!(
                receiver.try_recv(),
                Ok(IdeWorkerEvent::CommandFailed {
                    command: IdeWorkerCommandKind::Redo,
                    reason: IdeWorkerCommandFailure::EditRestore(_),
                })
            ),
            "a redo that never reaches the project is reported as a redo, \
             classified as a restore failure"
        );
        assert_eq!(history.redo.len(), 1, "and keeps its entry");
        assert!(history.undo.is_empty());
    }

    #[test]
    fn a_permanently_refused_restore_is_discarded_so_older_entries_stay_reachable() {
        let mut history = UndoHistory::default();
        history.record_edit(labelled("older", "clip gain"));
        history.record_edit(labelled("dead", "track rename"));

        let (event, consumed) = restore_failure_event(
            &mut history,
            &labelled("dead", "track rename"),
            false,
            IdeResourceRestoreError::ResourceMissing,
        );
        assert!(consumed, "a refusal that can never succeed is discarded");
        assert!(matches!(
            event,
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Undo,
                ..
            }
        ));
        assert_eq!(
            history.peek(false).unwrap().resource_id,
            "older",
            "the entry behind it becomes reachable"
        );

        let (retryable, kept) = restore_failure_event(
            &mut history,
            &labelled("older", "clip gain"),
            false,
            IdeResourceRestoreError::CapacityUnavailable,
        );
        assert!(!kept, "a retryable refusal keeps its entry");
        assert!(matches!(retryable, IdeWorkerEvent::CommandFailed { .. }));
        assert_eq!(history.undo.len(), 1);
    }

    #[test]
    fn a_restore_that_committed_before_failing_consumes_its_entry() {
        let mut history = UndoHistory::default();
        history.record_edit(labelled("scene", "scene rename"));
        let edit = captured_mixer_edit("committed");
        let reason = IdeResourceRestoreError::ReopenAfterPreparation {
            preparation: Box::new(IdeResourceRestoreError::ReopenAfterPublication {
                outcome: Box::new(edit.outcome.clone()),
                source: Box::new(IdeProjectOpenError::PlaybackSessionGenerationExhausted),
            }),
            reopen: Box::new(IdeProjectOpenError::PlaybackSessionGenerationExhausted),
        };
        assert!(
            reason.publication_committed(),
            "a committed publication is visible through the reopen wrapper"
        );

        let (event, consumed) = restore_failure_event(
            &mut history,
            &labelled("scene", "scene rename"),
            false,
            reason,
        );
        assert!(consumed);
        assert!(
            matches!(
                event,
                IdeWorkerEvent::EditRestoreCommittedWithWarning {
                    redo: false,
                    label: "scene rename",
                    ..
                }
            ),
            "the user is told the restore reached the project"
        );
        assert!(
            history.undo.is_empty(),
            "history agrees with disk rather than offering the undo again"
        );
    }

    #[test]
    fn the_restore_error_taxonomy_separates_committed_from_permanent() {
        let edit = captured_mixer_edit("taxonomy");
        let committed = IdeResourceRestoreError::ReopenAfterPublication {
            outcome: Box::new(edit.outcome.clone()),
            source: Box::new(IdeProjectOpenError::PlaybackSessionGenerationExhausted),
        };
        assert!(committed.publication_committed());
        assert!(!committed.is_permanent_refusal());

        for retryable in [
            IdeResourceRestoreError::CapacityUnavailable,
            IdeResourceRestoreError::NativeClose(IdeNativeCloseFailure::RuntimeFailed),
            IdeResourceRestoreError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            )),
        ] {
            assert!(
                !retryable.publication_committed(),
                "{retryable} did not reach the project"
            );
            assert!(
                !retryable.is_permanent_refusal(),
                "{retryable} may succeed later"
            );
        }

        for permanent in [
            IdeResourceRestoreError::ResourceMissing,
            IdeResourceRestoreError::CandidateInvalid(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            )),
        ] {
            assert!(
                permanent.is_permanent_refusal(),
                "{permanent} cannot succeed later"
            );
            assert!(!permanent.publication_committed());
        }
    }

    #[test]
    fn every_undoable_command_names_the_edit_it_reverses() {
        for (command, expected) in [
            (IdeWorkerCommandKind::SaveProject, "mixer save"),
            (
                IdeWorkerCommandKind::PrepareRuntime,
                "Runtime preparation save",
            ),
            (IdeWorkerCommandKind::RenameAudioTrack, "track rename"),
            (
                IdeWorkerCommandKind::SetAudioTrackActive,
                "track activation",
            ),
            (IdeWorkerCommandKind::AddArrangementPlacement, "placement"),
            (
                IdeWorkerCommandKind::MoveArrangementPlacement,
                "placement move",
            ),
            (
                IdeWorkerCommandKind::SetArrangementPlacementDuration,
                "placement duration",
            ),
            (
                IdeWorkerCommandKind::TrimArrangementPlacementFront,
                "placement trim",
            ),
            (
                IdeWorkerCommandKind::RemoveArrangementPlacement,
                "placement removal",
            ),
            (
                IdeWorkerCommandKind::SetArrangementPlacementActive,
                "placement activation",
            ),
            (
                IdeWorkerCommandKind::DuplicateArrangementPlacement,
                "placement duplication",
            ),
            (
                IdeWorkerCommandKind::SplitArrangementPlacement,
                "placement split",
            ),
            (
                IdeWorkerCommandKind::AddArrangementSceneLaunch,
                "scene marker",
            ),
            (
                IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                "scene marker move",
            ),
            (
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                "scene marker reassignment",
            ),
            (
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                "scene marker removal",
            ),
            (IdeWorkerCommandKind::RenameAudioClip, "clip rename"),
            (IdeWorkerCommandKind::SetAudioClipActive, "clip activation"),
            (IdeWorkerCommandKind::SetAudioClipGain, "clip gain"),
            (IdeWorkerCommandKind::SetAudioClipFades, "clip fades"),
            (IdeWorkerCommandKind::SetAudioClipLoop, "clip loop"),
            (IdeWorkerCommandKind::SetAudioClipReverse, "clip direction"),
            (
                IdeWorkerCommandKind::SetAudioClipSourceStart,
                "clip source slip",
            ),
            (IdeWorkerCommandKind::AddScene, "scene"),
            (IdeWorkerCommandKind::DuplicateScene, "scene duplication"),
            (IdeWorkerCommandKind::RenameScene, "scene rename"),
            (IdeWorkerCommandKind::SetSceneSlot, "scene slot edit"),
            (IdeWorkerCommandKind::SetSceneActive, "scene activation"),
            (IdeWorkerCommandKind::AddAutomationTake, "automation take"),
            (
                IdeWorkerCommandKind::SetAutomationTakeActive,
                "automation activation",
            ),
        ] {
            assert_eq!(undo_label(command), expected, "label for {command:?}");
        }
        assert_eq!(
            undo_label(IdeWorkerCommandKind::Transport),
            "edit",
            "commands that never produce history share one fallback"
        );
    }

    #[test]
    fn undo_without_history_fails_without_touching_the_project() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                ..
            }
        ));
        worker.try_send(IdeWorkerCommand::Undo).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Undo,
                reason: IdeWorkerCommandFailure::NothingToRestore,
            }
        ));
        worker.try_send(IdeWorkerCommand::Redo).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Redo,
                reason: IdeWorkerCommandFailure::NothingToRestore,
            }
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn export_reports_its_own_command_and_refuses_without_a_prepared_project() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                ..
            }
        ));

        worker
            .try_send(IdeWorkerCommand::ExportProject {
                wav_path: std::path::PathBuf::from("export.wav"),
                block_frames: 1_024,
            })
            .unwrap();
        assert!(
            matches!(recv(&worker), IdeWorkerEvent::ProjectExporting),
            "the export announces itself before doing blocking work"
        );
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ExportProject,
                reason: IdeWorkerCommandFailure::ProjectExport(reason),
            } if matches!(reason.as_ref(), IdeProjectExportError::ProjectSnapshotUnavailable)
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn control_mapping_edit_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::EditControlMapping {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: String::new(),
                },
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::EditControlMapping,
                reason: IdeWorkerCommandFailure::ControlMappingEdit(reason),
            } if matches!(reason.as_ref(), IdeControlMappingEditError::InvalidBindingIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::EditControlMapping {
                edit: IdeControlMappingEdit::SetEnabled {
                    binding_id: "018f47a2-6b2d-7c11-8a44-001122334475".to_owned(),
                    enabled: false,
                },
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::ControlMappingEditing {
                edit: IdeControlMappingEdit::SetEnabled { enabled: false, .. }
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::EditControlMapping,
                reason: IdeWorkerCommandFailure::ControlMappingEdit(reason),
            } if matches!(reason.as_ref(), IdeControlMappingEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn automation_take_activation_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAutomationTakeActive {
                take_id: String::new(),
                enabled: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAutomationTakeActive,
                reason: IdeWorkerCommandFailure::AutomationTakeActivation(reason),
            } if matches!(reason.as_ref(), IdeAutomationTakeEditError::InvalidTakeIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAutomationTakeActive {
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
                enabled: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AutomationTakeActivationChanging { enabled: false }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAutomationTakeActive,
                reason: IdeWorkerCommandFailure::AutomationTakeActivation(reason),
            } if matches!(reason.as_ref(), IdeAutomationTakeEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn scene_rename_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 3, 6).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RenameScene {
                scene_id: "scene-id".to_owned(),
                name: "   ".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameScene,
                reason: IdeWorkerCommandFailure::SceneRename(reason),
            } if matches!(reason.as_ref(), IdeSceneRenameError::InvalidName)
        ));
        worker
            .try_send(IdeWorkerCommand::RenameScene {
                scene_id: String::new(),
                name: "Verse".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameScene,
                reason: IdeWorkerCommandFailure::SceneRename(reason),
            } if matches!(reason.as_ref(), IdeSceneRenameError::InvalidSceneIdentity)
        ));
        worker
            .try_send(IdeWorkerCommand::RenameScene {
                scene_id: "scene-id".to_owned(),
                name: format!(" {} ", "\u{00e9}".repeat(120)),
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::SceneRenaming));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameScene,
                reason: IdeWorkerCommandFailure::SceneRename(reason),
            } if matches!(reason.as_ref(), IdeSceneRenameError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn scene_creation_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 4, 8).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::AddScene {
                name: "   ".to_owned(),
                quantization: IdeSceneLaunchQuantization::Immediate,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddScene,
                reason: IdeWorkerCommandFailure::SceneCreation(reason),
            } if matches!(reason.as_ref(), IdeSceneCreationError::InvalidName)
        ));

        worker
            .try_send(IdeWorkerCommand::AddScene {
                name: "Verse".to_owned(),
                quantization: IdeSceneLaunchQuantization::Grid { quantum_ticks: 0 },
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddScene,
                reason: IdeWorkerCommandFailure::SceneCreation(reason),
            } if matches!(reason.as_ref(), IdeSceneCreationError::InvalidQuantization)
        ));

        worker
            .try_send(IdeWorkerCommand::AddScene {
                name: " Verse ".to_owned(),
                quantization: IdeSceneLaunchQuantization::Grid {
                    quantum_ticks: 1_920,
                },
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::SceneAdding));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::AddScene,
                reason: IdeWorkerCommandFailure::SceneCreation(reason),
            } if matches!(reason.as_ref(), IdeSceneCreationError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn scene_duplication_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 5, 10).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        for (scene_id, name, expected) in [
            (
                "",
                "Copy",
                IdeSceneDuplicationError::InvalidSourceSceneIdentity,
            ),
            ("scene", "   ", IdeSceneDuplicationError::InvalidName),
        ] {
            worker
                .try_send(IdeWorkerCommand::DuplicateScene {
                    scene_id: scene_id.to_owned(),
                    name: name.to_owned(),
                })
                .unwrap();
            let event = recv(&worker);
            assert!(matches!(
                event,
                IdeWorkerEvent::CommandFailed {
                    command: IdeWorkerCommandKind::DuplicateScene,
                    reason: IdeWorkerCommandFailure::SceneDuplication(reason),
                } if core::mem::discriminant(reason.as_ref()) == core::mem::discriminant(&expected)
            ));
        }

        worker
            .try_send(IdeWorkerCommand::DuplicateScene {
                scene_id: "scene".to_owned(),
                name: "\u{00e9}".repeat(121),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateScene,
                reason: IdeWorkerCommandFailure::SceneDuplication(reason),
            } if matches!(reason.as_ref(), IdeSceneDuplicationError::InvalidName)
        ));

        worker
            .try_send(IdeWorkerCommand::DuplicateScene {
                scene_id: "scene".to_owned(),
                name: " Copy ".to_owned(),
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::SceneDuplicating));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateScene,
                reason: IdeWorkerCommandFailure::SceneDuplication(reason),
            } if matches!(reason.as_ref(), IdeSceneDuplicationError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn scene_slot_edit_validates_identities_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 5, 10).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        for (scene_id, track_id, edit, expected) in [
            (
                "",
                "track",
                IdeSceneSlotEdit::Stop,
                IdeSceneSlotEditError::InvalidSceneIdentity,
            ),
            (
                "scene",
                "",
                IdeSceneSlotEdit::Continue,
                IdeSceneSlotEditError::InvalidTrackIdentity,
            ),
            (
                "scene",
                "track",
                IdeSceneSlotEdit::Play {
                    clip_id: String::new(),
                },
                IdeSceneSlotEditError::InvalidClipIdentity,
            ),
        ] {
            worker
                .try_send(IdeWorkerCommand::SetSceneSlot {
                    scene_id: scene_id.to_owned(),
                    track_id: track_id.to_owned(),
                    edit,
                })
                .unwrap();
            let event = recv(&worker);
            assert!(matches!(
                event,
                IdeWorkerEvent::CommandFailed {
                    command: IdeWorkerCommandKind::SetSceneSlot,
                    reason: IdeWorkerCommandFailure::SceneSlotEdit(reason),
                } if core::mem::discriminant(reason.as_ref()) == core::mem::discriminant(&expected)
            ));
        }

        worker
            .try_send(IdeWorkerCommand::SetSceneSlot {
                scene_id: "scene".to_owned(),
                track_id: "track".to_owned(),
                edit: IdeSceneSlotEdit::Stop,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::SceneSlotChanging {
                edit: IdeSceneSlotEdit::Stop
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetSceneSlot,
                reason: IdeWorkerCommandFailure::SceneSlotEdit(reason),
            } if matches!(reason.as_ref(), IdeSceneSlotEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn scene_activation_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetSceneActive {
                scene_id: String::new(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetSceneActive,
                reason: IdeWorkerCommandFailure::SceneActivation(reason),
            } if matches!(reason.as_ref(), IdeSceneEditError::InvalidSceneIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetSceneActive {
                scene_id: "scene-id".to_owned(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::SceneActivationChanging { active: false }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetSceneActive,
                reason: IdeWorkerCommandFailure::SceneActivation(reason),
            } if matches!(reason.as_ref(), IdeSceneEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_duplication_validates_input_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 3, 6).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::DuplicateAudioClip {
                clip_id: String::new(),
                name: "Copy".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipDuplication(reason),
            } if matches!(reason.as_ref(), IdeAudioClipDuplicationError::InvalidSourceClipIdentity)
        ));
        worker
            .try_send(IdeWorkerCommand::DuplicateAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: "   ".to_owned(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipDuplication(reason),
            } if matches!(reason.as_ref(), IdeAudioClipDuplicationError::InvalidClipName)
        ));
        worker
            .try_send(IdeWorkerCommand::DuplicateAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: "\u{00e9}".repeat(121),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipDuplication(reason),
            } if matches!(reason.as_ref(), IdeAudioClipDuplicationError::InvalidClipName)
        ));

        worker
            .try_send(IdeWorkerCommand::DuplicateAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                name: format!(" {} ", "\u{00e9}".repeat(120)),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipDuplicating
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::DuplicateAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipDuplication(reason),
            } if matches!(reason.as_ref(), IdeAudioClipDuplicationError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_removal_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::RemoveAudioClip {
                clip_id: String::new(),
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRemoval(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRemovalError::InvalidClipIdentity)
        ));
        worker
            .try_send(IdeWorkerCommand::RemoveAudioClip {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
            })
            .unwrap();
        assert!(matches!(recv(&worker), IdeWorkerEvent::AudioClipRemoving));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RemoveAudioClip,
                reason: IdeWorkerCommandFailure::AudioClipRemoval(reason),
            } if matches!(reason.as_ref(), IdeAudioClipRemovalError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_duplication_publication_and_reopen_errors_retain_the_planned_identity() {
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334461";
        let plan = crate::IdeAudioClipCreationPlan {
            mixer: crate::IdeMixerSavePlan {
                project_path: std::env::temp_dir().join(uuid::Uuid::new_v4().to_string()),
                expected_project_sha256: [0; 32],
                resource_id: "mixer".to_owned(),
                resource_bytes: Vec::new(),
            },
            clip_id: clip_id.to_owned(),
            clip_bytes: Vec::new(),
            track_id: "track".to_owned(),
            track_bytes: Vec::new(),
            asset_id: "asset".to_owned(),
        };
        let publication = publish_audio_clip_duplication_plan(plan).unwrap_err();
        assert!(matches!(
            &publication,
            IdeAudioClipDuplicationError::Publication {
                clip_id: retained,
                ..
            } if retained == clip_id
        ));

        assert!(matches!(
            audio_clip_duplication_reopen_failure(
                Err(publication),
                IdeProjectOpenError::SelectedOutputUnavailable,
            ),
            IdeWorkerCommandFailure::AudioClipDuplication(reason)
                if matches!(reason.as_ref(),
                    IdeAudioClipDuplicationError::PublicationAndReopen {
                        clip_id: retained,
                        reopen,
                        ..
                    } if retained == clip_id
                        && matches!(reopen.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable))
        ));
    }

    #[test]
    fn audio_clip_gain_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipGain {
                clip_id: String::new(),
                gain_decibels: -12.5,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipGain,
                reason: IdeWorkerCommandFailure::AudioClipGain(reason),
            } if matches!(reason.as_ref(), IdeAudioClipGainEditError::InvalidClipIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipGain {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                gain_decibels: -12.5,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipGainChanging { gain_decibels }
                if gain_decibels.to_bits() == (-12.5_f64).to_bits()
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipGain,
                reason: IdeWorkerCommandFailure::AudioClipGain(reason),
            } if matches!(reason.as_ref(), IdeAudioClipGainEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_fades_validate_identity_and_wait_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipFades {
                clip_id: String::new(),
                fade_in_ticks: 1,
                fade_out_ticks: 2,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipFades,
                reason: IdeWorkerCommandFailure::AudioClipFades(reason),
            } if matches!(reason.as_ref(), IdeAudioClipFadeEditError::InvalidClipIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipFades {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                fade_in_ticks: 1,
                fade_out_ticks: 2,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipFadesChanging {
                fade_in_ticks: 1,
                fade_out_ticks: 2,
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipFades,
                reason: IdeWorkerCommandFailure::AudioClipFades(reason),
            } if matches!(reason.as_ref(), IdeAudioClipFadeEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_loop_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipLoop {
                clip_id: String::new(),
                enabled: true,
                start_tick: 1,
                end_tick: 3,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipLoop,
                reason: IdeWorkerCommandFailure::AudioClipLoop(reason),
            } if matches!(reason.as_ref(), IdeAudioClipLoopEditError::InvalidClipIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipLoop {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                enabled: true,
                start_tick: 1,
                end_tick: 3,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipLoopChanging {
                enabled: true,
                start_tick: 1,
                end_tick: 3,
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipLoop,
                reason: IdeWorkerCommandFailure::AudioClipLoop(reason),
            } if matches!(reason.as_ref(), IdeAudioClipLoopEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_reverse_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipReverse {
                clip_id: String::new(),
                reverse: true,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipReverse,
                reason: IdeWorkerCommandFailure::AudioClipReverse(reason),
            } if matches!(reason.as_ref(), IdeAudioClipReverseEditError::InvalidClipIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipReverse {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                reverse: true,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipReverseChanging { reverse: true }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipReverse,
                reason: IdeWorkerCommandFailure::AudioClipReverse(reason),
            } if matches!(reason.as_ref(), IdeAudioClipReverseEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_source_slip_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipSourceStart {
                clip_id: String::new(),
                source_start_frame: 25,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipSourceStart,
                reason: IdeWorkerCommandFailure::AudioClipSourceSlip(reason),
            } if matches!(reason.as_ref(), IdeAudioClipSourceSlipEditError::InvalidClipIdentity)
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipSourceStart {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                source_start_frame: 25,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipSourceStartChanging {
                source_start_frame: 25
            }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipSourceStart,
                reason: IdeWorkerCommandFailure::AudioClipSourceSlip(reason),
            } if matches!(reason.as_ref(), IdeAudioClipSourceSlipEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
        ));
        assert_eq!(shutdown_report(worker).native_failure(), None);
    }

    #[test]
    fn audio_clip_activation_validates_identity_and_waits_for_the_mixer_fence() {
        let running = RunningDesktopIdeApplication {
            application: closed_application(),
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                save_eligible: true,
                commit_mixer_fences: true,
                ..TestPlaybackSession::default()
            }),
        };
        let worker = DesktopIdeWorker::start_state(WorkerState::Running(running), 2, 5).unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                reason: IdeWorkerCommandFailure::ProjectSnapshotUnavailable,
            }
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipActive {
                clip_id: String::new(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipActive,
                reason: IdeWorkerCommandFailure::AudioClipActivation(reason),
            } if matches!(
                reason.as_ref(),
                IdeAudioClipActivationEditError::InvalidClipIdentity
            )
        ));

        worker
            .try_send(IdeWorkerCommand::SetAudioClipActive {
                clip_id: "018f47a2-6b2d-7c11-8a44-001122334460".to_owned(),
                active: false,
            })
            .unwrap();
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::AudioClipActivationChanging { active: false }
        ));
        assert!(matches!(recv(&worker), IdeWorkerEvent::Closed { .. }));
        assert!(matches!(
            recv(&worker),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipActive,
                reason: IdeWorkerCommandFailure::AudioClipActivation(reason),
            } if matches!(reason.as_ref(), IdeAudioClipActivationEditError::MixerSave(boxed)
                if matches!(boxed.as_ref(), IdeProjectSaveError::ProjectSnapshotUnavailable))
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

        assert!(publish_closed_project_mutation_failure(
            &mut state,
            closed_application(),
            &events,
            IdeWorkerCommandKind::SaveProject,
            IdeWorkerCommandFailure::ProjectSave(IdeProjectSaveError::ProjectSnapshotUnavailable,),
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
