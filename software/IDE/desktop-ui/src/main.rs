#![deny(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use hyper_desktop_ide::{
    AUDIO_IMPORT_FILE_FILTER_LABEL, AutomationChannelParameter, AutomationInterpolation,
    AutomationMainParameter, AutomationPoint, AutomationSendParameter, AutomationTarget,
    AutomationValue, ControlScope, ControlSource, ControlTarget, ControlTransform,
    DesktopAudioPreviewState, DesktopIdeWorker, Edge, IdeApplicationConfig, IdeAudioClip,
    IdeAudioClipWaveform, IdeAudioInputInventory, IdeAudioOutputInventory,
    IdeAudioPreviewControlCommand, IdeAutomationTakeRequest, IdeAutomationTarget,
    IdeControlBindingRequest, IdeControlChannelParameter, IdeControlMainParameter,
    IdeControlMappingEdit, IdeInputMeterObservation, IdeInputSelection, IdeLoopRangeFrames,
    IdeLoudnessCommand, IdeLoudnessDisplayUnit, IdeLoudnessMeasurementState,
    IdeLoudnessPresentationConfig, IdeLoudnessScale, IdeLoudnessScaleRegion,
    IdeMaximumTruePeakDisplay, IdeMixerChannel, IdeMixerChannelDestination, IdeMixerChannelKind,
    IdeMixerCommand, IdeMixerSendTap, IdeMonitoringCommand, IdeNativeCloseFailure, IdeNativeDevice,
    IdeNativeFilterEdit, IdeOutputMeterObservation, IdeOutputSelection, IdeOutputTestState,
    IdePlaybackArrangement, IdePlaybackAutomation, IdePlaybackAutomationLane, IdePlaybackExtent,
    IdeProjectExportFormat, IdeRecordingAbortOutcome, IdeRecordingCaptureKind,
    IdeRecordingImportOutcome, IdeRecordingTake, IdeResourceRestoreError, IdeRoutingBusKind,
    IdeRunningObservation, IdeScaledLoudnessValue, IdeScene, IdeSceneAvailability,
    IdeSceneLaunchQuantization, IdeSceneLaunchState, IdeSceneSlotEdit, IdeSceneTrackAction,
    IdeTimelineMarker, IdeTimelineMarkerEdit, IdeUnusedSampleCleanupSummary, IdeWorkerCommand,
    IdeWorkerCommandKind, IdeWorkerEvent, IdeWorkerPendingShutdownWait, IdeWorkerSendError,
    IdeWorkerShutdownOutcome, IdeWorkerShutdownReport, LocateState,
    MAX_COMPRESSOR_ATTACK_MICROSECONDS, MAX_COMPRESSOR_MAKEUP_MILLIDECIBELS,
    MAX_COMPRESSOR_RATIO_MILLIONTHS, MAX_COMPRESSOR_RELEASE_MICROSECONDS,
    MAX_COMPRESSOR_THRESHOLD_MILLIDECIBELS, MAX_FILTER_CUTOFF_MILLIHERTZ,
    MAX_FILTER_RESONANCE_MILLIONTHS, MAX_IDE_MIXER_SENDS_PER_CHANNEL, MAX_IDE_SCENES,
    MIN_COMPRESSOR_ATTACK_MICROSECONDS, MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS,
    MIN_COMPRESSOR_RATIO_MILLIONTHS, MIN_COMPRESSOR_RELEASE_MICROSECONDS,
    MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS, MIN_FILTER_CUTOFF_MILLIHERTZ,
    MIN_FILTER_RESONANCE_MILLIONTHS, MeterEvent, MetronomeGain, NativeFilterMode,
    OfflineExportRange, OfflineExportReport, OfflineTrackStemExportFormat,
    PlaybackRecoveryTransportState, ProjectHistory, RecentProject, SceneAction, TempoEvent,
    TimelineMapEdit, TransportAction, TransportCommand, TransportLoopRange, TriggerBehavior,
    audio_import_file_extensions, bpm_for_microseconds_per_quarter, desktop_ide_application_config,
    ide_audio_clip_gain_is_supported, meter_is_in_version_one_domain,
    microseconds_per_quarter_for_bpm, musical_position, musical_position_at_frame,
};
#[cfg(test)]
use hyper_desktop_ide::{
    IdeArrangementPlacementEditError, IdeAudioClipEditCommittedWarning, IdeAudioClipFadeEditError,
    IdeAudioClipLoopEditError, IdeAudioClipReverseEditError, IdeAudioClipSourceSlipEditError,
    IdeAudioClipSourceTrimEditError, IdeAudioImportBatchFailure, IdeAudioImportBatchOutcome,
    IdeAudioImportError, IdeAutomationTakeEditError, IdeSceneCreationError,
    IdeSceneDuplicationError, IdeSceneRemovalError, IdeSceneRenameError, IdeSceneSlotEditError,
    IdeTempoEditError, IdeWorkerCommandFailure,
};
#[cfg(test)]
use hyper_mixer_runtime::PlaybackRecoverySnapshot;
use slint::platform::PlatformError;
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, ModelTracker, SharedString, Timer,
    TimerMode, VecModel,
};

slint::include_modules!();

const WORKER_COMMAND_CAPACITY: usize = 16;
const EXPORT_BLOCK_FRAMES: usize = 1_024;
const WORKER_EVENT_CAPACITY: usize = 16;
const UI_POLL_INTERVAL: Duration = Duration::from_millis(50);
const INITIAL_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const FINAL_SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const MAX_VISIBLE_SCENE_NAME_CHARS: usize = 20;
const MAX_VISIBLE_MIXER_NAME_CHARS: usize = 10;
const MAX_VISIBLE_RECENT_PROJECT_NAME_CHARS: usize = 28;
const MAX_SAFE_TIMELINE_TICK: u64 = 9_007_199_254_740_991;
const FINAL_OUTPUT_METER_FLOOR_DBFS: f32 = -60.0;
const INPUT_METER_FLOOR_DBFS: f32 = -60.0;
const MIXER_STRIP_METER_FLOOR_DBFS: f32 = -60.0;

type WorkerCell = Rc<RefCell<Option<DesktopIdeWorker>>>;
type OutputRoutes = Rc<RefCell<Vec<Option<String>>>>;
type InputRoutes = Rc<RefCell<Vec<InputRoute>>>;
type RecentProjectRoutes = Rc<RefCell<Vec<PathBuf>>>;
type RecordingRoutes = Rc<RefCell<Vec<usize>>>;
type TakeRoutes = Rc<RefCell<Vec<TakeRoute>>>;
type AudioImportRoutes = Rc<RefCell<Vec<String>>>;
type ProjectUiStateCell = Rc<ProjectUiState>;
type UiCommandSubmitter = Rc<dyn Fn(&MainWindow, IdeWorkerCommand)>;

#[derive(Debug)]
enum AudioImportChoice {
    Selected(Vec<PathBuf>),
    Cancelled,
}

#[derive(Clone, Default)]
struct UiRoutes {
    outputs: OutputRoutes,
    inputs: InputRoutes,
    recent_projects: RecentProjectRoutes,
    recordings: RecordingRoutes,
    takes: TakeRoutes,
    audio_imports: AudioImportRoutes,
}

#[derive(Default)]
struct ProjectUiState {
    playback_end_frame: RefCell<Option<usize>>,
    playhead_frame: Cell<usize>,
    loop_marks: Cell<LoopMarks>,
    loop_engaged: Cell<bool>,
    timeline: RefCell<ProjectTimeline>,
    sample_rate_hz: Cell<u32>,
    arrangement: RefCell<Option<ArrangementBinding>>,
    audio_clips: RefCell<Vec<IdeAudioClip>>,
    scenes: RefCell<Vec<IdeScene>>,
    scene_launch_evidence: RefCell<SceneLaunchEvidence>,
    automation_channels: RefCell<Vec<MixerChannelFacts>>,
    automation_targets_initialized: Cell<bool>,
    automation: RefCell<Option<Arc<IdePlaybackAutomation>>>,
    automation_take_ids: RefCell<Vec<String>>,
    control_targets: RefCell<Vec<ControlTargetChoice>>,
    unused_sample_cleanup: Cell<Option<IdeUnusedSampleCleanupSummary>>,
    native_filter_tracks: RefCell<Vec<String>>,
    native_device_observation_generation: Cell<Option<u64>>,
    native_device_clipboard: RefCell<Option<NativeDeviceClipboard>>,
}

#[derive(Clone)]
struct NativeDeviceClipboard {
    source_chain_id: String,
    source_device_instance_id: String,
}

/// The two marks a user sets independently. They only become a transport range
/// once both exist and the end is beyond the start, which is why the marks are
/// kept rather than a range: marking out before in must not silently discard
/// the first mark.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct LoopMarks {
    start: Option<usize>,
    end: Option<usize>,
}

impl LoopMarks {
    /// The durable form of these marks: frames, which the application converts
    /// to ticks through the project's own timeline.
    fn persisted(self, enabled: bool) -> Option<IdeLoopRangeFrames> {
        let range = self.range()?;
        Some(IdeLoopRangeFrames {
            start_frame: range.start_frame(),
            end_frame: range.end_frame(),
            enabled,
        })
    }

    fn range(self) -> Option<TransportLoopRange> {
        let (start, end) = (self.start?, self.end?);
        TransportLoopRange::new(start, end).ok()
    }

    fn summary(self, sample_rate_hz: u32) -> String {
        match (self.start, self.end) {
            (None, None) => String::new(),
            (Some(start), None) => format!(
                "LOOP IN {}",
                format_transport_clock_frames(start, sample_rate_hz)
            ),
            (None, Some(end)) => format!(
                "LOOP OUT {}",
                format_transport_clock_frames(end, sample_rate_hz)
            ),
            (Some(start), Some(end)) if end > start => format!(
                "LOOP {} → {}",
                format_transport_clock_frames(start, sample_rate_hz),
                format_transport_clock_frames(end, sample_rate_hz)
            ),
            (Some(start), Some(end)) => format!(
                "LOOP OUT {} IS NOT AFTER IN {}",
                format_transport_clock_frames(end, sample_rate_hz),
                format_transport_clock_frames(start, sample_rate_hz)
            ),
        }
    }
}

fn format_transport_clock_frames(frame: usize, sample_rate_hz: u32) -> String {
    format_transport_clock(u64::try_from(frame).unwrap_or(u64::MAX), sample_rate_hz)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MixerChannelFacts {
    track_id: String,
    name: String,
    audio: bool,
    sends: Vec<MixerSendFacts>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SceneFacts {
    scene_id: String,
    name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MixerSendFacts {
    send_id: String,
    target_name: String,
}

#[derive(Clone, Debug, PartialEq)]
struct AutomationTargetChoice {
    label: String,
    domain: String,
    boolean: bool,
    target: IdeAutomationTarget,
    resolved: AutomationTarget,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SceneLaunchEvidence {
    playback_session_generation: Option<u64>,
    state: Option<IdeSceneLaunchState>,
}

impl SceneLaunchEvidence {
    fn observe(
        &mut self,
        playback_session_generation: u64,
        observed_state: Option<IdeSceneLaunchState>,
        scenes_changed: bool,
    ) -> bool {
        let previous_state = self.state;
        if self.playback_session_generation != Some(playback_session_generation) {
            self.playback_session_generation = Some(playback_session_generation);
            self.state = observed_state;
        } else if let Some(observed_state) = observed_state {
            self.state = Some(observed_state);
        } else if scenes_changed {
            self.state = None;
        }
        previous_state != self.state
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

struct ArrangementBinding {
    project_id: String,
    owner: Arc<IdePlaybackArrangement>,
    sample_rate_hz: u32,
    scene_ids: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum InputRoute {
    Disabled,
    SystemDefault,
    Routed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TakeRoute {
    recording_id: String,
    asset_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UiSubmissionFailure {
    Full,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SceneLaunchFailure {
    InvalidSelection,
    StaleSelection,
    NotReady,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MixerControlFailure {
    InvalidSelection,
    StaleSelection,
    StateUnavailable,
    SoloUnavailable,
    InvalidValue,
}

impl MixerControlFailure {
    const fn message(self) -> &'static str {
        match self {
            Self::InvalidSelection => "Mixer channel selection is invalid",
            Self::StaleSelection => "Mixer channel selection is no longer available",
            Self::StateUnavailable => "Mixer base state is not yet available",
            Self::SoloUnavailable => "Solo is unavailable for this mixer channel",
            Self::InvalidValue => "Mixer control value is outside its supported range",
        }
    }
}

impl SceneLaunchFailure {
    const fn message(self) -> &'static str {
        match self {
            Self::InvalidSelection => "Scene selection is invalid",
            Self::StaleSelection => "Scene selection is no longer available",
            Self::NotReady => "Scene is not ready for launch",
        }
    }
}

#[derive(Debug)]
struct OutputPresentation {
    routes: Vec<Option<String>>,
    names: Vec<SharedString>,
    selected: usize,
}

#[derive(Debug)]
struct InputPresentation {
    routes: Vec<InputRoute>,
    names: Vec<SharedString>,
    selected: usize,
}

#[derive(Debug)]
struct RecentProjectPresentation {
    routes: Vec<PathBuf>,
    names: Vec<SharedString>,
    display_names: Vec<SharedString>,
    paths: Vec<SharedString>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("desktop IDE failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let ui = MainWindow::new()?;
    configure_ui_contracts(&ui);
    let application = application_config()?;
    let worker = match ProjectHistory::for_current_user() {
        Ok(project_history) => DesktopIdeWorker::start_with_project_history(
            application,
            project_history,
            WORKER_COMMAND_CAPACITY,
            WORKER_EVENT_CAPACITY,
        )?,
        Err(error) => {
            ui.set_project_history_warning(format!("Recent projects unavailable: {error}").into());
            DesktopIdeWorker::start(application, WORKER_COMMAND_CAPACITY, WORKER_EVENT_CAPACITY)?
        }
    };
    let worker = Rc::new(RefCell::new(Some(worker)));
    let routes = UiRoutes::default();
    let project_state = Rc::new(ProjectUiState::default());

    bind_callbacks(&ui, &worker, &routes, &project_state);
    let timer = start_event_pump(&ui, &worker, &routes, &project_state);
    let run_result = ui.run();
    timer.stop();

    let shutdown_result = worker.borrow_mut().take().map_or(Ok(()), shutdown_worker);
    finish_exit(run_result, shutdown_result)?;
    Ok(())
}

fn configure_ui_contracts(ui: &MainWindow) {
    ui.set_max_scene_count(
        i32::try_from(MAX_IDE_SCENES)
            .expect("the project scene limit fits the Slint integer domain"),
    );
}

fn application_config() -> Result<IdeApplicationConfig, Box<dyn Error>> {
    Ok(desktop_ide_application_config()?)
}

fn bind_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    routes: &UiRoutes,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_retry = Rc::clone(worker);
    actions.on_retry_discovery(move || {
        submit(&weak, &worker_for_retry, IdeWorkerCommand::RetryDiscovery);
    });

    let weak = ui.as_weak();
    let worker_for_refresh = Rc::clone(worker);
    actions.on_refresh_outputs(move || {
        submit(&weak, &worker_for_refresh, IdeWorkerCommand::RefreshOutputs);
    });

    let weak = ui.as_weak();
    let worker_for_selection = Rc::clone(worker);
    let routes_for_selection = Rc::clone(&routes.outputs);
    actions.on_select_output(move |index| {
        let selection = usize::try_from(index)
            .ok()
            .and_then(|index| routes_for_selection.borrow().get(index).cloned());
        let Some(routing_id) = selection else {
            if let Some(ui) = weak.upgrade() {
                ui.set_error_text("Audio output selection is no longer available".into());
            }
            return;
        };
        let command = routing_id.map_or(
            IdeWorkerCommand::SelectSystemDefault,
            IdeWorkerCommand::SelectOutput,
        );
        submit(&weak, &worker_for_selection, command);
    });

    bind_output_test_callbacks(ui, worker);

    bind_input_selection_callbacks(ui, worker, &routes.inputs);

    bind_project_callbacks(ui, worker, &routes.recent_projects);

    bind_playback_callbacks(ui, worker, project_state);
    bind_monitoring_callbacks(ui, worker);
    bind_recording_callbacks(ui, worker, &routes.recordings);
    bind_scene_performance_capture_callbacks(ui, worker);
    bind_audio_preview_callbacks(ui, worker, &routes.takes);
    bind_audio_import_callback(ui, worker, &routes.audio_imports);
}

fn bind_audio_import_callback(ui: &MainWindow, worker: &WorkerCell, routes: &AudioImportRoutes) {
    bind_audio_import_callback_with(ui, worker, routes, |weak, worker, track_id| {
        let weak_for_dialog = weak.clone();
        let spawn_result = slint::spawn_local(async move {
            let extensions = audio_import_file_extensions().collect::<Vec<_>>();
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Import audio into project")
                .add_filter(AUDIO_IMPORT_FILE_FILTER_LABEL, &extensions)
                .pick_files()
                .await;
            let Some(ui) = weak_for_dialog.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::ImportingAudio {
                return;
            }
            let choice = selection.map_or(AudioImportChoice::Cancelled, |selection| {
                AudioImportChoice::Selected(
                    selection
                        .into_iter()
                        .map(|file| file.path().to_path_buf())
                        .collect(),
                )
            });
            complete_audio_import_choice(&ui, &worker, track_id, choice);
        });
        if spawn_result.is_err()
            && let Some(ui) = weak.upgrade()
        {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Audio file selection is unavailable".into());
        }
    });
}

fn complete_audio_import_choice(
    ui: &MainWindow,
    worker: &WorkerCell,
    track_id: String,
    choice: AudioImportChoice,
) {
    complete_audio_import_choice_with(ui, track_id, choice, |command| {
        submit_to_ui(ui, worker, command);
    });
}

fn complete_audio_import_choice_with(
    ui: &MainWindow,
    track_id: String,
    choice: AudioImportChoice,
    submit: impl FnOnce(IdeWorkerCommand),
) {
    match choice {
        AudioImportChoice::Selected(paths) => {
            submit(IdeWorkerCommand::ImportAudio { paths, track_id });
        }
        AudioImportChoice::Cancelled => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Audio import cancelled".into());
        }
    }
}

fn bind_audio_import_callback_with(
    ui: &MainWindow,
    worker: &WorkerCell,
    routes: &AudioImportRoutes,
    choose: impl Fn(slint::Weak<MainWindow>, WorkerCell, String) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let routes = Rc::clone(routes);
    actions.on_choose_audio_import(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let Some(track_id) = usize::try_from(index)
            .ok()
            .and_then(|index| routes.borrow().get(index).cloned())
        else {
            ui.set_error_text("Audio import destination is no longer available".into());
            return;
        };
        ui.set_app_mode(AppMode::ImportingAudio);
        ui.set_status_text("Waiting for audio files…".into());
        ui.set_error_text(SharedString::default());
        choose(weak.clone(), Rc::clone(&worker), track_id);
    });
}

fn bind_project_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    recent_project_routes: &RecentProjectRoutes,
) {
    bind_project_creation_callback(ui, worker);
    bind_project_open_callback(ui, worker);
    bind_recent_project_callback(ui, worker, recent_project_routes);
    bind_runtime_preparation_callback(ui, worker);
}

fn bind_recent_project_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    recent_project_routes: &RecentProjectRoutes,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let routes = Rc::clone(recent_project_routes);
    actions.on_open_recent_project(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Closed {
            return;
        }
        let Some(command) = recent_project_command(&routes, index) else {
            ui.set_error_text("Recent project selection is no longer available".into());
            return;
        };
        let IdeWorkerCommand::OpenRecentProject(path) = &command else {
            unreachable!("recent-project routing only creates recent-project commands");
        };
        ui.set_project_location(path.to_string_lossy().into_owned().into());
        submit_to_ui(&ui, &worker, command);
    });
}

fn recent_project_command(routes: &RecentProjectRoutes, index: i32) -> Option<IdeWorkerCommand> {
    usize::try_from(index)
        .ok()
        .and_then(|index| routes.borrow().get(index).cloned())
        .map(IdeWorkerCommand::OpenRecentProject)
}

fn bind_runtime_preparation_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_preparation = Rc::clone(worker);
    actions.on_prepare_runtime(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_mixer_main_state_available() {
            ui.set_error_text(
                "Committed mixer state is not available for Runtime preparation".into(),
            );
            return;
        }
        ui.set_app_mode(AppMode::PreparingRuntime);
        ui.set_error_text(SharedString::default());
        ui.set_status_text("Waiting for the Runtime bundle destination…".into());
        let weak_for_dialog = weak.clone();
        let worker = Rc::clone(&worker_for_preparation);
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Prepare verified Runtime bundle")
                .set_file_name("project.runtime")
                .save_file()
                .await;
            let Some(ui) = weak_for_dialog.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::PreparingRuntime {
                return;
            }
            let Some(selection) = selection else {
                ui.set_app_mode(AppMode::Running);
                ui.set_status_text("Runtime preparation cancelled".into());
                return;
            };
            let output_path = selection.path().to_path_buf();
            ui.set_runtime_bundle_destination(output_path.display().to_string().into());
            ui.set_runtime_bundle_summary(SharedString::default());
            submit_to_ui(
                &ui,
                &worker,
                IdeWorkerCommand::PrepareRuntime { output_path },
            );
        });
        if spawn_result.is_err() {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Runtime bundle destination selection is unavailable".into());
        }
    });
}

fn bind_project_creation_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_creation = Rc::clone(worker);
    actions.on_create_project(move |project_name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Closed || !ui.get_new_project_visible() {
            return;
        }
        let project_name = project_name.to_string();
        if project_name.is_empty() {
            ui.set_error_text("Enter a project name before choosing its location".into());
            return;
        }
        ui.set_app_mode(AppMode::Selecting);
        ui.set_error_text(SharedString::default());
        ui.set_status_text("Waiting for the new project parent folder…".into());
        let weak_for_dialog = weak.clone();
        let weak_for_error = weak.clone();
        let worker = Rc::clone(&worker_for_creation);
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Choose parent folder for the new Hyper Module project")
                .pick_folder()
                .await;
            let Some(ui) = weak_for_dialog.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Selecting || !ui.get_new_project_visible() {
                return;
            }
            let Some(selection) = selection else {
                ui.set_app_mode(AppMode::Closed);
                ui.set_status_text("New project creation cancelled".into());
                return;
            };
            let parent = selection.path().to_path_buf();
            let project_path = parent.join(&project_name);
            ui.set_project_location(project_path.to_string_lossy().into_owned().into());
            submit_to_ui(
                &ui,
                &worker,
                IdeWorkerCommand::CreateProject {
                    parent,
                    directory_name: project_name.clone(),
                    project_name,
                },
            );
        });
        if let Err(error) = spawn_result
            && let Some(ui) = weak_for_error.upgrade()
            && ui.get_app_mode() == AppMode::Selecting
        {
            ui.set_app_mode(AppMode::Closed);
            ui.set_error_text(format!("Project location dialog could not start: {error}").into());
        }
    });
}

fn bind_arrangement_duration_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_duration = Rc::clone(worker);
    let project_state_for_duration = Rc::clone(project_state);
    actions.on_set_arrangement_placement_duration(move |placement_id, duration_ticks| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement trimming is not available in the current state".into(),
            );
            return;
        }
        let duration_ticks = match parse_arrangement_duration_ticks(duration_ticks.as_str()) {
            Ok(duration_ticks) => duration_ticks,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let state = project_state_for_duration.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        if placement.duration_ticks() == duration_ticks {
            ui.set_status_text("Arrangement placement already has that duration".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_duration,
            IdeWorkerCommand::SetArrangementPlacementDuration {
                placement_id: owned_placement_id,
                duration_ticks,
            },
        );
    });
}

fn bind_arrangement_front_trim_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_trim = Rc::clone(worker);
    let project_state_for_trim = Rc::clone(project_state);
    actions.on_trim_arrangement_placement_front(move |placement_id, start_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement front trimming is not available in the current state".into(),
            );
            return;
        }
        let start_tick = match parse_arrangement_start_tick(start_tick.as_str()) {
            Ok(start_tick) => start_tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let state = project_state_for_trim.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        if placement.start_tick() == start_tick {
            ui.set_status_text("Arrangement placement already starts at that tick".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_trim,
            IdeWorkerCommand::TrimArrangementPlacementFront {
                placement_id: owned_placement_id,
                start_tick,
            },
        );
    });
}

fn bind_arrangement_gain_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_gain = Rc::clone(worker);
    let project_state_for_gain = Rc::clone(project_state);
    actions.on_set_arrangement_placement_gain(move |placement_id, value| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement gain is not available in the current state".into(),
            );
            return;
        }
        let gain_decibels = match parse_arrangement_placement_gain(value.as_str()) {
            Ok(gain_decibels) => gain_decibels,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let state = project_state_for_gain.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        if gain_decibels.to_bits() == placement.gain_decibels().to_bits() {
            ui.set_status_text(
                "Arrangement placement gain already matches the committed value".into(),
            );
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_gain,
            IdeWorkerCommand::SetArrangementPlacementGain {
                placement_id: owned_placement_id,
                gain_decibels,
            },
        );
    });
}

fn bind_arrangement_fades_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_fades = Rc::clone(worker);
    let project_state_for_fades = Rc::clone(project_state);
    actions.on_set_arrangement_placement_fades(move |placement_id, fade_in, fade_out| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement fades are not available in the current state".into(),
            );
            return;
        }
        let state = project_state_for_fades.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        let (fade_in_ticks, fade_out_ticks) = match parse_arrangement_placement_fades(
            fade_in.as_str(),
            fade_out.as_str(),
            placement.duration_ticks(),
        ) {
            Ok(fades) => fades,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        if fade_in_ticks == placement.fade_in_ticks()
            && fade_out_ticks == placement.fade_out_ticks()
        {
            ui.set_status_text(
                "Arrangement placement fades already match the committed values".into(),
            );
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_fades,
            IdeWorkerCommand::SetArrangementPlacementFades {
                placement_id: owned_placement_id,
                fade_in_ticks,
                fade_out_ticks,
            },
        );
    });
}

fn bind_arrangement_crossfade_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let project_state = Rc::clone(project_state);
    let worker = Rc::clone(worker);
    bind_arrangement_crossfade_callback_with(
        ui,
        move |outgoing_placement_id, incoming_placement_id| {
            let state = project_state.arrangement.borrow();
            let binding = state
                .as_ref()
                .ok_or("Arrangement placement authority is no longer available")?;
            resolve_arrangement_placement_crossfade(
                binding.owner.as_ref(),
                outgoing_placement_id,
                incoming_placement_id,
            )
        },
        move |ui, command| submit_to_ui(ui, &worker, command),
    );
}

fn bind_arrangement_crossfade_callback_with(
    ui: &MainWindow,
    resolve_crossfade: impl Fn(&str, &str) -> Result<(String, String), &'static str> + 'static,
    submit: impl Fn(&MainWindow, IdeWorkerCommand) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_apply_arrangement_placement_crossfade(
        move |outgoing_placement_id, incoming_placement_id| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running {
                ui.set_error_text(
                    "Arrangement crossfades are not available in the current state".into(),
                );
                return;
            }
            let (outgoing_placement_id, incoming_placement_id) = match resolve_crossfade(
                outgoing_placement_id.as_str(),
                incoming_placement_id.as_str(),
            ) {
                Ok(pair) => pair,
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
            submit(
                &ui,
                IdeWorkerCommand::ApplyArrangementPlacementCrossfade {
                    outgoing_placement_id,
                    incoming_placement_id,
                },
            );
        },
    );
}

fn resolve_arrangement_placement_crossfade(
    arrangement: &IdePlaybackArrangement,
    requested_outgoing_placement_id: &str,
    requested_incoming_placement_id: &str,
) -> Result<(String, String), &'static str> {
    let outgoing_index = arrangement
        .placements()
        .iter()
        .position(|placement| placement.id() == requested_outgoing_placement_id)
        .ok_or("The selected arrangement placement is no longer available")?;
    let (incoming_index, _) = arrangement_crossfade_target(arrangement, outgoing_index).ok_or(
        "A bounded arrangement crossfade is no longer available for the selected placement",
    )?;
    let outgoing = &arrangement.placements()[outgoing_index];
    let incoming = &arrangement.placements()[incoming_index];
    if incoming.id() != requested_incoming_placement_id {
        return Err("The selected crossfade target is no longer authoritative");
    }
    Ok((outgoing.id().to_owned(), incoming.id().to_owned()))
}

fn bind_project_open_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_project = Rc::clone(worker);
    actions.on_choose_project(move || {
        let weak = weak.clone();
        let error_weak = weak.clone();
        let worker = Rc::clone(&worker_for_project);
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Closed {
            return;
        }
        ui.set_app_mode(AppMode::Selecting);
        ui.set_error_text(SharedString::default());
        ui.set_status_text("Waiting for project folder selection…".into());
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Open Hyper Module project")
                .pick_folder()
                .await;
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Selecting {
                return;
            }
            let Some(selection) = selection else {
                ui.set_app_mode(AppMode::Closed);
                ui.set_status_text("Project selection cancelled".into());
                return;
            };
            let path = selection.path().to_path_buf();
            ui.set_project_location(path.to_string_lossy().into_owned().into());
            ui.set_app_mode(AppMode::Opening);
            ui.set_status_text("Recovering and verifying project…".into());
            submit_path(&ui, &worker, path);
        });
        if let Err(error) = spawn_result
            && let Some(ui) = error_weak.upgrade()
            && ui.get_app_mode() == AppMode::Selecting
        {
            ui.set_app_mode(AppMode::Closed);
            ui.set_error_text(format!("Project dialog could not start: {error}").into());
        }
    });
}

fn bind_output_test_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_start = Rc::clone(worker);
    actions.on_start_output_test(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Closed || !ui.get_output_test_confirming() {
            return;
        }
        ui.set_output_test_confirming(false);
        submit_to_ui(&ui, &worker_for_start, IdeWorkerCommand::StartOutputTest);
    });

    let weak = ui.as_weak();
    let worker_for_stop = Rc::clone(worker);
    actions.on_stop_output_test(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::OutputTesting {
            return;
        }
        submit_to_ui(&ui, &worker_for_stop, IdeWorkerCommand::StopOutputTest);
    });
}

fn bind_input_selection_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    input_routes: &InputRoutes,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_refresh = Rc::clone(worker);
    actions.on_refresh_inputs(move || {
        submit(&weak, &worker_for_refresh, IdeWorkerCommand::RefreshInputs);
    });

    let weak = ui.as_weak();
    let worker_for_selection = Rc::clone(worker);
    let routes_for_selection = Rc::clone(input_routes);
    actions.on_select_input(move |index| {
        let selection = usize::try_from(index)
            .ok()
            .and_then(|index| routes_for_selection.borrow().get(index).cloned());
        let Some(selection) = selection else {
            if let Some(ui) = weak.upgrade() {
                ui.set_error_text("Audio input selection is no longer available".into());
            }
            return;
        };
        let command = match selection {
            InputRoute::Disabled => IdeWorkerCommand::DisableInputMonitoring,
            InputRoute::SystemDefault => IdeWorkerCommand::SelectSystemDefaultInput,
            InputRoute::Routed(routing_id) => IdeWorkerCommand::SelectInput(routing_id),
        };
        submit(&weak, &worker_for_selection, command);
    });
}

fn bind_monitoring_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_gain = Rc::clone(worker);
    actions.on_monitor_gain(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Recording)
            || !ui.get_monitoring_active()
        {
            return;
        }
        match monitor_percent_to_millionths(percent) {
            Ok(gain_millionths) => submit_to_ui(
                &ui,
                &worker_for_gain,
                IdeWorkerCommand::Monitoring(IdeMonitoringCommand::Gain { gain_millionths }),
            ),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let worker_for_mute = Rc::clone(worker);
    actions.on_monitor_mute(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Recording)
            || !ui.get_monitoring_active()
        {
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_mute,
            IdeWorkerCommand::Monitoring(IdeMonitoringCommand::Mute {
                muted: !ui.get_monitoring_muted(),
            }),
        );
    });
}

fn bind_recording_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    recording_routes: &RecordingRoutes,
) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_start = Rc::clone(worker);
    let routes_for_start = Rc::clone(recording_routes);
    actions.on_start_recording(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_monitoring_active() {
            return;
        }
        let selected = usize::try_from(ui.get_selected_recording_target_index())
            .ok()
            .and_then(|index| routes_for_start.borrow().get(index).copied());
        let Some(channel_index) = selected else {
            ui.set_error_text("Recording target is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_start,
            IdeWorkerCommand::StartRecording { channel_index },
        );
    });

    let weak = ui.as_weak();
    let worker_for_stop = Rc::clone(worker);
    actions.on_stop_recording(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::Recording {
            submit_to_ui(&ui, &worker_for_stop, IdeWorkerCommand::StopRecording);
        }
    });

    let weak = ui.as_weak();
    let worker_for_abort = Rc::clone(worker);
    actions.on_abort_recording(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::Recording {
            submit_to_ui(&ui, &worker_for_abort, IdeWorkerCommand::AbortRecording);
        }
    });
}

fn bind_scene_performance_capture_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_start = Rc::clone(worker);
    actions.on_start_scene_performance_capture(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_transport_start_available()
            || ui.get_loop_engaged()
        {
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_start,
            IdeWorkerCommand::StartScenePerformanceCapture,
        );
    });

    let weak = ui.as_weak();
    let worker_for_stop = Rc::clone(worker);
    actions.on_stop_scene_performance_capture(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::CapturingScenePerformance {
            submit_to_ui(
                &ui,
                &worker_for_stop,
                IdeWorkerCommand::StopScenePerformanceCapture,
            );
        }
    });
}

fn bind_mixer_automation_capture_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_start = Rc::clone(worker);
    let project_state_for_start = Rc::clone(project_state);
    actions.on_start_mixer_automation_capture(move |name, target_index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_automation_capture_visible()
            || !ui.get_transport_start_available()
            || ui.get_loop_engaged()
        {
            return;
        }
        let choices =
            automation_target_choices(&project_state_for_start.automation_channels.borrow());
        let (name, target) =
            match mixer_automation_capture_request(&choices, name.as_str(), target_index) {
                Ok(request) => request,
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
        submit_to_ui(
            &ui,
            &worker_for_start,
            IdeWorkerCommand::StartMixerAutomationCapture { name, target },
        );
    });

    let weak = ui.as_weak();
    let worker_for_stop = Rc::clone(worker);
    actions.on_stop_mixer_automation_capture(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::CapturingMixerAutomation {
            submit_to_ui(
                &ui,
                &worker_for_stop,
                IdeWorkerCommand::StopMixerAutomationCapture,
            );
        }
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "the preview callbacks share one take-route authority and must be registered together"
)]
fn bind_audio_preview_callbacks(ui: &MainWindow, worker: &WorkerCell, take_routes: &TakeRoutes) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_toggle = Rc::clone(worker);
    let routes_for_toggle = Rc::clone(take_routes);
    actions.on_toggle_take_preview(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::Previewing && ui.get_preview_active() {
            submit_to_ui(&ui, &worker_for_toggle, IdeWorkerCommand::StopAudioPreview);
            return;
        }
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let asset_id = take_route(&routes_for_toggle, index).map(|route| route.asset_id);
        let Some(asset_id) = asset_id else {
            ui.set_error_text("Recorded take selection is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_toggle,
            IdeWorkerCommand::StartAudioPreview { asset_id },
        );
    });

    let weak = ui.as_weak();
    let worker_for_gain = Rc::clone(worker);
    actions.on_preview_gain(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Previewing) {
            return;
        }
        match preview_percent_to_millionths(percent) {
            Ok(gain_millionths) => submit_to_ui(
                &ui,
                &worker_for_gain,
                IdeWorkerCommand::AudioPreviewControl(IdeAudioPreviewControlCommand::Gain {
                    gain_millionths,
                }),
            ),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let worker_for_mute = Rc::clone(worker);
    actions.on_preview_mute(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Previewing) {
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_mute,
            IdeWorkerCommand::AudioPreviewControl(IdeAudioPreviewControlCommand::Mute {
                muted: !ui.get_preview_muted(),
            }),
        );
    });

    let weak = ui.as_weak();
    let worker_for_creation = Rc::clone(worker);
    let routes_for_creation = Rc::clone(take_routes);
    actions.on_create_audio_clip_from_take(move |index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_take_clip_visible() {
            return;
        }
        let Some(command) = take_clip_creation_command(&routes_for_creation, index, name.as_str())
        else {
            ui.set_error_text("Recorded take selection is no longer available".into());
            return;
        };
        submit_to_ui(&ui, &worker_for_creation, command);
    });

    let weak = ui.as_weak();
    let worker_for_placement = Rc::clone(worker);
    let routes_for_placement = Rc::clone(take_routes);
    actions.on_create_and_place_audio_clip_from_take(move |index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_take_clip_visible() {
            return;
        }
        let Some(command) =
            take_clip_placement_command(&routes_for_placement, index, name.as_str())
        else {
            ui.set_error_text("Recorded take selection is no longer available".into());
            return;
        };
        submit_to_ui(&ui, &worker_for_placement, command);
    });

    let weak = ui.as_weak();
    let worker_for_discard = Rc::clone(worker);
    let routes_for_discard = Rc::clone(take_routes);
    actions.on_discard_recording_take(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let Some(command) = take_discard_command(&routes_for_discard, index) else {
            ui.set_error_text("Recorded take selection is no longer available".into());
            return;
        };
        submit_to_ui(&ui, &worker_for_discard, command);
    });

    let weak = ui.as_weak();
    let worker_for_rename = Rc::clone(worker);
    let routes_for_rename = Rc::clone(take_routes);
    actions.on_rename_recording_take(move |index, label| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_take_rename_visible() {
            return;
        }
        let label = label.trim();
        if label.is_empty() || label.chars().count() > 120 {
            ui.set_error_text("Recorded-take label must contain 1 through 120 characters".into());
            return;
        }
        let Some(route) = take_route(&routes_for_rename, index) else {
            ui.set_error_text("Recorded take selection is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_rename,
            IdeWorkerCommand::RenameRecordingTake {
                recording_id: route.recording_id,
                label: label.to_owned(),
            },
        );
    });
}

fn take_route(routes: &TakeRoutes, index: i32) -> Option<TakeRoute> {
    usize::try_from(index)
        .ok()
        .and_then(|index| routes.borrow().get(index).cloned())
}

fn take_clip_creation_command(
    routes: &TakeRoutes,
    index: i32,
    name: &str,
) -> Option<IdeWorkerCommand> {
    take_route(routes, index).map(|route| IdeWorkerCommand::CreateAudioClipFromTake {
        recording_id: route.recording_id,
        name: name.to_owned(),
    })
}

fn take_clip_placement_command(
    routes: &TakeRoutes,
    index: i32,
    name: &str,
) -> Option<IdeWorkerCommand> {
    take_route(routes, index).map(|route| IdeWorkerCommand::CreateAndPlaceAudioClipFromTake {
        recording_id: route.recording_id,
        name: name.to_owned(),
    })
}

fn take_discard_command(routes: &TakeRoutes, index: i32) -> Option<IdeWorkerCommand> {
    take_route(routes, index).map(|route| IdeWorkerCommand::DiscardRecordingTake {
        recording_id: route.recording_id,
    })
}

fn bind_playback_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_start = Rc::clone(worker);
    actions.on_transport_start(move || {
        submit(
            &weak,
            &worker_for_start,
            IdeWorkerCommand::Transport(TransportCommand::Start),
        );
    });

    let weak = ui.as_weak();
    let worker_for_stop = Rc::clone(worker);
    actions.on_transport_stop(move || {
        submit(
            &weak,
            &worker_for_stop,
            IdeWorkerCommand::Transport(TransportCommand::Stop),
        );
    });

    let worker_for_metronome = Rc::clone(worker);
    bind_metronome_toggle_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker_for_metronome, command)),
    );
    let worker_for_metronome_gain = Rc::clone(worker);
    bind_metronome_gain_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker_for_metronome_gain, command)),
    );

    let weak = ui.as_weak();
    let worker_for_return = Rc::clone(worker);
    actions.on_transport_return_start(move || {
        submit(
            &weak,
            &worker_for_return,
            IdeWorkerCommand::Transport(return_to_start_command()),
        );
    });

    let weak = ui.as_weak();
    let worker_for_seek = Rc::clone(worker);
    let state_for_seek = Rc::clone(project_state);
    actions.on_transport_seek(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let Some(end_frame) = *state_for_seek.playback_end_frame.borrow() else {
            ui.set_error_text("Finite playback extent is no longer available".into());
            return;
        };
        match seek_percent_to_frame(percent, end_frame) {
            Ok(frame) => submit_to_ui(&ui, &worker_for_seek, IdeWorkerCommand::Seek { frame }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    bind_arrangement_callback(ui, worker, project_state);
    bind_audio_clip_callback(ui, worker, project_state);
    bind_loudness_callbacks(ui, worker);
    bind_output_peak_hold_callback(ui, worker);
    bind_scene_callbacks(ui, worker, project_state);

    bind_project_save_callback(ui, worker, project_state);
    bind_project_copy_callback(ui, worker);
    bind_project_rename_callback(ui, worker);

    bind_audio_track_callback(ui, worker);

    bind_project_close_callback(ui, worker);

    bind_mixer_callbacks(ui, worker);
    bind_native_filter_callbacks(ui, worker, project_state);
    bind_window_close_callback(ui);
}

fn bind_metronome_toggle_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_metronome_toggle(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_metronome_available() {
            return;
        }
        submitter(
            &ui,
            IdeWorkerCommand::Transport(TransportCommand::SetMetronomeEnabled(
                !ui.get_metronome_enabled(),
            )),
        );
    });
}

fn bind_metronome_gain_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_metronome_gain(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_metronome_available() {
            return;
        }
        match metronome_percent_to_gain(percent) {
            Ok(gain) => submitter(
                &ui,
                IdeWorkerCommand::Transport(TransportCommand::SetMetronomeGain(gain)),
            ),
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn bind_project_close_callback(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_project_close_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

fn bind_project_close_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let close_submitter = Rc::clone(&submitter);
    actions.on_close_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_close_unsaved_mixer_confirming() {
            return;
        }
        close_submitter(&ui, IdeWorkerCommand::CloseProject);
    });
    let weak = ui.as_weak();
    let confirm_submitter = Rc::clone(&submitter);
    actions.on_confirm_close_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Previewing)
            || !ui.get_close_unsaved_mixer_confirming()
        {
            return;
        }
        confirm_submitter(&ui, IdeWorkerCommand::DiscardCloseProject);
    });
    let weak = ui.as_weak();
    let cancel_submitter = Rc::clone(&submitter);
    actions.on_cancel_close_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !matches!(ui.get_app_mode(), AppMode::Running | AppMode::Previewing)
            || !ui.get_close_unsaved_mixer_confirming()
        {
            return;
        }
        cancel_submitter(&ui, IdeWorkerCommand::CancelCloseProject);
    });
    let weak = ui.as_weak();
    let restore_submitter = Rc::clone(&submitter);
    actions.on_restore_mixer_recovery(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_mixer_recovery_available() {
            restore_submitter(&ui, IdeWorkerCommand::RestoreMixerRecovery);
        }
    });
    let weak = ui.as_weak();
    actions.on_discard_mixer_recovery(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_mixer_recovery_available() {
            submitter(&ui, IdeWorkerCommand::DiscardMixerRecovery);
        }
    });
}

fn bind_window_close_callback(ui: &MainWindow) {
    let weak = ui.as_weak();
    ui.window().on_close_requested(move || {
        let Some(ui) = weak.upgrade() else {
            return CloseRequestResponse::HideWindow;
        };
        window_close_response(&ui)
    });
}

fn window_close_response(ui: &MainWindow) -> CloseRequestResponse {
    if matches!(ui.get_app_mode(), AppMode::Running | AppMode::Previewing) {
        ui.global::<IdeActions>().invoke_close_project();
        CloseRequestResponse::KeepWindowShown
    } else {
        CloseRequestResponse::HideWindow
    }
}

fn bind_project_save_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let worker = Rc::clone(worker);
    bind_project_save_callback_with(
        ui,
        project_state,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

fn bind_project_save_callback_with(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    submitter: UiCommandSubmitter,
) {
    let weak = ui.as_weak();
    let project_state = Rc::clone(project_state);
    ui.global::<IdeActions>().on_save_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_mixer_main_state_available() {
            ui.set_error_text("Committed mixer state is not available to save".into());
            return;
        }
        submitter(&ui, project_save_command(&project_state));
    });
}

fn bind_project_copy_callback(ui: &MainWindow, worker: &WorkerCell) {
    bind_project_copy_callback_with(ui, worker, |weak, worker, project_name, switch_to| {
        let weak_for_dialog = weak.clone();
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Choose parent folder for the independent Hyper Module project")
                .pick_folder()
                .await;
            let Some(ui) = weak_for_dialog.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Saving || !ui.get_save_project_as_visible() {
                return;
            }
            complete_project_copy_choice(
                &ui,
                &worker,
                project_name,
                switch_to,
                selection.map_or(ProjectCopyChoice::Cancelled, |selection| {
                    ProjectCopyChoice::Selected(selection.path().to_path_buf())
                }),
            );
        });
        if let Err(error) = spawn_result
            && let Some(ui) = weak.upgrade()
        {
            ui.set_app_mode(AppMode::Running);
            ui.set_error_text(format!("Save As location dialog could not start: {error}").into());
        }
    });
}

#[derive(Debug)]
enum ProjectCopyChoice {
    Selected(PathBuf),
    Cancelled,
}

fn complete_project_copy_choice(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_name: String,
    switch_to: bool,
    choice: ProjectCopyChoice,
) {
    complete_project_copy_choice_with(ui, project_name, switch_to, choice, |command| {
        submit_to_ui(ui, worker, command);
    });
}

fn complete_project_copy_choice_with(
    ui: &MainWindow,
    project_name: String,
    switch_to: bool,
    choice: ProjectCopyChoice,
    submit: impl FnOnce(IdeWorkerCommand),
) {
    match choice {
        ProjectCopyChoice::Selected(parent) => submit(IdeWorkerCommand::SaveProjectAs {
            parent,
            directory_name: project_name.clone(),
            project_name,
            switch_to,
        }),
        ProjectCopyChoice::Cancelled => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Save As cancelled".into());
        }
    }
}

fn bind_project_copy_callback_with(
    ui: &MainWindow,
    worker: &WorkerCell,
    choose: impl Fn(slint::Weak<MainWindow>, WorkerCell, String, bool) + 'static,
) {
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    ui.global::<IdeActions>()
        .on_save_project_as(move |project_name, switch_to| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running || !ui.get_save_project_as_visible() {
                return;
            }
            let project_name = project_name.trim().to_owned();
            if project_name.is_empty() {
                ui.set_error_text("Enter a project name before choosing its location".into());
                return;
            }
            ui.set_app_mode(AppMode::Saving);
            ui.set_error_text(SharedString::default());
            ui.set_status_text("Waiting for the independent project destination…".into());
            choose(weak.clone(), Rc::clone(&worker), project_name, switch_to);
        });
}

fn bind_project_rename_callback(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_project_rename_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

fn bind_project_rename_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let weak = ui.as_weak();
    ui.global::<IdeActions>()
        .on_rename_project(move |project_name| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running || !ui.get_rename_project_visible() {
                return;
            }
            let project_name = project_name.trim().to_owned();
            if project_name.is_empty() {
                ui.set_error_text("Enter a project name before renaming".into());
                return;
            }
            ui.set_error_text(SharedString::default());
            submitter(&ui, IdeWorkerCommand::RenameProject { project_name });
        });
}

fn bind_native_filter_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let worker = Rc::clone(worker);
    bind_native_filter_callbacks_with(
        ui,
        project_state,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "the generated native-device actions share one authoritative UI-to-worker binding"
)]
fn bind_native_filter_callbacks_with(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    submitter: UiCommandSubmitter,
) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let submit_add = Rc::clone(&submitter);
    let state_for_add = Rc::clone(project_state);
    actions.on_add_native_filter(move |track_index, mode_index, cutoff, resonance| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let edit = native_filter_addition(
            ui.get_app_mode(),
            &state_for_add.native_filter_tracks.borrow(),
            track_index,
            mode_index,
            cutoff.as_str(),
            resonance.as_str(),
        );
        match edit {
            Ok(edit) => submit_add(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let submit_update = Rc::clone(&submitter);
    actions.on_update_native_filter(move |index, enabled, mode_index, cutoff, resonance| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let edit = native_filter_update(
            ui.get_app_mode(),
            &ui.get_native_devices(),
            index,
            enabled,
            mode_index,
            cutoff.as_str(),
            resonance.as_str(),
        );
        match edit {
            Ok(edit) => submit_update(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let submit_add = Rc::clone(&submitter);
    let state_for_add = Rc::clone(project_state);
    actions.on_add_native_compressor(
        move |track_index, threshold, ratio, attack, release, makeup| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match native_compressor_addition(
                ui.get_app_mode(),
                &state_for_add.native_filter_tracks.borrow(),
                track_index,
                threshold.as_str(),
                ratio.as_str(),
                attack.as_str(),
                release.as_str(),
                makeup.as_str(),
            ) {
                Ok(edit) => submit_add(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
                Err(message) => ui.set_error_text(message.into()),
            }
        },
    );

    let weak = ui.as_weak();
    let submit_update = Rc::clone(&submitter);
    actions.on_update_native_compressor(
        move |index, enabled, threshold, ratio, attack, release, makeup| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            match native_compressor_update(
                ui.get_app_mode(),
                &ui.get_native_devices(),
                index,
                enabled,
                threshold.as_str(),
                ratio.as_str(),
                attack.as_str(),
                release.as_str(),
                makeup.as_str(),
            ) {
                Ok(edit) => submit_update(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
                Err(message) => ui.set_error_text(message.into()),
            }
        },
    );

    let weak = ui.as_weak();
    let submit_move = Rc::clone(&submitter);
    actions.on_move_native_device(move |index, offset| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match native_device_move(ui.get_app_mode(), &ui.get_native_devices(), index, offset) {
            Ok(edit) => submit_move(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let submit_duplicate = Rc::clone(&submitter);
    actions.on_duplicate_native_device(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match native_device_duplication(ui.get_app_mode(), &ui.get_native_devices(), index) {
            Ok(edit) => submit_duplicate(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let state_for_copy = Rc::clone(project_state);
    actions.on_copy_native_device(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match native_device_copy_selection(ui.get_app_mode(), &ui.get_native_devices(), index) {
            Ok(selection) => {
                *state_for_copy.native_device_clipboard.borrow_mut() = Some(selection);
                ui.set_native_device_copy_available(true);
                ui.set_status_text(
                    "Native device selected. Choose a target in another chain and paste after it."
                        .into(),
                );
                ui.set_error_text(SharedString::default());
            }
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let submit_paste = Rc::clone(&submitter);
    let state_for_paste = Rc::clone(project_state);
    actions.on_paste_native_device(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let selection = state_for_paste.native_device_clipboard.borrow().clone();
        match native_device_copy_to(
            ui.get_app_mode(),
            &ui.get_native_devices(),
            selection.as_ref(),
            index,
        ) {
            Ok(edit) => submit_paste(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let submit_remove = submitter;
    actions.on_remove_native_filter(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match native_filter_removal(ui.get_app_mode(), &ui.get_native_devices(), index) {
            Ok(edit) => submit_remove(&ui, IdeWorkerCommand::EditNativeFilter { edit }),
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn native_filter_addition(
    mode: AppMode,
    track_ids: &[String],
    track_index: i32,
    mode_index: i32,
    cutoff: &str,
    resonance: &str,
) -> Result<IdeNativeFilterEdit, &'static str> {
    if mode != AppMode::Running {
        return Err("Native Device editing is unavailable in the current state");
    }
    let track_id = usize::try_from(track_index)
        .ok()
        .and_then(|index| track_ids.get(index))
        .filter(|track_id| !track_id.is_empty())
        .ok_or("The selected mixer channel is no longer available")?;
    let (mode, cutoff_millihertz, resonance_millionths) =
        parse_native_filter_state(mode_index, cutoff, resonance)?;
    Ok(IdeNativeFilterEdit::Add {
        track_id: track_id.clone(),
        mode,
        cutoff_millihertz,
        resonance_millionths,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the generated compressor form emits its five persisted parameters independently"
)]
fn native_compressor_addition(
    mode: AppMode,
    track_ids: &[String],
    track_index: i32,
    threshold: &str,
    ratio: &str,
    attack: &str,
    release: &str,
    makeup: &str,
) -> Result<IdeNativeFilterEdit, &'static str> {
    if mode != AppMode::Running {
        return Err("Native Device editing is unavailable in the current state");
    }
    let track_id = usize::try_from(track_index)
        .ok()
        .and_then(|index| track_ids.get(index))
        .filter(|id| !id.is_empty())
        .ok_or("The selected mixer channel is no longer available")?;
    let (
        threshold_millidecibels,
        ratio_millionths,
        attack_microseconds,
        release_microseconds,
        makeup_millidecibels,
    ) = parse_native_compressor_state(threshold, ratio, attack, release, makeup)?;
    Ok(IdeNativeFilterEdit::AddCompressor {
        track_id: track_id.clone(),
        threshold_millidecibels,
        ratio_millionths,
        attack_microseconds,
        release_microseconds,
        makeup_millidecibels,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the generated compressor form emits its persisted state as separate controls"
)]
fn native_compressor_update(
    mode: AppMode,
    devices: &ModelRc<NativeDeviceRow>,
    index: i32,
    enabled: bool,
    threshold: &str,
    ratio: &str,
    attack: &str,
    release: &str,
    makeup: &str,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let row = native_filter_row(mode, devices, index)?;
    if row.kind_index != 1 {
        return Err("The selected Native Device is not a compressor");
    }
    let (
        threshold_millidecibels,
        ratio_millionths,
        attack_microseconds,
        release_microseconds,
        makeup_millidecibels,
    ) = parse_native_compressor_state(threshold, ratio, attack, release, makeup)?;
    Ok(IdeNativeFilterEdit::UpdateCompressor {
        chain_id: row.chain_id.to_string(),
        device_instance_id: row.device_instance_id.to_string(),
        enabled,
        threshold_millidecibels,
        ratio_millionths,
        attack_microseconds,
        release_microseconds,
        makeup_millidecibels,
    })
}

fn native_filter_update(
    mode: AppMode,
    filters: &ModelRc<NativeDeviceRow>,
    index: i32,
    enabled: bool,
    mode_index: i32,
    cutoff: &str,
    resonance: &str,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let row = native_filter_row(mode, filters, index)?;
    if row.kind_index != 0 {
        return Err("The selected Native Device is not a filter");
    }
    let (mode, cutoff_millihertz, resonance_millionths) =
        parse_native_filter_state(mode_index, cutoff, resonance)?;
    Ok(IdeNativeFilterEdit::Update {
        chain_id: row.chain_id.to_string(),
        device_instance_id: row.device_instance_id.to_string(),
        enabled,
        mode,
        cutoff_millihertz,
        resonance_millionths,
    })
}

fn native_filter_removal(
    mode: AppMode,
    filters: &ModelRc<NativeDeviceRow>,
    index: i32,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let row = native_filter_row(mode, filters, index)?;
    Ok(IdeNativeFilterEdit::Remove {
        chain_id: row.chain_id.to_string(),
        device_instance_id: row.device_instance_id.to_string(),
    })
}

fn native_device_move(
    mode: AppMode,
    devices: &ModelRc<NativeDeviceRow>,
    index: i32,
    offset: i32,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let row = native_filter_row(mode, devices, index)?;
    let next_index = index
        .checked_add(offset)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or("The selected native device cannot move beyond its chain")?;
    let target = devices
        .row_data(next_index)
        .filter(|target| target.chain_id == row.chain_id)
        .ok_or("The selected native device cannot move beyond its chain")?;
    let permitted = match offset {
        -1 => row.can_move_earlier,
        1 => row.can_move_later,
        _ => false,
    };
    if !permitted {
        return Err("The selected native device cannot move beyond its chain");
    }
    let destination_index = (0..next_index)
        .filter_map(|candidate| devices.row_data(candidate))
        .filter(|candidate| candidate.chain_id == row.chain_id)
        .count();
    if target.device_instance_id.is_empty() {
        return Err("The selected native device is no longer available");
    }
    Ok(IdeNativeFilterEdit::Move {
        chain_id: row.chain_id.to_string(),
        device_instance_id: row.device_instance_id.to_string(),
        destination_index,
    })
}

fn native_device_duplication(
    mode: AppMode,
    devices: &ModelRc<NativeDeviceRow>,
    index: i32,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let row = native_filter_row(mode, devices, index)?;
    if !row.can_duplicate {
        return Err("The selected native device chain cannot contain another device");
    }
    Ok(IdeNativeFilterEdit::Duplicate {
        chain_id: row.chain_id.to_string(),
        device_instance_id: row.device_instance_id.to_string(),
    })
}

fn native_device_copy_selection(
    mode: AppMode,
    devices: &ModelRc<NativeDeviceRow>,
    index: i32,
) -> Result<NativeDeviceClipboard, &'static str> {
    let row = native_filter_row(mode, devices, index)?;
    Ok(NativeDeviceClipboard {
        source_chain_id: row.chain_id.to_string(),
        source_device_instance_id: row.device_instance_id.to_string(),
    })
}

fn native_device_copy_to(
    mode: AppMode,
    devices: &ModelRc<NativeDeviceRow>,
    selection: Option<&NativeDeviceClipboard>,
    index: i32,
) -> Result<IdeNativeFilterEdit, &'static str> {
    let source = selection.ok_or("Copy a native device before choosing a paste target")?;
    let source_is_current = (0..devices.row_count())
        .filter_map(|candidate| devices.row_data(candidate))
        .any(|candidate| {
            candidate.chain_id == source.source_chain_id
                && candidate.device_instance_id == source.source_device_instance_id
        });
    if !source_is_current {
        return Err("The copied native device is no longer available");
    }
    let target = native_filter_row(mode, devices, index)?;
    if !target.can_duplicate {
        return Err("The selected native device chain cannot contain another device");
    }
    if target.chain_id == source.source_chain_id {
        return Err("Choose a paste target in another native device chain");
    }
    Ok(IdeNativeFilterEdit::CopyTo {
        source_chain_id: source.source_chain_id.clone(),
        source_device_instance_id: source.source_device_instance_id.clone(),
        target_chain_id: target.chain_id.to_string(),
        target_device_instance_id: target.device_instance_id.to_string(),
    })
}

fn native_filter_row(
    mode: AppMode,
    filters: &ModelRc<NativeDeviceRow>,
    index: i32,
) -> Result<NativeDeviceRow, &'static str> {
    if mode != AppMode::Running {
        return Err("Native Device editing is unavailable in the current state");
    }
    let row = usize::try_from(index)
        .ok()
        .and_then(|index| filters.row_data(index))
        .filter(|row| row.index == index)
        .filter(|row| !row.chain_id.is_empty() && !row.device_instance_id.is_empty())
        .ok_or("The selected native device is no longer available")?;
    Ok(row)
}

fn parse_native_filter_state(
    mode_index: i32,
    cutoff: &str,
    resonance: &str,
) -> Result<(NativeFilterMode, u32, u32), &'static str> {
    let mode = match mode_index {
        0 => NativeFilterMode::LowPass,
        1 => NativeFilterMode::HighPass,
        _ => return Err("Native filter mode is invalid"),
    };
    let cutoff_millihertz = parse_bounded_u32(
        cutoff,
        MIN_FILTER_CUTOFF_MILLIHERTZ,
        MAX_FILTER_CUTOFF_MILLIHERTZ,
        "Cutoff must be whole millihertz from 20000 through 20000000",
    )?;
    let resonance_millionths = parse_bounded_u32(
        resonance,
        MIN_FILTER_RESONANCE_MILLIONTHS,
        MAX_FILTER_RESONANCE_MILLIONTHS,
        "Resonance must be whole millionths from 100000 through 20000000",
    )?;
    Ok((mode, cutoff_millihertz, resonance_millionths))
}

fn parse_bounded_u32(
    value: &str,
    minimum: u32,
    maximum: u32,
    message: &'static str,
) -> Result<u32, &'static str> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(message);
    }
    let parsed = value.parse::<u32>().map_err(|_| message)?;
    if (minimum..=maximum).contains(&parsed) {
        Ok(parsed)
    } else {
        Err(message)
    }
}

fn parse_native_compressor_state(
    threshold: &str,
    ratio: &str,
    attack: &str,
    release: &str,
    makeup: &str,
) -> Result<(i32, u32, u32, u32, i32), &'static str> {
    Ok((
        parse_bounded_i32(
            threshold,
            MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS,
            MAX_COMPRESSOR_THRESHOLD_MILLIDECIBELS,
            "Threshold must be whole millidecibels from -80000 through 0",
        )?,
        parse_bounded_u32(
            ratio,
            MIN_COMPRESSOR_RATIO_MILLIONTHS,
            MAX_COMPRESSOR_RATIO_MILLIONTHS,
            "Ratio must be whole millionths from 1000000 through 100000000",
        )?,
        parse_bounded_u32(
            attack,
            MIN_COMPRESSOR_ATTACK_MICROSECONDS,
            MAX_COMPRESSOR_ATTACK_MICROSECONDS,
            "Attack must be whole microseconds from 100 through 500000",
        )?,
        parse_bounded_u32(
            release,
            MIN_COMPRESSOR_RELEASE_MICROSECONDS,
            MAX_COMPRESSOR_RELEASE_MICROSECONDS,
            "Release must be whole microseconds from 100 through 2000000",
        )?,
        parse_bounded_i32(
            makeup,
            MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS,
            MAX_COMPRESSOR_MAKEUP_MILLIDECIBELS,
            "Makeup must be whole millidecibels from -24000 through 24000",
        )?,
    ))
}

fn parse_bounded_i32(
    value: &str,
    minimum: i32,
    maximum: i32,
    message: &'static str,
) -> Result<i32, &'static str> {
    let value = value.parse::<i32>().map_err(|_| message)?;
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(message)
    }
}

fn bind_scene_creation_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let state_for_scene_creation = Rc::clone(project_state);
    actions.on_begin_add_scene(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let scenes = state_for_scene_creation.scenes.borrow();
        if ui.get_app_mode() != AppMode::Running || !scene_capacity_available(scenes.len()) {
            ui.set_error_text("The current project cannot add another scene".into());
            return;
        }
        ui.set_scene_create_name(next_scene_name(&scenes).into());
        ui.set_scene_create_quantization_index(0);
        ui.set_scene_create_quantum_ticks(SharedString::default());
        ui.set_scene_create_visible(true);
        ui.set_error_text(SharedString::default());
    });

    let weak = ui.as_weak();
    let worker_for_scene_creation = Rc::clone(worker);
    let state_for_scene_creation = Rc::clone(project_state);
    actions.on_add_scene(move |name, quantization_index, quantum_ticks| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_scene_create_visible() {
            ui.set_error_text("Scene creation is no longer current".into());
            return;
        }
        if !scene_capacity_available(state_for_scene_creation.scenes.borrow().len()) {
            ui.set_error_text("The current project cannot add another scene".into());
            return;
        }
        match scene_creation_command(name.as_str(), quantization_index, quantum_ticks.as_str()) {
            Ok(command) => submit_to_ui(&ui, &worker_for_scene_creation, command),
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn bind_scene_slot_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let state_for_scene_slots = Rc::clone(project_state);
    actions.on_open_scene_slots(move |scene_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Scene track actions are unavailable in the current state".into());
            return;
        }
        match open_scene_slots(&ui, &state_for_scene_slots, scene_id.as_str()) {
            Ok(()) => ui.set_error_text(SharedString::default()),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let state_for_scene_slot_selection = Rc::clone(project_state);
    actions.on_select_scene_slot(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_scene_slots_visible() {
            ui.set_error_text("Scene track-action selection is no longer current".into());
            return;
        }
        match select_scene_slot(&ui, &state_for_scene_slot_selection, index) {
            Ok(()) => ui.set_error_text(SharedString::default()),
            Err(message) => ui.set_error_text(message.into()),
        }
    });

    let weak = ui.as_weak();
    let worker_for_scene_slot = Rc::clone(worker);
    let state_for_scene_slot = Rc::clone(project_state);
    actions.on_set_scene_slot(move |scene_id, track_id, action_index, clip_index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_scene_slots_visible()
            || ui.get_scene_slots_scene_id() != scene_id
            || ui.get_selected_scene_slot_track_id() != track_id
        {
            ui.set_error_text("Scene track-action selection is no longer current".into());
            return;
        }
        match scene_slot_command(
            &state_for_scene_slot,
            scene_id.as_str(),
            track_id.as_str(),
            action_index,
            clip_index,
        ) {
            Ok(Some(command)) => submit_to_ui(&ui, &worker_for_scene_slot, command),
            Ok(None) => {
                ui.set_status_text("Scene track action already matches the committed value".into());
                ui.set_error_text(SharedString::default());
            }
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn bind_scene_rename_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_scene_rename = Rc::clone(worker);
    actions.on_rename_scene(move |scene_id, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_scene_rename_visible()
            || ui.get_scene_rename_id() != scene_id
        {
            ui.set_error_text("Scene rename selection is no longer current".into());
            return;
        }
        let scenes = ui.get_scenes();
        let Some(scene) = scenes.iter().find(|scene| scene.id == scene_id) else {
            ui.set_error_text("The selected scene is no longer available".into());
            return;
        };
        let command =
            match scene_rename_command(scene.id.as_str(), scene.name.as_str(), name.as_str()) {
                Ok(Some(command)) => command,
                Ok(None) => {
                    reset_scene_rename_form(&ui);
                    ui.set_status_text("Scene name already matches the committed value".into());
                    ui.set_error_text(SharedString::default());
                    return;
                }
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
        submit_to_ui(&ui, &worker_for_scene_rename, command);
    });
}

fn bind_scene_duplication_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_scene_duplication = Rc::clone(worker);
    let state_for_scene_duplication = Rc::clone(project_state);
    actions.on_duplicate_scene(move |scene_id, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_scene_duplicate_visible()
            || ui.get_scene_duplicate_id() != scene_id
        {
            ui.set_error_text("Scene duplication selection is no longer current".into());
            return;
        }
        let scenes = state_for_scene_duplication.scenes.borrow();
        if !scene_capacity_available(scenes.len()) {
            ui.set_error_text("The current project cannot add another scene".into());
            return;
        }
        let Some(scene) = scenes.iter().find(|scene| scene.id() == scene_id.as_str()) else {
            ui.set_error_text("The selected scene is no longer available".into());
            return;
        };
        let command = match scene_duplication_command(scene.id(), name.as_str()) {
            Ok(command) => command,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(scenes);
        submit_to_ui(&ui, &worker_for_scene_duplication, command);
    });
}

fn bind_scene_callbacks(ui: &MainWindow, worker: &WorkerCell, project_state: &ProjectUiStateCell) {
    bind_scene_creation_callbacks(ui, worker, project_state);
    bind_scene_slot_callbacks(ui, worker, project_state);
    bind_scene_rename_callback(ui, worker);
    bind_scene_duplication_callback(ui, worker, project_state);
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_scene = Rc::clone(worker);
    actions.on_launch_scene(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let scene_index = match validate_scene_launch(ui.get_app_mode(), index, &ui.get_scenes()) {
            Ok(scene_index) => scene_index,
            Err(reason) => {
                ui.set_error_text(reason.message().into());
                return;
            }
        };
        submit_to_ui(
            &ui,
            &worker_for_scene,
            IdeWorkerCommand::Transport(TransportCommand::LaunchScene { scene_index }),
        );
    });

    let weak = ui.as_weak();
    let worker_for_scene_activation = Rc::clone(worker);
    actions.on_set_scene_active(move |scene_id, active| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Scene activation is unavailable in the current state".into());
            return;
        }
        let scenes = ui.get_scenes();
        let Some(scene) = scenes.iter().find(|scene| scene.id == scene_id) else {
            ui.set_error_text("The selected scene is no longer available".into());
            return;
        };
        let command = match scene_activation_command(scene.id.as_str(), scene.active, active) {
            Ok(Some(command)) => command,
            Ok(None) => {
                ui.set_status_text("Scene activation already matches the committed value".into());
                ui.set_error_text(SharedString::default());
                return;
            }
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        submit_to_ui(&ui, &worker_for_scene_activation, command);
    });

    let weak = ui.as_weak();
    let worker_for_scene_removal = Rc::clone(worker);
    actions.on_remove_scene(move |scene_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Scene removal is unavailable in the current state".into());
            return;
        }
        let scenes = ui.get_scenes();
        let Some(scene) = scenes.iter().find(|scene| scene.id == scene_id) else {
            ui.set_error_text("The selected scene is no longer available".into());
            return;
        };
        if !scene.removable {
            ui.set_error_text(
                "Clear this scene's arrangement launches and control mappings before removing it"
                    .into(),
            );
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_scene_removal,
            IdeWorkerCommand::RemoveScene {
                scene_id: scene.id.to_string(),
            },
        );
    });
}

fn bind_audio_track_callback(ui: &MainWindow, worker: &WorkerCell) {
    bind_audio_track_add_callback(ui, worker);
    bind_routing_bus_callbacks(ui, worker);
    bind_mixer_strip_duplication_callback(ui, worker);
    bind_audio_track_rename_callback(ui, worker);
    bind_routing_bus_rename_callback(ui, worker);
    bind_mixer_strip_reorder_callback(ui, worker);
    bind_audio_track_activation_callback(ui, worker);
    bind_audio_track_removal_callback(ui, worker);
    bind_audio_track_content_removal_callback(ui, worker);
    bind_audio_track_freeze_callbacks(ui, worker);
}

fn bind_routing_bus_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    bind_routing_bus_removal_callback(ui, worker);
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_begin_routing_bus_creation(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || ui.get_track_count() >= 8
            || ui.get_routing_bus_create_visible()
        {
            return;
        }
        begin_routing_bus_creation(&ui);
    });

    let weak = ui.as_weak();
    actions.on_toggle_routing_bus_source(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_routing_bus_create_visible() {
            ui.set_error_text("Routing-bus source selection is no longer available".into());
            return;
        }
        let Ok(index) = usize::try_from(index) else {
            ui.set_error_text("The selected audio source is no longer available".into());
            return;
        };
        let sources = ui.get_routing_bus_sources();
        let Some(mut source) = sources.row_data(index) else {
            ui.set_error_text("The selected audio source is no longer available".into());
            return;
        };
        source.selected = !source.selected;
        let mut next = sources.iter().collect::<Vec<_>>();
        next[index] = source;
        let selected_count = next.iter().filter(|source| source.selected).count();
        ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(next))));
        ui.set_routing_bus_selected_source_count(i32::try_from(selected_count).unwrap_or(-1));
        ui.set_error_text(SharedString::default());
    });

    let weak = ui.as_weak();
    let worker_for_bus = Rc::clone(worker);
    actions.on_add_routing_bus(move |kind_index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_routing_bus_create_visible() {
            ui.set_error_text("Routing-bus creation is unavailable in the current state".into());
            return;
        }
        if ui.get_track_count() >= 8 {
            ui.set_error_text("The current project cannot add another routing bus".into());
            return;
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            ui.set_error_text("Routing-bus name must contain 1 through 120 characters".into());
            return;
        }
        let kind = match kind_index {
            0 => IdeRoutingBusKind::Group,
            1 => IdeRoutingBusKind::Return,
            _ => {
                ui.set_error_text("The selected routing-bus type is no longer available".into());
                return;
            }
        };
        let source_track_ids = ui
            .get_routing_bus_sources()
            .iter()
            .filter(|source| source.selected)
            .map(|source| source.track_id.to_string())
            .collect::<Vec<_>>();
        if source_track_ids.is_empty() {
            ui.set_error_text("Select at least one audio source for the routing bus".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_bus,
            IdeWorkerCommand::AddRoutingBus {
                kind,
                name: name.to_owned(),
                source_track_ids,
            },
        );
    });
}

fn bind_routing_bus_removal_callback(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_routing_bus_removal_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the generated callback retains the shared command submitter for the window lifetime"
)]
fn bind_routing_bus_removal_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_remove_routing_bus(move |track_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_routing_bus_removal_visible()
            || ui.get_routing_bus_removal_id() != track_id
        {
            ui.set_error_text("Routing-bus removal confirmation is no longer current".into());
            return;
        }
        let Ok(track_id) = validate_routing_bus_removal(
            ui.get_app_mode(),
            track_id.as_str(),
            &ui.get_mixer_channels(),
        ) else {
            ui.set_error_text("The selected routing bus is no longer available".into());
            return;
        };
        submitter(&ui, IdeWorkerCommand::RemoveRoutingBus { track_id });
    });
}

fn bind_audio_track_add_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_track = Rc::clone(worker);
    actions.on_add_audio_track(move |name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || ui.get_track_count() >= 8 {
            ui.set_error_text("The current project cannot add another audio track".into());
            return;
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            ui.set_error_text("Audio-track name must contain 1 through 120 characters".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_track,
            IdeWorkerCommand::AddAudioTrack {
                name: name.to_owned(),
            },
        );
    });
}

fn bind_mixer_strip_duplication_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_duplication = Rc::clone(worker);
    actions.on_duplicate_mixer_strip(move |index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match mixer_strip_duplication_command(
            ui.get_app_mode(),
            index,
            name.as_str(),
            ui.get_track_count(),
            &ui.get_mixer_channels(),
        ) {
            Ok(command) => submit_to_ui(&ui, &worker_for_duplication, command),
            Err(reason) => ui.set_error_text(reason.message().into()),
        }
    });
}

fn bind_audio_track_rename_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_rename = Rc::clone(worker);
    actions.on_rename_audio_track(move |index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Audio-track rename is not available in the current state".into());
            return;
        }
        let Ok((_, row)) =
            validate_mixer_channel(ui.get_app_mode(), index, &ui.get_mixer_channels())
        else {
            ui.set_error_text("The selected audio track is no longer available".into());
            return;
        };
        if !row.renameable {
            ui.set_error_text("Only audio tracks can be renamed in this workflow".into());
            return;
        }
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            ui.set_error_text("Audio-track name must contain 1 through 120 characters".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_rename,
            IdeWorkerCommand::RenameAudioTrack {
                track_id: row.track_id.to_string(),
                name: name.to_owned(),
            },
        );
    });
}

fn bind_routing_bus_rename_callback(ui: &MainWindow, worker: &WorkerCell) {
    let worker_for_rename = Rc::clone(worker);
    bind_routing_bus_rename_callback_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker_for_rename, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the generated callback retains the shared command submitter for the window lifetime"
)]
fn bind_routing_bus_rename_callback_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_rename_routing_bus(move |index, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match routing_bus_rename_command(
            ui.get_app_mode(),
            index,
            name.as_str(),
            &ui.get_mixer_channels(),
        ) {
            Ok(command) => submitter(&ui, command),
            Err(reason) => ui.set_error_text(reason.message().into()),
        }
    });
}

fn bind_mixer_strip_reorder_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_reorder = Rc::clone(worker);
    actions.on_reorder_audio_track(move |index, earlier| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        match mixer_strip_reorder_command(
            ui.get_app_mode(),
            index,
            earlier,
            &ui.get_mixer_channels(),
        ) {
            Ok(command) => submit_to_ui(&ui, &worker_for_reorder, command),
            Err(reason) => ui.set_error_text(reason.message().into()),
        }
    });
}

fn bind_audio_track_activation_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_activation = Rc::clone(worker);
    actions.on_set_audio_track_active(move |index, active| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Ok((_, row)) =
            validate_mixer_channel(ui.get_app_mode(), index, &ui.get_mixer_channels())
        else {
            ui.set_error_text("The selected audio track is no longer available".into());
            return;
        };
        if !row.activation_toggleable {
            ui.set_error_text("Only audio tracks can be enabled or disabled".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_activation,
            IdeWorkerCommand::SetAudioTrackActive {
                track_id: row.track_id.to_string(),
                active,
            },
        );
    });
}

fn bind_audio_track_removal_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_removal = Rc::clone(worker);
    actions.on_remove_audio_track(move |track_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Ok(track_id) = validate_audio_track_removal(
            ui.get_app_mode(),
            track_id.as_str(),
            &ui.get_mixer_channels(),
        ) else {
            ui.set_error_text("The selected audio track is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_removal,
            IdeWorkerCommand::RemoveAudioTrack { track_id },
        );
    });
}

fn bind_audio_track_content_removal_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_removal = Rc::clone(worker);
    actions.on_remove_audio_track_content(move |track_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_remove_track_content_visible()
            || ui.get_remove_track_content_id() != track_id
        {
            ui.set_error_text(
                "Audio-track content removal confirmation is no longer current".into(),
            );
            return;
        }
        let Ok(track_id) = validate_audio_track_content_removal(
            ui.get_app_mode(),
            track_id.as_str(),
            &ui.get_mixer_channels(),
        ) else {
            ui.set_error_text("The selected audio track is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_removal,
            IdeWorkerCommand::RemoveAudioTrackContent { track_id },
        );
    });
}

fn bind_audio_track_freeze_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_audio_track_freeze_callbacks_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the generated callbacks retain the shared command submitter for the window lifetime"
)]
fn bind_audio_track_freeze_callbacks_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let freeze_submitter = Rc::clone(&submitter);
    actions.on_freeze_audio_track(move |track_id| {
        submit_audio_track_freeze_request(&weak, &freeze_submitter, track_id.as_str(), false);
    });

    let weak = ui.as_weak();
    let unfreeze_submitter = Rc::clone(&submitter);
    actions.on_unfreeze_audio_track(move |track_id| {
        submit_audio_track_freeze_request(&weak, &unfreeze_submitter, track_id.as_str(), true);
    });

    let weak = ui.as_weak();
    actions.on_flatten_audio_track(move |track_id| {
        submit_audio_track_flatten_request(&weak, &submitter, track_id.as_str());
    });
}

fn submit_audio_track_freeze_request(
    weak: &slint::Weak<MainWindow>,
    submitter: &UiCommandSubmitter,
    requested_track_id: &str,
    unfreeze: bool,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    if ui.get_app_mode() != AppMode::Running
        || !ui.get_track_freeze_visible()
        || ui.get_track_freeze_id() != requested_track_id
        || ui.get_track_freeze_is_unfreeze() != unfreeze
        || ui.get_track_freeze_is_flatten()
    {
        ui.set_error_text("Audio-track freeze confirmation is no longer current".into());
        return;
    }
    let Ok(track_id) = validate_audio_track_freeze_operation(
        ui.get_app_mode(),
        requested_track_id,
        &ui.get_mixer_channels(),
        unfreeze,
    ) else {
        ui.set_error_text("The selected audio track is no longer available".into());
        return;
    };
    submitter(
        &ui,
        if unfreeze {
            IdeWorkerCommand::UnfreezeAudioTrack { track_id }
        } else {
            IdeWorkerCommand::FreezeAudioTrack { track_id }
        },
    );
}

fn submit_audio_track_flatten_request(
    weak: &slint::Weak<MainWindow>,
    submitter: &UiCommandSubmitter,
    requested_track_id: &str,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    if ui.get_app_mode() != AppMode::Running
        || !ui.get_track_freeze_visible()
        || ui.get_track_freeze_id() != requested_track_id
        || ui.get_track_freeze_is_unfreeze()
        || !ui.get_track_freeze_is_flatten()
    {
        ui.set_error_text("Audio-track flatten confirmation is no longer current".into());
        return;
    }
    let Ok(track_id) = validate_audio_track_flatten_operation(
        ui.get_app_mode(),
        requested_track_id,
        &ui.get_mixer_channels(),
    ) else {
        ui.set_error_text(
            "The selected frozen audio track no longer has complete freeze authority".into(),
        );
        return;
    };
    submitter(&ui, IdeWorkerCommand::FlattenAudioTrack { track_id });
}

fn bind_loudness_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_run = Rc::clone(worker);
    actions.on_loudness_run(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_run,
            IdeWorkerCommand::Loudness(IdeLoudnessCommand::Run),
        );
    });

    let weak = ui.as_weak();
    let worker_for_standby = Rc::clone(worker);
    actions.on_loudness_standby(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_standby,
            IdeWorkerCommand::Loudness(IdeLoudnessCommand::Standby),
        );
    });

    let weak = ui.as_weak();
    let worker_for_reset = Rc::clone(worker);
    actions.on_loudness_reset(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_reset,
            IdeWorkerCommand::Loudness(IdeLoudnessCommand::Reset),
        );
    });
}

fn bind_output_peak_hold_callback(ui: &MainWindow, worker: &WorkerCell) {
    bind_output_peak_hold_callback_with(ui, {
        let worker = Rc::clone(worker);
        move |ui| try_submit_to_ui(ui, &worker, IdeWorkerCommand::ResetOutputPeakHold)
    });
}

fn bind_output_peak_hold_callback_with(
    ui: &MainWindow,
    submit: impl Fn(&MainWindow) -> bool + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_reset_output_peak_hold(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_output_peak_hold_available()
            || ui.get_output_peak_hold_reset_pending()
        {
            return;
        }
        if submit(&ui) {
            ui.set_output_peak_hold_reset_pending(true);
        }
    });
}

fn bind_mixer_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_gain = Rc::clone(worker);
    actions.on_mixer_channel_gain(move |index, percent| {
        submit_channel_mixer_command(&weak, &worker_for_gain, index, |channel_index, _| {
            Ok(IdeMixerCommand::ChannelGain {
                channel_index,
                gain_millionths: gain_percent_to_millionths(percent)?,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_pan = Rc::clone(worker);
    actions.on_mixer_channel_pan(move |index, percent| {
        submit_channel_mixer_command(&weak, &worker_for_pan, index, |channel_index, _| {
            Ok(IdeMixerCommand::ChannelPan {
                channel_index,
                pan_millionths: pan_percent_to_millionths(percent)?,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_mute = Rc::clone(worker);
    actions.on_mixer_channel_mute(move |index| {
        submit_channel_mixer_command(&weak, &worker_for_mute, index, |channel_index, row| {
            Ok(IdeMixerCommand::ChannelMute {
                channel_index,
                muted: !row.muted,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_solo = Rc::clone(worker);
    actions.on_mixer_channel_solo(move |index| {
        submit_channel_mixer_command(&weak, &worker_for_solo, index, |channel_index, row| {
            if !row.solo_available {
                return Err(MixerControlFailure::SoloUnavailable);
            }
            Ok(IdeMixerCommand::ChannelSolo {
                channel_index,
                solo: !row.solo,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_main_gain = Rc::clone(worker);
    actions.on_mixer_main_gain(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_mixer_main_state_available() {
            ui.set_error_text(MixerControlFailure::StateUnavailable.message().into());
            return;
        }
        match gain_percent_to_millionths(percent) {
            Ok(gain_millionths) => submit_to_ui(
                &ui,
                &worker_for_main_gain,
                IdeWorkerCommand::Mixer(IdeMixerCommand::MainGain { gain_millionths }),
            ),
            Err(reason) => ui.set_error_text(reason.message().into()),
        }
    });

    let weak = ui.as_weak();
    let worker_for_main_mute = Rc::clone(worker);
    actions.on_mixer_main_mute(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_mixer_main_state_available() {
            ui.set_error_text(MixerControlFailure::StateUnavailable.message().into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_main_mute,
            IdeWorkerCommand::Mixer(IdeMixerCommand::MainMute {
                muted: !ui.get_mixer_main_muted(),
            }),
        );
    });

    bind_send_callbacks(ui, worker);
}

fn bind_send_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let worker_for_send_gain = Rc::clone(worker);
    actions.on_mixer_send_gain(move |index, percent| {
        submit_send_mixer_command(&weak, &worker_for_send_gain, index, |row| {
            Ok(IdeMixerCommand::SendGain {
                channel_index: usize::try_from(row.channel_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                send_index: usize::try_from(row.send_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                gain_millionths: gain_percent_to_millionths(percent)?,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_send_pan = Rc::clone(worker);
    actions.on_mixer_send_pan(move |index, percent| {
        submit_send_mixer_command(&weak, &worker_for_send_pan, index, |row| {
            Ok(IdeMixerCommand::SendPan {
                channel_index: usize::try_from(row.channel_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                send_index: usize::try_from(row.send_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                pan_millionths: pan_percent_to_millionths(percent)?,
            })
        });
    });

    let weak = ui.as_weak();
    let worker_for_send_enabled = Rc::clone(worker);
    actions.on_mixer_send_enabled(move |index| {
        submit_send_mixer_command(&weak, &worker_for_send_enabled, index, |row| {
            Ok(IdeMixerCommand::SendEnabled {
                channel_index: usize::try_from(row.channel_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                send_index: usize::try_from(row.send_index)
                    .map_err(|_| MixerControlFailure::InvalidSelection)?,
                enabled: !row.enabled,
            })
        });
    });

    bind_return_send_callbacks(ui, worker);
    bind_audio_primary_route_callbacks(ui, worker);
}

fn bind_return_send_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_return_send_callbacks_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "each generated callback retains the shared command submitter for the window lifetime"
)]
fn bind_return_send_callbacks_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();

    let weak = ui.as_weak();
    let add_submitter = Rc::clone(&submitter);
    actions.on_add_return_send(move |source_track_id, return_track_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_return_send_candidates().iter().any(|candidate| {
                candidate.source_track_id == source_track_id
                    && candidate.return_track_id == return_track_id
            })
        {
            ui.set_error_text("The selected return-send route is no longer available".into());
            return;
        }
        add_submitter(
            &ui,
            IdeWorkerCommand::AddReturnSend {
                source_track_id: source_track_id.to_string(),
                return_track_id: return_track_id.to_string(),
            },
        );
    });

    let weak = ui.as_weak();
    actions.on_remove_return_send(move |send_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui
                .get_mixer_sends()
                .iter()
                .any(|send| send.send_id == send_id)
        {
            ui.set_error_text("The selected return send is no longer available".into());
            return;
        }
        submitter(
            &ui,
            IdeWorkerCommand::RemoveReturnSend {
                send_id: send_id.to_string(),
            },
        );
    });
}

fn bind_audio_primary_route_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_audio_primary_route_callbacks_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the generated callback retains the shared command submitter for the window lifetime"
)]
fn bind_audio_primary_route_callbacks_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_set_audio_primary_route(
        move |source_track_id, target_group_track_id, targets_main| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running
                || !ui.get_primary_route_candidates().iter().any(|candidate| {
                    candidate.source_track_id == source_track_id
                        && candidate.target_group_track_id == target_group_track_id
                        && candidate.targets_main == targets_main
                })
            {
                ui.set_error_text("The selected primary route is no longer available".into());
                return;
            }
            let target_group_track_id = if targets_main {
                None
            } else {
                Some(target_group_track_id.to_string())
            };
            submitter(
                &ui,
                IdeWorkerCommand::SetAudioPrimaryRoute {
                    source_track_id: source_track_id.to_string(),
                    target_group_track_id,
                },
            );
        },
    );
}

fn bind_arrangement_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    bind_arrangement_scene_launch_callback(ui, worker, project_state);
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_locate = Rc::clone(worker);
    let project_state_for_locate = Rc::clone(project_state);
    actions.on_locate_placement(move |index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let Ok(index) = usize::try_from(index) else {
            ui.set_error_text("Arrangement placement selection is invalid".into());
            return;
        };
        let state = project_state_for_locate.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding.owner.placements().get(index) else {
            ui.set_error_text("Arrangement placement selection is no longer current".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker_for_locate,
            IdeWorkerCommand::LocatePlacement {
                project_id: binding.project_id.clone(),
                index,
                placement_id: placement.id().to_owned(),
            },
        );
    });

    let weak = ui.as_weak();
    let worker_for_activation = Rc::clone(worker);
    let project_state_for_activation = Rc::clone(project_state);
    actions.on_set_arrangement_placement_active(move |index, active| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement activation is not available in the current state".into(),
            );
            return;
        }
        let Ok(index) = usize::try_from(index) else {
            ui.set_error_text("Arrangement placement selection is invalid".into());
            return;
        };
        let state = project_state_for_activation.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding.owner.placements().get(index) else {
            ui.set_error_text("Arrangement placement selection is no longer current".into());
            return;
        };
        if active == placement.enabled() {
            ui.set_error_text("Arrangement placement state is no longer current".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_activation,
            IdeWorkerCommand::SetArrangementPlacementActive {
                placement_id: placement.id().to_owned(),
                active,
            },
        );
    });

    bind_arrangement_move_callback(ui, worker, project_state);
    bind_arrangement_duplication_callback(ui, worker, project_state);
    bind_arrangement_split_callback(ui, worker, project_state);
    bind_arrangement_duration_callback(ui, worker, project_state);
    bind_arrangement_gain_callback(ui, worker, project_state);
    bind_arrangement_fades_callback(ui, worker, project_state);
    bind_arrangement_crossfade_callback(ui, worker, project_state);
    bind_arrangement_front_trim_callback(ui, worker, project_state);
    bind_arrangement_removal_callback(ui, worker, project_state);
}

fn bind_arrangement_scene_launch_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    bind_arrangement_scene_launch_addition_callback(ui, worker, project_state);
    bind_arrangement_scene_launch_move_callback(ui, worker, project_state);
    bind_mixer_automation_capture_callbacks(ui, worker, project_state);
    bind_automation_take_callback(ui, worker, project_state);
    bind_automation_take_edit_callbacks(ui, worker, project_state);
    bind_automation_take_activation_callback(ui, worker);
    bind_automation_take_removal_callback(ui, worker, project_state);
    bind_control_mapping_callbacks(ui, worker, project_state);
    bind_project_export_callback(ui, worker, project_state);
    bind_project_stem_export_callback(ui, worker);
    bind_undo_callbacks(ui, worker);
    bind_loop_callbacks(ui, worker, project_state);
    bind_tempo_callback(ui, worker);
    bind_meter_callback(ui, worker);
    bind_timeline_map_callbacks(ui, worker);
    bind_timeline_marker_callbacks(ui, worker, project_state);
    bind_arrangement_scene_launch_locate_callback(ui, worker, project_state);
    bind_arrangement_scene_launch_scene_callback(ui, worker, project_state);
    bind_arrangement_scene_launch_removal_callback(ui, worker, project_state);
}

#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "the public return type is owned by a transitive crate that this binary does not link directly"
)]
fn bind_arrangement_scene_launch_locate_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_locate = Rc::clone(worker);
    let project_state_for_locate = Rc::clone(project_state);
    actions.on_locate_arrangement_scene_launch(move |index, launch_id, play| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let state = project_state_for_locate.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement scene-marker authority is no longer available".into());
            return;
        };
        let current_launch_id = usize::try_from(index).ok().and_then(|index| {
            binding
                .owner
                .scene_launches()
                .get(index)
                .map(|launch| launch.id())
        });
        let command = match arrangement_scene_launch_locate_command(
            ui.get_app_mode(),
            binding.project_id.as_str(),
            index,
            launch_id.as_str(),
            current_launch_id,
            play,
        ) {
            Ok(command) => command,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(state);
        submit_to_ui(&ui, &worker_for_locate, command);
    });
}

fn arrangement_scene_launch_locate_command(
    app_mode: AppMode,
    project_id: &str,
    index: i32,
    requested_launch_id: &str,
    current_launch_id: Option<&str>,
    play: bool,
) -> Result<IdeWorkerCommand, &'static str> {
    if app_mode != AppMode::Running {
        return Err("Scene-marker locate is unavailable in the current state");
    }
    let index =
        usize::try_from(index).map_err(|_| "The selected scene marker is no longer available")?;
    if current_launch_id != Some(requested_launch_id) {
        return Err("The selected scene marker is no longer available");
    }
    Ok(IdeWorkerCommand::LocateSceneLaunch {
        project_id: project_id.to_owned(),
        index,
        launch_id: requested_launch_id.to_owned(),
        state: if play {
            LocateState::Rolling
        } else {
            LocateState::Preserve
        },
    })
}

fn bind_arrangement_scene_launch_scene_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_scene = Rc::clone(worker);
    let project_state_for_scene = Rc::clone(project_state);
    actions.on_set_arrangement_scene_launch_scene(move |launch_id, target_index| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !arrangement_scene_launch_form_is_current(
            ui.get_app_mode(),
            ui.get_set_arrangement_scene_launch_scene_visible(),
            ui.get_set_arrangement_scene_launch_scene_id().as_str(),
            launch_id.as_str(),
        ) {
            ui.set_error_text("Scene-marker scene request is no longer current".into());
            return;
        }
        let Ok(confirmed_tick) = parse_arrangement_scene_launch_tick(
            ui.get_set_arrangement_scene_launch_scene_tick().as_str(),
        ) else {
            ui.set_error_text("Scene-marker scene request is no longer current".into());
            return;
        };
        let Ok(current_index) =
            usize::try_from(ui.get_set_arrangement_scene_launch_scene_current_index())
        else {
            ui.set_error_text("Scene-marker scene request is no longer current".into());
            return;
        };
        let Ok(target_index) = usize::try_from(target_index) else {
            ui.set_error_text("The replacement scene is no longer available".into());
            return;
        };
        let arrangement = project_state_for_scene.arrangement.borrow();
        let Some(binding) = arrangement.as_ref() else {
            ui.set_error_text("Arrangement scene-marker authority is no longer available".into());
            return;
        };
        let Some(launch) = binding
            .owner
            .scene_launches()
            .iter()
            .find(|launch| launch.id() == launch_id.as_str())
        else {
            ui.set_error_text("The selected scene marker is no longer available".into());
            return;
        };
        let scenes = project_state_for_scene.scenes.borrow();
        let current_scene_id = ui.get_set_arrangement_scene_launch_scene_current_id();
        if !arrangement_scene_launch_scene_authority_is_current(
            launch.tick(),
            confirmed_tick,
            launch.scene_index(),
            current_index,
            scenes.get(current_index).map(IdeScene::id),
            current_scene_id.as_str(),
        ) {
            ui.set_error_text("The selected scene marker has changed since it was opened".into());
            return;
        }
        let launch_id = launch.id().to_owned();
        let command = match arrangement_scene_launch_scene_command(
            &launch_id,
            current_index,
            target_index,
            scenes.get(target_index).map(IdeScene::id),
        ) {
            Ok(Some(command)) => command,
            Ok(None) => {
                ui.set_status_text("Scene marker already launches that scene".into());
                ui.set_error_text(SharedString::default());
                return;
            }
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(scenes);
        drop(arrangement);
        submit_to_ui(&ui, &worker_for_scene, command);
    });
}

fn arrangement_scene_launch_scene_authority_is_current(
    launch_tick: u64,
    confirmed_tick: u64,
    launch_scene_index: usize,
    confirmed_scene_index: usize,
    authoritative_scene_id: Option<&str>,
    confirmed_scene_id: &str,
) -> bool {
    launch_tick == confirmed_tick
        && launch_scene_index == confirmed_scene_index
        && authoritative_scene_id == Some(confirmed_scene_id)
}

fn arrangement_scene_launch_scene_command(
    launch_id: &str,
    current_scene_index: usize,
    target_scene_index: usize,
    target_scene_id: Option<&str>,
) -> Result<Option<IdeWorkerCommand>, &'static str> {
    let target_scene_id = target_scene_id.ok_or("The replacement scene is no longer available")?;
    if target_scene_index == current_scene_index {
        return Ok(None);
    }
    Ok(Some(IdeWorkerCommand::SetArrangementSceneLaunchScene {
        launch_id: launch_id.to_owned(),
        scene_id: target_scene_id.to_owned(),
    }))
}

fn bind_arrangement_scene_launch_addition_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_addition = Rc::clone(worker);
    let project_state_for_addition = Rc::clone(project_state);
    actions.on_add_arrangement_scene_launch(move |scene_id, tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_add_arrangement_scene_launch_visible()
            || ui.get_add_arrangement_scene_launch_id().as_str() != scene_id.as_str()
        {
            ui.set_error_text("Scene-marker request is no longer current".into());
            return;
        }
        let tick = match parse_arrangement_scene_launch_tick(tick.as_str()) {
            Ok(tick) => tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        if !project_state_for_addition
            .scenes
            .borrow()
            .iter()
            .any(|scene| scene.id() == scene_id.as_str())
        {
            ui.set_error_text("The selected scene is no longer available".into());
            return;
        }
        if project_state_for_addition
            .arrangement
            .borrow()
            .as_ref()
            .is_some_and(|binding| {
                binding
                    .owner
                    .scene_launches()
                    .iter()
                    .any(|launch| launch.tick() == tick)
            })
        {
            ui.set_error_text("The arrangement already has a scene marker at that tick".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_addition,
            IdeWorkerCommand::AddArrangementSceneLaunch {
                scene_id: scene_id.to_string(),
                tick,
            },
        );
    });
}

fn bind_arrangement_scene_launch_move_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_move = Rc::clone(worker);
    let project_state_for_move = Rc::clone(project_state);
    actions.on_move_arrangement_scene_launch(move |launch_id, tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !arrangement_scene_launch_form_is_current(
            ui.get_app_mode(),
            ui.get_move_arrangement_scene_launch_visible(),
            ui.get_move_arrangement_scene_launch_id().as_str(),
            launch_id.as_str(),
        ) {
            ui.set_error_text("Scene-marker move request is no longer current".into());
            return;
        }
        let tick = match parse_arrangement_scene_launch_tick(tick.as_str()) {
            Ok(tick) => tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let Ok(original_tick) = parse_arrangement_scene_launch_tick(
            ui.get_move_arrangement_scene_launch_original_tick()
                .as_str(),
        ) else {
            ui.set_error_text("Scene-marker move request is no longer current".into());
            return;
        };
        let state = project_state_for_move.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement scene-marker authority is no longer available".into());
            return;
        };
        let Some(launch) = binding
            .owner
            .scene_launches()
            .iter()
            .find(|launch| launch.id() == launch_id.as_str())
        else {
            ui.set_error_text("The selected scene marker is no longer available".into());
            return;
        };
        if launch.tick() != original_tick {
            ui.set_error_text("The selected scene marker has changed since it was opened".into());
            return;
        }
        if launch.tick() == tick {
            ui.set_status_text("Scene marker already uses that project tick".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        if binding
            .owner
            .scene_launches()
            .iter()
            .any(|candidate| candidate.id() != launch.id() && candidate.tick() == tick)
        {
            ui.set_error_text("The arrangement already has a scene marker at that tick".into());
            return;
        }
        let launch_id = launch.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_move,
            IdeWorkerCommand::MoveArrangementSceneLaunch { launch_id, tick },
        );
    });
}

fn bind_arrangement_scene_launch_removal_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_removal = Rc::clone(worker);
    let project_state_for_removal = Rc::clone(project_state);
    actions.on_remove_arrangement_scene_launch(move |launch_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !arrangement_scene_launch_form_is_current(
            ui.get_app_mode(),
            ui.get_remove_arrangement_scene_launch_visible(),
            ui.get_remove_arrangement_scene_launch_id().as_str(),
            launch_id.as_str(),
        ) {
            ui.set_error_text("Scene-marker removal confirmation is no longer current".into());
            return;
        }
        let Ok(confirmed_tick) = parse_arrangement_scene_launch_tick(
            ui.get_remove_arrangement_scene_launch_tick().as_str(),
        ) else {
            ui.set_error_text("Scene-marker removal confirmation is no longer current".into());
            return;
        };
        let state = project_state_for_removal.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement scene-marker authority is no longer available".into());
            return;
        };
        let Some(launch) = binding
            .owner
            .scene_launches()
            .iter()
            .find(|launch| launch.id() == launch_id.as_str())
        else {
            ui.set_error_text("The selected scene marker is no longer available".into());
            return;
        };
        if launch.tick() != confirmed_tick {
            ui.set_error_text("The selected scene marker has changed since it was opened".into());
            return;
        }
        let launch_id = launch.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_removal,
            IdeWorkerCommand::RemoveArrangementSceneLaunch { launch_id },
        );
    });
}

fn arrangement_scene_launch_form_is_current(
    app_mode: AppMode,
    visible: bool,
    selected_launch_id: &str,
    requested_launch_id: &str,
) -> bool {
    app_mode == AppMode::Running
        && visible
        && !selected_launch_id.is_empty()
        && selected_launch_id == requested_launch_id
}

fn bind_arrangement_duplication_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let project_state = Rc::clone(project_state);
    let worker = Rc::clone(worker);
    bind_arrangement_duplication_callback_with(
        ui,
        move |placement_id| {
            let state = project_state.arrangement.borrow();
            let binding = state
                .as_ref()
                .ok_or("Arrangement placement authority is no longer available")?;
            binding
                .owner
                .placements()
                .iter()
                .find(|placement| placement.id() == placement_id)
                .map(|placement| placement.id().to_owned())
                .ok_or("The selected arrangement placement is no longer available")
        },
        move |ui, command| submit_to_ui(ui, &worker, command),
    );
}

fn bind_arrangement_duplication_callback_with(
    ui: &MainWindow,
    resolve_placement: impl Fn(&str) -> Result<String, &'static str> + 'static,
    submit: impl Fn(&MainWindow, IdeWorkerCommand) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_duplicate_arrangement_placement(move |placement_id, start_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement copying is not available in the current state".into(),
            );
            return;
        }
        let start_tick = match parse_arrangement_start_tick(start_tick.as_str()) {
            Ok(start_tick) => start_tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let owned_placement_id = match resolve_placement(placement_id.as_str()) {
            Ok(placement_id) => placement_id,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        submit(
            &ui,
            IdeWorkerCommand::DuplicateArrangementPlacement {
                placement_id: owned_placement_id,
                start_tick,
            },
        );
    });
}

fn bind_arrangement_split_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_split = Rc::clone(worker);
    let project_state_for_split = Rc::clone(project_state);
    actions.on_split_arrangement_placement(move |placement_id, split_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement splitting is not available in the current state".into(),
            );
            return;
        }
        let split_tick = match parse_arrangement_start_tick(split_tick.as_str()) {
            Ok(split_tick) => split_tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let state = project_state_for_split.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        let Some(end_tick) = placement
            .start_tick()
            .checked_add(placement.duration_ticks())
        else {
            ui.set_error_text("The selected placement end is outside the exact tick domain".into());
            return;
        };
        if split_tick <= placement.start_tick() || split_tick >= end_tick {
            ui.set_error_text(
                format!(
                    "Split tick must be strictly inside the placement ({} through {})",
                    placement.start_tick() + 1,
                    end_tick - 1
                )
                .into(),
            );
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_split,
            IdeWorkerCommand::SplitArrangementPlacement {
                placement_id: owned_placement_id,
                split_tick,
            },
        );
    });
}

fn bind_arrangement_removal_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_removal = Rc::clone(worker);
    let project_state_for_removal = Rc::clone(project_state);
    actions.on_remove_arrangement_placement(move |placement_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !arrangement_removal_confirmation_is_current(
            ui.get_app_mode(),
            ui.get_remove_arrangement_placement_visible(),
            ui.get_remove_arrangement_placement_id().as_str(),
            placement_id.as_str(),
        ) {
            ui.set_error_text(
                "Arrangement placement removal confirmation is no longer current".into(),
            );
            return;
        }
        let state = project_state_for_removal.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_removal,
            IdeWorkerCommand::RemoveArrangementPlacement {
                placement_id: owned_placement_id,
            },
        );
    });
}

fn arrangement_removal_confirmation_is_current(
    app_mode: AppMode,
    visible: bool,
    confirmed_placement_id: &str,
    requested_placement_id: &str,
) -> bool {
    app_mode == AppMode::Running
        && visible
        && !confirmed_placement_id.is_empty()
        && confirmed_placement_id == requested_placement_id
}

fn bind_arrangement_move_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_move = Rc::clone(worker);
    let project_state_for_move = Rc::clone(project_state);
    actions.on_move_arrangement_placement(move |placement_id, start_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Arrangement placement movement is not available in the current state".into(),
            );
            return;
        }
        let start_tick = match parse_arrangement_start_tick(start_tick.as_str()) {
            Ok(start_tick) => start_tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let state = project_state_for_move.arrangement.borrow();
        let Some(binding) = state.as_ref() else {
            ui.set_error_text("Arrangement placement authority is no longer available".into());
            return;
        };
        let Some(placement) = binding
            .owner
            .placements()
            .iter()
            .find(|placement| placement.id() == placement_id.as_str())
        else {
            ui.set_error_text("The selected arrangement placement is no longer available".into());
            return;
        };
        if placement.start_tick() == start_tick {
            ui.set_status_text("Arrangement placement already starts at that tick".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_move,
            IdeWorkerCommand::MoveArrangementPlacement {
                placement_id: owned_placement_id,
                start_tick,
            },
        );
    });
}

fn bind_audio_clip_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    bind_audio_clip_rename_callback(ui, worker, project_state);
    bind_audio_clip_duplication_callback(ui, worker, project_state);
    bind_audio_clip_removal_callback(ui, worker, project_state);
    bind_unused_sample_cleanup_callback(ui, worker, project_state);
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_activation = Rc::clone(worker);
    let project_state_for_activation = Rc::clone(project_state);
    actions.on_set_audio_clip_active(move |clip_id, active| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Audio-clip activation is not available in the current state".into());
            return;
        }
        let clips = project_state_for_activation.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        if active == clip.active() {
            ui.set_error_text("Audio-clip activation is no longer current".into());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker_for_activation,
            IdeWorkerCommand::SetAudioClipActive {
                clip_id: owned_clip_id,
                active,
            },
        );
    });

    let weak = ui.as_weak();
    let worker_for_gain = Rc::clone(worker);
    let project_state_for_gain = Rc::clone(project_state);
    actions.on_set_audio_clip_gain(move |clip_id, value| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Audio-clip gain is not available in the current state".into());
            return;
        }
        let gain_decibels = match parse_audio_clip_gain(value.as_str()) {
            Ok(gain_decibels) => gain_decibels,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let clips = project_state_for_gain.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        if gain_decibels.to_bits() == clip.gain_decibels().to_bits() {
            ui.set_clip_gain_visible(false);
            ui.set_status_text("Audio-clip gain already matches the committed value".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker_for_gain,
            IdeWorkerCommand::SetAudioClipGain {
                clip_id: owned_clip_id,
                gain_decibels,
            },
        );
    });

    bind_audio_clip_fade_callback(ui, worker, project_state);
    bind_audio_clip_loop_callback(ui, worker, project_state);
    bind_audio_clip_reverse_callback(ui, worker, project_state);
    bind_audio_clip_source_slip_callback(ui, worker, project_state);
    bind_audio_clip_source_trim_callback(ui, worker, project_state);
    bind_audio_clip_waveform_callback(ui, worker, project_state);
    bind_audio_clip_preview_callback(ui, worker, project_state);

    bind_arrangement_placement_addition_callback(ui, worker, project_state);
}

fn bind_audio_clip_preview_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_toggle_audio_clip_preview(move |clip_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() == AppMode::Previewing && ui.get_preview_clip_id() == clip_id {
            submit_to_ui(&ui, &worker, IdeWorkerCommand::StopAudioPreview);
            return;
        }
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("Audio clip selection is no longer available".into());
            return;
        };
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::StartAudioClipPreview {
                clip_id: clip.id().to_owned(),
            },
        );
    });
}

fn bind_audio_clip_waveform_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_inspect_audio_clip_waveform(move |clip_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            reset_audio_clip_waveform_dialog(&ui);
            ui.set_error_text(
                "Audio-clip waveform inspection is not available in the current state".into(),
            );
            return;
        }
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            drop(clips);
            reset_audio_clip_waveform_dialog(&ui);
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::InspectAudioClipWaveform {
                clip_id: owned_clip_id,
            },
        );
    });
}

fn bind_audio_clip_reverse_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_set_audio_clip_reverse(move |clip_id, reverse| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Audio-clip direction editing is not available in the current state".into(),
            );
            return;
        }
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        if reverse == clip.reverse() {
            ui.set_error_text("Audio-clip direction is no longer current".into());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::SetAudioClipReverse {
                clip_id: owned_clip_id,
                reverse,
            },
        );
    });
}

fn bind_audio_clip_source_slip_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_set_audio_clip_source_start(move |clip_id, value| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Audio-clip source-slip editing is not available in the current state".into(),
            );
            return;
        }
        let source_start_frame = match parse_audio_clip_source_start(value.as_str()) {
            Ok(source_start_frame) => source_start_frame,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        if usize::try_from(source_start_frame).ok() == Some(clip.source_start_frame()) {
            reset_audio_clip_source_slip_form(&ui);
            ui.set_status_text(
                "Audio-clip source start already matches the committed value".into(),
            );
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::SetAudioClipSourceStart {
                clip_id: owned_clip_id,
                source_start_frame,
            },
        );
    });
}

fn bind_audio_clip_source_trim_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    bind_audio_clip_source_trim_callback_with(
        ui,
        move |clip_id| {
            project_state
                .audio_clips
                .borrow()
                .iter()
                .find(|clip| clip.id() == clip_id)
                .map(|clip| {
                    (
                        clip.id().to_owned(),
                        clip.source_start_frame(),
                        clip.source_frame_count(),
                    )
                })
        },
        move |ui, command| submit_to_ui(ui, &worker, command),
    );
}

fn bind_audio_clip_source_trim_callback_with(
    ui: &MainWindow,
    resolve_clip: impl Fn(&str) -> Option<(String, usize, usize)> + 'static,
    submit: impl Fn(&MainWindow, IdeWorkerCommand) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_trim_audio_clip_source(move |clip_id, start, frame_count| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Audio-clip source trimming is not available in the current state".into(),
            );
            return;
        }
        let source_start_frame = match parse_audio_clip_source_start(start.as_str()) {
            Ok(source_start_frame) => source_start_frame,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let source_frame_count = match parse_audio_clip_source_frame_count(frame_count.as_str()) {
            Ok(source_frame_count) => source_frame_count,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let Some((owned_clip_id, committed_start, committed_frame_count)) =
            resolve_clip(clip_id.as_str())
        else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        if source_frame_count > u64::try_from(committed_frame_count).unwrap_or(u64::MAX) {
            ui.set_error_text(
                "Source trim may not extend the committed reusable source view".into(),
            );
            return;
        }
        if usize::try_from(source_start_frame).ok() == Some(committed_start)
            && usize::try_from(source_frame_count).ok() == Some(committed_frame_count)
        {
            reset_audio_clip_source_trim_form(&ui);
            ui.set_status_text("Audio-clip source view already matches the committed value".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        submit(
            &ui,
            IdeWorkerCommand::TrimAudioClipSource {
                clip_id: owned_clip_id,
                source_start_frame,
                source_frame_count,
            },
        );
    });
}

fn bind_audio_clip_fade_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_set_audio_clip_fades(move |clip_id, fade_in, fade_out| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text("Audio-clip fades are not available in the current state".into());
            return;
        }
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let (fade_in_ticks, fade_out_ticks) = match parse_audio_clip_fades(
            fade_in.as_str(),
            fade_out.as_str(),
            clip.duration_ticks(),
        ) {
            Ok(fades) => fades,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        if fade_in_ticks == clip.fade_in_ticks() && fade_out_ticks == clip.fade_out_ticks() {
            ui.set_clip_fades_visible(false);
            ui.set_status_text("Audio-clip fades already match the committed values".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::SetAudioClipFades {
                clip_id: owned_clip_id,
                fade_in_ticks,
                fade_out_ticks,
            },
        );
    });
}

fn bind_audio_clip_loop_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let project_state = Rc::clone(project_state);
    actions.on_set_audio_clip_loop(move |clip_id, enabled, start, end| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            ui.set_error_text(
                "Audio-clip loop editing is not available in the current state".into(),
            );
            return;
        }
        let clips = project_state.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let (start_tick, end_tick) =
            match parse_audio_clip_loop(start.as_str(), end.as_str(), clip.duration_ticks()) {
                Ok(loop_range) => loop_range,
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
        if enabled == clip.loop_enabled()
            && start_tick == clip.loop_start_tick()
            && end_tick == clip.loop_end_tick()
        {
            ui.set_clip_loop_visible(false);
            ui.set_status_text("Audio-clip loop already matches the committed values".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker,
            IdeWorkerCommand::SetAudioClipLoop {
                clip_id: owned_clip_id,
                enabled,
                start_tick,
                end_tick,
            },
        );
    });
}

fn bind_audio_clip_rename_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_rename = Rc::clone(worker);
    let project_state_for_rename = Rc::clone(project_state);
    actions.on_rename_audio_clip(move |clip_id, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_clip_rename_visible()
            || ui.get_clip_rename_id() != clip_id
        {
            ui.set_error_text("Audio-clip rename selection is no longer current".into());
            return;
        }
        let clips = project_state_for_rename.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let command = match audio_clip_rename_command(clip.id(), clip.name(), name.as_str()) {
            Ok(Some(command)) => command,
            Ok(None) => {
                ui.set_clip_rename_visible(false);
                ui.set_status_text("Audio-clip name already matches the committed value".into());
                ui.set_error_text(SharedString::default());
                return;
            }
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(clips);
        submit_to_ui(&ui, &worker_for_rename, command);
    });
}

fn bind_audio_clip_duplication_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_duplication = Rc::clone(worker);
    let project_state_for_duplication = Rc::clone(project_state);
    actions.on_duplicate_audio_clip(move |clip_id, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_clip_duplicate_visible()
            || ui.get_clip_duplicate_id() != clip_id
        {
            ui.set_error_text("Audio-clip duplication selection is no longer current".into());
            return;
        }
        let clips = project_state_for_duplication.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let command = match audio_clip_duplication_command(clip.id(), name.as_str()) {
            Ok(command) => command,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(clips);
        submit_to_ui(&ui, &worker_for_duplication, command);
    });
}

fn bind_audio_clip_removal_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let project_state = Rc::clone(project_state);
    let worker = Rc::clone(worker);
    bind_audio_clip_removal_callback_with(
        ui,
        move |clip_id| {
            project_state
                .audio_clips
                .borrow()
                .iter()
                .find(|clip| clip.id() == clip_id)
                .map(|clip| clip.id().to_owned())
        },
        move |ui, command| submit_to_ui(ui, &worker, command),
    );
}

fn bind_audio_clip_removal_callback_with(
    ui: &MainWindow,
    resolve_clip: impl Fn(&str) -> Option<String> + 'static,
    submit: impl Fn(&MainWindow, IdeWorkerCommand) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_remove_audio_clip(move |clip_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_clip_remove_visible()
            || ui.get_clip_remove_id() != clip_id
        {
            ui.set_error_text("Audio-clip removal confirmation is no longer current".into());
            return;
        }
        let Some(owned_clip_id) = resolve_clip(clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        submit(
            &ui,
            IdeWorkerCommand::RemoveAudioClip {
                clip_id: owned_clip_id,
            },
        );
    });
}

fn bind_unused_sample_cleanup_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let state = Rc::clone(project_state);
    let worker = Rc::clone(worker);
    bind_unused_sample_cleanup_callback_with(
        ui,
        move || state.unused_sample_cleanup.get(),
        move |ui, command| submit_to_ui(ui, &worker, command),
    );
}

fn bind_unused_sample_cleanup_callback_with(
    ui: &MainWindow,
    resolve: impl Fn() -> Option<IdeUnusedSampleCleanupSummary> + 'static,
    submit: impl Fn(&MainWindow, IdeWorkerCommand) + 'static,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    actions.on_clean_unused_samples(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_unused_sample_cleanup_confirming() {
            ui.set_error_text("Unused-media cleanup confirmation is no longer current".into());
            return;
        }
        let Some(confirmation) = resolve() else {
            ui.set_error_text("Unused-media cleanup authority is unavailable".into());
            return;
        };
        if confirmation.is_empty() {
            ui.set_error_text("The project has no unused sample media".into());
            return;
        }
        submit(&ui, IdeWorkerCommand::CleanUnusedSamples { confirmation });
    });
}

fn bind_arrangement_placement_addition_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_placement = Rc::clone(worker);
    let project_state_for_placement = Rc::clone(project_state);
    actions.on_add_arrangement_placement(move |clip_id, start_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || !ui.get_add_arrangement_placement_visible()
            || ui.get_add_arrangement_placement_clip_id() != clip_id
        {
            ui.set_error_text("Audio-clip placement selection is no longer current".into());
            return;
        }
        let start_tick = match parse_arrangement_start_tick(start_tick.as_str()) {
            Ok(start_tick) => start_tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let clips = project_state_for_placement.audio_clips.borrow();
        let Some(clip) = clips.iter().find(|clip| clip.id() == clip_id.as_str()) else {
            ui.set_error_text("The selected audio clip is no longer available".into());
            return;
        };
        let owned_clip_id = clip.id().to_owned();
        drop(clips);
        submit_to_ui(
            &ui,
            &worker_for_placement,
            IdeWorkerCommand::AddArrangementPlacement {
                clip_id: owned_clip_id,
                start_tick,
            },
        );
    });
}

fn normalize_audio_clip_name(value: &str) -> Result<&str, &'static str> {
    let name = value.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err("Audio-clip name must contain 1 to 120 characters");
    }
    Ok(name)
}

fn audio_clip_rename_command(
    clip_id: &str,
    current_name: &str,
    proposed_name: &str,
) -> Result<Option<IdeWorkerCommand>, &'static str> {
    let name = normalize_audio_clip_name(proposed_name)?;
    if name == current_name {
        return Ok(None);
    }
    Ok(Some(IdeWorkerCommand::RenameAudioClip {
        clip_id: clip_id.to_owned(),
        name: name.to_owned(),
    }))
}

fn audio_clip_duplication_command(
    source_clip_id: &str,
    proposed_name: &str,
) -> Result<IdeWorkerCommand, &'static str> {
    let name = normalize_audio_clip_name(proposed_name)?;
    Ok(IdeWorkerCommand::DuplicateAudioClip {
        clip_id: source_clip_id.to_owned(),
        name: name.to_owned(),
    })
}

fn parse_audio_clip_gain(value: &str) -> Result<f64, &'static str> {
    let gain_decibels = value
        .trim()
        .parse::<f64>()
        .map_err(|_| "Clip gain must be a decimal value in dB")?;
    if !ide_audio_clip_gain_is_supported(gain_decibels) {
        return Err("Clip gain must be finite and at or below +6.0206 dB");
    }
    Ok(gain_decibels)
}

fn parse_arrangement_placement_gain(value: &str) -> Result<f64, &'static str> {
    let gain_decibels = value
        .trim()
        .parse::<f64>()
        .map_err(|_| "Arrangement placement gain must be a decimal value in dB")?;
    if !gain_decibels.is_finite() {
        return Err("Arrangement placement gain must be finite");
    }
    Ok(gain_decibels)
}

fn parse_arrangement_placement_fades(
    fade_in: &str,
    fade_out: &str,
    duration_ticks: u64,
) -> Result<(u64, u64), &'static str> {
    let fade_in_ticks = parse_exact_arrangement_tick(
        fade_in,
        "Placement fade-in duration must be a non-negative whole number of ticks",
        "Placement fade-in duration is outside the supported range",
        "Placement fade-in duration exceeds the exact project range",
    )?;
    let fade_out_ticks = parse_exact_arrangement_tick(
        fade_out,
        "Placement fade-out duration must be a non-negative whole number of ticks",
        "Placement fade-out duration is outside the supported range",
        "Placement fade-out duration exceeds the exact project range",
    )?;
    if fade_out_ticks > duration_ticks || fade_in_ticks > duration_ticks - fade_out_ticks {
        return Err("Combined placement fades may not exceed the placement duration");
    }
    Ok((fade_in_ticks, fade_out_ticks))
}

fn parse_audio_clip_fades(
    fade_in: &str,
    fade_out: &str,
    duration_ticks: u64,
) -> Result<(u64, u64), &'static str> {
    let fade_in_ticks = parse_exact_arrangement_tick(
        fade_in,
        "Fade-in duration must be a non-negative whole number of ticks",
        "Fade-in duration is outside the supported range",
        "Fade-in duration exceeds the exact project range",
    )?;
    let fade_out_ticks = parse_exact_arrangement_tick(
        fade_out,
        "Fade-out duration must be a non-negative whole number of ticks",
        "Fade-out duration is outside the supported range",
        "Fade-out duration exceeds the exact project range",
    )?;
    if fade_out_ticks > duration_ticks || fade_in_ticks > duration_ticks - fade_out_ticks {
        return Err("Combined fades may not exceed the audio-clip duration");
    }
    Ok((fade_in_ticks, fade_out_ticks))
}

fn parse_audio_clip_loop(
    start: &str,
    end: &str,
    duration_ticks: u64,
) -> Result<(u64, u64), &'static str> {
    let start_tick = parse_exact_arrangement_tick(
        start,
        "Loop start must be a non-negative whole number of ticks",
        "Loop start is outside the supported range",
        "Loop start exceeds the exact project range",
    )?;
    let end_tick = parse_exact_arrangement_tick(
        end,
        "Loop end must be a positive whole number of ticks",
        "Loop end is outside the supported range",
        "Loop end exceeds the exact project range",
    )?;
    if start_tick >= end_tick {
        return Err("Loop start must be before loop end");
    }
    if end_tick > duration_ticks {
        return Err("Loop end may not exceed the audio-clip duration");
    }
    Ok((start_tick, end_tick))
}

fn parse_audio_clip_source_start(value: &str) -> Result<u64, &'static str> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Source start must be a non-negative whole frame number");
    }
    let frame = value
        .parse::<u64>()
        .map_err(|_| "Source start is outside the supported range")?;
    if frame > MAX_SAFE_TIMELINE_TICK {
        return Err("Source start exceeds the exact project range");
    }
    Ok(frame)
}

fn parse_audio_clip_source_frame_count(value: &str) -> Result<u64, &'static str> {
    let frame_count = parse_audio_clip_source_start(value).map_err(|message| match message {
        "Source start must be a non-negative whole frame number" => {
            "Source frame count must be a positive whole frame number"
        }
        "Source start is outside the supported range" => {
            "Source frame count is outside the supported range"
        }
        "Source start exceeds the exact project range" => {
            "Source frame count exceeds the exact project range"
        }
        _ => "Source frame count is invalid",
    })?;
    if frame_count == 0 {
        return Err("Source frame count must be a positive whole frame number");
    }
    Ok(frame_count)
}

fn parse_arrangement_start_tick(value: &str) -> Result<u64, &'static str> {
    parse_exact_arrangement_tick(
        value,
        "Arrangement start tick must be a non-negative whole number",
        "Arrangement start tick is outside the supported range",
        "Arrangement start tick exceeds the exact project range",
    )
}

fn parse_arrangement_scene_launch_tick(value: &str) -> Result<u64, &'static str> {
    parse_exact_arrangement_tick(
        value,
        "Scene-marker tick must be a non-negative whole number",
        "Scene-marker tick is outside the supported range",
        "Scene-marker tick exceeds the exact project range",
    )
}

fn parse_arrangement_duration_ticks(value: &str) -> Result<u64, &'static str> {
    let duration_ticks = parse_exact_arrangement_tick(
        value,
        "Arrangement duration must be a positive whole number of ticks",
        "Arrangement duration is outside the supported range",
        "Arrangement duration exceeds the exact project range",
    )?;
    if duration_ticks == 0 {
        return Err("Arrangement duration must be greater than zero");
    }
    Ok(duration_ticks)
}

fn parse_exact_arrangement_tick(
    value: &str,
    invalid_message: &'static str,
    overflow_message: &'static str,
    exact_range_message: &'static str,
) -> Result<u64, &'static str> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_message);
    }
    let tick = value.parse::<u64>().map_err(|_| overflow_message)?;
    if tick > MAX_SAFE_TIMELINE_TICK {
        return Err(exact_range_message);
    }
    Ok(tick)
}

fn start_event_pump(
    ui: &MainWindow,
    worker: &WorkerCell,
    routes: &UiRoutes,
    project_state: &ProjectUiStateCell,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let routes = routes.clone();
    let project_state = Rc::clone(project_state);
    let mut observation_pending = false;
    timer.start(TimerMode::Repeated, UI_POLL_INTERVAL, move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let mut worker_ref = worker.borrow_mut();
        let Some(worker) = worker_ref.as_mut() else {
            return;
        };
        loop {
            match worker.try_recv() {
                Ok(Some(event)) => {
                    if completes_observation(&event) {
                        observation_pending = false;
                    }
                    apply_event(&ui, &routes, &project_state, event);
                }
                Ok(None) => break,
                Err(error) => {
                    ui.set_app_mode(AppMode::Unavailable);
                    ui.set_error_text(error.to_string().into());
                    return;
                }
            }
        }
        if !matches!(
            ui.get_app_mode(),
            AppMode::Running
                | AppMode::Previewing
                | AppMode::Recording
                | AppMode::CapturingScenePerformance
                | AppMode::StoppingScenePerformance
                | AppMode::CapturingMixerAutomation
                | AppMode::StoppingMixerAutomation
                | AppMode::OutputTesting
        ) {
            observation_pending = false;
        } else if !observation_pending {
            match worker.try_send(IdeWorkerCommand::Observe) {
                Ok(()) => observation_pending = true,
                Err(IdeWorkerSendError::Full(_)) => {}
                Err(error) => {
                    ui.set_app_mode(AppMode::Unavailable);
                    ui.set_error_text(error.to_string().into());
                }
            }
        }
    });
    timer
}

fn completes_observation(event: &IdeWorkerEvent) -> bool {
    matches!(
        event,
        IdeWorkerEvent::Running(_)
            | IdeWorkerEvent::Recording(_)
            | IdeWorkerEvent::ScenePerformanceCaptureProgress { .. }
            | IdeWorkerEvent::ScenePerformanceCaptureStopping { .. }
            | IdeWorkerEvent::MixerAutomationCaptureProgress { .. }
            | IdeWorkerEvent::MixerAutomationCaptureStopping { .. }
            | IdeWorkerEvent::OutputTest(_)
            | IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::Observe,
                ..
            }
    )
}

fn submit(weak: &slint::Weak<MainWindow>, worker: &WorkerCell, command: IdeWorkerCommand) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    submit_to_ui(&ui, worker, command);
}

fn submit_path(ui: &MainWindow, worker: &WorkerCell, path: PathBuf) {
    submit_to_ui(ui, worker, IdeWorkerCommand::OpenProject(path));
}

fn submit_to_ui(ui: &MainWindow, worker: &WorkerCell, command: IdeWorkerCommand) {
    let _ = try_submit_to_ui(ui, worker, command);
}

fn try_submit_to_ui(ui: &MainWindow, worker: &WorkerCell, command: IdeWorkerCommand) -> bool {
    let command_kind = command.kind();
    let worker_ref = worker.borrow();
    let Some(worker) = worker_ref.as_ref() else {
        ui.set_app_mode(mode_after_submission_failure(
            ui.get_app_mode(),
            command_kind,
            UiSubmissionFailure::Disconnected,
        ));
        ui.set_error_text("IDE worker is no longer available".into());
        return false;
    };
    match worker.try_send(command) {
        Ok(()) => {
            ui.set_app_mode(mode_after_successful_submission(
                ui.get_app_mode(),
                command_kind,
            ));
            if command_kind == IdeWorkerCommandKind::AddArrangementPlacement {
                ui.set_add_arrangement_placement_visible(false);
            }
            if command_kind == IdeWorkerCommandKind::AddArrangementSceneLaunch {
                ui.set_add_arrangement_scene_launch_visible(false);
            }
            ui.set_error_text(SharedString::default());
            ui.set_status_text(command_status(command_kind).into());
            true
        }
        Err(error) => {
            let failure = match error {
                IdeWorkerSendError::Full(_) => UiSubmissionFailure::Full,
                IdeWorkerSendError::Disconnected(_) => UiSubmissionFailure::Disconnected,
            };
            ui.set_app_mode(mode_after_submission_failure(
                ui.get_app_mode(),
                command_kind,
                failure,
            ));
            ui.set_error_text(error.to_string().into());
            false
        }
    }
}

fn mode_after_successful_submission(current: AppMode, command: IdeWorkerCommandKind) -> AppMode {
    match command {
        IdeWorkerCommandKind::StartOutputTest => AppMode::StartingOutputTest,
        IdeWorkerCommandKind::StopOutputTest => AppMode::StoppingOutputTest,
        IdeWorkerCommandKind::CreateProject => AppMode::Creating,
        IdeWorkerCommandKind::OpenProject => AppMode::Opening,
        IdeWorkerCommandKind::SaveProject
        | IdeWorkerCommandKind::SaveProjectAs
        | IdeWorkerCommandKind::RenameProject
        | IdeWorkerCommandKind::ReorderMixerStrips
        | IdeWorkerCommandKind::AddReturnSend
        | IdeWorkerCommandKind::RemoveReturnSend
        | IdeWorkerCommandKind::SetAudioPrimaryRoute
        | IdeWorkerCommandKind::RemoveRoutingBus
        | IdeWorkerCommandKind::AddScene
        | IdeWorkerCommandKind::SetSceneSlot
        | IdeWorkerCommandKind::SetArrangementLoopRange
        | IdeWorkerCommandKind::SetTempo
        | IdeWorkerCommandKind::SetMeter
        | IdeWorkerCommandKind::EditTimelineMap
        | IdeWorkerCommandKind::EditTimelineMarker => AppMode::Saving,
        IdeWorkerCommandKind::AddAudioTrack => AppMode::AddingTrack,
        IdeWorkerCommandKind::AddRoutingBus => AppMode::AddingRoutingBus,
        IdeWorkerCommandKind::DuplicateAudioTrack => AppMode::DuplicatingTrack,
        IdeWorkerCommandKind::RemoveAudioTrack | IdeWorkerCommandKind::RemoveAudioTrackContent => {
            AppMode::RemovingTrack
        }
        IdeWorkerCommandKind::FreezeAudioTrack => AppMode::FreezingTrack,
        IdeWorkerCommandKind::UnfreezeAudioTrack => AppMode::UnfreezingTrack,
        IdeWorkerCommandKind::FlattenAudioTrack => AppMode::FlatteningTrack,
        IdeWorkerCommandKind::RenameAudioTrack | IdeWorkerCommandKind::RenameRoutingBus => {
            AppMode::RenamingTrack
        }
        IdeWorkerCommandKind::SetAudioTrackActive => AppMode::ChangingTrackActivation,
        IdeWorkerCommandKind::ImportAudio => AppMode::ImportingAudio,
        IdeWorkerCommandKind::SetArrangementPlacementActive
        | IdeWorkerCommandKind::MoveArrangementPlacement
        | IdeWorkerCommandKind::SetArrangementPlacementDuration
        | IdeWorkerCommandKind::SetArrangementPlacementGain
        | IdeWorkerCommandKind::SetArrangementPlacementFades
        | IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade
        | IdeWorkerCommandKind::TrimArrangementPlacementFront
        | IdeWorkerCommandKind::RemoveArrangementPlacement
        | IdeWorkerCommandKind::AddArrangementPlacement
        | IdeWorkerCommandKind::DuplicateArrangementPlacement
        | IdeWorkerCommandKind::SplitArrangementPlacement => {
            arrangement_placement_submission_mode(command)
        }
        IdeWorkerCommandKind::AddArrangementSceneLaunch => AppMode::AddingArrangementSceneLaunch,
        IdeWorkerCommandKind::AddAutomationTake => AppMode::AddingAutomationTake,
        IdeWorkerCommandKind::SetAutomationTakeActive => AppMode::ChangingAutomationTakeActivation,
        IdeWorkerCommandKind::EditAutomationTake => AppMode::EditingAutomationTake,
        IdeWorkerCommandKind::RemoveAutomationTake => AppMode::RemovingAutomationTake,
        IdeWorkerCommandKind::EditControlMapping => AppMode::EditingControlMapping,
        IdeWorkerCommandKind::EditNativeFilter => AppMode::EditingNativeFilter,
        IdeWorkerCommandKind::ExportProject | IdeWorkerCommandKind::ExportTrackStems => {
            AppMode::ExportingProject
        }
        IdeWorkerCommandKind::PrepareRuntime => AppMode::PreparingRuntime,
        IdeWorkerCommandKind::Undo | IdeWorkerCommandKind::Redo => AppMode::RestoringEdit,
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => AppMode::MovingArrangementSceneLaunch,
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene => {
            AppMode::ChangingArrangementSceneLaunchScene
        }
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch => {
            AppMode::RemovingArrangementSceneLaunch
        }
        IdeWorkerCommandKind::RenameAudioClip => AppMode::RenamingClip,
        IdeWorkerCommandKind::DuplicateAudioClip => AppMode::DuplicatingClip,
        IdeWorkerCommandKind::RemoveAudioClip => AppMode::RemovingClip,
        IdeWorkerCommandKind::CleanUnusedSamples => AppMode::CleaningSamples,
        IdeWorkerCommandKind::SetAudioClipActive => AppMode::ChangingClipActivation,
        IdeWorkerCommandKind::SetAudioClipGain => AppMode::ChangingClipGain,
        IdeWorkerCommandKind::SetAudioClipFades => AppMode::ChangingClipFades,
        IdeWorkerCommandKind::SetAudioClipLoop => AppMode::ChangingClipLoop,
        IdeWorkerCommandKind::SetAudioClipReverse => AppMode::ChangingClipReverse,
        IdeWorkerCommandKind::SetAudioClipSourceStart => AppMode::ChangingClipSource,
        IdeWorkerCommandKind::TrimAudioClipSource => AppMode::TrimmingClipSource,
        IdeWorkerCommandKind::RenameScene => AppMode::RenamingScene,
        IdeWorkerCommandKind::RemoveScene => AppMode::RemovingScene,
        IdeWorkerCommandKind::DuplicateScene => AppMode::DuplicatingScene,
        IdeWorkerCommandKind::SetSceneActive => AppMode::ChangingSceneActivation,
        IdeWorkerCommandKind::CreateAudioClipFromTake
        | IdeWorkerCommandKind::CreateAndPlaceAudioClipFromTake => AppMode::CreatingClip,
        IdeWorkerCommandKind::StartRecording => AppMode::PreparingRecording,
        IdeWorkerCommandKind::StopRecording | IdeWorkerCommandKind::AbortRecording => {
            AppMode::FinalizingRecording
        }
        IdeWorkerCommandKind::CaptureScenePerformance => {
            scene_performance_capture_submission_mode(current)
        }
        IdeWorkerCommandKind::CaptureMixerAutomation => {
            mixer_automation_capture_submission_mode(current)
        }
        IdeWorkerCommandKind::StartAudioPreview | IdeWorkerCommandKind::StartAudioClipPreview => {
            AppMode::PreparingPreview
        }
        _ => current,
    }
}

fn arrangement_placement_submission_mode(command: IdeWorkerCommandKind) -> AppMode {
    match command {
        IdeWorkerCommandKind::SetArrangementPlacementActive => {
            AppMode::ChangingArrangementActivation
        }
        IdeWorkerCommandKind::MoveArrangementPlacement => AppMode::MovingArrangementPlacement,
        IdeWorkerCommandKind::SetArrangementPlacementDuration
        | IdeWorkerCommandKind::TrimArrangementPlacementFront => {
            AppMode::TrimmingArrangementPlacement
        }
        IdeWorkerCommandKind::SetArrangementPlacementGain => {
            AppMode::ChangingArrangementPlacementGain
        }
        IdeWorkerCommandKind::SetArrangementPlacementFades => {
            AppMode::ChangingArrangementPlacementFades
        }
        IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade => {
            AppMode::ChangingArrangementPlacementCrossfade
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement => AppMode::RemovingArrangementPlacement,
        IdeWorkerCommandKind::AddArrangementPlacement => AppMode::AddingArrangementPlacement,
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            AppMode::DuplicatingArrangementPlacement
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => AppMode::SplittingArrangementPlacement,
        _ => unreachable!("only arrangement placement commands reach this mode mapper"),
    }
}

fn scene_performance_capture_submission_mode(current: AppMode) -> AppMode {
    if matches!(
        current,
        AppMode::CapturingScenePerformance | AppMode::StoppingScenePerformance
    ) {
        AppMode::StoppingScenePerformance
    } else {
        AppMode::CapturingScenePerformance
    }
}

fn mixer_automation_capture_submission_mode(current: AppMode) -> AppMode {
    if matches!(
        current,
        AppMode::CapturingMixerAutomation | AppMode::StoppingMixerAutomation
    ) {
        AppMode::StoppingMixerAutomation
    } else {
        AppMode::CapturingMixerAutomation
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive status mapping keeps every worker command user-visible"
)]
fn command_status(command: IdeWorkerCommandKind) -> &'static str {
    match command {
        IdeWorkerCommandKind::Initialize => "Discovering desktop audio outputs…",
        IdeWorkerCommandKind::RetryDiscovery => "Retrying desktop audio discovery…",
        IdeWorkerCommandKind::RefreshOutputs => "Refreshing desktop audio outputs…",
        IdeWorkerCommandKind::RefreshInputs => "Refreshing desktop audio inputs…",
        IdeWorkerCommandKind::SelectSystemDefault | IdeWorkerCommandKind::SelectOutput => {
            "Applying audio output selection…"
        }
        IdeWorkerCommandKind::DisableInputMonitoring
        | IdeWorkerCommandKind::SelectSystemDefaultInput
        | IdeWorkerCommandKind::SelectInput => "Applying input cue selection…",
        IdeWorkerCommandKind::StartOutputTest => "Starting exclusive output diagnostic…",
        IdeWorkerCommandKind::StopOutputTest => "Stopping output diagnostic…",
        IdeWorkerCommandKind::CreateProject => "Creating, verifying, and opening the new project…",
        IdeWorkerCommandKind::OpenProject => "Recovering and verifying project…",
        IdeWorkerCommandKind::Transport => "Transport command queued…",
        IdeWorkerCommandKind::Seek => "Bounded transport locate queued…",
        IdeWorkerCommandKind::LocatePlacement => "Arrangement placement locate queued…",
        IdeWorkerCommandKind::LocateSceneLaunch => "Scene-marker locate queued…",
        IdeWorkerCommandKind::LocateTimelineMarker => "Navigation-marker locate queued…",
        IdeWorkerCommandKind::Loudness => "Loudness measurement command queued…",
        IdeWorkerCommandKind::ResetOutputPeakHold => "Clearing final-output peak hold…",
        IdeWorkerCommandKind::Mixer => "Mixer command queued…",
        IdeWorkerCommandKind::Monitoring => "Applying input cue control…",
        IdeWorkerCommandKind::SaveProject => "Saving mixer state and reopening project…",
        IdeWorkerCommandKind::SaveProjectAs => "Saving and verifying an independent project copy…",
        IdeWorkerCommandKind::RenameProject => {
            "Renaming the verified project package and reopening it…"
        }
        IdeWorkerCommandKind::PrepareRuntime => {
            "Saving the project and compiling a verified Runtime bundle…"
        }
        IdeWorkerCommandKind::AddArrangementSceneLaunch => {
            "Preserving mixer state, adding the scene marker, and reopening project..."
        }
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => {
            "Preserving mixer state, moving the scene marker, and reopening project..."
        }
        IdeWorkerCommandKind::SetArrangementLoopRange => {
            "Preserving mixer state, saving the loop range, and reopening project..."
        }
        IdeWorkerCommandKind::SetTempo => {
            "Preserving mixer state, saving the tempo, and reopening project..."
        }
        IdeWorkerCommandKind::SetMeter => {
            "Preserving mixer state, saving the time signature, and reopening project..."
        }
        IdeWorkerCommandKind::EditTimelineMap => {
            "Preserving mixer state, saving the timeline map, and reopening project..."
        }
        IdeWorkerCommandKind::EditTimelineMarker => {
            "Preserving mixer state, saving the navigation marker, and reopening project..."
        }
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene => {
            "Preserving mixer state, changing the marker scene, and reopening project..."
        }
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch => {
            "Preserving mixer state, removing the scene marker, and reopening project..."
        }
        IdeWorkerCommandKind::AddAudioTrack => {
            "Saving mixer state, adding the audio track, and reopening project…"
        }
        IdeWorkerCommandKind::AddRoutingBus => {
            "Saving mixer state, creating the routing bus, and reopening project…"
        }
        IdeWorkerCommandKind::RemoveRoutingBus => {
            "Saving mixer state, removing the routing bus, and reopening project…"
        }
        IdeWorkerCommandKind::AddReturnSend | IdeWorkerCommandKind::RemoveReturnSend => {
            "Preserving mixer state, editing the return send, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioPrimaryRoute => {
            "Preserving mixer state, changing the primary route, and reopening project…"
        }
        IdeWorkerCommandKind::DuplicateAudioTrack => {
            "Preserving mixer state, duplicating the mixer strip, and reopening project…"
        }
        IdeWorkerCommandKind::RemoveAudioTrack => {
            "Preserving mixer state, removing the empty audio track, and reopening project…"
        }
        IdeWorkerCommandKind::RemoveAudioTrackContent => {
            "Preserving mixer state, removing the audio track and its content, and reopening project…"
        }
        IdeWorkerCommandKind::FreezeAudioTrack => {
            "Saving mixer state, rendering the audio track, and reopening project…"
        }
        IdeWorkerCommandKind::UnfreezeAudioTrack => {
            "Saving mixer state, restoring the retained audio-track sources, and reopening project…"
        }
        IdeWorkerCommandKind::FlattenAudioTrack => {
            "Saving mixer state, accepting the rendered audio track, and reopening project…"
        }
        IdeWorkerCommandKind::ReorderMixerStrips => {
            "Preserving mixer state, reordering mixer strips, and reopening project…"
        }
        IdeWorkerCommandKind::RenameAudioTrack => {
            "Preserving mixer state, renaming the audio track, and reopening project…"
        }
        IdeWorkerCommandKind::RenameRoutingBus => {
            "Preserving mixer state, renaming the routing bus, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioTrackActive => {
            "Preserving mixer state, updating the audio track, and reopening project…"
        }
        IdeWorkerCommandKind::ImportAudio => {
            "Preserving mixer state, importing project-owned audio, and reopening project…"
        }
        IdeWorkerCommandKind::SetArrangementPlacementActive => {
            "Preserving mixer state, updating the arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::MoveArrangementPlacement => {
            "Preserving mixer state, moving the arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::SetArrangementPlacementDuration => {
            "Preserving mixer state, trimming the arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::SetArrangementPlacementGain => {
            "Preserving mixer state, updating arrangement placement gain, and reopening project…"
        }
        IdeWorkerCommandKind::SetArrangementPlacementFades => {
            "Preserving mixer state, updating arrangement placement fades, and reopening project…"
        }
        IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade => {
            "Preserving mixer state, applying the bounded arrangement crossfade, and reopening project…"
        }
        IdeWorkerCommandKind::TrimArrangementPlacementFront => {
            "Preserving mixer state, trimming the arrangement front, and reopening project…"
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement => {
            "Preserving mixer state, removing the arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::AddArrangementPlacement => {
            "Preserving mixer state, placing the full audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            "Preserving mixer state, copying the exact arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => {
            "Preserving mixer state, splitting the arrangement placement, and reopening project…"
        }
        IdeWorkerCommandKind::RenameAudioClip => {
            "Preserving mixer state, renaming the audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::DuplicateAudioClip => {
            "Preserving mixer state, duplicating the audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::RemoveAudioClip => {
            "Preserving mixer state, removing the unused audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::CleanUnusedSamples => {
            "Preserving mixer state, permanently removing unused sample copies, and reopening project…"
        }
        IdeWorkerCommandKind::DiscardRecordingTake => {
            "Preserving mixer state, permanently discarding the unpromoted recorded take, and reopening project…"
        }
        IdeWorkerCommandKind::RenameRecordingTake => {
            "Preserving mixer state, saving the recorded-take label, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioClipActive => {
            "Preserving mixer state, updating audio-clip activation, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioClipGain => {
            "Preserving mixer state, updating the audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioClipFades => {
            "Preserving mixer state, updating audio-clip fades, and reopening project..."
        }
        IdeWorkerCommandKind::SetAudioClipLoop => {
            "Preserving mixer state, updating the audio-clip loop, and reopening project..."
        }
        IdeWorkerCommandKind::SetAudioClipReverse => {
            "Preserving mixer state, updating audio-clip direction, and reopening project..."
        }
        IdeWorkerCommandKind::AddScene => {
            "Preserving mixer state, creating the scene, and reopening project..."
        }
        IdeWorkerCommandKind::RemoveScene => {
            "Preserving mixer state, removing the scene, and reopening project..."
        }
        IdeWorkerCommandKind::AddAutomationTake => {
            "Preserving mixer state, adding the automation take, and reopening project..."
        }
        IdeWorkerCommandKind::EditAutomationTake
        | IdeWorkerCommandKind::SetAutomationTakeActive => {
            "Preserving mixer state, editing the automation take, and reopening project..."
        }
        IdeWorkerCommandKind::RemoveAutomationTake => {
            "Preserving mixer state, removing the automation take, and reopening project..."
        }
        IdeWorkerCommandKind::EditControlMapping => {
            "Preserving mixer state, editing control mappings, and reopening project..."
        }
        IdeWorkerCommandKind::EditNativeFilter => {
            "Preserving mixer state, editing the native device chain, and reopening project..."
        }
        IdeWorkerCommandKind::ExportProject | IdeWorkerCommandKind::ExportTrackStems => {
            "Rendering the project through the playback runtime..."
        }
        IdeWorkerCommandKind::Undo => {
            "Preserving mixer state, restoring the previous edit, and reopening project..."
        }
        IdeWorkerCommandKind::Redo => {
            "Preserving mixer state, reapplying the undone edit, and reopening project..."
        }
        IdeWorkerCommandKind::DuplicateScene => {
            "Preserving mixer state, duplicating the scene, and reopening project..."
        }
        IdeWorkerCommandKind::SetSceneSlot => {
            "Preserving mixer state, updating the scene slot, and reopening project..."
        }
        IdeWorkerCommandKind::SetAudioClipSourceStart => {
            "Preserving mixer state, slipping the reusable audio-clip source, and reopening project..."
        }
        IdeWorkerCommandKind::TrimAudioClipSource => {
            "Preserving mixer state, trimming the reusable audio-clip source, and reopening project..."
        }
        IdeWorkerCommandKind::InspectAudioClipWaveform => {
            "Reading the current reusable audio-clip source view…"
        }
        IdeWorkerCommandKind::RenameScene => "Renaming the scene and reopening project…",
        IdeWorkerCommandKind::SetSceneActive => "Updating scene activation and reopening project…",
        IdeWorkerCommandKind::CreateAudioClipFromTake => {
            "Preserving mixer state, creating the reusable audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::CreateAndPlaceAudioClipFromTake => {
            "Preserving mixer state, creating and placing the recorded take, and reopening project…"
        }
        IdeWorkerCommandKind::StartRecording => {
            "Stopping playback and preparing an exclusive monitored capture…"
        }
        IdeWorkerCommandKind::StopRecording => "Verifying and importing the recorded take…",
        IdeWorkerCommandKind::AbortRecording => "Aborting capture and retaining recovery data…",
        IdeWorkerCommandKind::CaptureScenePerformance => {
            "Scene-performance capture command queued…"
        }
        IdeWorkerCommandKind::CaptureMixerAutomation => {
            "Live mixer automation capture command queued…"
        }
        IdeWorkerCommandKind::StartAudioPreview => {
            "Stopping transport and preparing dry take audition…"
        }
        IdeWorkerCommandKind::StartAudioClipPreview => {
            "Stopping transport and preparing dry clip audition…"
        }
        IdeWorkerCommandKind::StopAudioPreview => "Stopping audio audition…",
        IdeWorkerCommandKind::AudioPreviewControl => "Applying audio audition control…",
        IdeWorkerCommandKind::Observe => "Refreshing runtime evidence…",
        IdeWorkerCommandKind::CloseProject => "Closing project and native output…",
        IdeWorkerCommandKind::Shutdown => "Shutting down…",
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the event dispatcher keeps exhaustive UI state transitions visible in one match"
)]
fn apply_event(
    ui: &MainWindow,
    routes: &UiRoutes,
    project_state: &ProjectUiStateCell,
    event: IdeWorkerEvent,
) {
    match event {
        IdeWorkerEvent::RecentProjectsChanged { projects } => {
            apply_recent_projects(ui, &routes.recent_projects, &projects);
        }
        IdeWorkerEvent::ProjectHistoryWarning(error) => {
            ui.set_project_history_warning(format!("Recent projects unavailable: {error}").into());
        }
        IdeWorkerEvent::MixerRecoveryAvailable => {
            ui.set_app_mode(AppMode::Closed);
            ui.set_mixer_recovery_available(true);
            ui.set_status_text(
                "Recovery data is available; choose Restore or Discard before opening".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::Closed {
            snapshot,
            native_failure,
            input_failure,
        } => {
            apply_output_inventory(
                ui,
                &routes.outputs,
                snapshot.outputs(),
                snapshot.selection(),
            );
            apply_input_inventory(
                ui,
                &routes.inputs,
                snapshot.inputs(),
                snapshot.input_selection(),
            );
            routes.audio_imports.borrow_mut().clear();
            ui.set_audio_import_track_names(ModelRc::default());
            ui.set_selected_audio_import_track_index(-1);
            reset_project(ui, &routes.recordings, &routes.takes, project_state);
            ui.set_output_test_confirming(false);
            ui.set_mixer_recovery_available(false);
            let mode = mode_after_closed_event(ui.get_app_mode());
            ui.set_app_mode(mode);
            if mode == AppMode::Closed {
                ui.set_status_text("Ready to open a verified project".into());
                let error = native_failure.map(native_failure_message).or_else(|| {
                    input_failure.map(|failure| format!("Audio input discovery failed: {failure}"))
                });
                ui.set_error_text(error.unwrap_or_default().into());
            }
        }
        IdeWorkerEvent::Running(observation) => apply_running_event(
            ui,
            &routes.recordings,
            &routes.takes,
            &routes.audio_imports,
            project_state,
            &observation,
        ),
        IdeWorkerEvent::OutputPeakHoldReset(observation) => {
            apply_final_output_meter_observation(ui, &observation);
            ui.set_output_peak_hold_reset_pending(false);
            ui.set_status_text("Final-output sample-peak hold cleared".into());
            ui.set_error_text(SharedString::default());
        }
        event @ (IdeWorkerEvent::OutputTest(_)
        | IdeWorkerEvent::OutputTestStopping
        | IdeWorkerEvent::OutputTestStopped) => apply_output_test_event(ui, &event),
        IdeWorkerEvent::TransportQueued(command) => {
            ui.set_status_text(
                match command {
                    TransportCommand::Start => "Start queued for the audio runtime",
                    TransportCommand::Stop => "Stop queued for the audio runtime",
                    TransportCommand::Locate { .. } => "Locate queued for the audio runtime",
                    TransportCommand::LaunchScene { .. } => {
                        "Scene launch queued for the audio runtime"
                    }
                    TransportCommand::StopInteractiveScenes => {
                        "Interactive scenes stop queued for the audio runtime"
                    }
                    TransportCommand::SetLoopRange(Some(_)) => {
                        "Loop range queued for the audio runtime"
                    }
                    TransportCommand::SetLoopRange(None) => {
                        "Loop range cleared for the audio runtime"
                    }
                    TransportCommand::SetLoopEnabled(true) => "Loop queued for the audio runtime",
                    TransportCommand::SetLoopEnabled(false) => {
                        "Loop release queued for the audio runtime"
                    }
                    TransportCommand::SetMetronomeEnabled(true) => {
                        "Metronome enable queued for the audio runtime"
                    }
                    TransportCommand::SetMetronomeEnabled(false) => {
                        "Metronome disable queued for the audio runtime"
                    }
                    TransportCommand::SetMetronomeGain(_) => {
                        "Metronome level queued for the audio runtime"
                    }
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::LoudnessQueued(command) => apply_loudness_queued(ui, command),
        IdeWorkerEvent::MixerQueued(_) => {
            ui.set_status_text("Mixer base command queued for the audio runtime".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MixerRecoverySnapshotSaved => {
            ui.set_status_text("Live mixer recovery snapshot updated".into());
        }
        IdeWorkerEvent::MixerRecoverySnapshotWarning(reason) => {
            ui.set_error_text(format!("Live mixer recovery is unavailable: {reason}").into());
        }
        IdeWorkerEvent::CloseDiscardRequired => {
            apply_close_discard_required(ui);
        }
        IdeWorkerEvent::CloseDiscardCancelled => {
            apply_close_discard_cancelled(ui);
        }
        IdeWorkerEvent::MonitoringApplied(_) => {
            ui.set_status_text("Input cue control applied".into());
            ui.set_error_text(SharedString::default());
        }
        event @ IdeWorkerEvent::ProjectSaved { .. } => apply_project_saved_event(ui, &event),
        IdeWorkerEvent::ProjectCopied {
            project_path,
            opened,
        } => {
            ui.set_status_text(
                if opened {
                    format!(
                        "Independent project verified and opened at {}",
                        project_path.display()
                    )
                } else {
                    format!(
                        "Independent project verified at {}; source project remains open",
                        project_path.display()
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ProjectCopyCommittedWithWarning {
            project_path,
            opened,
            warning,
        } => {
            ui.set_status_text(
                if opened {
                    format!("Independent project opened at {} with a durability warning", project_path.display())
                } else {
                    format!("Independent project verified at {}; source project remains open with a durability warning", project_path.display())
                }
                .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::ProjectRenamed {
            revision,
            changed,
            mixer_changed,
            name,
        } => {
            ui.set_rename_project_visible(false);
            ui.set_status_text(
                if changed {
                    format!("Project renamed to {name} in project revision {revision}")
                } else if mixer_changed {
                    format!(
                        "Project already uses the name {name}; live mixer changes were saved in project revision {revision}"
                    )
                } else {
                    format!("Project already uses the name {name}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ProjectRenameCommittedWithWarning { name, warning } => {
            ui.set_rename_project_visible(false);
            ui.set_status_text(
                format!("Project was renamed to {name}, but recovery needs attention").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        event @ (IdeWorkerEvent::AudioTrackAdded { .. } | IdeWorkerEvent::AudioTrackAdding) => {
            apply_audio_track_event(ui, &event);
        }
        event @ (IdeWorkerEvent::RoutingBusAdded { .. }
        | IdeWorkerEvent::RoutingBusAdding { .. }
        | IdeWorkerEvent::RoutingBusRemoved { .. }) => {
            apply_routing_bus_event(ui, &event);
        }
        IdeWorkerEvent::ReturnSendEdited { revision } => {
            ui.set_status_text(format!("Return send edited in project revision {revision}").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioPrimaryRouteChanged { revision } => {
            ui.set_status_text(
                format!("Primary route changed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        event @ (IdeWorkerEvent::AudioTrackRemoved { .. } | IdeWorkerEvent::AudioTrackRemoving) => {
            apply_audio_track_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackContentRemoved { .. }
        | IdeWorkerEvent::AudioTrackContentRemoving
        | IdeWorkerEvent::AudioTrackContentRemovalCommittedWithWarning { .. }) => {
            apply_audio_track_content_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackFrozen { .. }
        | IdeWorkerEvent::AudioTrackFreezing
        | IdeWorkerEvent::AudioTrackFreezeCommittedWithWarning { .. }) => {
            apply_audio_track_freeze_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackUnfrozen { .. }
        | IdeWorkerEvent::AudioTrackUnfreezing
        | IdeWorkerEvent::AudioTrackUnfreezeCommittedWithWarning { .. }) => {
            apply_audio_track_unfreeze_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackFlattened { .. }
        | IdeWorkerEvent::AudioTrackFlattening
        | IdeWorkerEvent::AudioTrackFlattenCommittedWithWarning { .. }) => {
            apply_audio_track_flatten_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackDuplicated { .. }
        | IdeWorkerEvent::AudioTrackDuplicating) => {
            apply_audio_track_duplication_event(ui, &event);
        }
        event @ (IdeWorkerEvent::MixerStripsReordered { .. }
        | IdeWorkerEvent::MixerStripsReordering) => {
            apply_mixer_strip_reorder_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackRenamed { .. } | IdeWorkerEvent::AudioTrackRenaming) => {
            apply_audio_track_rename_event(ui, &event);
        }
        event @ (IdeWorkerEvent::RoutingBusRenamed { .. } | IdeWorkerEvent::RoutingBusRenaming) => {
            apply_routing_bus_rename_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackActivationChanged { .. }
        | IdeWorkerEvent::AudioTrackActivationChanging { .. }) => {
            apply_audio_track_activation_event(ui, &event);
        }
        IdeWorkerEvent::AudioImporting { files } => {
            ui.set_app_mode(AppMode::ImportingAudio);
            ui.set_status_text(format!("Importing {files} audio files…").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioImported(outcome) => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(
                format!(
                    "Imported {} audio files in project revision {}",
                    outcome.imported(),
                    outcome.final_revision()
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        event @ (IdeWorkerEvent::ArrangementPlacementActivationChanged { .. }
        | IdeWorkerEvent::ArrangementPlacementActivationChanging { .. }) => {
            apply_arrangement_placement_activation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementMoved { .. }
        | IdeWorkerEvent::ArrangementPlacementMoving { .. }) => {
            apply_arrangement_placement_move_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementDurationChanged { .. }
        | IdeWorkerEvent::ArrangementPlacementDurationChanging { .. }) => {
            apply_arrangement_placement_duration_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementGainChanged { .. }
        | IdeWorkerEvent::ArrangementPlacementGainChanging { .. }) => {
            apply_arrangement_placement_gain_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementFadesChanged { .. }
        | IdeWorkerEvent::ArrangementPlacementFadesChanging { .. }) => {
            apply_arrangement_placement_fades_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementCrossfadeApplied { .. }
        | IdeWorkerEvent::ArrangementPlacementCrossfadeApplying) => {
            apply_arrangement_placement_crossfade_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementFrontTrimmed { .. }
        | IdeWorkerEvent::ArrangementPlacementFrontTrimming { .. }) => {
            apply_arrangement_placement_front_trim_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementRemoved { .. }
        | IdeWorkerEvent::ArrangementPlacementRemoving) => {
            apply_arrangement_placement_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementAdded { .. }
        | IdeWorkerEvent::ArrangementPlacementAdding { .. }) => {
            apply_arrangement_placement_addition_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementSceneLaunchAdded { .. }
        | IdeWorkerEvent::ArrangementSceneLaunchAdding { .. }) => {
            apply_arrangement_scene_launch_addition_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementSceneLaunchMoved { .. }
        | IdeWorkerEvent::ArrangementSceneLaunchMoving { .. }) => {
            apply_arrangement_scene_launch_move_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementSceneLaunchSceneChanged { .. }
        | IdeWorkerEvent::ArrangementSceneLaunchSceneChanging) => {
            apply_arrangement_scene_launch_scene_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementSceneLaunchRemoved { .. }
        | IdeWorkerEvent::ArrangementSceneLaunchRemoving) => {
            apply_arrangement_scene_launch_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementDuplicated { .. }
        | IdeWorkerEvent::ArrangementPlacementDuplicating { .. }) => {
            apply_arrangement_placement_duplication_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ArrangementPlacementSplit { .. }
        | IdeWorkerEvent::ArrangementPlacementSplitting { .. }) => {
            apply_arrangement_placement_split_event(ui, &event);
        }
        event @ IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning { .. } => {
            apply_arrangement_placement_committed_warning(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipRenamed { .. } | IdeWorkerEvent::AudioClipRenaming) => {
            apply_audio_clip_rename_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipDuplicated { .. }
        | IdeWorkerEvent::AudioClipDuplicationCommittedWithWarning { .. }
        | IdeWorkerEvent::AudioClipDuplicating) => {
            apply_audio_clip_duplication_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipRemoved { .. }
        | IdeWorkerEvent::AudioClipRemovalCommittedWithWarning { .. }
        | IdeWorkerEvent::AudioClipRemoving) => {
            apply_audio_clip_removal_event(ui, &event);
        }
        IdeWorkerEvent::AudioClipWaveform(waveform) => {
            apply_audio_clip_waveform(ui, &waveform);
        }
        event @ (IdeWorkerEvent::UnusedSamplesCleaned { .. }
        | IdeWorkerEvent::UnusedSampleCleanupCommittedWithWarning { .. }
        | IdeWorkerEvent::UnusedSampleCleanupStarting { .. }) => {
            apply_unused_sample_cleanup_event(ui, &event);
        }
        event @ IdeWorkerEvent::AudioClipEditCommittedWithWarning { .. } => {
            apply_audio_clip_edit_committed_warning(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipActivationChanged { .. }
        | IdeWorkerEvent::AudioClipActivationChanging { .. }) => {
            apply_audio_clip_activation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipGainChanged { .. }
        | IdeWorkerEvent::AudioClipGainChanging { .. }) => {
            apply_audio_clip_gain_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipFadesChanged { .. }
        | IdeWorkerEvent::AudioClipFadesChanging { .. }) => {
            apply_audio_clip_fade_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipLoopChanged { .. }
        | IdeWorkerEvent::AudioClipLoopChanging { .. }) => {
            apply_audio_clip_loop_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipReverseChanged { .. }
        | IdeWorkerEvent::AudioClipReverseChanging { .. }) => {
            apply_audio_clip_reverse_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipSourceStartChanged { .. }
        | IdeWorkerEvent::AudioClipSourceStartChanging { .. }) => {
            apply_audio_clip_source_slip_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipSourceTrimmed { .. }
        | IdeWorkerEvent::AudioClipSourceTrimming { .. }) => {
            apply_audio_clip_source_trim_event(ui, &event);
        }
        event @ (IdeWorkerEvent::SceneAdded { .. }
        | IdeWorkerEvent::SceneCreationCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneAdding) => apply_scene_creation_event(ui, &event),
        event @ (IdeWorkerEvent::SceneRemoved { .. }
        | IdeWorkerEvent::SceneRemovalCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneRemoving) => apply_scene_removal_event(ui, &event),
        event @ (IdeWorkerEvent::AutomationTakeAdded { .. }
        | IdeWorkerEvent::AutomationTakeCreationCommittedWithWarning { .. }
        | IdeWorkerEvent::AutomationTakeAdding) => {
            apply_automation_take_creation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AutomationTakeActivationChanged { .. }
        | IdeWorkerEvent::AutomationTakeActivationCommittedWithWarning { .. }
        | IdeWorkerEvent::AutomationTakeActivationChanging { .. }) => {
            apply_automation_take_activation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AutomationTakeEdited { .. }
        | IdeWorkerEvent::AutomationTakeEditCommittedWithWarning { .. }
        | IdeWorkerEvent::AutomationTakeEditing { .. }) => {
            apply_automation_take_edit_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AutomationTakeRemoved { .. }
        | IdeWorkerEvent::AutomationTakeRemovalCommittedWithWarning { .. }
        | IdeWorkerEvent::AutomationTakeRemoving { .. }) => {
            apply_automation_take_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ControlMappingEdited { .. }
        | IdeWorkerEvent::ControlMappingEditCommittedWithWarning { .. }
        | IdeWorkerEvent::ControlMappingEditing { .. }) => {
            apply_control_mapping_event(ui, &event);
        }
        event @ (IdeWorkerEvent::NativeFilterEditing { .. }
        | IdeWorkerEvent::NativeFilterEdited { .. }
        | IdeWorkerEvent::NativeFilterEditCommittedWithWarning { .. }) => {
            apply_native_filter_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ProjectExporting | IdeWorkerEvent::ProjectExported { .. }) => {
            apply_project_export_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ProjectStemExporting
        | IdeWorkerEvent::ProjectStemsExported { .. }
        | IdeWorkerEvent::ProjectStemsExportedWithWarning { .. }) => {
            apply_track_stem_export_event(ui, &event);
        }
        IdeWorkerEvent::RuntimePreparing => {
            ui.set_app_mode(AppMode::PreparingRuntime);
            ui.set_status_text("Saving the project before compiling its Runtime bundle…".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RuntimePrepared { bundle } => {
            let identity = encode_sha256(bundle.bundle_sha256());
            ui.set_app_mode(AppMode::Running);
            ui.set_runtime_bundle_destination(bundle.output_path().display().to_string().into());
            ui.set_runtime_bundle_summary(
                format!(
                    "{} Runtime bundle · {} bytes · SHA-256 {}",
                    if bundle.reused() {
                        "Reused verified"
                    } else {
                        "Prepared"
                    },
                    bundle.bundle_byte_length(),
                    identity
                )
                .into(),
            );
            ui.set_status_text("Runtime bundle is ready".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MeterChanging => {
            ui.set_status_text("Saving the time signature...".into());
        }
        IdeWorkerEvent::MeterChanged {
            revision,
            numerator,
            denominator,
        } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_meter_signature(format!("{numerator}/{denominator}").into());
            ui.set_status_text(
                format!("Time signature saved in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MeterCommittedWithWarning { warning } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(SharedString::default());
            ui.set_error_text(
                format!(
                    "The time signature was saved but the project may need attention: {warning}"
                )
                .into(),
            );
        }
        IdeWorkerEvent::TempoChanging => {
            ui.set_status_text("Saving the tempo...".into());
        }
        IdeWorkerEvent::TempoChanged {
            revision,
            microseconds_per_quarter,
        } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_tempo_bpm(format_tempo_bpm(microseconds_per_quarter).into());
            ui.set_status_text(format!("Tempo saved in project revision {revision}").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::TempoCommittedWithWarning { warning } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(SharedString::default());
            ui.set_error_text(
                format!("The tempo was saved but the project may need attention: {warning}").into(),
            );
        }
        IdeWorkerEvent::TimelineMapChanging => {
            ui.set_status_text("Saving the timeline map...".into());
        }
        IdeWorkerEvent::TimelineMapChanged { revision } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_timeline_tempo_add_visible(false);
            ui.set_timeline_tempo_edit_visible(false);
            ui.set_timeline_tempo_move_visible(false);
            ui.set_timeline_meter_add_visible(false);
            ui.set_timeline_meter_edit_visible(false);
            ui.set_timeline_meter_move_visible(false);
            reset_timeline_marker_forms(ui);
            ui.set_status_text(format!("Timeline map saved in project revision {revision}").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::TimelineMapCommittedWithWarning { warning } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(SharedString::default());
            ui.set_error_text(
                format!("The timeline map was saved but the project may need attention: {warning}")
                    .into(),
            );
        }
        IdeWorkerEvent::TimelineMarkerChanging => {
            ui.set_status_text("Saving the navigation marker...".into());
        }
        IdeWorkerEvent::TimelineMarkerChanged { revision } => {
            ui.set_app_mode(AppMode::Running);
            reset_timeline_marker_forms(ui);
            ui.set_status_text(
                format!("Navigation marker saved in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::TimelineMarkerCommittedWithWarning { warning } => {
            ui.set_app_mode(AppMode::Running);
            reset_timeline_marker_forms(ui);
            ui.set_status_text(SharedString::default());
            ui.set_error_text(
                format!(
                    "The navigation marker was saved but the project may need attention: {warning}"
                )
                .into(),
            );
        }
        IdeWorkerEvent::ArrangementLoopRangeChanging => {
            ui.set_status_text("Saving the loop range...".into());
        }
        IdeWorkerEvent::ArrangementLoopRangeChanged { revision } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(format!("Loop range saved in project revision {revision}").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementLoopRangeCommittedWithWarning { warning } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(SharedString::default());
            ui.set_error_text(
                format!("The loop range was saved but the project may need attention: {warning}")
                    .into(),
            );
        }
        IdeWorkerEvent::UndoStateChanged {
            undo_label,
            redo_label,
        } => apply_undo_state(ui, undo_label, redo_label),
        IdeWorkerEvent::EditRestoring { redo, label } => apply_restore_progress(ui, redo, label),
        IdeWorkerEvent::EditRestored { revision, redo, .. } => {
            apply_restore_result(ui, restored_status_text(redo, revision), String::new());
        }
        IdeWorkerEvent::EditRestoreCommittedWithWarning {
            redo,
            label,
            warning,
        } => apply_restore_result(
            ui,
            String::new(),
            restore_warning_text(redo, label, warning.as_ref()),
        ),
        event @ (IdeWorkerEvent::SceneDuplicated { .. }
        | IdeWorkerEvent::SceneDuplicationCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneDuplicating) => apply_scene_duplication_event(ui, &event),
        event @ (IdeWorkerEvent::SceneSlotChanged { .. }
        | IdeWorkerEvent::SceneSlotEditCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneSlotChanging { .. }) => apply_scene_slot_event(ui, &event),
        event @ (IdeWorkerEvent::SceneRenamed { .. }
        | IdeWorkerEvent::SceneRenameCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneRenaming) => apply_scene_rename_event(ui, &event),
        event @ (IdeWorkerEvent::SceneActivationChanged { .. }
        | IdeWorkerEvent::SceneActivationCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneActivationChanging { .. }) => {
            apply_scene_activation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioClipCreatedFromTake { .. }
        | IdeWorkerEvent::AudioClipCreatingFromTake
        | IdeWorkerEvent::AudioClipCreatedAndPlacedFromTake { .. }
        | IdeWorkerEvent::AudioClipCreatingAndPlacingFromTake) => {
            apply_recording_clip_creation_event(ui, &event);
        }
        event @ (IdeWorkerEvent::RecordingTakeDiscarding
        | IdeWorkerEvent::RecordingTakeDiscarded { .. }) => {
            apply_recording_take_discard_event(ui, &event);
        }
        event @ (IdeWorkerEvent::RecordingTakeRenaming
        | IdeWorkerEvent::RecordingTakeRenamed { .. }) => {
            apply_recording_take_rename_event(ui, &event);
        }
        IdeWorkerEvent::RecordingPreparing => {
            ui.set_app_mode(AppMode::PreparingRecording);
            ui.set_transport_state("Stopped for capture".into());
            ui.set_status_text("Saving mixer state and transferring audio ownership…".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::Recording(observation) => apply_recording_event(ui, &observation),
        IdeWorkerEvent::RecordingFinalizing => {
            ui.set_app_mode(AppMode::FinalizingRecording);
            ui.set_status_text("Stopping capture and verifying the recorded WAV…".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RecordingImported(outcome) => apply_recording_imported(ui, &outcome),
        IdeWorkerEvent::RecordingAborted(outcome) => apply_recording_aborted(ui, &outcome),
        event @ (IdeWorkerEvent::ScenePerformanceCaptureStarted { .. }
        | IdeWorkerEvent::ScenePerformanceCaptureProgress { .. }
        | IdeWorkerEvent::ScenePerformanceCaptureStopping { .. }
        | IdeWorkerEvent::ScenePerformanceCaptureSaving { .. }
        | IdeWorkerEvent::ScenePerformanceCaptureSaved { .. }) => {
            apply_scene_performance_capture_event(ui, &event);
        }
        event @ (IdeWorkerEvent::MixerAutomationCaptureStarted { .. }
        | IdeWorkerEvent::MixerAutomationCaptureProgress { .. }
        | IdeWorkerEvent::MixerAutomationCaptureStopping { .. }
        | IdeWorkerEvent::MixerAutomationCaptureSaving { .. }) => {
            apply_mixer_automation_capture_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioPreviewPreparing
        | IdeWorkerEvent::AudioPreviewStopRequested
        | IdeWorkerEvent::AudioPreviewControlApplied(_)) => apply_audio_preview_event(ui, &event),
        IdeWorkerEvent::Saving => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text("Waiting for native output before atomic save…".into());
        }
        IdeWorkerEvent::ProjectRenaming => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text(
                "Waiting for native output before atomically renaming the project…".into(),
            );
        }
        IdeWorkerEvent::Closing => {
            ui.set_app_mode(AppMode::Closing);
            ui.set_status_text("Waiting for native output to close…".into());
        }
        IdeWorkerEvent::CommandFailed { command, reason } => {
            if command == IdeWorkerCommandKind::ResetOutputPeakHold {
                ui.set_output_peak_hold_reset_pending(false);
            }
            ui.set_app_mode(mode_after_failure(ui.get_app_mode(), command));
            ui.set_error_text(format!("{}: {reason}", command_status(command)).into());
        }
    }
}

fn apply_close_discard_required(ui: &MainWindow) {
    ui.set_close_unsaved_mixer_confirming(true);
    ui.set_status_text("Live mixer changes need a save or explicit discard before closing".into());
    ui.set_error_text(SharedString::default());
}

fn apply_close_discard_cancelled(ui: &MainWindow) {
    ui.set_close_unsaved_mixer_confirming(false);
    ui.set_status_text("Project close canceled; live mixer changes remain active".into());
    ui.set_error_text(SharedString::default());
}

fn apply_recent_projects(
    ui: &MainWindow,
    routes: &RecentProjectRoutes,
    projects: &[RecentProject],
) {
    let presentation = recent_project_presentation(projects);
    *routes.borrow_mut() = presentation.routes;
    ui.set_recent_project_names(ModelRc::from(Rc::new(VecModel::from(presentation.names))));
    ui.set_recent_project_display_names(ModelRc::from(Rc::new(VecModel::from(
        presentation.display_names,
    ))));
    ui.set_recent_project_paths(ModelRc::from(Rc::new(VecModel::from(presentation.paths))));
    ui.set_project_history_warning(SharedString::default());
}

fn recent_project_presentation(projects: &[RecentProject]) -> RecentProjectPresentation {
    RecentProjectPresentation {
        routes: projects
            .iter()
            .map(|project| project.path().to_path_buf())
            .collect(),
        names: projects
            .iter()
            .map(|project| SharedString::from(project.name()))
            .collect(),
        display_names: projects
            .iter()
            .map(|project| {
                SharedString::from(compact_name(
                    project.name(),
                    MAX_VISIBLE_RECENT_PROJECT_NAME_CHARS,
                ))
            })
            .collect(),
        paths: projects
            .iter()
            .map(|project| project.path().display().to_string().into())
            .collect(),
    }
}

fn apply_project_saved_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    let IdeWorkerEvent::ProjectSaved { revision, changed } = event else {
        unreachable!("project-save event dispatcher received another event");
    };
    ui.set_status_text(
        if *changed {
            format!("Project revision {revision} saved and reopened")
        } else {
            format!("Project revision {revision} already matched the live mixer")
        }
        .into(),
    );
    ui.set_error_text(SharedString::default());
}

fn apply_audio_track_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackAdded { revision } => {
            ui.set_new_track_name(next_audio_track_name(ui.get_track_count()));
            ui.set_new_track_visible(false);
            ui.set_status_text(format!("Audio track added in project revision {revision}").into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackAdding => {
            ui.set_app_mode(AppMode::AddingTrack);
            ui.set_status_text("Waiting for native output before adding the audio track…".into());
        }
        _ => unreachable!("audio-track event dispatcher received another event"),
    }
}

fn apply_routing_bus_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::RoutingBusAdded { revision, track_id } => {
            ui.set_routing_bus_create_visible(false);
            ui.set_status_text(
                format!("Routing bus {track_id} added in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RoutingBusAdding { kind } => {
            ui.set_app_mode(AppMode::AddingRoutingBus);
            ui.set_status_text(
                match kind {
                    IdeRoutingBusKind::Group => {
                        "Waiting for native output before creating the routing group…"
                    }
                    IdeRoutingBusKind::Return => {
                        "Waiting for native output before creating the routing return…"
                    }
                }
                .into(),
            );
        }
        IdeWorkerEvent::RoutingBusRemoved { revision, track_id } => {
            ui.set_routing_bus_removal_visible(false);
            ui.set_routing_bus_removal_id(SharedString::default());
            ui.set_routing_bus_removal_name(SharedString::default());
            ui.set_status_text(
                format!("Routing bus {track_id} removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("routing-bus event dispatcher received another event"),
    }
}

fn apply_audio_track_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackRemoved { revision } => {
            ui.set_remove_track_visible(false);
            ui.set_remove_track_id(SharedString::default());
            ui.set_remove_track_name(SharedString::default());
            ui.set_status_text(
                format!("Empty audio track removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackRemoving => {
            ui.set_app_mode(AppMode::RemovingTrack);
            ui.set_status_text("Waiting for native output before removing the audio track…".into());
        }
        _ => unreachable!("audio-track removal event dispatcher received another event"),
    }
}

fn apply_audio_track_content_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackContentRemoved { revision, track_id } => {
            ui.set_remove_track_content_visible(false);
            ui.set_remove_track_content_id(SharedString::default());
            ui.set_remove_track_content_name(SharedString::default());
            ui.set_status_text(
                format!("Audio track {track_id} and its content were removed in project revision {revision}")
                    .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackContentRemoving => {
            ui.set_app_mode(AppMode::RemovingTrack);
            ui.set_status_text(
                "Waiting for native output before removing the audio track and its content…".into(),
            );
        }
        IdeWorkerEvent::AudioTrackContentRemovalCommittedWithWarning { warning } => {
            ui.set_remove_track_content_visible(false);
            ui.set_remove_track_content_id(SharedString::default());
            ui.set_remove_track_content_name(SharedString::default());
            ui.set_status_text(
                "Audio track content removal committed, but recovery needs attention".into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("audio-track content removal event dispatcher received another event"),
    }
}

fn reset_audio_track_freeze_form(ui: &MainWindow) {
    ui.set_track_freeze_visible(false);
    ui.set_track_freeze_id(SharedString::default());
    ui.set_track_freeze_name(SharedString::default());
    ui.set_track_freeze_is_unfreeze(false);
    ui.set_track_freeze_is_flatten(false);
}

fn apply_audio_track_freeze_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackFrozen { revision, track_id } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text(
                format!("Audio track {track_id} frozen in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackFreezing => {
            ui.set_app_mode(AppMode::FreezingTrack);
            ui.set_status_text("Waiting for native output before freezing the audio track…".into());
        }
        IdeWorkerEvent::AudioTrackFreezeCommittedWithWarning { warning } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text("Audio track freeze committed, but recovery needs attention".into());
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("audio-track freeze event dispatcher received another event"),
    }
}

fn apply_audio_track_unfreeze_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackUnfrozen { revision, track_id } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text(
                format!("Audio track {track_id} unfrozen in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackUnfreezing => {
            ui.set_app_mode(AppMode::UnfreezingTrack);
            ui.set_status_text(
                "Waiting for native output before unfreezing the audio track…".into(),
            );
        }
        IdeWorkerEvent::AudioTrackUnfreezeCommittedWithWarning { warning } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text(
                "Audio track unfreeze committed, but recovery needs attention".into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("audio-track unfreeze event dispatcher received another event"),
    }
}

fn apply_audio_track_flatten_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackFlattened { revision, track_id } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text(
                format!("Audio track {track_id} flattened in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackFlattening => {
            ui.set_app_mode(AppMode::FlatteningTrack);
            ui.set_status_text(
                "Waiting for native output before flattening the audio track…".into(),
            );
        }
        IdeWorkerEvent::AudioTrackFlattenCommittedWithWarning { warning } => {
            reset_audio_track_freeze_form(ui);
            ui.set_status_text(
                "Audio track flatten committed, but recovery needs attention".into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("audio-track flatten event dispatcher received another event"),
    }
}

fn reset_audio_track_duplication_form(ui: &MainWindow) {
    ui.set_duplicate_track_visible(false);
    ui.set_duplicate_track_index(-1);
    ui.set_duplicate_track_source_name(SharedString::default());
    ui.set_duplicate_track_name(SharedString::default());
}

fn apply_audio_track_duplication_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackDuplicated { revision, .. } => {
            reset_audio_track_duplication_form(ui);
            ui.set_status_text(
                format!("Mixer strip duplicated in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackDuplicating => {
            ui.set_app_mode(AppMode::DuplicatingTrack);
            ui.set_status_text(
                "Waiting for native output before duplicating the mixer strip…".into(),
            );
        }
        _ => unreachable!("audio-track duplication event dispatcher received another event"),
    }
}

fn apply_mixer_strip_reorder_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::MixerStripsReordered { revision } => {
            ui.set_status_text(
                format!("Mixer strips reordered in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MixerStripsReordering => {
            ui.set_status_text("Waiting for native output before reordering mixer strips…".into());
        }
        _ => unreachable!("mixer-strip reorder event dispatcher received another event"),
    }
}

fn apply_audio_track_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackRenamed { revision, changed } => {
            ui.set_rename_track_visible(false);
            ui.set_rename_track_index(-1);
            ui.set_rename_track_original_name(SharedString::default());
            ui.set_rename_track_name(SharedString::default());
            ui.set_rename_track_kind(SharedString::default());
            ui.set_status_text(
                if *changed {
                    format!("Audio track renamed in project revision {revision}")
                } else {
                    format!("Audio track already had that name in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackRenaming => {
            ui.set_app_mode(AppMode::RenamingTrack);
            ui.set_status_text("Waiting for native output before renaming the audio track…".into());
        }
        _ => unreachable!("audio-track rename event dispatcher received another event"),
    }
}

fn apply_routing_bus_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::RoutingBusRenamed { revision, changed } => {
            ui.set_rename_track_visible(false);
            ui.set_rename_track_index(-1);
            ui.set_rename_track_original_name(SharedString::default());
            ui.set_rename_track_name(SharedString::default());
            ui.set_rename_track_kind(SharedString::default());
            ui.set_status_text(
                if *changed {
                    format!("Routing bus renamed in project revision {revision}")
                } else {
                    format!("Routing bus already had that name in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RoutingBusRenaming => {
            ui.set_app_mode(AppMode::RenamingTrack);
            ui.set_status_text("Waiting for native output before renaming the routing bus…".into());
        }
        _ => unreachable!("routing-bus rename event dispatcher received another event"),
    }
}

fn apply_audio_track_activation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackActivationChanged {
            revision,
            changed,
            active,
        } => {
            let state = if *active { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!("Audio track {state} in project revision {revision}")
                } else {
                    format!("Audio track was already {state} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioTrackActivationChanging { active } => {
            ui.set_app_mode(AppMode::ChangingTrackActivation);
            ui.set_status_text(
                if *active {
                    "Waiting for native output before enabling the audio track…"
                } else {
                    "Waiting for native output before disabling the audio track…"
                }
                .into(),
            );
        }
        _ => unreachable!("audio-track activation event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_activation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementActivationChanged {
            revision,
            changed,
            active,
        } => {
            let state = if *active { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!("Arrangement placement {state} in project revision {revision}")
                } else {
                    format!(
                        "Arrangement placement was already {state} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementActivationChanging { active } => {
            ui.set_app_mode(AppMode::ChangingArrangementActivation);
            ui.set_status_text(
                if *active {
                    "Waiting for native output before enabling the arrangement placement…"
                } else {
                    "Waiting for native output before disabling the arrangement placement…"
                }
                .into(),
            );
        }
        _ => unreachable!("arrangement activation event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_move_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementMoved {
            revision,
            changed,
            start_tick,
        } => {
            ui.set_status_text(
                if *changed {
                    format!(
                        "Arrangement placement moved to tick {start_tick} in project revision {revision}"
                    )
                } else {
                    format!(
                        "Arrangement placement already started at tick {start_tick} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementMoving { start_tick } => {
            ui.set_app_mode(AppMode::MovingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before moving the arrangement placement to tick {start_tick}…"
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement move event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_duration_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementDurationChanged {
            revision,
            changed,
            duration_ticks,
        } => {
            ui.set_status_text(
                if *changed {
                    format!(
                        "Arrangement placement trimmed to {duration_ticks} ticks in project revision {revision}"
                    )
                } else {
                    format!(
                        "Arrangement placement already had a duration of {duration_ticks} ticks in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementDurationChanging { duration_ticks } => {
            ui.set_app_mode(AppMode::TrimmingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before trimming the arrangement placement to {duration_ticks} ticks…"
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement duration event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_gain_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementGainChanged {
            revision,
            changed,
            gain_decibels,
        } => {
            ui.set_status_text(
                if *changed {
                    format!(
                        "Arrangement placement gain set to {gain_decibels:.4} dB in project revision {revision}"
                    )
                } else {
                    format!(
                        "Arrangement placement gain already matched {gain_decibels:.4} dB in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementGainChanging { gain_decibels } => {
            ui.set_app_mode(AppMode::ChangingArrangementPlacementGain);
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting arrangement placement gain to {gain_decibels:.4} dB…"
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement placement gain event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_fades_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementFadesChanged {
            revision,
            changed,
            fade_in_ticks,
            fade_out_ticks,
        } => {
            ui.set_status_text(
                if *changed {
                    format!(
                        "Arrangement placement fades set to {fade_in_ticks} in / {fade_out_ticks} out ticks in project revision {revision}"
                    )
                } else {
                    format!(
                        "Arrangement placement fades already matched {fade_in_ticks} in / {fade_out_ticks} out ticks in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementFadesChanging {
            fade_in_ticks,
            fade_out_ticks,
        } => {
            ui.set_app_mode(AppMode::ChangingArrangementPlacementFades);
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting arrangement placement fades to {fade_in_ticks} in / {fade_out_ticks} out ticks…"
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement placement fades event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_crossfade_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementCrossfadeApplied { revision, changed } => {
            ui.set_status_text(
                if *changed {
                    format!("Bounded arrangement crossfade applied in project revision {revision}")
                } else {
                    format!(
                        "Arrangement placement fades already matched the bounded crossfade in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementCrossfadeApplying => {
            ui.set_app_mode(AppMode::ChangingArrangementPlacementCrossfade);
            ui.set_status_text(
                "Waiting for native output before applying the bounded arrangement crossfade…"
                    .into(),
            );
        }
        _ => unreachable!("arrangement crossfade event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_front_trim_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementFrontTrimmed {
            revision,
            changed,
            start_tick,
        } => {
            ui.set_status_text(
                if *changed {
                    format!(
                        "Arrangement placement front trimmed to tick {start_tick} in project revision {revision}"
                    )
                } else {
                    format!(
                        "Arrangement placement already started at tick {start_tick} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementFrontTrimming { start_tick } => {
            ui.set_app_mode(AppMode::TrimmingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before trimming the arrangement front to tick {start_tick}..."
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement front-trim event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementRemoved { revision } => {
            ui.set_remove_arrangement_placement_visible(false);
            ui.set_remove_arrangement_placement_id(SharedString::default());
            ui.set_remove_arrangement_placement_clip_name(SharedString::default());
            ui.set_remove_arrangement_placement_track_name(SharedString::default());
            ui.set_status_text(
                format!("Arrangement placement removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementRemoving => {
            ui.set_app_mode(AppMode::RemovingArrangementPlacement);
            ui.set_status_text(
                "Waiting for native output before removing the arrangement placement…".into(),
            );
        }
        _ => unreachable!("arrangement removal event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_addition_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementAdded {
            revision,
            start_tick,
        } => {
            ui.set_add_arrangement_placement_visible(false);
            ui.set_add_arrangement_placement_clip_id(SharedString::default());
            ui.set_add_arrangement_placement_clip_name(SharedString::default());
            ui.set_add_arrangement_placement_track_name(SharedString::default());
            ui.set_add_arrangement_placement_duration_ticks(SharedString::default());
            ui.set_add_arrangement_placement_start_tick("0".into());
            ui.set_status_text(
                format!(
                    "Placed the full audio clip at tick {start_tick} in project revision {revision}"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementAdding { start_tick } => {
            ui.set_app_mode(AppMode::AddingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before placing the full audio clip at tick {start_tick}…"
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement addition event dispatcher received another event"),
    }
}

fn apply_arrangement_scene_launch_addition_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementSceneLaunchAdded {
            revision,
            scene_id,
            tick,
        } => {
            reset_arrangement_scene_launch_addition_form(ui);
            ui.set_status_text(
                format!("Added scene {scene_id} at tick {tick} in project revision {revision}")
                    .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementSceneLaunchAdding { tick } => {
            ui.set_app_mode(AppMode::AddingArrangementSceneLaunch);
            ui.set_status_text(
                format!(
                    "Waiting for native output before adding the scene marker at tick {tick}..."
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement scene-launch dispatcher received another event"),
    }
}

fn apply_arrangement_scene_launch_move_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementSceneLaunchMoved {
            revision,
            launch_id,
            tick,
        } => {
            reset_arrangement_scene_launch_move_form(ui);
            ui.set_status_text(
                format!(
                    "Scene marker {launch_id} moved to tick {tick} in project revision {revision}"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementSceneLaunchMoving { tick } => {
            ui.set_app_mode(AppMode::MovingArrangementSceneLaunch);
            ui.set_status_text(
                format!(
                    "Waiting for native output before moving the scene marker to tick {tick}..."
                )
                .into(),
            );
        }
        _ => unreachable!("arrangement scene-launch move dispatcher received another event"),
    }
}

fn apply_arrangement_scene_launch_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementSceneLaunchRemoved {
            revision,
            launch_id,
        } => {
            reset_arrangement_scene_launch_removal_form(ui);
            ui.set_status_text(
                format!("Scene marker {launch_id} removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementSceneLaunchRemoving => {
            ui.set_app_mode(AppMode::RemovingArrangementSceneLaunch);
            ui.set_status_text(
                "Waiting for native output before removing the scene marker...".into(),
            );
        }
        _ => unreachable!("arrangement scene-launch removal dispatcher received another event"),
    }
}

fn apply_arrangement_scene_launch_scene_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementSceneLaunchSceneChanged {
            revision,
            launch_id,
            scene_id,
        } => {
            reset_arrangement_scene_launch_scene_form(ui);
            ui.set_status_text(
                format!(
                    "Scene marker {launch_id} now launches scene {scene_id} in project revision {revision}"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementSceneLaunchSceneChanging => {
            ui.set_app_mode(AppMode::ChangingArrangementSceneLaunchScene);
            ui.set_status_text(
                "Waiting for native output before changing the scene marker...".into(),
            );
        }
        _ => unreachable!("arrangement scene-launch scene dispatcher received another event"),
    }
}

fn apply_arrangement_placement_duplication_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementDuplicated {
            revision,
            placement_id,
            start_tick,
        } => {
            ui.set_status_text(
                format!(
                    "Arrangement placement copied to tick {start_tick} as {placement_id} in project revision {revision}; the same clip and exact source view were reused"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementDuplicating { start_tick } => {
            ui.set_app_mode(AppMode::DuplicatingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before copying the exact arrangement placement to tick {start_tick}…"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("arrangement duplication event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_split_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ArrangementPlacementSplit {
            revision,
            placement_id,
            split_tick,
        } => {
            ui.set_status_text(
                format!(
                    "Arrangement placement split at tick {split_tick}; right half {placement_id} committed in project revision {revision}"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ArrangementPlacementSplitting { split_tick } => {
            ui.set_app_mode(AppMode::SplittingArrangementPlacement);
            ui.set_status_text(
                format!(
                    "Waiting for native output before splitting the arrangement placement at tick {split_tick}…"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("arrangement split event dispatcher received another event"),
    }
}

fn apply_arrangement_placement_committed_warning(ui: &MainWindow, event: &IdeWorkerEvent) {
    let IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning { command, warning } = event
    else {
        unreachable!("arrangement committed-warning dispatcher received another event");
    };
    let action = match command {
        IdeWorkerCommandKind::CaptureScenePerformance => "Scene-performance capture".to_owned(),
        IdeWorkerCommandKind::AddArrangementSceneLaunch => {
            reset_arrangement_scene_launch_addition_form(ui);
            "Arrangement scene marker".to_owned()
        }
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => {
            reset_arrangement_scene_launch_move_form(ui);
            "Arrangement scene marker move".to_owned()
        }
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene => {
            reset_arrangement_scene_launch_scene_form(ui);
            "Arrangement scene marker reassignment".to_owned()
        }
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch => {
            reset_arrangement_scene_launch_removal_form(ui);
            "Arrangement scene marker removal".to_owned()
        }
        IdeWorkerCommandKind::AddArrangementPlacement => {
            reset_arrangement_placement_addition_form(ui);
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement addition warning must retain its identity");
            format!("Arrangement placement {placement_id}")
        }
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement duplication warning must retain its identity");
            format!("Copied arrangement placement {placement_id}")
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => {
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement split warning must retain its right-half identity");
            format!("Split arrangement placement right half {placement_id}")
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement => {
            reset_arrangement_placement_removal_confirmation(ui);
            "Arrangement placement removal".to_owned()
        }
        IdeWorkerCommandKind::SetArrangementPlacementGain => {
            "Arrangement placement gain".to_owned()
        }
        IdeWorkerCommandKind::SetArrangementPlacementFades => {
            "Arrangement placement fades".to_owned()
        }
        IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade => {
            "Arrangement placement crossfade".to_owned()
        }
        _ => "Arrangement placement edit".to_owned(),
    };
    ui.set_status_text(committed_warning_status(ui.get_app_mode(), &action).into());
    ui.set_error_text(warning.to_string().into());
}

fn reset_arrangement_placement_addition_form(ui: &MainWindow) {
    ui.set_add_arrangement_placement_visible(false);
    ui.set_add_arrangement_placement_clip_id(SharedString::default());
    ui.set_add_arrangement_placement_clip_name(SharedString::default());
    ui.set_add_arrangement_placement_track_name(SharedString::default());
    ui.set_add_arrangement_placement_duration_ticks(SharedString::default());
    ui.set_add_arrangement_placement_start_tick("0".into());
}

fn reset_arrangement_scene_launch_move_form(ui: &MainWindow) {
    ui.set_move_arrangement_scene_launch_visible(false);
    ui.set_move_arrangement_scene_launch_id(SharedString::default());
    ui.set_move_arrangement_scene_launch_name(SharedString::default());
    ui.set_move_arrangement_scene_launch_original_tick(SharedString::default());
    ui.set_move_arrangement_scene_launch_tick(SharedString::default());
}

fn reset_arrangement_scene_launch_addition_form(ui: &MainWindow) {
    ui.set_add_arrangement_scene_launch_visible(false);
    ui.set_add_arrangement_scene_launch_id(SharedString::default());
    ui.set_add_arrangement_scene_launch_name(SharedString::default());
    ui.set_add_arrangement_scene_launch_tick("0".into());
}

fn reset_arrangement_scene_launch_removal_form(ui: &MainWindow) {
    ui.set_remove_arrangement_scene_launch_visible(false);
    ui.set_remove_arrangement_scene_launch_id(SharedString::default());
    ui.set_remove_arrangement_scene_launch_name(SharedString::default());
    ui.set_remove_arrangement_scene_launch_tick(SharedString::default());
}

fn reset_arrangement_scene_launch_scene_form(ui: &MainWindow) {
    ui.set_set_arrangement_scene_launch_scene_visible(false);
    ui.set_set_arrangement_scene_launch_scene_id(SharedString::default());
    ui.set_set_arrangement_scene_launch_scene_name(SharedString::default());
    ui.set_set_arrangement_scene_launch_scene_current_id(SharedString::default());
    ui.set_set_arrangement_scene_launch_scene_current_index(-1);
    ui.set_set_arrangement_scene_launch_scene_tick(SharedString::default());
    ui.set_set_arrangement_scene_launch_scene_target_index(-1);
}

fn reset_arrangement_placement_removal_confirmation(ui: &MainWindow) {
    ui.set_remove_arrangement_placement_visible(false);
    ui.set_remove_arrangement_placement_id(SharedString::default());
    ui.set_remove_arrangement_placement_clip_name(SharedString::default());
    ui.set_remove_arrangement_placement_track_name(SharedString::default());
}

fn apply_audio_clip_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipRenamed {
            revision,
            changed,
            name,
        } => {
            reset_audio_clip_rename_form(ui);
            ui.set_status_text(
                if *changed {
                    format!("Audio clip renamed to {name} in project revision {revision}")
                } else {
                    format!("Audio clip was already named {name} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipRenaming => {
            ui.set_app_mode(AppMode::RenamingClip);
            ui.set_status_text("Waiting for native output before renaming the audio clip…".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("audio-clip rename event dispatcher received another event"),
    }
}

fn reset_audio_clip_rename_form(ui: &MainWindow) {
    ui.set_clip_rename_visible(false);
    ui.set_clip_rename_id(SharedString::default());
    ui.set_clip_rename_original(SharedString::default());
    ui.set_clip_rename_name(SharedString::default());
}

fn apply_scene_creation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneAdded {
            revision,
            scene_id: _,
            name,
        } => {
            reset_scene_creation_form(ui);
            ui.set_status_text(
                format!("Scene {name} created in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneCreationCommittedWithWarning { name, warning } => {
            reset_scene_creation_form(ui);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), &format!("Scene creation for {name}"))
                    .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneAdding => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text("Waiting for native output before creating the scene...".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene creation event dispatcher received another event"),
    }
}

fn apply_scene_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneRemoved { revision, scene_id } => {
            reset_scene_removal_form(ui);
            ui.set_status_text(
                format!("Scene {scene_id} removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneRemovalCommittedWithWarning { warning } => {
            reset_scene_removal_form(ui);
            ui.set_status_text(committed_warning_status(ui.get_app_mode(), "Scene removal").into());
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneRemoving => {
            ui.set_app_mode(AppMode::RemovingScene);
            ui.set_status_text("Waiting for native output before removing the scene…".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene removal event dispatcher received another event"),
    }
}

fn apply_scene_duplication_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneDuplicated {
            revision,
            scene_id,
            name,
        } => {
            reset_scene_duplication_form(ui);
            ui.set_status_text(
                format!(
                    "Scene {name} ({scene_id}) duplicated in project revision {revision}; existing arrangement launches were unchanged"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneDuplicationCommittedWithWarning { name, warning } => {
            reset_scene_duplication_form(ui);
            ui.set_status_text(
                committed_warning_status(
                    ui.get_app_mode(),
                    &format!("Scene duplication for {name}"),
                )
                .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneDuplicating => {
            ui.set_app_mode(AppMode::DuplicatingScene);
            ui.set_status_text("Waiting for native output before duplicating the scene...".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene duplication event dispatcher received another event"),
    }
}

fn scene_slot_action_label(edit: &IdeSceneSlotEdit) -> &'static str {
    match edit {
        IdeSceneSlotEdit::Continue => "continue",
        IdeSceneSlotEdit::Stop => "stop",
        IdeSceneSlotEdit::Play { .. } => "play",
    }
}

fn apply_scene_slot_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneSlotChanged {
            revision,
            changed,
            edit,
        } => {
            let action = scene_slot_action_label(edit);
            ui.set_status_text(
                if *changed {
                    format!("Scene slot set to {action} in project revision {revision}")
                } else {
                    format!("Scene slot was already set to {action} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneSlotEditCommittedWithWarning { edit, warning } => {
            reset_scene_slots_form(ui);
            ui.set_status_text(
                committed_warning_status(
                    ui.get_app_mode(),
                    &format!("Scene slot {}", scene_slot_action_label(edit)),
                )
                .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneSlotChanging { edit } => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text(
                format!(
                    "Waiting for native output before applying scene slot {}...",
                    scene_slot_action_label(edit)
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene slot event dispatcher received another event"),
    }
}

fn apply_scene_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneRenamed {
            revision,
            changed,
            name,
        } => {
            reset_scene_rename_form(ui);
            ui.set_status_text(
                if *changed {
                    format!("Scene renamed to {name} in project revision {revision}")
                } else {
                    format!("Scene was already named {name} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneRenameCommittedWithWarning { name, warning } => {
            reset_scene_rename_form(ui);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), &format!("Scene rename to {name}"))
                    .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneRenaming => {
            ui.set_app_mode(AppMode::RenamingScene);
            ui.set_status_text("Waiting for native output before renaming the scene…".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene rename event dispatcher received another event"),
    }
}

fn apply_scene_activation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::SceneActivationChanged {
            revision,
            changed,
            active,
        } => {
            let state = if *active { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!("Scene {state} in project revision {revision}")
                } else {
                    format!("Scene was already {state} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::SceneActivationCommittedWithWarning { active, warning } => {
            let state = if *active { "enable" } else { "disable" };
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), &format!("Scene {state}")).into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::SceneActivationChanging { active } => {
            ui.set_app_mode(AppMode::ChangingSceneActivation);
            ui.set_status_text(
                if *active {
                    "Waiting for native output before enabling the scene…"
                } else {
                    "Waiting for native output before disabling the scene…"
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene activation event dispatcher received another event"),
    }
}

fn reset_scene_rename_form(ui: &MainWindow) {
    ui.set_scene_rename_visible(false);
    ui.set_scene_rename_id(SharedString::default());
    ui.set_scene_rename_original(SharedString::default());
    ui.set_scene_rename_name(SharedString::default());
}

fn reset_scene_duplication_form(ui: &MainWindow) {
    ui.set_scene_duplicate_visible(false);
    ui.set_scene_duplicate_id(SharedString::default());
    ui.set_scene_duplicate_source_name(SharedString::default());
    ui.set_scene_duplicate_name(SharedString::default());
}

fn reset_scene_removal_form(ui: &MainWindow) {
    ui.set_scene_remove_visible(false);
    ui.set_scene_remove_id(SharedString::default());
    ui.set_scene_remove_name(SharedString::default());
}

fn apply_audio_clip_duplication_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipDuplicated {
            revision,
            clip_id,
            name,
        } => {
            reset_audio_clip_duplication_form(ui);
            ui.set_status_text(
                format!(
                    "Reusable audio clip {name} ({clip_id}) duplicated in project revision {revision}; no media or arrangement placement was created"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipDuplicationCommittedWithWarning { name, warning } => {
            let clip_id = warning
                .committed_clip_id()
                .expect("committed duplication warning must retain its clip identity");
            reset_audio_clip_duplication_form(ui);
            ui.set_status_text(
                committed_warning_status(
                    ui.get_app_mode(),
                    &format!("Reusable audio clip {name} ({clip_id})"),
                )
                .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AudioClipDuplicating => {
            ui.set_app_mode(AppMode::DuplicatingClip);
            ui.set_status_text(
                "Waiting for native output before duplicating the reusable audio clip…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("audio-clip duplication event dispatcher received another event"),
    }
}

fn reset_audio_clip_duplication_form(ui: &MainWindow) {
    ui.set_clip_duplicate_visible(false);
    ui.set_clip_duplicate_id(SharedString::default());
    ui.set_clip_duplicate_source_name(SharedString::default());
    ui.set_clip_duplicate_name(SharedString::default());
}

fn apply_audio_clip_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipRemoved { revision, clip_id } => {
            reset_audio_clip_removal_form(ui);
            ui.set_status_text(
                format!(
                    "Reusable audio clip {clip_id} removed in project revision {revision}; source media was retained"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipRemovalCommittedWithWarning { warning } => {
            reset_audio_clip_removal_form(ui);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Reusable audio clip removal").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AudioClipRemoving => {
            ui.set_app_mode(AppMode::RemovingClip);
            ui.set_status_text(
                "Waiting for native output before removing the unused reusable clip…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("audio-clip removal event dispatcher received another event"),
    }
}

fn reset_audio_clip_removal_form(ui: &MainWindow) {
    ui.set_clip_remove_visible(false);
    ui.set_clip_remove_id(SharedString::default());
    ui.set_clip_remove_name(SharedString::default());
}

fn apply_unused_sample_cleanup_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::UnusedSampleCleanupStarting { assets, bytes } => {
            ui.set_app_mode(AppMode::CleaningSamples);
            ui.set_status_text(
                format!(
                    "Waiting for native output before permanently removing {assets} unused sample copies ({bytes} bytes)…"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::UnusedSamplesCleaned {
            revision,
            assets,
            bytes,
        } => {
            ui.set_unused_sample_cleanup_confirming(false);
            ui.set_status_text(
                format!(
                    "Permanently removed {assets} unused project sample copies ({bytes} bytes) in revision {revision}; recording takes and external originals were retained"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::UnusedSampleCleanupCommittedWithWarning { warning } => {
            ui.set_unused_sample_cleanup_confirming(false);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Unused sample cleanup").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("unused-sample cleanup event dispatcher received another event"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioClipEditWarningForm {
    None,
    Rename,
    Gain,
    Fades,
    Loop,
    Reverse,
    SourceSlip,
    SourceTrim,
}

fn audio_clip_edit_warning_presentation(
    command: IdeWorkerCommandKind,
) -> Option<(AudioClipEditWarningForm, &'static str)> {
    match command {
        IdeWorkerCommandKind::RenameAudioClip => {
            Some((AudioClipEditWarningForm::Rename, "Audio-clip rename"))
        }
        IdeWorkerCommandKind::SetAudioClipActive => {
            Some((AudioClipEditWarningForm::None, "Audio-clip activation edit"))
        }
        IdeWorkerCommandKind::SetAudioClipGain => {
            Some((AudioClipEditWarningForm::Gain, "Audio-clip gain edit"))
        }
        IdeWorkerCommandKind::SetAudioClipFades => {
            Some((AudioClipEditWarningForm::Fades, "Audio-clip fade edit"))
        }
        IdeWorkerCommandKind::SetAudioClipLoop => {
            Some((AudioClipEditWarningForm::Loop, "Audio-clip loop edit"))
        }
        IdeWorkerCommandKind::SetAudioClipReverse => Some((
            AudioClipEditWarningForm::Reverse,
            "Audio-clip direction edit",
        )),
        IdeWorkerCommandKind::SetAudioClipSourceStart => Some((
            AudioClipEditWarningForm::SourceSlip,
            "Audio-clip source-slip edit",
        )),
        IdeWorkerCommandKind::TrimAudioClipSource => Some((
            AudioClipEditWarningForm::SourceTrim,
            "Audio-clip source-trim edit",
        )),
        _ => None,
    }
}

fn apply_audio_clip_edit_committed_warning(ui: &MainWindow, event: &IdeWorkerEvent) {
    let IdeWorkerEvent::AudioClipEditCommittedWithWarning { command, warning } = event else {
        unreachable!("audio-clip edit committed-warning dispatcher received another event");
    };
    let Some((form, action)) = audio_clip_edit_warning_presentation(*command) else {
        unreachable!("committed-warning event used an unrelated command");
    };
    match form {
        AudioClipEditWarningForm::None | AudioClipEditWarningForm::Reverse => {}
        AudioClipEditWarningForm::Rename => reset_audio_clip_rename_form(ui),
        AudioClipEditWarningForm::Gain => reset_audio_clip_gain_form(ui),
        AudioClipEditWarningForm::Fades => reset_audio_clip_fade_form(ui),
        AudioClipEditWarningForm::Loop => reset_audio_clip_loop_form(ui),
        AudioClipEditWarningForm::SourceSlip => reset_audio_clip_source_slip_form(ui),
        AudioClipEditWarningForm::SourceTrim => reset_audio_clip_source_trim_form(ui),
    }
    ui.set_status_text(committed_warning_status(ui.get_app_mode(), action).into());
    ui.set_error_text(warning.to_string().into());
}

fn committed_warning_status(mode: AppMode, action: &str) -> String {
    match mode {
        AppMode::Running => {
            format!("{action} committed and reopened with a project recovery warning")
        }
        AppMode::Closed => format!("{action} committed, but the project could not be reopened"),
        _ => format!("{action} committed with a project recovery warning"),
    }
}

fn apply_audio_clip_activation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipActivationChanged {
            revision,
            changed,
            active,
        } => {
            let state = if *active { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!("Audio clip {state} globally in project revision {revision}")
                } else {
                    format!("Audio clip was already {state} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipActivationChanging { active } => {
            ui.set_app_mode(AppMode::ChangingClipActivation);
            ui.set_status_text(
                if *active {
                    "Waiting for native output before enabling the audio clip globally…"
                } else {
                    "Waiting for native output before disabling the audio clip globally…"
                }
                .into(),
            );
        }
        _ => unreachable!("audio-clip activation event dispatcher received another event"),
    }
}

fn apply_audio_clip_gain_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipGainChanged {
            revision,
            changed,
            gain_decibels,
        } => {
            reset_audio_clip_gain_form(ui);
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip gain set to {gain_decibels:.4} dB in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip gain already matched {gain_decibels:.4} dB in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipGainChanging { gain_decibels } => {
            ui.set_app_mode(AppMode::ChangingClipGain);
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting audio-clip gain to {gain_decibels:.4} dB…"
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip gain event dispatcher received another event"),
    }
}

fn reset_audio_clip_gain_form(ui: &MainWindow) {
    ui.set_clip_gain_visible(false);
    ui.set_clip_gain_id(SharedString::default());
    ui.set_clip_gain_name(SharedString::default());
    ui.set_clip_gain_original(SharedString::default());
    ui.set_clip_gain_value(SharedString::default());
}

fn apply_audio_clip_fade_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipFadesChanged {
            revision,
            changed,
            fade_in_ticks,
            fade_out_ticks,
        } => {
            reset_audio_clip_fade_form(ui);
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip fades set to {fade_in_ticks} in / {fade_out_ticks} out ticks in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip fades already matched {fade_in_ticks} in / {fade_out_ticks} out ticks in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipFadesChanging {
            fade_in_ticks,
            fade_out_ticks,
        } => {
            ui.set_app_mode(AppMode::ChangingClipFades);
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting audio-clip fades to {fade_in_ticks} in / {fade_out_ticks} out ticks..."
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip fade event dispatcher received another event"),
    }
}

fn apply_audio_clip_loop_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipLoopChanged {
            revision,
            changed,
            enabled,
            start_tick,
            end_tick,
        } => {
            reset_audio_clip_loop_form(ui);
            let state = if *enabled { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip loop {state} at {start_tick}–{end_tick} ticks in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip loop already matched {state} at {start_tick}–{end_tick} ticks in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipLoopChanging {
            enabled,
            start_tick,
            end_tick,
        } => {
            ui.set_app_mode(AppMode::ChangingClipLoop);
            let state = if *enabled { "enabled" } else { "disabled" };
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting the audio-clip loop {state} at {start_tick}–{end_tick} ticks..."
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip loop event dispatcher received another event"),
    }
}

fn apply_audio_clip_reverse_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipReverseChanged {
            revision,
            changed,
            reverse,
        } => {
            let direction = if *reverse { "reverse" } else { "forward" };
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip playback direction set to {direction} in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip playback direction already matched {direction} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipReverseChanging { reverse } => {
            ui.set_app_mode(AppMode::ChangingClipReverse);
            let direction = if *reverse { "reverse" } else { "forward" };
            ui.set_status_text(
                format!(
                    "Waiting for native output before setting reusable audio-clip playback to {direction}..."
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip reverse event dispatcher received another event"),
    }
}

fn apply_audio_clip_source_slip_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipSourceStartChanged {
            revision,
            changed,
            source_start_frame,
        } => {
            reset_audio_clip_source_slip_form(ui);
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip source start set to frame {source_start_frame} in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip source start already matched frame {source_start_frame} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipSourceStartChanging { source_start_frame } => {
            ui.set_app_mode(AppMode::ChangingClipSource);
            ui.set_status_text(
                format!(
                    "Waiting for native output before slipping the reusable audio clip to source frame {source_start_frame}..."
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip source-slip event dispatcher received another event"),
    }
}

fn reset_audio_clip_source_slip_form(ui: &MainWindow) {
    ui.set_clip_source_slip_visible(false);
    ui.set_clip_source_slip_id(SharedString::default());
    ui.set_clip_source_slip_name(SharedString::default());
    ui.set_clip_source_slip_frame_count(SharedString::default());
    ui.set_clip_source_slip_original(SharedString::default());
    ui.set_clip_source_slip_start(SharedString::default());
}

fn apply_audio_clip_source_trim_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipSourceTrimmed {
            revision,
            changed,
            source_start_frame,
            source_frame_count,
        } => {
            reset_audio_clip_source_trim_form(ui);
            ui.set_status_text(
                if *changed {
                    format!(
                        "Audio-clip source view trimmed to frame {source_start_frame} for {source_frame_count} frames in project revision {revision}"
                    )
                } else {
                    format!(
                        "Audio-clip source view already matched frame {source_start_frame} for {source_frame_count} frames in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipSourceTrimming {
            source_start_frame,
            source_frame_count,
        } => {
            ui.set_app_mode(AppMode::TrimmingClipSource);
            ui.set_status_text(
                format!(
                    "Waiting for native output before trimming the reusable audio-clip source to frame {source_start_frame} for {source_frame_count} frames..."
                )
                .into(),
            );
        }
        _ => unreachable!("audio-clip source-trim event dispatcher received another event"),
    }
}

fn reset_audio_clip_source_trim_form(ui: &MainWindow) {
    ui.set_clip_source_trim_visible(false);
    ui.set_clip_source_trim_id(SharedString::default());
    ui.set_clip_source_trim_name(SharedString::default());
    ui.set_clip_source_trim_original_start(SharedString::default());
    ui.set_clip_source_trim_original_frame_count(SharedString::default());
    ui.set_clip_source_trim_start(SharedString::default());
    ui.set_clip_source_trim_frame_count(SharedString::default());
}

fn apply_audio_clip_waveform(ui: &MainWindow, waveform: &IdeAudioClipWaveform) {
    if !ui.get_clip_waveform_visible() || ui.get_clip_waveform_id() != waveform.clip_id() {
        return;
    }
    ui.set_clip_waveform_commands(waveform_path_commands(waveform).into());
    ui.set_clip_waveform_bin_count(waveform.bins().len().to_string().into());
    ui.set_clip_waveform_loading(false);
    ui.set_status_text("Audio-clip source waveform ready".into());
    ui.set_error_text(SharedString::default());
}

fn waveform_path_commands(waveform: &IdeAudioClipWaveform) -> String {
    let bins = waveform.bins();
    if bins.is_empty() {
        return String::new();
    }
    let bin_count = u16::try_from(bins.len())
        .expect("the application bounds one waveform observation to 128 bins");
    let bin_width = 128.0_f32 / f32::from(bin_count);
    let mut commands = String::new();
    for (index, bin) in bins.iter().enumerate() {
        let index = f32::from(
            u16::try_from(index)
                .expect("the application bounds one waveform observation to 128 bins"),
        );
        let left = index * bin_width;
        let right = (index + 0.78) * bin_width;
        let top = 1.0 - bin.maximum().clamp(-1.0, 1.0);
        let bottom = 1.0 - bin.minimum().clamp(-1.0, 1.0);
        write!(
            commands,
            "M {left} {top} L {right} {top} L {right} {bottom} L {left} {bottom} Z "
        )
        .expect("writing SVG path commands to a String cannot fail");
    }
    commands
}

fn reset_audio_clip_waveform_dialog(ui: &MainWindow) {
    ui.set_clip_waveform_visible(false);
    ui.set_clip_waveform_loading(false);
    ui.set_clip_waveform_id(SharedString::default());
    ui.set_clip_waveform_name(SharedString::default());
    ui.set_clip_waveform_commands(SharedString::default());
    ui.set_clip_waveform_bin_count("0".into());
}

fn apply_recording_clip_creation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioClipCreatedFromTake { revision, clip_id } => {
            ui.set_take_clip_visible(false);
            ui.set_take_clip_index(-1);
            ui.set_take_clip_track_name(SharedString::default());
            ui.set_take_clip_name(SharedString::default());
            ui.set_status_text(
                format!(
                    "Reusable audio clip {clip_id} created in project revision {revision}; no arrangement placement was created"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipCreatingFromTake => {
            ui.set_app_mode(AppMode::CreatingClip);
            ui.set_status_text(
                "Waiting for native output before creating the reusable audio clip…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipCreatedAndPlacedFromTake { revision, clip_id } => {
            ui.set_take_clip_visible(false);
            ui.set_take_clip_index(-1);
            ui.set_take_clip_track_name(SharedString::default());
            ui.set_take_clip_name(SharedString::default());
            ui.set_status_text(
                format!(
                    "Recorded take clip {clip_id} was created and placed in project revision {revision}"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AudioClipCreatingAndPlacingFromTake => {
            ui.set_app_mode(AppMode::CreatingClip);
            ui.set_status_text(
                "Waiting for native output before creating and placing the recorded take…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("recording clip creation dispatcher received another event"),
    }
}

fn apply_recording_take_discard_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::RecordingTakeDiscarding => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text(
                "Waiting for native output before discarding the recorded take…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RecordingTakeDiscarded {
            revision,
            recording_id,
        } => {
            ui.set_take_discard_confirming_index(-1);
            ui.set_status_text(
                format!("Recorded take {recording_id} was permanently discarded in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("recording-take discard dispatcher received another event"),
    }
}

fn apply_recording_take_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::RecordingTakeRenaming => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text(
                "Waiting for native output before renaming the recorded take…".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::RecordingTakeRenamed {
            revision,
            changed,
            label,
        } => {
            ui.set_take_rename_visible(false);
            ui.set_take_rename_index(-1);
            ui.set_take_rename_label(SharedString::default());
            ui.set_status_text(
                if *changed {
                    format!("Recorded take renamed to {label} in project revision {revision}")
                } else {
                    format!(
                        "Recorded take was already named {label} in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("recording-take rename dispatcher received another event"),
    }
}

fn apply_output_test_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::OutputTest(snapshot) => {
            ui.set_app_mode(AppMode::OutputTesting);
            ui.set_output_test_confirming(false);
            ui.set_output_test_signal(
                format!(
                    "{:.0} Hz · {:.1} dBFS · stereo",
                    snapshot.frequency_hz(),
                    snapshot.level_dbfs()
                )
                .into(),
            );
            ui.set_output_test_detail(
                format!(
                    "{} · {} callbacks · {} rendered frames",
                    output_test_state_label(snapshot.state()),
                    snapshot.output().rendered_callback_count,
                    snapshot.output().rendered_frame_count,
                )
                .into(),
            );
            apply_output_test_status(ui, snapshot.state());
        }
        IdeWorkerEvent::OutputTestStopping => {
            ui.set_app_mode(AppMode::StoppingOutputTest);
            ui.set_status_text("Waiting for exclusive output diagnostic to stop…".into());
        }
        IdeWorkerEvent::OutputTestStopped => {
            ui.set_app_mode(AppMode::Closed);
            ui.set_output_test_detail("Digital output check; not physical calibration".into());
            ui.set_status_text("Output diagnostic stopped".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("only output-test events are routed to this handler"),
    }
}

fn output_test_state_label(state: IdeOutputTestState) -> &'static str {
    match state {
        IdeOutputTestState::Starting => "Starting",
        IdeOutputTestState::Running => "Running",
        IdeOutputTestState::StopRequested => "Stop requested",
        IdeOutputTestState::Stopped => "Stopped",
        IdeOutputTestState::Failed => "Failed",
    }
}

fn apply_output_test_status(ui: &MainWindow, state: IdeOutputTestState) {
    match state {
        IdeOutputTestState::Starting => {
            ui.set_status_text("Digital output diagnostic starting…".into());
            ui.set_error_text(SharedString::default());
        }
        IdeOutputTestState::Running => {
            ui.set_status_text(
                "Digital output diagnostic active · not physical calibration".into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeOutputTestState::StopRequested => {
            ui.set_status_text("Digital output diagnostic stop requested…".into());
            ui.set_error_text(SharedString::default());
        }
        IdeOutputTestState::Stopped => {
            ui.set_status_text("Output diagnostic stream stopped; release the route".into());
            ui.set_error_text("Stop the diagnostic before continuing".into());
        }
        IdeOutputTestState::Failed => {
            ui.set_status_text("Output diagnostic stream failed; release the route".into());
            ui.set_error_text("Native output is no longer rendering; stop the diagnostic".into());
        }
    }
}

fn apply_loudness_queued(ui: &MainWindow, command: IdeLoudnessCommand) {
    ui.set_status_text(
        match command {
            IdeLoudnessCommand::Run => "Programme loudness resume queued",
            IdeLoudnessCommand::Standby => "Programme loudness standby queued",
            IdeLoudnessCommand::Reset => "Programme loudness reset queued",
        }
        .into(),
    );
    if command == IdeLoudnessCommand::Reset {
        ui.set_loudness_reset_confirming(false);
    }
    ui.set_error_text(SharedString::default());
}

fn apply_audio_preview_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioPreviewPreparing => {
            ui.set_app_mode(AppMode::PreparingPreview);
            ui.set_transport_state("Stopping for audition".into());
            ui.set_status_text("Waiting for a committed stopped transport cycle…".into());
        }
        IdeWorkerEvent::AudioPreviewStopRequested => {
            ui.set_status_text("Ramping take audition to silence…".into());
        }
        IdeWorkerEvent::AudioPreviewControlApplied(_) => {
            ui.set_status_text("Take audition control applied".into());
        }
        _ => unreachable!("only audio-preview events are routed to this handler"),
    }
    ui.set_error_text(SharedString::default());
}

fn apply_recording_imported(ui: &MainWindow, outcome: &IdeRecordingImportOutcome) {
    let monitor_warning = outcome
        .monitor_output_error()
        .map_or_else(String::new, |error| {
            format!("; cue output shutdown warning: {error}")
        });
    ui.set_status_text(
        format!(
            "Imported {} frames as project revision {} ({}) · recovery journal retained at {}{}",
            outcome.frames(),
            outcome.project_revision(),
            outcome.recording_path(),
            outcome.journal_path().display(),
            monitor_warning,
        )
        .into(),
    );
    ui.set_error_text(SharedString::default());
}

fn apply_recording_aborted(ui: &MainWindow, outcome: &IdeRecordingAbortOutcome) {
    let monitor_warning = outcome
        .monitor_output_error()
        .map_or_else(String::new, |error| {
            format!("; cue output shutdown warning: {error}")
        });
    ui.set_status_text(
        format!(
            "Capture aborted after {} frames; recovery journal retained at {}{}",
            outcome.frames(),
            outcome.journal_path().display(),
            monitor_warning,
        )
        .into(),
    );
    ui.set_error_text(SharedString::default());
}

#[allow(
    clippy::too_many_lines,
    reason = "one Running observation must update all Rust-owned presentation authority together"
)]
fn apply_running_event(
    ui: &MainWindow,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    audio_import_routes: &AudioImportRoutes,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    ui.set_mixer_recovery_available(false);
    ui.set_new_project_visible(false);
    ui.set_save_project_as_visible(false);
    ui.set_rename_project_visible(false);
    ui.set_project_name(observation.project_name().into());
    ui.set_project_id(observation.project_id().into());
    ui.set_sample_rate(format!("{} Hz", observation.sample_rate_hz()).into());
    ui.set_scene_count(observation.scene_count().to_string().into());
    ui.set_track_count(i32::try_from(observation.track_count()).unwrap_or(8));
    if !ui.get_new_track_visible() {
        ui.set_new_track_name(next_audio_track_name(ui.get_track_count()));
    }
    let scenes_changed = set_observed_scenes(ui, project_state, observation);
    set_native_filter_observation(ui, project_state, observation);
    apply_mixer_observation(ui, observation);
    set_routing_bus_sources(ui, observation.mixer_channels());
    set_recording_targets(ui, recording_routes, observation.mixer_channels());
    set_audio_import_targets(ui, audio_import_routes, observation.mixer_channels());
    apply_unused_sample_cleanup_observation(ui, project_state, observation);
    set_recording_takes(ui, take_routes, observation);
    set_playback_extent(ui, project_state, observation);
    set_tempo_and_loop(ui, project_state, observation);
    set_arrangement(ui, project_state, observation);
    set_automation(ui, project_state, observation);
    set_control_mappings(ui, project_state, observation);
    let clips_changed = set_audio_clips(ui, project_state, observation.audio_clips());
    refresh_scene_slots_if_changed(ui, project_state, scenes_changed || clips_changed);
    ui.set_stream_state(format!("{:?}", observation.output_state()).into());
    ui.set_callback_count(observation.output().callback_count.to_string().into());
    if let Some(monitoring) = observation.monitoring() {
        ui.set_monitoring_active(true);
        ui.set_monitoring_gain_percent(monitoring.gain * 100.0);
        ui.set_monitoring_muted(monitoring.muted);
        ui.set_monitoring_state(format!("{:?}", monitoring.input_state).into());
        ui.set_monitoring_frame_count(monitoring.bridge.accepted_frame_count.to_string().into());
        let dropped_frames = monitoring
            .input
            .dropped_frame_count
            .saturating_add(monitoring.bridge.overflow_frame_count)
            .saturating_add(monitoring.bridge.consumer_closed_frame_count);
        ui.set_monitoring_drop_count(dropped_frames.to_string().into());
        ui.set_monitoring_underflow_count(monitoring.bridge.underflow_count.to_string().into());
        apply_input_meter_observation(ui, observation.input_meter_observation());
    } else {
        reset_monitoring(ui);
    }
    ui.set_xrun_count(
        observation
            .output()
            .native_xruns
            .map_or_else(
                || "Unavailable".to_owned(),
                |xrun| {
                    xrun.recovery_count.map_or_else(
                        || "Observed, count unavailable".to_owned(),
                        |count| count.to_string(),
                    )
                },
            )
            .into(),
    );
    let position_frame = observation.recovery().position_frame();
    set_playhead(ui, project_state, position_frame);
    let transport_state = observation.recovery().state();
    apply_transport_recovery_availability(ui, transport_state);
    apply_metronome_observation(ui, observation);
    ui.set_transport_clock(
        format_transport_clock(
            u64::try_from(position_frame).unwrap_or(u64::MAX),
            observation.sample_rate_hz(),
        )
        .into(),
    );
    ui.set_transport_state(
        if observation.preview().state().is_active() {
            if observation.preview().clip_id().is_some() {
                "Auditioning clip source · transport stopped"
            } else {
                "Auditioning take · transport stopped"
            }
        } else {
            match transport_state {
                PlaybackRecoveryTransportState::Stopped => "Stopped",
                PlaybackRecoveryTransportState::Rolling => "Rolling",
            }
        }
        .into(),
    );
    apply_output_meter_observation(ui, observation);
    match observation.preview().state() {
        DesktopAudioPreviewState::Starting
        | DesktopAudioPreviewState::Playing
        | DesktopAudioPreviewState::Stopping => {
            ui.set_app_mode(AppMode::Previewing);
            ui.set_status_text(
                if observation.preview().clip_id().is_some() {
                    "Dry clip-source audition · input cue temporarily suppressed"
                } else {
                    "Dry take audition · input cue temporarily suppressed"
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        DesktopAudioPreviewState::Complete => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text(
                if observation.preview().clip_id().is_some() {
                    "Clip-source audition complete"
                } else {
                    "Take audition complete"
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        DesktopAudioPreviewState::Failed => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Audio audition stopped".into());
            ui.set_error_text(
                if observation.preview().clip_id().is_some() {
                    "The audio callback could not render this clip source view"
                } else {
                    "The audio callback could not render this recorded take"
                }
                .into(),
            );
        }
        DesktopAudioPreviewState::Idle => {
            apply_idle_running_event(ui);
        }
    }
}

fn apply_unused_sample_cleanup_observation(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    let unused = observation.unused_sample_cleanup();
    project_state.unused_sample_cleanup.set(Some(unused));
    ui.set_unused_sample_assets(i32::try_from(unused.assets()).unwrap_or(i32::MAX));
    ui.set_unused_sample_bytes(format!("{} bytes", unused.bytes()).into());
    ui.set_unused_sample_cleanup_confirming(false);
}

fn next_audio_track_name(track_count: i32) -> SharedString {
    format!("Audio {}", track_count.saturating_add(1)).into()
}

fn apply_output_meter_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    apply_final_output_meter_observation(ui, observation.output_meter_observation());
    apply_mixer_strip_meter_observation(ui, observation);
    apply_loudness_observation(ui, observation);
}

fn apply_input_meter_observation(ui: &MainWindow, observation: &IdeInputMeterObservation) {
    let (left_level, left_reading, right_level, right_reading) =
        observation.sample_peak_display().map_or_else(
            || (0.0, "— dBFS".to_owned(), 0.0, "— dBFS".to_owned()),
            |snapshot| {
                let left = snapshot.left();
                let right = snapshot.right();
                let (left_level, left_reading) =
                    input_meter_presentation(left.measurement_available(), left.level_dbfs());
                let (right_level, right_reading) =
                    input_meter_presentation(right.measurement_available(), right.level_dbfs());
                (left_level, left_reading, right_level, right_reading)
            },
        );
    ui.set_input_meter_left_level(left_level);
    ui.set_input_meter_right_level(right_level);
    ui.set_input_meter_left_reading(left_reading.into());
    ui.set_input_meter_right_reading(right_reading.into());
}

fn reset_input_meter_observation(ui: &MainWindow) {
    ui.set_input_meter_left_level(0.0);
    ui.set_input_meter_right_level(0.0);
    ui.set_input_meter_left_reading("— dBFS".into());
    ui.set_input_meter_right_reading("— dBFS".into());
}

fn apply_final_output_meter_observation(ui: &MainWindow, observation: &IdeOutputMeterObservation) {
    let peak_hold_available = observation.peak_hold_available();
    let (
        left_level,
        left_reading,
        left_peak_reading,
        left_peak_overload_latched,
        right_level,
        right_reading,
        right_peak_reading,
        right_peak_overload_latched,
    ) = observation.sample_peak_ballistics().map_or_else(
        || {
            (
                0.0,
                "— dBFS".to_owned(),
                "hold —".to_owned(),
                false,
                0.0,
                "— dBFS".to_owned(),
                "hold —".to_owned(),
                false,
            )
        },
        |snapshot| {
            let left = snapshot.left();
            let right = snapshot.right();
            let (left_level, left_reading) =
                meter_presentation(left.measurement_available(), left.level_dbfs());
            let (left_peak_reading, left_peak_overload_latched) =
                output_peak_hold_presentation(peak_hold_available, left.held_peak_dbfs());
            let (right_level, right_reading) =
                meter_presentation(right.measurement_available(), right.level_dbfs());
            let (right_peak_reading, right_peak_overload_latched) =
                output_peak_hold_presentation(peak_hold_available, right.held_peak_dbfs());
            (
                left_level,
                left_reading,
                left_peak_reading,
                left_peak_overload_latched,
                right_level,
                right_reading,
                right_peak_reading,
                right_peak_overload_latched,
            )
        },
    );
    ui.set_left_meter(left_level);
    ui.set_right_meter(right_level);
    ui.set_left_reading(left_reading.into());
    ui.set_right_reading(right_reading.into());
    ui.set_left_peak_reading(left_peak_reading.into());
    ui.set_right_peak_reading(right_peak_reading.into());
    ui.set_left_peak_overload_latched(left_peak_overload_latched);
    ui.set_right_peak_overload_latched(right_peak_overload_latched);
    ui.set_output_peak_hold_available(peak_hold_available);
}

fn reset_output_peak_hold_presentation(ui: &MainWindow) {
    ui.set_left_peak_reading("hold —".into());
    ui.set_right_peak_reading("hold —".into());
    ui.set_left_peak_overload_latched(false);
    ui.set_right_peak_overload_latched(false);
    ui.set_output_peak_hold_available(false);
    ui.set_output_peak_hold_reset_pending(false);
}

fn apply_mixer_strip_meter_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    let meter = observation.output_meter_observation();
    let channels = ui.get_mixer_channels();
    let Some(strip_meters) = meter.mixer_strip_ballistics() else {
        clear_mixer_strip_meter_observation(&channels);
        return;
    };
    if meter.latest().sender_disconnected() {
        clear_mixer_strip_meter_observation(&channels);
        return;
    }
    let layout_matches = channels.row_count() == strip_meters.slot_count()
        && (0..strip_meters.slot_count()).all(|slot| {
            channels.row_data(slot).is_some_and(|row| {
                row.index == i32::try_from(slot).unwrap_or(-1) && strip_meters.slot(slot).is_some()
            })
        });
    if !layout_matches {
        clear_mixer_strip_meter_observation(&channels);
        return;
    }
    for slot in 0..strip_meters.slot_count() {
        let Some(strip) = strip_meters.slot(slot) else {
            clear_mixer_strip_meter_observation(&channels);
            return;
        };
        let Some(mut row) = channels.row_data(slot) else {
            clear_mixer_strip_meter_observation(&channels);
            return;
        };
        let left = strip.left();
        let right = strip.right();
        let (left_level, left_reading) =
            mixer_strip_meter_presentation(left.measurement_available(), left.level_dbfs());
        let (right_level, right_reading) =
            mixer_strip_meter_presentation(right.measurement_available(), right.level_dbfs());
        row.meter_left_level = left_level;
        row.meter_right_level = right_level;
        row.meter_left_reading = left_reading.into();
        row.meter_right_reading = right_reading.into();
        channels.set_row_data(slot, row);
    }
}

fn clear_mixer_strip_meter_observation(channels: &ModelRc<MixerChannelRow>) {
    for slot in 0..channels.row_count() {
        if let Some(mut row) = channels.row_data(slot) {
            row.meter_left_level = 0.0;
            row.meter_right_level = 0.0;
            row.meter_left_reading = "—".into();
            row.meter_right_reading = "—".into();
            channels.set_row_data(slot, row);
        }
    }
}

fn apply_loudness_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    let config = IdeLoudnessPresentationConfig::new(
        if ui.get_loudness_wide_scale() {
            IdeLoudnessScale::Plus18
        } else {
            IdeLoudnessScale::Plus9
        },
        if ui.get_loudness_relative() {
            IdeLoudnessDisplayUnit::RelativeLu
        } else {
            IdeLoudnessDisplayUnit::AbsoluteLufs
        },
    );
    ui.set_loudness_scale_name(config.scale().name().into());
    let Some(presentation) = observation.loudness(config) else {
        reset_loudness_observation(ui);
        return;
    };
    let unit = config.unit().symbol();
    ui.set_loudness_available(true);
    ui.set_loudness_state(
        match presentation.state() {
            IdeLoudnessMeasurementState::Running => "RUNNING",
            IdeLoudnessMeasurementState::Standby => "STANDBY",
        }
        .into(),
    );
    ui.set_loudness_momentary(format_loudness(presentation.momentary(), unit).into());
    ui.set_loudness_short_term(format_loudness(presentation.short_term(), unit).into());
    ui.set_loudness_integrated(format_loudness(presentation.integrated(), unit).into());
    ui.set_loudness_maximum_momentary(
        format_loudness(presentation.maximum_momentary(), unit).into(),
    );
    ui.set_loudness_maximum_short_term(
        format_loudness(presentation.maximum_short_term(), unit).into(),
    );
    ui.set_loudness_range(
        presentation
            .loudness_range_lu()
            .map_or_else(|| "— LU".to_owned(), |value| format!("{value:.1} LU"))
            .into(),
    );
    ui.set_loudness_maximum_true_peak(
        format_maximum_true_peak(presentation.maximum_true_peak()).into(),
    );
    let programme_time = format_transport_clock(
        presentation.programme_frame_count(),
        observation.sample_rate_hz(),
    );
    ui.set_loudness_programme_time(programme_time.clone().into());
    ui.set_loudness_lra_status(
        if presentation.lra_stable() {
            "Stable (60 s programme minimum)".to_owned()
        } else {
            format!("Stabilizing · {programme_time} / 00:01:00.000")
        }
        .into(),
    );
}

fn reset_loudness_observation(ui: &MainWindow) {
    let loudness_unit = if ui.get_loudness_relative() {
        "LU"
    } else {
        "LUFS"
    };
    ui.set_loudness_available(false);
    ui.set_loudness_state("UNAVAILABLE".into());
    ui.set_loudness_momentary(format!("— {loudness_unit}").into());
    ui.set_loudness_short_term(format!("— {loudness_unit}").into());
    ui.set_loudness_integrated(format!("— {loudness_unit}").into());
    ui.set_loudness_maximum_momentary(format!("— {loudness_unit}").into());
    ui.set_loudness_maximum_short_term(format!("— {loudness_unit}").into());
    ui.set_loudness_range("— LU".into());
    ui.set_loudness_maximum_true_peak("— dBTP".into());
    ui.set_loudness_programme_time("00:00:00.000".into());
    ui.set_loudness_lra_status("Waiting for final-output meter evidence".into());
}

fn format_loudness(value: Option<IdeScaledLoudnessValue>, unit: &str) -> String {
    value.map_or_else(
        || format!("— {unit}"),
        |value| {
            let bound = match value.region() {
                IdeLoudnessScaleRegion::Below => "<",
                IdeLoudnessScaleRegion::OnScale => "",
                IdeLoudnessScaleRegion::Above => ">",
            };
            format!("{bound}{:.1} {unit}", value.displayed())
        },
    )
}

fn format_maximum_true_peak(value: Option<IdeMaximumTruePeakDisplay>) -> String {
    value.map_or_else(
        || "— dBTP".to_owned(),
        |value| {
            value
                .displayed_dbtp()
                .map_or_else(|| "−∞ dBTP".to_owned(), |dbtp| format!("{dbtp:.1} dBTP"))
        },
    )
}

fn set_recording_takes(
    ui: &MainWindow,
    take_routes: &TakeRoutes,
    observation: &IdeRunningObservation,
) {
    ui.set_take_discard_confirming_index(-1);
    let preview = observation.preview();
    let active_asset_id = preview.asset_id();
    let takes = observation.recording_takes();
    let mut routes = Vec::with_capacity(takes.len());
    let mut rows = Vec::with_capacity(takes.len());
    for (index, take) in takes.iter().enumerate() {
        routes.push(TakeRoute {
            recording_id: take.id().to_owned(),
            asset_id: take.asset_id().to_owned(),
        });
        let active = active_asset_id == Some(take.asset_id());
        rows.push(TakeRow {
            index: i32::try_from(index).unwrap_or(-1),
            track_name: take.track_name().into(),
            label: take.label().unwrap_or(take.track_name()).into(),
            captured_at: format_capture_time(take.captured_at_utc()).into(),
            detail: format_take_detail(take).into(),
            timeline_start: format!(
                "Timeline tick {} · {}",
                take.timeline_start_tick(),
                capture_kind_name(take.capture_kind())
            )
            .into(),
            active,
            state: if active {
                format!("{:?}", preview.state()).into()
            } else {
                SharedString::default()
            },
            discardable: take.discardable(),
        });
    }
    *take_routes.borrow_mut() = routes;
    ui.set_recording_takes(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_preview_active(preview.state().is_active());
    ui.set_preview_clip_id(preview.clip_id().unwrap_or_default().into());
    ui.set_preview_gain_percent(preview.gain() * 100.0);
    ui.set_preview_muted(preview.muted());
    ui.set_preview_state(format!("{:?}", preview.state()).into());
    ui.set_preview_progress(if preview.total_frames() == 0 {
        "—".into()
    } else {
        format!(
            "{} / {}",
            format_transport_clock(preview.played_frames(), observation.sample_rate_hz()),
            format_transport_clock(preview.total_frames(), observation.sample_rate_hz())
        )
        .into()
    });
}

const fn capture_kind_name(kind: IdeRecordingCaptureKind) -> &'static str {
    match kind {
        IdeRecordingCaptureKind::New => "NEW TAKE",
        IdeRecordingCaptureKind::Overdub => "OVERDUB",
    }
}

fn format_capture_time(captured_at_utc: &str) -> String {
    captured_at_utc
        .strip_suffix('Z')
        .unwrap_or(captured_at_utc)
        .replace('T', " ")
}

fn format_take_detail(take: &IdeRecordingTake) -> String {
    let channels = match take.channels() {
        1 => "Mono".to_owned(),
        2 => "Stereo".to_owned(),
        count => format!("{count} channels"),
    };
    format!(
        "{} · {} · {} Hz",
        channels,
        format_transport_clock(take.frame_count(), take.sample_rate_hz()),
        take.sample_rate_hz()
    )
}

fn set_playback_extent(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    match observation.playback_extent() {
        IdePlaybackExtent::Bounded { end_frame } => {
            *project_state.playback_end_frame.borrow_mut() = Some(end_frame);
            ui.set_transport_extent_bounded(true);
            ui.set_transport_seek_available(end_frame > 0);
            ui.set_export_available(export_available(observation.playback_extent()));
            ui.set_transport_end_clock(
                format_transport_clock(
                    u64::try_from(end_frame).unwrap_or(u64::MAX),
                    observation.sample_rate_hz(),
                )
                .into(),
            );
            ui.set_transport_progress_percent(frame_progress_percent(
                observation.recovery().position_frame(),
                end_frame,
            ));
        }
        IdePlaybackExtent::Unbounded => {
            *project_state.playback_end_frame.borrow_mut() = None;
            ui.set_transport_extent_bounded(false);
            ui.set_transport_seek_available(false);
            ui.set_export_available(export_available(observation.playback_extent()));
            ui.set_transport_end_clock("—".into());
            ui.set_transport_progress_percent(0.0);
        }
    }
}

struct ArrangementListModel {
    owner: Arc<IdePlaybackArrangement>,
    sample_rate_hz: u32,
}

/// Finds the one UI-presentable incoming placement for a bounded crossfade.
///
/// The worker remains authoritative; this only prevents the UI from offering a
/// pair the committed arrangement cannot presently represent. A conflicting
/// candidate fails closed rather than choosing an arbitrary edit target.
fn arrangement_crossfade_target(
    arrangement: &IdePlaybackArrangement,
    outgoing_index: usize,
) -> Option<(usize, u64)> {
    let outgoing = arrangement.placements().get(outgoing_index)?;
    if !outgoing.active() {
        return None;
    }
    let outgoing_end_tick = outgoing
        .start_tick()
        .checked_add(outgoing.duration_ticks())?;
    let outgoing_available_ticks = outgoing
        .duration_ticks()
        .checked_sub(outgoing.fade_in_ticks())?;
    let mut candidate = None;

    for (incoming_index, incoming) in arrangement.placements().iter().enumerate() {
        if incoming_index == outgoing_index
            || !incoming.active()
            || incoming.track_index() != outgoing.track_index()
            || outgoing.start_tick() >= incoming.start_tick()
            || incoming.start_tick() >= outgoing_end_tick
        {
            continue;
        }
        let incoming_end_tick = incoming
            .start_tick()
            .checked_add(incoming.duration_ticks())?;
        if incoming_end_tick < outgoing_end_tick {
            continue;
        }
        let crossfade_ticks = outgoing_end_tick - incoming.start_tick();
        let incoming_available_ticks = incoming
            .duration_ticks()
            .checked_sub(incoming.fade_out_ticks())?;
        if crossfade_ticks > outgoing_available_ticks || crossfade_ticks > incoming_available_ticks
        {
            continue;
        }
        if candidate.is_some() {
            return None;
        }
        candidate = Some((incoming_index, crossfade_ticks));
    }

    candidate
}

struct ArrangementSceneLaunchListModel {
    owner: Arc<IdePlaybackArrangement>,
    scene_ids: Vec<String>,
}

impl Model for ArrangementSceneLaunchListModel {
    type Data = ArrangementSceneLaunchRow;

    fn row_count(&self) -> usize {
        self.owner.scene_launches().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let launch = self.owner.scene_launches().get(row)?;
        Some(ArrangementSceneLaunchRow {
            index: i32::try_from(row).ok()?,
            id: launch.id().into(),
            scene_index: i32::try_from(launch.scene_index()).ok()?,
            scene_id: self.scene_ids.get(launch.scene_index())?.as_str().into(),
            scene_name: self.owner.scene_name(launch.scene_index())?.into(),
            tick: launch.tick().to_string().into(),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

impl Model for ArrangementListModel {
    type Data = ArrangementRow;

    fn row_count(&self) -> usize {
        self.owner.placements().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let placement = self.owner.placements().get(row)?;
        let track_name = self.owner.track_name(placement.track_index())?;
        let clip_name = self.owner.clip_name(placement.clip_index())?;
        let start_percent = frame_progress_percent(placement.start_frame(), self.owner.end_frame());
        let end_percent = frame_progress_percent(placement.end_frame(), self.owner.end_frame());
        let (crossfade_target_id, crossfade_target_label) =
            if let Some((target_index, crossfade_ticks)) =
                arrangement_crossfade_target(self.owner.as_ref(), row)
            {
                let target = self.owner.placements().get(target_index)?;
                let target_clip_name = self.owner.clip_name(target.clip_index())?;
                (
                    target.id().into(),
                    format!("{target_clip_name} ({crossfade_ticks} ticks)").into(),
                )
            } else {
                (SharedString::default(), SharedString::default())
            };
        Some(ArrangementRow {
            index: i32::try_from(row).ok()?,
            id: placement.id().into(),
            track_name: track_name.into(),
            clip_name: clip_name.into(),
            start_tick: placement.start_tick().to_string().into(),
            clip_start_tick: placement.clip_start_tick().to_string().into(),
            duration_ticks: placement.duration_ticks().to_string().into(),
            gain_edit: format_gain_decibels(placement.gain_decibels()).into(),
            fade_in_edit: placement.fade_in_ticks().to_string().into(),
            fade_out_edit: placement.fade_out_ticks().to_string().into(),
            crossfade_target_id,
            crossfade_target_label,
            start_clock: format_frame_clock(placement.start_frame(), self.sample_rate_hz).into(),
            end_clock: format_frame_clock(placement.end_frame(), self.sample_rate_hz).into(),
            start_percent,
            width_percent: (end_percent - start_percent).max(0.0),
            enabled: placement.enabled(),
            active: placement.active(),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

fn set_arrangement(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    let owner = observation.arrangement();
    let scene_ids: Vec<String> = project_state
        .scenes
        .borrow()
        .iter()
        .map(|scene| scene.id().to_owned())
        .collect();
    let unchanged = project_state
        .arrangement
        .borrow()
        .as_ref()
        .is_some_and(|current| {
            current.project_id == observation.project_id()
                && current.sample_rate_hz == observation.sample_rate_hz()
                && current.scene_ids == scene_ids
                && Arc::ptr_eq(&current.owner, owner)
        });
    if unchanged {
        return;
    }
    ui.set_arrangement_placements(ModelRc::new(ArrangementListModel {
        owner: Arc::clone(owner),
        sample_rate_hz: observation.sample_rate_hz(),
    }));
    ui.set_arrangement_scene_launches(ModelRc::new(ArrangementSceneLaunchListModel {
        owner: Arc::clone(owner),
        scene_ids: scene_ids.clone(),
    }));
    ui.set_arrangement_span_clock(
        format_frame_clock(owner.end_frame(), observation.sample_rate_hz()).into(),
    );
    *project_state.arrangement.borrow_mut() = Some(ArrangementBinding {
        project_id: observation.project_id().to_owned(),
        owner: Arc::clone(owner),
        sample_rate_hz: observation.sample_rate_hz(),
        scene_ids,
    });
    ui.set_loop_export_available(current_persisted_loop_export_range(project_state).is_some());
}

fn set_audio_clips(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    clips: &[IdeAudioClip],
) -> bool {
    if project_state.audio_clips.borrow().as_slice() == clips {
        return false;
    }
    reset_audio_clip_waveform_dialog(ui);
    let rows: Vec<ClipRow> = clips
        .iter()
        .map(|clip| ClipRow {
            id: clip.id().into(),
            name: clip.name().into(),
            track_name: clip.track_name().into(),
            active: clip.active(),
            gain_label: format!("{:+.2} dB", clip.gain_decibels()).into(),
            gain_edit: format_gain_decibels(clip.gain_decibels()).into(),
            loop_enabled: clip.loop_enabled(),
            loop_label: format!(
                "{} {}–{}",
                if clip.loop_enabled() { "ON" } else { "OFF" },
                clip.loop_start_tick(),
                clip.loop_end_tick()
            )
            .into(),
            loop_start_edit: clip.loop_start_tick().to_string().into(),
            loop_end_edit: clip.loop_end_tick().to_string().into(),
            reverse: clip.reverse(),
            direction_label: if clip.reverse() { "REVERSE" } else { "FORWARD" }.into(),
            fade_label: format!(
                "{} in / {} out",
                clip.fade_in_ticks(),
                clip.fade_out_ticks()
            )
            .into(),
            fade_in_edit: clip.fade_in_ticks().to_string().into(),
            fade_out_edit: clip.fade_out_ticks().to_string().into(),
            duration_ticks: clip.duration_ticks().to_string().into(),
            source_start_edit: clip.source_start_frame().to_string().into(),
            source_frame_count: clip.source_frame_count().to_string().into(),
        })
        .collect();
    ui.set_audio_clips(ModelRc::from(Rc::new(VecModel::from(rows))));
    *project_state.audio_clips.borrow_mut() = clips.to_vec();
    true
}

fn format_gain_decibels(gain_decibels: f64) -> String {
    if gain_decibels == 0.0 {
        "0".to_owned()
    } else {
        gain_decibels.to_string()
    }
}

fn reset_audio_clip_fade_form(ui: &MainWindow) {
    ui.set_clip_fades_visible(false);
    ui.set_clip_fades_id(SharedString::default());
    ui.set_clip_fades_name(SharedString::default());
    ui.set_clip_fades_duration(SharedString::default());
    ui.set_clip_fades_original_in(SharedString::default());
    ui.set_clip_fades_original_out(SharedString::default());
    ui.set_clip_fades_in(SharedString::default());
    ui.set_clip_fades_out(SharedString::default());
}

fn reset_audio_clip_loop_form(ui: &MainWindow) {
    ui.set_clip_loop_visible(false);
    ui.set_clip_loop_id(SharedString::default());
    ui.set_clip_loop_name(SharedString::default());
    ui.set_clip_loop_duration(SharedString::default());
    ui.set_clip_loop_original_enabled(false);
    ui.set_clip_loop_original_start(SharedString::default());
    ui.set_clip_loop_original_end(SharedString::default());
    ui.set_clip_loop_enabled(false);
    ui.set_clip_loop_start(SharedString::default());
    ui.set_clip_loop_end(SharedString::default());
}

fn format_frame_clock(frame: usize, sample_rate_hz: u32) -> String {
    format_transport_clock(u64::try_from(frame).unwrap_or(u64::MAX), sample_rate_hz)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite seek percent is bounded before conversion to one-millionth resolution"
)]
fn seek_percent_to_frame(percent: f32, end_frame: usize) -> Result<usize, &'static str> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err("Transport seek position is outside its supported range");
    }
    let millionths = (percent * 10_000.0).round() as u32;
    let scaled = (end_frame as u128)
        .checked_mul(u128::from(millionths))
        .and_then(|value| value.checked_add(500_000))
        .ok_or("Transport seek position exceeds the supported frame domain")?;
    usize::try_from(scaled / 1_000_000)
        .map_err(|_| "Transport seek position exceeds the supported frame domain")
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the computed progress domain is at most one million and exactly representable by f32"
)]
fn frame_progress_percent(position_frame: usize, end_frame: usize) -> f32 {
    if end_frame == 0 {
        return 0.0;
    }
    let millionths =
        u32::try_from((position_frame.min(end_frame) as u128) * 1_000_000 / (end_frame as u128))
            .unwrap_or(1_000_000);
    millionths as f32 / 10_000.0
}

fn apply_recording_event(ui: &MainWindow, state: &hyper_desktop_ide::IdeRecordingState) {
    let observation = state.observation();
    ui.set_app_mode(AppMode::Recording);
    ui.set_project_name(state.project_name().into());
    ui.set_project_id(state.project_id().into());
    ui.set_sample_rate(format!("{} Hz", state.sample_rate_hz()).into());
    ui.set_recording_target(state.track_name().into());
    ui.set_recording_frame_count(observation.accepted_frames().to_string().into());
    ui.set_recording_drop_count(observation.rejected_frames().to_string().into());
    ui.set_transport_state("Recording · playback paused".into());
    ui.set_stream_state(format!("Capture {:?}", observation.input_state()).into());
    ui.set_frame_position(state.start_frame().to_string().into());
    ui.set_transport_clock(
        format_transport_clock(observation.accepted_frames(), state.sample_rate_hz()).into(),
    );
    ui.set_callback_count(observation.input().callback_count.to_string().into());
    ui.set_monitoring_active(true);
    ui.set_monitoring_gain_percent(observation.monitor_gain() * 100.0);
    ui.set_monitoring_muted(observation.monitor_muted());
    ui.set_monitoring_state(format!("{:?}", observation.input_state()).into());
    ui.set_monitoring_frame_count(
        observation
            .monitoring()
            .accepted_frame_count
            .to_string()
            .into(),
    );
    let monitoring_drops = observation
        .input()
        .dropped_frame_count
        .saturating_add(observation.monitoring().overflow_frame_count)
        .saturating_add(observation.monitoring().consumer_closed_frame_count);
    ui.set_monitoring_drop_count(monitoring_drops.to_string().into());
    ui.set_monitoring_underflow_count(observation.monitoring().underflow_count.to_string().into());
    apply_input_meter_observation(ui, &observation.input_meter());
    ui.set_xrun_count(
        observation
            .output()
            .native_xruns
            .map_or_else(
                || "Unavailable".to_owned(),
                |xrun| {
                    xrun.recovery_count.map_or_else(
                        || "Observed, count unavailable".to_owned(),
                        |count| count.to_string(),
                    )
                },
            )
            .into(),
    );
    ui.set_left_meter(0.0);
    ui.set_right_meter(0.0);
    ui.set_left_reading("—".into());
    ui.set_right_reading("—".into());
    reset_output_peak_hold_presentation(ui);
    ui.set_status_text("Capturing a monitored take; project playback remains paused".into());
    ui.set_error_text(SharedString::default());
}

fn apply_scene_performance_capture_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ScenePerformanceCaptureStarted {
            maximum_launch_count,
        } => {
            ui.set_app_mode(AppMode::CapturingScenePerformance);
            ui.set_scene_performance_capture_launch_count(0);
            ui.set_status_text(
                format!(
                    "Scene capture armed: transport start is queued; committed grid launches will be retained (0 of {maximum_launch_count})"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ScenePerformanceCaptureProgress { launch_count } => {
            ui.set_app_mode(AppMode::CapturingScenePerformance);
            ui.set_scene_performance_capture_launch_count(
                i32::try_from(*launch_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Scene capture active: {launch_count} committed grid {} will be saved as one arrangement edit",
                    if *launch_count == 1 { "launch" } else { "launches" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ScenePerformanceCaptureStopping { launch_count } => {
            ui.set_app_mode(AppMode::StoppingScenePerformance);
            ui.set_scene_performance_capture_launch_count(
                i32::try_from(*launch_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Ending scene capture after {launch_count} committed grid {}; waiting for the stopped audio cycle",
                    if *launch_count == 1 { "launch" } else { "launches" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ScenePerformanceCaptureSaving { launch_count } => {
            ui.set_app_mode(AppMode::SavingScenePerformance);
            ui.set_scene_performance_capture_launch_count(
                i32::try_from(*launch_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Saving {launch_count} committed grid {} as one atomic arrangement edit…",
                    if *launch_count == 1 {
                        "launch"
                    } else {
                        "launches"
                    }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ScenePerformanceCaptureSaved {
            revision,
            launch_count,
        } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_scene_performance_capture_launch_count(0);
            ui.set_status_text(
                format!(
                    "Saved {launch_count} committed grid {} in project revision {revision} as one arrangement edit",
                    if *launch_count == 1 { "launch" } else { "launches" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("scene-performance capture dispatcher received another event"),
    }
}

fn apply_mixer_automation_capture_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::MixerAutomationCaptureStarted {
            maximum_point_count,
        } => {
            ui.set_app_mode(AppMode::CapturingMixerAutomation);
            ui.set_mixer_automation_capture_point_count(0);
            ui.set_mixer_automation_capture_maximum_point_count(
                i32::try_from(*maximum_point_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Live mixer automation capture armed: transport start is queued; committed rolling changes will be retained (0 of {maximum_point_count})"
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MixerAutomationCaptureProgress { point_count } => {
            ui.set_app_mode(AppMode::CapturingMixerAutomation);
            ui.set_mixer_automation_capture_point_count(
                i32::try_from(*point_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Live mixer automation capture active: {point_count} committed {} will be saved as one bounded take",
                    if *point_count == 1 { "point" } else { "points" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MixerAutomationCaptureStopping { point_count } => {
            ui.set_app_mode(AppMode::StoppingMixerAutomation);
            ui.set_mixer_automation_capture_point_count(
                i32::try_from(*point_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Ending live mixer automation capture after {point_count} committed {}; waiting for the stopped audio cycle",
                    if *point_count == 1 { "point" } else { "points" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::MixerAutomationCaptureSaving { point_count } => {
            ui.set_app_mode(AppMode::SavingMixerAutomation);
            ui.set_mixer_automation_capture_point_count(
                i32::try_from(*point_count).unwrap_or(i32::MAX),
            );
            ui.set_status_text(
                format!(
                    "Saving {point_count} committed mixer {} as one bounded automation take…",
                    if *point_count == 1 { "point" } else { "points" }
                )
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("mixer-automation capture dispatcher received another event"),
    }
}

fn set_recording_targets(
    ui: &MainWindow,
    recording_routes: &RecordingRoutes,
    channels: &[IdeMixerChannel],
) {
    let previous = usize::try_from(ui.get_selected_recording_target_index())
        .ok()
        .and_then(|index| recording_routes.borrow().get(index).copied());
    let mut routes = Vec::new();
    let mut names = Vec::new();
    for (channel_index, channel) in channels.iter().enumerate() {
        if channel.kind() == IdeMixerChannelKind::Audio {
            routes.push(channel_index);
            names.push(SharedString::from(channel.name()));
        }
    }
    let selected = previous
        .and_then(|previous| routes.iter().position(|route| *route == previous))
        .unwrap_or(0);
    *recording_routes.borrow_mut() = routes;
    ui.set_recording_target_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_selected_recording_target_index(i32::try_from(selected).unwrap_or(-1));
}

fn set_audio_import_targets(
    ui: &MainWindow,
    routes: &AudioImportRoutes,
    channels: &[IdeMixerChannel],
) {
    let previous = usize::try_from(ui.get_selected_audio_import_track_index())
        .ok()
        .and_then(|index| routes.borrow().get(index).cloned());
    let mut next_routes = Vec::new();
    let mut names = Vec::new();
    for channel in channels {
        if channel.kind() == IdeMixerChannelKind::Audio {
            next_routes.push(channel.id().to_owned());
            names.push(SharedString::from(channel.name()));
        }
    }
    let selected = previous
        .as_ref()
        .and_then(|previous| next_routes.iter().position(|route| route == previous))
        .or_else(|| (!next_routes.is_empty()).then_some(0));
    *routes.borrow_mut() = next_routes;
    ui.set_audio_import_track_names(ModelRc::from(Rc::new(VecModel::from(names))));
    ui.set_selected_audio_import_track_index(
        selected
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1),
    );
}

fn apply_mixer_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    set_mixer(
        ui,
        observation.mixer_channels(),
        observation.flat_mixer_strip_reorder_available(),
    );
    let unsaved_changes = observation.mixer_unsaved_changes();
    ui.set_mixer_unsaved_changes(unsaved_changes);
    if !unsaved_changes {
        ui.set_close_unsaved_mixer_confirming(false);
    }
    if let Some(main) = observation.mixer_main() {
        ui.set_mixer_main_state_available(true);
        ui.set_mixer_main_gain_percent(main.gain * 100.0);
        ui.set_mixer_main_muted(main.muted);
    } else {
        ui.set_mixer_main_state_available(false);
        ui.set_mixer_main_gain_percent(0.0);
        ui.set_mixer_main_muted(false);
    }
}

fn set_routing_bus_sources(ui: &MainWindow, channels: &[IdeMixerChannel]) {
    let audio_channels = channels
        .iter()
        .filter(|channel| channel.kind() == IdeMixerChannelKind::Audio)
        .collect::<Vec<_>>();
    let current = ui.get_routing_bus_sources();
    let unchanged = current.row_count() == audio_channels.len()
        && audio_channels.iter().enumerate().all(|(index, channel)| {
            current.row_data(index).is_some_and(|source| {
                source.index == i32::try_from(index).unwrap_or(-1)
                    && source.track_id.as_str() == channel.id()
                    && source.name.as_str() == channel.name()
            })
        });
    if unchanged {
        return;
    }
    let rows = audio_channels
        .iter()
        .enumerate()
        .map(|(index, channel)| RoutingBusSourceRow {
            index: i32::try_from(index).unwrap_or(-1),
            track_id: channel.id().into(),
            name: channel.name().into(),
            selected: false,
        })
        .collect::<Vec<_>>();
    ui.set_routing_bus_selected_source_count(0);
    ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn begin_routing_bus_creation(ui: &MainWindow) {
    let rows = ui
        .get_routing_bus_sources()
        .iter()
        .map(|mut source| {
            source.selected = false;
            source
        })
        .collect::<Vec<_>>();
    ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_routing_bus_selected_source_count(0);
    ui.set_routing_bus_kind_index(0);
    ui.set_routing_bus_name("Group".into());
    ui.set_routing_bus_create_visible(true);
    ui.set_error_text(SharedString::default());
}

fn reset_native_filter_ui(ui: &MainWindow, project_state: &ProjectUiStateCell) {
    ui.set_native_devices(ModelRc::default());
    ui.set_native_filter_track_names(ModelRc::default());
    ui.set_mixer_native_filter_visible(false);
    ui.set_native_filter_add_visible(false);
    ui.set_native_filter_edit_visible(false);
    ui.set_native_filter_edit_index(-1);
    ui.set_native_filter_remove_confirming_index(-1);
    ui.set_native_compressor_add_visible(false);
    ui.set_native_compressor_edit_visible(false);
    ui.set_native_compressor_edit_index(-1);
    project_state.native_filter_tracks.borrow_mut().clear();
    project_state.native_device_observation_generation.set(None);
    clear_native_device_copy_selection(ui, project_state);
}

fn clear_native_device_copy_selection(ui: &MainWindow, project_state: &ProjectUiStateCell) {
    ui.set_native_device_copy_available(false);
    *project_state.native_device_clipboard.borrow_mut() = None;
}

fn observe_native_device_copy_authority(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    playback_session_generation: u64,
) {
    if project_state
        .native_device_observation_generation
        .replace(Some(playback_session_generation))
        != Some(playback_session_generation)
    {
        clear_native_device_copy_selection(ui, project_state);
    }
}

fn set_native_filter_observation(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    observe_native_device_copy_authority(
        ui,
        project_state,
        observation.playback_session_generation(),
    );
    *project_state.native_filter_tracks.borrow_mut() = observation
        .mixer_channels()
        .iter()
        .map(|channel| channel.id().to_owned())
        .collect();
    set_native_devices(
        ui,
        observation.native_devices(),
        observation.mixer_channels(),
    );
}

fn set_native_devices(ui: &MainWindow, devices: &[IdeNativeDevice], channels: &[IdeMixerChannel]) {
    let track_names = channels
        .iter()
        .map(|channel| SharedString::from(channel.name()))
        .collect::<Vec<_>>();
    ui.set_native_filter_track_names(ModelRc::from(Rc::new(VecModel::from(track_names))));
    ui.set_native_devices(ModelRc::from(Rc::new(VecModel::from(native_device_rows(
        devices, channels,
    )))));
}

#[allow(
    clippy::too_many_lines,
    reason = "the filter and compressor cases deliberately populate one complete device-row shape"
)]
fn native_device_rows(
    devices: &[IdeNativeDevice],
    channels: &[IdeMixerChannel],
) -> Vec<NativeDeviceRow> {
    devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let (
                chain_id,
                device_instance_id,
                track_id,
                enabled,
                kind_index,
                kind_name,
                detail,
                mode_index,
                mode_name,
                cutoff_millihertz,
                cutoff_label,
                resonance_millionths,
                resonance_label,
                threshold_millidecibels,
                ratio_millionths,
                attack_microseconds,
                release_microseconds,
                makeup_millidecibels,
            ) = match device {
                IdeNativeDevice::Filter(filter) => (
                    filter.chain_id(),
                    filter.device_instance_id(),
                    filter.track_id(),
                    filter.enabled(),
                    0,
                    "FILTER",
                    format!(
                        "{} · Q {:.3}",
                        format_millihertz(filter.cutoff_millihertz()),
                        f64::from(filter.resonance_millionths()) / 1_000_000.0
                    ),
                    match filter.mode() {
                        NativeFilterMode::LowPass => 0,
                        NativeFilterMode::HighPass => 1,
                    },
                    match filter.mode() {
                        NativeFilterMode::LowPass => "LOW-PASS",
                        NativeFilterMode::HighPass => "HIGH-PASS",
                    },
                    filter.cutoff_millihertz().to_string(),
                    format_millihertz(filter.cutoff_millihertz()),
                    filter.resonance_millionths().to_string(),
                    format!(
                        "Q {:.3}",
                        f64::from(filter.resonance_millionths()) / 1_000_000.0
                    ),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ),
                IdeNativeDevice::Compressor(compressor) => (
                    compressor.chain_id(),
                    compressor.device_instance_id(),
                    compressor.track_id(),
                    compressor.enabled(),
                    1,
                    "COMPRESSOR",
                    format!(
                        "{} dB · {:.3}:1 · {} / {} ms · {} dB",
                        f64::from(compressor.threshold_millidecibels()) / 1_000.0,
                        f64::from(compressor.ratio_millionths()) / 1_000_000.0,
                        f64::from(compressor.attack_microseconds()) / 1_000.0,
                        f64::from(compressor.release_microseconds()) / 1_000.0,
                        f64::from(compressor.makeup_millidecibels()) / 1_000.0
                    ),
                    0,
                    "",
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    compressor.threshold_millidecibels().to_string(),
                    compressor.ratio_millionths().to_string(),
                    compressor.attack_microseconds().to_string(),
                    compressor.release_microseconds().to_string(),
                    compressor.makeup_millidecibels().to_string(),
                ),
            };
            let track_name = channels
                .iter()
                .find(|channel| channel.id() == track_id)
                .map_or(track_id, IdeMixerChannel::name);
            let (can_move_earlier, can_move_later, can_duplicate) = match device {
                IdeNativeDevice::Filter(filter) => (
                    filter.can_move_earlier(),
                    filter.can_move_later(),
                    filter.can_duplicate(),
                ),
                IdeNativeDevice::Compressor(compressor) => (
                    compressor.can_move_earlier(),
                    compressor.can_move_later(),
                    compressor.can_duplicate(),
                ),
            };
            NativeDeviceRow {
                index: i32::try_from(index).unwrap_or(-1),
                chain_id: chain_id.into(),
                device_instance_id: device_instance_id.into(),
                track_id: track_id.into(),
                track_name: track_name.into(),
                enabled,
                can_move_earlier,
                can_move_later,
                can_duplicate,
                kind_index,
                kind_name: kind_name.into(),
                detail: detail.into(),
                mode_index,
                mode_name: mode_name.into(),
                cutoff_millihertz: cutoff_millihertz.into(),
                cutoff_label: cutoff_label.into(),
                resonance_millionths: resonance_millionths.into(),
                resonance_label: resonance_label.into(),
                threshold_millidecibels: threshold_millidecibels.into(),
                ratio_millionths: ratio_millionths.into(),
                attack_microseconds: attack_microseconds.into(),
                release_microseconds: release_microseconds.into(),
                makeup_millidecibels: makeup_millidecibels.into(),
            }
        })
        .collect()
}

fn format_millihertz(value: u32) -> String {
    if value.is_multiple_of(1_000) {
        format!("{} Hz", value / 1_000)
    } else {
        format!("{:.3} Hz", f64::from(value) / 1_000.0)
    }
}

fn apply_native_filter_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::NativeFilterEditing { .. } => {
            ui.set_app_mode(AppMode::EditingNativeFilter);
            ui.set_status_text("Saving the native device chain...".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::NativeFilterEdited {
            revision, changed, ..
        } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_native_filter_add_visible(false);
            ui.set_native_filter_edit_visible(false);
            ui.set_native_filter_edit_index(-1);
            ui.set_native_compressor_add_visible(false);
            ui.set_native_compressor_edit_visible(false);
            ui.set_native_compressor_edit_index(-1);
            ui.set_native_filter_remove_confirming_index(-1);
            ui.set_status_text(
                if *changed {
                    format!("Native device chain saved in project revision {revision}")
                } else {
                    format!("Native device chain already matched project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::NativeFilterEditCommittedWithWarning { warning, .. } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_native_filter_add_visible(false);
            ui.set_native_filter_edit_visible(false);
            ui.set_native_filter_edit_index(-1);
            ui.set_native_compressor_add_visible(false);
            ui.set_native_compressor_edit_visible(false);
            ui.set_native_compressor_edit_index(-1);
            ui.set_native_filter_remove_confirming_index(-1);
            ui.set_status_text("Native device change committed".into());
            ui.set_error_text(
                format!(
                    "The native device change was saved but the project may need attention: {warning}"
                )
                .into(),
            );
        }
        _ => unreachable!("native-filter event dispatcher received another event"),
    }
}

fn apply_output_inventory(
    ui: &MainWindow,
    output_routes: &OutputRoutes,
    inventory: &IdeAudioOutputInventory,
    selection: &IdeOutputSelection,
) {
    let presentation = output_presentation(inventory, selection);
    *output_routes.borrow_mut() = presentation.routes;
    ui.set_output_names(ModelRc::from(Rc::new(VecModel::from(presentation.names))));
    ui.set_selected_output_index(i32::try_from(presentation.selected).unwrap_or(-1));
}

fn output_presentation(
    inventory: &IdeAudioOutputInventory,
    selection: &IdeOutputSelection,
) -> OutputPresentation {
    let mut routes = Vec::with_capacity(inventory.outputs().len() + 1);
    let mut names = Vec::with_capacity(inventory.outputs().len() + 1);
    routes.push(None);
    names.push(SharedString::from("System default"));
    for output in inventory.outputs() {
        routes.push(Some(output.routing_id().to_owned()));
        names.push(output.display_name().into());
    }
    let selected = match selection {
        IdeOutputSelection::SystemDefault => 0,
        IdeOutputSelection::Routed(selected) => routes
            .iter()
            .position(|route| route.as_deref() == Some(selected.as_str()))
            .unwrap_or_else(|| {
                let unavailable = routes.len();
                routes.push(Some(selected.clone()));
                names.push("Unavailable selected output".into());
                unavailable
            }),
    };
    OutputPresentation {
        routes,
        names,
        selected,
    }
}

fn apply_input_inventory(
    ui: &MainWindow,
    input_routes: &InputRoutes,
    inventory: Option<&IdeAudioInputInventory>,
    selection: &IdeInputSelection,
) {
    let presentation = input_presentation(inventory, selection);
    *input_routes.borrow_mut() = presentation.routes;
    ui.set_input_names(ModelRc::from(Rc::new(VecModel::from(presentation.names))));
    ui.set_selected_input_index(i32::try_from(presentation.selected).unwrap_or(-1));
    ui.set_input_discovery_available(inventory.is_some());
}

fn input_presentation(
    inventory: Option<&IdeAudioInputInventory>,
    selection: &IdeInputSelection,
) -> InputPresentation {
    let capacity = inventory.map_or(1, |inventory| inventory.inputs().len() + 2);
    let mut routes = Vec::with_capacity(capacity);
    let mut names = Vec::with_capacity(capacity);
    routes.push(InputRoute::Disabled);
    names.push(SharedString::from("Input cue off"));
    if let Some(inventory) = inventory {
        if !inventory.inputs().is_empty() {
            routes.push(InputRoute::SystemDefault);
            names.push(SharedString::from("System default input"));
        }
        for input in inventory.inputs() {
            routes.push(InputRoute::Routed(input.routing_id().to_owned()));
            names.push(input.display_name().into());
        }
    }
    let selected = match selection {
        IdeInputSelection::Disabled => 0,
        IdeInputSelection::SystemDefault => routes
            .iter()
            .position(|route| matches!(route, InputRoute::SystemDefault))
            .unwrap_or_else(|| {
                let unavailable = routes.len();
                routes.push(InputRoute::SystemDefault);
                names.push("Unavailable system default input".into());
                unavailable
            }),
        IdeInputSelection::Routed(selected) => routes
            .iter()
            .position(|route| matches!(route, InputRoute::Routed(route) if route == selected))
            .unwrap_or_else(|| {
                let unavailable = routes.len();
                routes.push(InputRoute::Routed(selected.clone()));
                names.push("Unavailable selected input".into());
                unavailable
            }),
    };
    InputPresentation {
        routes,
        names,
        selected,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "project closure clears every Rust-owned presentation capability as one transition"
)]
fn reset_project(
    ui: &MainWindow,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    project_state: &ProjectUiStateCell,
) {
    ui.set_rename_project_visible(false);
    ui.set_rename_project_name(SharedString::default());
    ui.set_project_name("No project open".into());
    ui.set_project_id("—".into());
    ui.set_sample_rate("—".into());
    ui.set_scene_count("—".into());
    ui.set_track_count(0);
    reset_scene_creation_form(ui);
    ui.set_new_track_visible(false);
    ui.set_new_track_name("Audio 2".into());
    ui.set_routing_bus_create_visible(false);
    ui.set_routing_bus_kind_index(0);
    ui.set_routing_bus_name("Group".into());
    ui.set_routing_bus_sources(ModelRc::default());
    ui.set_routing_bus_selected_source_count(0);
    reset_audio_track_duplication_form(ui);
    ui.set_rename_track_visible(false);
    ui.set_remove_track_visible(false);
    ui.set_remove_track_id(SharedString::default());
    ui.set_remove_track_name(SharedString::default());
    ui.set_remove_track_content_visible(false);
    ui.set_remove_track_content_id(SharedString::default());
    ui.set_remove_track_content_name(SharedString::default());
    reset_audio_track_freeze_form(ui);
    ui.set_routing_bus_removal_visible(false);
    ui.set_routing_bus_removal_id(SharedString::default());
    ui.set_routing_bus_removal_name(SharedString::default());
    ui.set_rename_track_index(-1);
    ui.set_rename_track_original_name(SharedString::default());
    ui.set_rename_track_name(SharedString::default());
    ui.set_rename_track_kind(SharedString::default());
    ui.set_scenes(ModelRc::default());
    ui.set_scene_names(ModelRc::default());
    reset_scene_rename_form(ui);
    reset_scene_duplication_form(ui);
    reset_scene_removal_form(ui);
    reset_scene_slots_form(ui);
    ui.set_mixer_visible(false);
    ui.set_mixer_sends_visible(false);
    ui.set_mixer_channels(ModelRc::default());
    ui.set_mixer_sends(ModelRc::default());
    ui.set_return_send_candidates(ModelRc::default());
    ui.set_primary_route_candidates(ModelRc::default());
    reset_native_filter_ui(ui, project_state);
    ui.set_mixer_main_state_available(false);
    ui.set_mixer_main_gain_percent(0.0);
    ui.set_mixer_main_muted(false);
    ui.set_mixer_unsaved_changes(false);
    ui.set_close_unsaved_mixer_confirming(false);
    recording_routes.borrow_mut().clear();
    ui.set_recording_target_names(ModelRc::default());
    ui.set_selected_recording_target_index(0);
    ui.set_recording_target("—".into());
    ui.set_recording_frame_count("0".into());
    ui.set_recording_drop_count("0".into());
    take_routes.borrow_mut().clear();
    ui.set_recording_takes(ModelRc::default());
    ui.set_take_clip_visible(false);
    ui.set_take_clip_index(-1);
    ui.set_take_rename_visible(false);
    ui.set_take_rename_index(-1);
    ui.set_take_rename_label(SharedString::default());
    ui.set_take_clip_track_name(SharedString::default());
    ui.set_take_clip_name(SharedString::default());
    ui.set_takes_visible(false);
    reset_arrangement_ui(ui);
    project_state.loop_marks.set(LoopMarks::default());
    project_state.loop_engaged.set(false);
    ui.set_clips_visible(false);
    ui.set_clip_rename_visible(false);
    ui.set_clip_rename_id(SharedString::default());
    ui.set_clip_rename_original(SharedString::default());
    ui.set_clip_rename_name(SharedString::default());
    reset_audio_clip_duplication_form(ui);
    reset_audio_clip_removal_form(ui);
    ui.set_clip_gain_visible(false);
    ui.set_clip_gain_id(SharedString::default());
    ui.set_clip_gain_name(SharedString::default());
    ui.set_clip_gain_original(SharedString::default());
    ui.set_clip_gain_value(SharedString::default());
    reset_audio_clip_fade_form(ui);
    reset_audio_clip_loop_form(ui);
    reset_audio_clip_source_slip_form(ui);
    reset_audio_clip_source_trim_form(ui);
    reset_audio_clip_waveform_dialog(ui);
    ui.set_audio_clips(ModelRc::default());
    ui.set_loudness_visible(false);
    ui.set_loudness_reset_confirming(false);
    ui.set_arrangement_span_clock("—".into());
    ui.set_preview_active(false);
    ui.set_preview_gain_percent(25.0);
    ui.set_preview_muted(false);
    ui.set_preview_state("Idle".into());
    ui.set_preview_progress("—".into());
    ui.set_preview_clip_id(SharedString::default());
    *project_state.playback_end_frame.borrow_mut() = None;
    *project_state.arrangement.borrow_mut() = None;
    project_state.audio_clips.borrow_mut().clear();
    project_state.unused_sample_cleanup.set(None);
    ui.set_unused_sample_assets(0);
    ui.set_unused_sample_bytes("0 bytes".into());
    ui.set_unused_sample_cleanup_confirming(false);
    project_state.scenes.borrow_mut().clear();
    project_state.automation_channels.borrow_mut().clear();
    project_state.automation_targets_initialized.set(false);
    *project_state.automation.borrow_mut() = None;
    project_state.automation_take_ids.borrow_mut().clear();
    project_state.control_targets.borrow_mut().clear();
    project_state.scene_launch_evidence.borrow_mut().clear();
    ui.set_transport_extent_bounded(false);
    ui.set_transport_seek_available(false);
    ui.set_transport_progress_percent(0.0);
    ui.set_transport_end_clock("—".into());
    ui.set_transport_state("Stopped".into());
    ui.set_transport_start_available(false);
    ui.set_transport_stop_available(false);
    ui.set_stream_state("No stream".into());
    ui.set_frame_position("0".into());
    ui.set_transport_clock("00:00:00.000".into());
    ui.set_callback_count("0".into());
    ui.set_xrun_count("Unavailable".into());
    reset_monitoring(ui);
    ui.set_left_meter(0.0);
    ui.set_right_meter(0.0);
    ui.set_left_reading("—".into());
    ui.set_right_reading("—".into());
    reset_output_peak_hold_presentation(ui);
    reset_loudness_observation(ui);
}

fn apply_transport_recovery_availability(ui: &MainWindow, state: PlaybackRecoveryTransportState) {
    ui.set_transport_start_available(state == PlaybackRecoveryTransportState::Stopped);
    ui.set_transport_stop_available(state == PlaybackRecoveryTransportState::Rolling);
}

fn apply_metronome_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    let cycle = observation.transport_cycle();
    ui.set_metronome_available(cycle.metronome_available());
    ui.set_metronome_enabled(cycle.metronome_enabled());
    ui.set_metronome_gain_percent(
        cycle
            .metronome_gain()
            .map_or(0.0, metronome_gain_to_percent),
    );
}

fn reset_arrangement_ui(ui: &MainWindow) {
    ui.set_arrangement_visible(false);
    reset_arrangement_placement_removal_confirmation(ui);
    reset_arrangement_placement_addition_form(ui);
    ui.set_arrangement_placements(ModelRc::default());
    ui.set_arrangement_scene_launches(ModelRc::default());
    ui.set_automation_takes(ModelRc::default());
    ui.set_automation_target_names(ModelRc::default());
    ui.set_automation_target_boolean(ModelRc::default());
    ui.set_automation_target_domain(ModelRc::default());
    ui.set_mixer_automation_visible(false);
    reset_mixer_automation_capture_form(ui);
    reset_automation_creation_form(ui);
    reset_automation_take_edit_form(ui);
    ui.set_control_bindings(ModelRc::default());
    ui.set_control_source_names(ModelRc::default());
    ui.set_control_source_continuous(ModelRc::default());
    ui.set_control_target_names(ModelRc::default());
    ui.set_control_target_continuous(ModelRc::default());
    ui.set_control_target_boolean(ModelRc::default());
    ui.set_control_target_range(ModelRc::default());
    ui.set_mixer_control_visible(false);
    ui.set_control_create_visible(false);
    ui.set_export_available(false);
    ui.set_export_destination(SharedString::default());
    ui.set_export_summary(SharedString::default());
    ui.set_runtime_bundle_destination(SharedString::default());
    ui.set_runtime_bundle_summary(SharedString::default());
    ui.set_undo_label(SharedString::default());
    ui.set_redo_label(SharedString::default());
    ui.set_undo_available(false);
    ui.set_redo_available(false);
    ui.set_loop_summary(SharedString::default());
    ui.set_tempo_bpm(SharedString::default());
    ui.set_tempo_available(false);
    ui.set_tempo_locked_reason(SharedString::default());
    ui.set_meter_signature(SharedString::default());
    ui.set_meter_available(false);
    ui.set_timeline_tempo_markers(ModelRc::default());
    ui.set_timeline_meter_markers(ModelRc::default());
    ui.set_timeline_navigation_markers(ModelRc::default());
    ui.set_timeline_map_visible(false);
    ui.set_timeline_tempo_add_visible(false);
    ui.set_timeline_tempo_edit_visible(false);
    ui.set_timeline_tempo_move_visible(false);
    ui.set_timeline_meter_add_visible(false);
    ui.set_timeline_meter_edit_visible(false);
    ui.set_timeline_meter_move_visible(false);
    reset_timeline_marker_forms(ui);
    ui.set_timeline_map_tick(SharedString::default());
    ui.set_timeline_map_value(SharedString::default());
    ui.set_timeline_map_source_tick(SharedString::default());
    ui.set_loop_marking_available(false);
    ui.set_loop_toggle_available(false);
    ui.set_loop_export_available(false);
    ui.set_loop_engaged(false);
    ui.set_metronome_available(false);
    ui.set_metronome_enabled(false);
    ui.set_metronome_gain_percent(0.0);
    reset_arrangement_scene_launch_addition_form(ui);
    reset_arrangement_scene_launch_move_form(ui);
    reset_arrangement_scene_launch_scene_form(ui);
    reset_arrangement_scene_launch_removal_form(ui);
}

fn reset_monitoring(ui: &MainWindow) {
    ui.set_monitoring_active(false);
    ui.set_monitoring_gain_percent(0.0);
    ui.set_monitoring_muted(false);
    ui.set_monitoring_state("Off".into());
    ui.set_monitoring_frame_count("0".into());
    ui.set_monitoring_drop_count("0".into());
    ui.set_monitoring_underflow_count("0".into());
    reset_input_meter_observation(ui);
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive command-to-pending-mode recovery table stays auditable in one match"
)]
fn mode_after_failure(current: AppMode, command: IdeWorkerCommandKind) -> AppMode {
    match command {
        IdeWorkerCommandKind::Initialize if current == AppMode::Discovering => AppMode::Unavailable,
        IdeWorkerCommandKind::RetryDiscovery if current == AppMode::Unavailable => {
            AppMode::Unavailable
        }
        IdeWorkerCommandKind::CreateProject if current == AppMode::Creating => AppMode::Closed,
        IdeWorkerCommandKind::OpenProject if current == AppMode::Opening => AppMode::Closed,
        IdeWorkerCommandKind::Observe if current == AppMode::Opening => AppMode::Running,
        IdeWorkerCommandKind::CloseProject if current == AppMode::Closing => AppMode::Running,
        IdeWorkerCommandKind::SaveProject
        | IdeWorkerCommandKind::DiscardRecordingTake
        | IdeWorkerCommandKind::RenameRecordingTake
        | IdeWorkerCommandKind::ReorderMixerStrips
        | IdeWorkerCommandKind::AddReturnSend
        | IdeWorkerCommandKind::RemoveReturnSend
        | IdeWorkerCommandKind::SetAudioPrimaryRoute
        | IdeWorkerCommandKind::RemoveRoutingBus
        | IdeWorkerCommandKind::AddScene
        | IdeWorkerCommandKind::SetSceneSlot
        | IdeWorkerCommandKind::SetArrangementLoopRange
        | IdeWorkerCommandKind::SetTempo
        | IdeWorkerCommandKind::SetMeter
        | IdeWorkerCommandKind::EditTimelineMap
        | IdeWorkerCommandKind::EditTimelineMarker
            if current == AppMode::Saving =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddAudioTrack if current == AppMode::AddingTrack => AppMode::Running,
        IdeWorkerCommandKind::AddRoutingBus if current == AppMode::AddingRoutingBus => {
            AppMode::Running
        }
        IdeWorkerCommandKind::DuplicateAudioTrack if current == AppMode::DuplicatingTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RemoveAudioTrack | IdeWorkerCommandKind::RemoveAudioTrackContent
            if current == AppMode::RemovingTrack =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::FreezeAudioTrack if current == AppMode::FreezingTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::UnfreezeAudioTrack if current == AppMode::UnfreezingTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::FlattenAudioTrack if current == AppMode::FlatteningTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RenameAudioTrack | IdeWorkerCommandKind::RenameRoutingBus
            if current == AppMode::RenamingTrack =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioTrackActive
            if current == AppMode::ChangingTrackActivation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::ImportAudio if current == AppMode::ImportingAudio => AppMode::Running,
        IdeWorkerCommandKind::SetArrangementPlacementActive
            if current == AppMode::ChangingArrangementActivation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::MoveArrangementPlacement
            if current == AppMode::MovingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetArrangementPlacementDuration
        | IdeWorkerCommandKind::TrimArrangementPlacementFront
            if current == AppMode::TrimmingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetArrangementPlacementGain
            if current == AppMode::ChangingArrangementPlacementGain =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetArrangementPlacementFades
            if current == AppMode::ChangingArrangementPlacementFades =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade
            if current == AppMode::ChangingArrangementPlacementCrossfade =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement
            if current == AppMode::RemovingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddArrangementPlacement
            if current == AppMode::AddingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddArrangementSceneLaunch
            if current == AppMode::AddingArrangementSceneLaunch =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddAutomationTake if current == AppMode::AddingAutomationTake => {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddAutomationTake | IdeWorkerCommandKind::CaptureMixerAutomation
            if current == AppMode::SavingMixerAutomation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAutomationTakeActive
            if current == AppMode::ChangingAutomationTakeActivation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::EditAutomationTake if current == AppMode::EditingAutomationTake => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RemoveAutomationTake
            if current == AppMode::RemovingAutomationTake =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::EditControlMapping if current == AppMode::EditingControlMapping => {
            AppMode::Running
        }
        IdeWorkerCommandKind::EditNativeFilter if current == AppMode::EditingNativeFilter => {
            AppMode::Running
        }
        IdeWorkerCommandKind::ExportProject | IdeWorkerCommandKind::ExportTrackStems
            if current == AppMode::ExportingProject =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::PrepareRuntime if current == AppMode::PreparingRuntime => {
            AppMode::Running
        }
        IdeWorkerCommandKind::Undo | IdeWorkerCommandKind::Redo
            if current == AppMode::RestoringEdit =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::MoveArrangementSceneLaunch
            if current == AppMode::MovingArrangementSceneLaunch =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene
            if current == AppMode::ChangingArrangementSceneLaunchScene =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch
            if current == AppMode::RemovingArrangementSceneLaunch =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::DuplicateArrangementPlacement
            if current == AppMode::DuplicatingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::SplitArrangementPlacement
            if current == AppMode::SplittingArrangementPlacement =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::RenameAudioClip if current == AppMode::RenamingClip => {
            AppMode::Running
        }
        IdeWorkerCommandKind::DuplicateAudioClip if current == AppMode::DuplicatingClip => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RemoveAudioClip if current == AppMode::RemovingClip => {
            AppMode::Running
        }
        IdeWorkerCommandKind::CleanUnusedSamples if current == AppMode::CleaningSamples => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipActive if current == AppMode::ChangingClipActivation => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipGain if current == AppMode::ChangingClipGain => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipFades if current == AppMode::ChangingClipFades => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipLoop if current == AppMode::ChangingClipLoop => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipReverse if current == AppMode::ChangingClipReverse => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioClipSourceStart if current == AppMode::ChangingClipSource => {
            AppMode::Running
        }
        IdeWorkerCommandKind::TrimAudioClipSource if current == AppMode::TrimmingClipSource => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RenameScene if current == AppMode::RenamingScene => AppMode::Running,
        IdeWorkerCommandKind::RemoveScene if current == AppMode::RemovingScene => AppMode::Running,
        IdeWorkerCommandKind::DuplicateScene if current == AppMode::DuplicatingScene => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetSceneActive if current == AppMode::ChangingSceneActivation => {
            AppMode::Running
        }
        IdeWorkerCommandKind::CreateAudioClipFromTake
        | IdeWorkerCommandKind::CreateAndPlaceAudioClipFromTake
            if current == AppMode::CreatingClip =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::StartRecording if current == AppMode::PreparingRecording => {
            AppMode::Running
        }
        IdeWorkerCommandKind::StopRecording | IdeWorkerCommandKind::AbortRecording
            if current == AppMode::FinalizingRecording =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::CaptureScenePerformance
            if current == AppMode::CapturingScenePerformance =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::CaptureScenePerformance
            if current == AppMode::StoppingScenePerformance =>
        {
            AppMode::CapturingScenePerformance
        }
        IdeWorkerCommandKind::CaptureScenePerformance
            if current == AppMode::SavingScenePerformance =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::CaptureMixerAutomation
            if current == AppMode::CapturingMixerAutomation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::CaptureMixerAutomation
            if current == AppMode::StoppingMixerAutomation =>
        {
            AppMode::CapturingMixerAutomation
        }
        IdeWorkerCommandKind::StartAudioPreview | IdeWorkerCommandKind::StartAudioClipPreview
            if current == AppMode::PreparingPreview =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::StartOutputTest if current == AppMode::StartingOutputTest => {
            AppMode::Closed
        }
        IdeWorkerCommandKind::StopOutputTest if current == AppMode::StoppingOutputTest => {
            AppMode::OutputTesting
        }
        _ => current,
    }
}

fn scene_rows(
    scenes: &[IdeScene],
    launch_state: Option<IdeSceneLaunchState>,
    sample_rate_hz: u32,
) -> Vec<SceneRow> {
    scenes
        .iter()
        .enumerate()
        .map(|(index, scene)| {
            let (availability, launchable) = scene_availability_presentation(scene.availability());
            let (queued_scene_index, queued_launch_frame, current_scene_index) = launch_state
                .map_or((None, None, None), |state| {
                    (
                        state.queued_scene_index(),
                        state.queued_launch_frame(),
                        state.last_launched_scene_index(),
                    )
                });
            let (queued, current, runtime_state) = scene_runtime_presentation(
                index,
                queued_scene_index,
                queued_launch_frame,
                current_scene_index,
                sample_rate_hz,
            );
            SceneRow {
                index: i32::try_from(index).unwrap_or(-1),
                id: scene.id().into(),
                name: scene.name().into(),
                display_name: scene_display_name(scene.name()).into(),
                availability: availability.into(),
                runtime_state: runtime_state.into(),
                active: scene.active(),
                removable: scene.removable(),
                launchable,
                queued,
                current,
            }
        })
        .collect()
}

fn scene_runtime_presentation(
    scene_index: usize,
    queued_scene_index: Option<usize>,
    queued_launch_frame: Option<usize>,
    current_scene_index: Option<usize>,
    sample_rate_hz: u32,
) -> (bool, bool, String) {
    let queued = queued_scene_index == Some(scene_index);
    let current = current_scene_index == Some(scene_index);
    let runtime_state = match (queued, current, queued_launch_frame) {
        (true, true, Some(frame)) => format!(
            "Current · queued for {}",
            format_transport_clock(u64::try_from(frame).unwrap_or(u64::MAX), sample_rate_hz)
        ),
        (true, false, Some(frame)) => format!(
            "Queued for {}",
            format_transport_clock(u64::try_from(frame).unwrap_or(u64::MAX), sample_rate_hz)
        ),
        (false, true, _) => "Current".to_owned(),
        _ => String::new(),
    };
    (queued, current, runtime_state)
}

fn next_scene_name(scenes: &[IdeScene]) -> String {
    (1..=scenes.len().saturating_add(1))
        .map(|index| format!("Scene {index}"))
        .find(|candidate| scenes.iter().all(|scene| scene.name() != candidate))
        .expect("one more candidate than existing scenes guarantees an unused name")
}

const fn scene_capacity_available(scene_count: usize) -> bool {
    scene_count < MAX_IDE_SCENES
}

fn scene_creation_command(
    proposed_name: &str,
    quantization_index: i32,
    quantum_ticks: &str,
) -> Result<IdeWorkerCommand, &'static str> {
    let name = normalize_scene_name(proposed_name)?;
    let quantization = match quantization_index {
        0 => IdeSceneLaunchQuantization::Immediate,
        1 => {
            let quantum_ticks = parse_exact_arrangement_tick(
                quantum_ticks,
                "Scene grid quantum must be a positive whole number of ticks",
                "Scene grid quantum is outside the supported range",
                "Scene grid quantum exceeds the exact project range",
            )?;
            if quantum_ticks == 0 {
                return Err("Scene grid quantum must be greater than zero");
            }
            IdeSceneLaunchQuantization::Grid { quantum_ticks }
        }
        _ => return Err("Scene launch timing selection is invalid"),
    };
    Ok(IdeWorkerCommand::AddScene {
        name: name.to_owned(),
        quantization,
    })
}

fn scene_slot_rows(scene: &IdeScene, clips: &[IdeAudioClip]) -> Vec<SceneSlotRow> {
    scene
        .track_slots()
        .iter()
        .enumerate()
        .map(|(index, slot)| {
            let (action, detail) = match slot.action() {
                IdeSceneTrackAction::Continue => {
                    ("CONTINUE", "Keep the current track state".to_owned())
                }
                IdeSceneTrackAction::Stop => ("STOP", "End launched material".to_owned()),
                IdeSceneTrackAction::Play { clip_id } => (
                    "PLAY",
                    clips.iter().find(|clip| clip.id() == clip_id).map_or_else(
                        || "Referenced clip unavailable".to_owned(),
                        |clip| clip.name().to_owned(),
                    ),
                ),
            };
            SceneSlotRow {
                index: i32::try_from(index).unwrap_or(-1),
                track_name: slot.track_name().into(),
                action: action.into(),
                detail: detail.into(),
            }
        })
        .collect()
}

fn refresh_scene_slots_if_changed(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    project_data_changed: bool,
) {
    if !project_data_changed || !ui.get_scene_slots_visible() {
        return;
    }
    let scene_id = ui.get_scene_slots_scene_id();
    let selected_track_id = ui.get_selected_scene_slot_track_id();
    match open_scene_slots(ui, project_state, scene_id.as_str()) {
        Ok(()) if !selected_track_id.is_empty() => {
            let scenes = project_state.scenes.borrow();
            let selected_index = scenes
                .iter()
                .find(|scene| scene.id() == scene_id.as_str())
                .and_then(|scene| {
                    scene
                        .track_slots()
                        .iter()
                        .position(|slot| slot.track_id() == selected_track_id.as_str())
                })
                .and_then(|index| i32::try_from(index).ok());
            drop(scenes);
            if let Some(index) = selected_index {
                let _ = select_scene_slot(ui, project_state, index);
            }
        }
        Ok(()) => {}
        Err(_) => reset_scene_slots_form(ui),
    }
}

fn open_scene_slots(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    scene_id: &str,
) -> Result<(), &'static str> {
    let scenes = project_state.scenes.borrow();
    let scene = scenes
        .iter()
        .find(|scene| scene.id() == scene_id)
        .ok_or("The selected scene is no longer available")?;
    let clips = project_state.audio_clips.borrow();
    let rows = scene_slot_rows(scene, &clips);
    let has_slots = !rows.is_empty();
    ui.set_scene_slots_scene_id(scene.id().into());
    ui.set_scene_slots_scene_name(scene.name().into());
    ui.set_scene_slots_scene_removable(scene.removable());
    ui.set_scene_slots(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_scene_slots_visible(true);
    drop(clips);
    drop(scenes);
    if has_slots {
        select_scene_slot(ui, project_state, 0)
    } else {
        reset_scene_slot_selection(ui);
        Ok(())
    }
}

fn select_scene_slot(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    index: i32,
) -> Result<(), &'static str> {
    let index = usize::try_from(index).map_err(|_| "Scene track selection is invalid")?;
    let scenes = project_state.scenes.borrow();
    let scene = scenes
        .iter()
        .find(|scene| scene.id() == ui.get_scene_slots_scene_id().as_str())
        .ok_or("The selected scene is no longer available")?;
    let slot = scene
        .track_slots()
        .get(index)
        .ok_or("The selected scene track is no longer available")?;
    let clips = project_state.audio_clips.borrow();
    let track_clips: Vec<&IdeAudioClip> = clips
        .iter()
        .filter(|clip| clip.track_id() == slot.track_id())
        .collect();
    let (action_index, selected_clip_index) = match slot.action() {
        IdeSceneTrackAction::Continue => (0, -1),
        IdeSceneTrackAction::Stop => (1, -1),
        IdeSceneTrackAction::Play { clip_id } => {
            let selected = track_clips
                .iter()
                .position(|clip| clip.id() == clip_id)
                .and_then(|index| i32::try_from(index).ok())
                .ok_or("The scene references a clip that is no longer available")?;
            (2, selected)
        }
    };
    let clip_names: Vec<SharedString> = track_clips
        .iter()
        .map(|clip| SharedString::from(clip.name()))
        .collect();
    ui.set_selected_scene_slot_index(i32::try_from(index).unwrap_or(-1));
    ui.set_selected_scene_slot_track_id(slot.track_id().into());
    ui.set_selected_scene_slot_track_name(slot.track_name().into());
    ui.set_scene_slot_action_index(action_index);
    ui.set_scene_slot_clip_names(ModelRc::from(Rc::new(VecModel::from(clip_names))));
    ui.set_selected_scene_slot_clip_index(selected_clip_index);
    Ok(())
}

fn scene_slot_command(
    project_state: &ProjectUiStateCell,
    scene_id: &str,
    track_id: &str,
    action_index: i32,
    clip_index: i32,
) -> Result<Option<IdeWorkerCommand>, &'static str> {
    let scenes = project_state.scenes.borrow();
    let scene = scenes
        .iter()
        .find(|scene| scene.id() == scene_id)
        .ok_or("The selected scene is no longer available")?;
    let slot = scene
        .track_slots()
        .iter()
        .find(|slot| slot.track_id() == track_id)
        .ok_or("The selected scene track is no longer available")?;
    let clips = project_state.audio_clips.borrow();
    let edit = selected_scene_slot_edit(
        action_index,
        clip_index,
        clips
            .iter()
            .filter(|clip| clip.track_id() == track_id)
            .map(IdeAudioClip::id),
    )?;
    if scene_slot_action_matches(slot.action(), &edit) {
        return Ok(None);
    }
    Ok(Some(IdeWorkerCommand::SetSceneSlot {
        scene_id: scene_id.to_owned(),
        track_id: track_id.to_owned(),
        edit,
    }))
}

fn selected_scene_slot_edit<'a>(
    action_index: i32,
    clip_index: i32,
    mut track_clip_ids: impl Iterator<Item = &'a str>,
) -> Result<IdeSceneSlotEdit, &'static str> {
    match action_index {
        0 => Ok(IdeSceneSlotEdit::Continue),
        1 => Ok(IdeSceneSlotEdit::Stop),
        2 => {
            let clip_index =
                usize::try_from(clip_index).map_err(|_| "Select a reusable clip for the track")?;
            let clip_id = track_clip_ids
                .nth(clip_index)
                .ok_or("The selected reusable clip is no longer available")?;
            Ok(IdeSceneSlotEdit::Play {
                clip_id: clip_id.to_owned(),
            })
        }
        _ => Err("Scene track action selection is invalid"),
    }
}

fn scene_slot_action_matches(current: &IdeSceneTrackAction, edit: &IdeSceneSlotEdit) -> bool {
    matches!(
        (current, edit),
        (IdeSceneTrackAction::Continue, IdeSceneSlotEdit::Continue)
            | (IdeSceneTrackAction::Stop, IdeSceneSlotEdit::Stop)
    ) || matches!(
        (current, edit),
        (
            IdeSceneTrackAction::Play { clip_id: current },
            IdeSceneSlotEdit::Play { clip_id: requested },
        ) if current == requested
    )
}

fn reset_scene_creation_form(ui: &MainWindow) {
    ui.set_scene_create_visible(false);
    ui.set_scene_create_name(SharedString::default());
    ui.set_scene_create_quantization_index(0);
    ui.set_scene_create_quantum_ticks(SharedString::default());
}

fn reset_scene_slot_selection(ui: &MainWindow) {
    ui.set_selected_scene_slot_index(-1);
    ui.set_selected_scene_slot_track_id(SharedString::default());
    ui.set_selected_scene_slot_track_name(SharedString::default());
    ui.set_scene_slot_action_index(0);
    ui.set_scene_slot_clip_names(ModelRc::default());
    ui.set_selected_scene_slot_clip_index(-1);
}

fn reset_scene_slots_form(ui: &MainWindow) {
    ui.set_scene_slots_visible(false);
    ui.set_scene_slots_scene_id(SharedString::default());
    ui.set_scene_slots_scene_name(SharedString::default());
    ui.set_scene_slots_scene_removable(false);
    ui.set_scene_slots(ModelRc::default());
    reset_scene_slot_selection(ui);
}

fn scene_display_name(name: &str) -> String {
    if name.chars().count() <= MAX_VISIBLE_SCENE_NAME_CHARS {
        return name.to_owned();
    }
    let mut display = name
        .chars()
        .take(MAX_VISIBLE_SCENE_NAME_CHARS - 1)
        .collect::<String>();
    display.push('…');
    display
}

fn scene_rename_command(
    scene_id: &str,
    current_name: &str,
    proposed_name: &str,
) -> Result<Option<IdeWorkerCommand>, &'static str> {
    let name = normalize_scene_name(proposed_name)?;
    if name == current_name {
        return Ok(None);
    }
    Ok(Some(IdeWorkerCommand::RenameScene {
        scene_id: scene_id.to_owned(),
        name: name.to_owned(),
    }))
}

fn normalize_scene_name(value: &str) -> Result<&str, &'static str> {
    let name = value.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err("Scene name must contain 1 to 120 characters");
    }
    Ok(name)
}

fn scene_duplication_command(
    source_scene_id: &str,
    proposed_name: &str,
) -> Result<IdeWorkerCommand, &'static str> {
    if source_scene_id.is_empty() {
        return Err("Scene duplication requires a current scene identity");
    }
    let name = normalize_scene_name(proposed_name)?;
    Ok(IdeWorkerCommand::DuplicateScene {
        scene_id: source_scene_id.to_owned(),
        name: name.to_owned(),
    })
}

fn scene_activation_command(
    scene_id: &str,
    current: bool,
    requested: bool,
) -> Result<Option<IdeWorkerCommand>, &'static str> {
    if scene_id.is_empty() {
        return Err("Scene activation requires a current scene identity");
    }
    if current == requested {
        return Ok(None);
    }
    Ok(Some(IdeWorkerCommand::SetSceneActive {
        scene_id: scene_id.to_owned(),
        active: requested,
    }))
}

const fn scene_availability_presentation(
    availability: IdeSceneAvailability,
) -> (&'static str, bool) {
    match availability {
        IdeSceneAvailability::Ready => ("Ready", true),
        IdeSceneAvailability::Disabled => ("Disabled", false),
        IdeSceneAvailability::ConsolidationRequired => ("Consolidation required", false),
        IdeSceneAvailability::NotPrepared => ("Audio unavailable", false),
    }
}

fn validate_scene_launch(
    mode: AppMode,
    index: i32,
    scenes: &ModelRc<SceneRow>,
) -> Result<usize, SceneLaunchFailure> {
    let scene_index = usize::try_from(index).map_err(|_| SceneLaunchFailure::InvalidSelection)?;
    let scene = scenes
        .row_data(scene_index)
        .ok_or(SceneLaunchFailure::StaleSelection)?;
    if scene.index != index {
        return Err(SceneLaunchFailure::StaleSelection);
    }
    if !matches!(mode, AppMode::Running | AppMode::CapturingScenePerformance) || !scene.launchable {
        return Err(SceneLaunchFailure::NotReady);
    }
    Ok(scene_index)
}

fn submit_channel_mixer_command(
    weak: &slint::Weak<MainWindow>,
    worker: &WorkerCell,
    index: i32,
    build: impl FnOnce(usize, &MixerChannelRow) -> Result<IdeMixerCommand, MixerControlFailure>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let (channel_index, row) =
        match validate_mixer_channel(ui.get_app_mode(), index, &ui.get_mixer_channels()) {
            Ok(validated) => validated,
            Err(reason) => {
                ui.set_error_text(reason.message().into());
                return;
            }
        };
    match build(channel_index, &row) {
        Ok(command) => submit_to_ui(&ui, worker, IdeWorkerCommand::Mixer(command)),
        Err(reason) => ui.set_error_text(reason.message().into()),
    }
}

fn submit_send_mixer_command(
    weak: &slint::Weak<MainWindow>,
    worker: &WorkerCell,
    index: i32,
    build: impl FnOnce(&MixerSendRow) -> Result<IdeMixerCommand, MixerControlFailure>,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let row = match validate_mixer_send(ui.get_app_mode(), index, &ui.get_mixer_sends()) {
        Ok(row) => row,
        Err(reason) => {
            ui.set_error_text(reason.message().into());
            return;
        }
    };
    match build(&row) {
        Ok(command) => submit_to_ui(&ui, worker, IdeWorkerCommand::Mixer(command)),
        Err(reason) => ui.set_error_text(reason.message().into()),
    }
}

fn validate_mixer_send(
    mode: AppMode,
    index: i32,
    sends: &ModelRc<MixerSendRow>,
) -> Result<MixerSendRow, MixerControlFailure> {
    let send_index = usize::try_from(index).map_err(|_| MixerControlFailure::InvalidSelection)?;
    let row = sends
        .row_data(send_index)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if row.index != index {
        return Err(MixerControlFailure::StaleSelection);
    }
    if mode != AppMode::Running || !row.state_available {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(row)
}

fn validate_mixer_channel(
    mode: AppMode,
    index: i32,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<(usize, MixerChannelRow), MixerControlFailure> {
    let channel_index =
        usize::try_from(index).map_err(|_| MixerControlFailure::InvalidSelection)?;
    let row = channels
        .row_data(channel_index)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if row.index != index {
        return Err(MixerControlFailure::StaleSelection);
    }
    if mode != AppMode::Running || !row.state_available {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok((channel_index, row))
}

fn mixer_strip_reorder_command(
    mode: AppMode,
    index: i32,
    earlier: bool,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<IdeWorkerCommand, MixerControlFailure> {
    let (current_index, row) = validate_mixer_channel(mode, index, channels)?;
    if (earlier && !row.can_move_earlier) || (!earlier && !row.can_move_later) {
        return Err(MixerControlFailure::StateUnavailable);
    }
    let destination_index = if earlier {
        current_index.checked_sub(1)
    } else {
        current_index.checked_add(1)
    }
    .ok_or(MixerControlFailure::InvalidSelection)?;
    Ok(IdeWorkerCommand::ReorderMixerStrips {
        track_id: row.track_id.to_string(),
        destination_index,
    })
}

fn mixer_strip_duplication_command(
    mode: AppMode,
    index: i32,
    name: &str,
    track_count: i32,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<IdeWorkerCommand, MixerControlFailure> {
    if track_count >= 8 {
        return Err(MixerControlFailure::StateUnavailable);
    }
    let (_, row) = validate_mixer_channel(mode, index, channels)?;
    if !row.renameable {
        return Err(MixerControlFailure::StateUnavailable);
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(MixerControlFailure::InvalidValue);
    }
    Ok(IdeWorkerCommand::DuplicateAudioTrack {
        track_id: row.track_id.to_string(),
        name: name.to_owned(),
    })
}

fn validate_audio_track_removal(
    mode: AppMode,
    track_id: &str,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<String, MixerControlFailure> {
    if track_id.is_empty() {
        return Err(MixerControlFailure::InvalidSelection);
    }
    let row = (0..channels.row_count())
        .filter_map(|index| channels.row_data(index))
        .find(|row| row.track_id.as_str() == track_id)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if mode != AppMode::Running || !row.state_available || !row.removable {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(row.track_id.to_string())
}

fn validate_routing_bus_removal(
    mode: AppMode,
    track_id: &str,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<String, MixerControlFailure> {
    if track_id.is_empty() {
        return Err(MixerControlFailure::InvalidSelection);
    }
    let row = (0..channels.row_count())
        .filter_map(|index| channels.row_data(index))
        .find(|row| row.track_id.as_str() == track_id)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if mode != AppMode::Running
        || !row.state_available
        || !matches!(row.kind.as_str(), "GROUP" | "RETURN")
    {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(row.track_id.to_string())
}

fn routing_bus_rename_command(
    mode: AppMode,
    index: i32,
    name: &str,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<IdeWorkerCommand, MixerControlFailure> {
    let (_, row) = validate_mixer_channel(mode, index, channels)?;
    if !matches!(row.kind.as_str(), "GROUP" | "RETURN") {
        return Err(MixerControlFailure::StateUnavailable);
    }
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(MixerControlFailure::InvalidValue);
    }
    Ok(IdeWorkerCommand::RenameRoutingBus {
        track_id: row.track_id.to_string(),
        name: name.to_owned(),
    })
}

fn validate_audio_track_content_removal(
    mode: AppMode,
    track_id: &str,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<String, MixerControlFailure> {
    if track_id.is_empty() {
        return Err(MixerControlFailure::InvalidSelection);
    }
    let row = (0..channels.row_count())
        .filter_map(|index| channels.row_data(index))
        .find(|row| row.track_id.as_str() == track_id)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if mode != AppMode::Running || !row.state_available || !row.activation_toggleable {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(row.track_id.to_string())
}

fn validate_audio_track_freeze_operation(
    mode: AppMode,
    track_id: &str,
    channels: &ModelRc<MixerChannelRow>,
    expected_frozen: bool,
) -> Result<String, MixerControlFailure> {
    if track_id.is_empty() {
        return Err(MixerControlFailure::InvalidSelection);
    }
    let row = (0..channels.row_count())
        .filter_map(|index| channels.row_data(index))
        .find(|row| row.track_id.as_str() == track_id)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if mode != AppMode::Running
        || !row.state_available
        || !row.activation_toggleable
        || row.kind.as_str() != "AUDIO"
        || row.frozen != expected_frozen
    {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(row.track_id.to_string())
}

fn validate_audio_track_flatten_operation(
    mode: AppMode,
    track_id: &str,
    channels: &ModelRc<MixerChannelRow>,
) -> Result<String, MixerControlFailure> {
    let track_id = validate_audio_track_freeze_operation(mode, track_id, channels, true)?;
    let row = (0..channels.row_count())
        .filter_map(|index| channels.row_data(index))
        .find(|row| row.track_id.as_str() == track_id)
        .ok_or(MixerControlFailure::StaleSelection)?;
    if !row.frozen || !row.flattenable {
        return Err(MixerControlFailure::StateUnavailable);
    }
    Ok(track_id)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite mixer gain percent is range-checked before rounding to its persisted domain"
)]
fn gain_percent_to_millionths(percent: f32) -> Result<u32, MixerControlFailure> {
    if !percent.is_finite() || !(0.0..=200.0).contains(&percent) {
        return Err(MixerControlFailure::InvalidValue);
    }
    Ok((percent * 10_000.0).round() as u32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite input cue percent is range-checked before rounding to the backend domain"
)]
fn monitor_percent_to_millionths(percent: f32) -> Result<u32, &'static str> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err("Input cue gain is outside its supported range");
    }
    Ok((percent * 10_000.0).round() as u32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite audition percent is range-checked before rounding to the backend domain"
)]
fn preview_percent_to_millionths(percent: f32) -> Result<u32, &'static str> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err("Take audition gain is outside its supported range");
    }
    Ok((percent * 10_000.0).round() as u32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the UI accepts only finite values in the metronome's bounded 0..=100 percent domain"
)]
fn metronome_percent_to_gain(percent: f32) -> Result<MetronomeGain, &'static str> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err("Metronome click level is outside its supported range");
    }
    let millionths = (percent * 10_000.0).round() as u32;
    MetronomeGain::from_millionths(millionths)
        .map_err(|_| "Metronome click level is outside its supported range")
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the exact zero-through-unity millionths domain is presented as a bounded percentage"
)]
fn metronome_gain_to_percent(gain: MetronomeGain) -> f32 {
    gain.millionths() as f32 / 10_000.0
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "finite mixer pan percent is range-checked before rounding to its persisted domain"
)]
fn pan_percent_to_millionths(percent: f32) -> Result<i32, MixerControlFailure> {
    if !percent.is_finite() || !(-100.0..=100.0).contains(&percent) {
        return Err(MixerControlFailure::InvalidValue);
    }
    Ok((percent * 10_000.0).round() as i32)
}

fn set_scenes(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    scenes: &[IdeScene],
    playback_session_generation: u64,
    observed_launch_state: Option<IdeSceneLaunchState>,
    sample_rate_hz: u32,
) -> bool {
    let scenes_changed = project_state.scenes.borrow().as_slice() != scenes;
    let launch_state_changed = project_state.scene_launch_evidence.borrow_mut().observe(
        playback_session_generation,
        observed_launch_state,
        scenes_changed,
    );
    if scenes_changed || launch_state_changed {
        ui.set_scene_names(ModelRc::from(Rc::new(VecModel::from(
            scenes
                .iter()
                .map(|scene| SharedString::from(scene.name()))
                .collect::<Vec<_>>(),
        ))));
        ui.set_scenes(ModelRc::from(Rc::new(VecModel::from(scene_rows(
            scenes,
            project_state.scene_launch_evidence.borrow().state,
            sample_rate_hz,
        )))));
        *project_state.scenes.borrow_mut() = scenes.to_vec();
    }
    scenes_changed
}

fn set_observed_scenes(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) -> bool {
    set_scenes(
        ui,
        project_state,
        observation.scenes(),
        observation.playback_session_generation(),
        observation.scene_launch_state(),
        observation.sample_rate_hz(),
    )
}

fn mixer_rows(
    channels: &[IdeMixerChannel],
    flat_mixer_strip_reorder_available: bool,
) -> Vec<MixerChannelRow> {
    channels
        .iter()
        .enumerate()
        .map(|(index, channel)| {
            let state = channel.state();
            MixerChannelRow {
                index: i32::try_from(index).unwrap_or(-1),
                track_id: channel.id().into(),
                name: channel.name().into(),
                display_name: compact_name(channel.name(), MAX_VISIBLE_MIXER_NAME_CHARS).into(),
                kind: mixer_kind_name(channel.kind()).into(),
                frozen: channel.frozen(),
                flattenable: channel.flattenable(),
                state_available: state.is_some(),
                gain_percent: state.map_or(0.0, |state| state.gain * 100.0),
                pan_percent: state.map_or(0.0, |state| state.pan * 100.0),
                muted: state.is_some_and(|state| state.muted),
                solo: state.is_some_and(|state| state.solo),
                meter_left_level: 0.0,
                meter_right_level: 0.0,
                meter_left_reading: "—".into(),
                meter_right_reading: "—".into(),
                solo_available: channel.kind() == IdeMixerChannelKind::Audio,
                renameable: matches!(
                    channel.kind(),
                    IdeMixerChannelKind::Audio
                        | IdeMixerChannelKind::Group
                        | IdeMixerChannelKind::Return
                ),
                removable: channel.kind() == IdeMixerChannelKind::Audio,
                can_move_earlier: flat_mixer_strip_reorder_available && index > 0,
                can_move_later: flat_mixer_strip_reorder_available && index + 1 < channels.len(),
                active: channel.active(),
                activation_toggleable: channel.kind() == IdeMixerChannelKind::Audio,
            }
        })
        .collect()
}

fn mixer_send_rows(channels: &[IdeMixerChannel]) -> Vec<MixerSendRow> {
    channels
        .iter()
        .enumerate()
        .flat_map(|(channel_index, channel)| {
            channel
                .sends()
                .iter()
                .enumerate()
                .map(move |(send_index, send)| {
                    let state = send.state();
                    MixerSendRow {
                        index: -1,
                        channel_index: i32::try_from(channel_index).unwrap_or(-1),
                        send_index: i32::try_from(send_index).unwrap_or(-1),
                        send_id: send.id().into(),
                        source_name: channel.name().into(),
                        target_name: send.target_name().into(),
                        tap: match send.tap() {
                            IdeMixerSendTap::PreFader => "PRE-FADER",
                            IdeMixerSendTap::PostFader => "POST-FADER",
                        }
                        .into(),
                        state_available: state.is_some(),
                        gain_percent: state.map_or(0.0, |state| state.gain * 100.0),
                        pan_percent: state.map_or(0.0, |state| state.pan * 100.0),
                        enabled: state.is_some_and(|state| state.enabled),
                    }
                })
        })
        .enumerate()
        .map(|(index, mut row)| {
            row.index = i32::try_from(index).unwrap_or(-1);
            row
        })
        .collect()
}

fn return_send_candidate_rows(channels: &[IdeMixerChannel]) -> Vec<ReturnSendCandidateRow> {
    channels
        .iter()
        .filter(|source| {
            matches!(
                source.kind(),
                IdeMixerChannelKind::Audio | IdeMixerChannelKind::Group
            ) && source.sends().len() < MAX_IDE_MIXER_SENDS_PER_CHANNEL
        })
        .flat_map(|source| {
            channels
                .iter()
                .filter(move |target| {
                    target.kind() == IdeMixerChannelKind::Return
                        && !source
                            .sends()
                            .iter()
                            .any(|send| send.target_track_id() == target.id())
                })
                .map(move |target| ReturnSendCandidateRow {
                    source_track_id: source.id().into(),
                    source_name: source.name().into(),
                    return_track_id: target.id().into(),
                    return_name: target.name().into(),
                })
        })
        .collect()
}

fn primary_route_candidate_rows(channels: &[IdeMixerChannel]) -> Vec<PrimaryRouteCandidateRow> {
    channels
        .iter()
        .filter(|source| source.kind() == IdeMixerChannelKind::Audio)
        .flat_map(|source| {
            let main = matches!(source.destination(), IdeMixerChannelDestination::Group { .. })
                .then(|| PrimaryRouteCandidateRow {
                    source_track_id: source.id().into(),
                    source_name: source.name().into(),
                    target_group_track_id: SharedString::default(),
                    target_name: "Main".into(),
                    targets_main: true,
                });
            let groups = channels.iter().filter_map(move |target| {
                if target.kind() != IdeMixerChannelKind::Group
                    || matches!(
                        source.destination(),
                        IdeMixerChannelDestination::Group { track_id, .. } if track_id == target.id()
                    )
                {
                    return None;
                }
                Some(PrimaryRouteCandidateRow {
                    source_track_id: source.id().into(),
                    source_name: source.name().into(),
                    target_group_track_id: target.id().into(),
                    target_name: target.name().into(),
                    targets_main: false,
                })
            });
            main.into_iter().chain(groups)
        })
        .collect()
}

const fn mixer_kind_name(kind: IdeMixerChannelKind) -> &'static str {
    match kind {
        IdeMixerChannelKind::Audio => "AUDIO",
        IdeMixerChannelKind::Group => "GROUP",
        IdeMixerChannelKind::Return => "RETURN",
    }
}

const AUTOMATION_UNIT_DOMAIN: &str = "0-2000000 (1000000 = unity)";
const AUTOMATION_PAN_DOMAIN: &str = "-1000000 to 1000000";
const AUTOMATION_SWITCH_DOMAIN: &str = "on / off";

fn automation_main_choices() -> Vec<AutomationTargetChoice> {
    vec![
        AutomationTargetChoice {
            label: "Main gain".to_owned(),
            domain: AUTOMATION_UNIT_DOMAIN.to_owned(),
            boolean: false,
            target: IdeAutomationTarget::Main {
                parameter: AutomationMainParameter::Gain,
            },
            resolved: AutomationTarget::Main {
                parameter: AutomationMainParameter::Gain,
            },
        },
        AutomationTargetChoice {
            label: "Main mute".to_owned(),
            domain: AUTOMATION_SWITCH_DOMAIN.to_owned(),
            boolean: true,
            target: IdeAutomationTarget::Main {
                parameter: AutomationMainParameter::Mute,
            },
            resolved: AutomationTarget::Main {
                parameter: AutomationMainParameter::Mute,
            },
        },
    ]
}

fn automation_channel_choices(
    slot: usize,
    channel: &MixerChannelFacts,
    choices: &mut Vec<AutomationTargetChoice>,
) {
    for (suffix, domain, boolean, parameter) in [
        (
            "gain",
            AUTOMATION_UNIT_DOMAIN,
            false,
            AutomationChannelParameter::Gain,
        ),
        (
            "pan",
            AUTOMATION_PAN_DOMAIN,
            false,
            AutomationChannelParameter::Pan,
        ),
        (
            "mute",
            AUTOMATION_SWITCH_DOMAIN,
            true,
            AutomationChannelParameter::Mute,
        ),
    ] {
        choices.push(AutomationTargetChoice {
            label: format!("{} {suffix}", channel.name),
            domain: domain.to_owned(),
            boolean,
            target: IdeAutomationTarget::Channel {
                track_id: channel.track_id.clone(),
                parameter,
            },
            resolved: AutomationTarget::Channel { slot, parameter },
        });
    }
    if channel.audio {
        choices.push(AutomationTargetChoice {
            label: format!("{} solo", channel.name),
            domain: AUTOMATION_SWITCH_DOMAIN.to_owned(),
            boolean: true,
            target: IdeAutomationTarget::Channel {
                track_id: channel.track_id.clone(),
                parameter: AutomationChannelParameter::Solo,
            },
            resolved: AutomationTarget::Channel {
                slot,
                parameter: AutomationChannelParameter::Solo,
            },
        });
    }
}

fn automation_send_choices(
    slot: usize,
    channel: &MixerChannelFacts,
    choices: &mut Vec<AutomationTargetChoice>,
) {
    for (send_index, send) in channel.sends.iter().enumerate() {
        for (suffix, domain, boolean, parameter) in [
            (
                "send gain",
                AUTOMATION_UNIT_DOMAIN,
                false,
                AutomationSendParameter::Gain,
            ),
            (
                "send pan",
                AUTOMATION_PAN_DOMAIN,
                false,
                AutomationSendParameter::Pan,
            ),
            (
                "send enabled",
                AUTOMATION_SWITCH_DOMAIN,
                true,
                AutomationSendParameter::Enabled,
            ),
        ] {
            choices.push(AutomationTargetChoice {
                label: format!("{} to {} {suffix}", channel.name, send.target_name),
                domain: domain.to_owned(),
                boolean,
                target: IdeAutomationTarget::Send {
                    send_id: send.send_id.clone(),
                    parameter,
                },
                resolved: AutomationTarget::Send {
                    slot,
                    send: send_index,
                    parameter,
                },
            });
        }
    }
}

fn automation_target_choices(channels: &[MixerChannelFacts]) -> Vec<AutomationTargetChoice> {
    let mut choices = automation_main_choices();
    for (slot, channel) in channels.iter().enumerate() {
        automation_channel_choices(slot, channel, &mut choices);
        automation_send_choices(slot, channel, &mut choices);
    }
    choices
}

/// Projects the compiled mixer topology the automation target table depends on.
/// Slot and send positions are the compiled graph's own ordering, so the
/// resolved targets match the runtime addresses published for existing takes.
fn mixer_channel_facts(channels: &[IdeMixerChannel]) -> Vec<MixerChannelFacts> {
    channels
        .iter()
        .map(|channel| MixerChannelFacts {
            track_id: channel.id().to_owned(),
            name: channel.name().to_owned(),
            audio: channel.kind() == IdeMixerChannelKind::Audio,
            sends: channel
                .sends()
                .iter()
                .map(|send| MixerSendFacts {
                    send_id: send.id().to_owned(),
                    target_name: send.target_name().to_owned(),
                })
                .collect(),
        })
        .collect()
}

fn set_automation_targets(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    channels: &[MixerChannelFacts],
) {
    if project_state.automation_targets_initialized.get()
        && project_state.automation_channels.borrow().as_slice() == channels
    {
        return;
    }
    let choices = automation_target_choices(channels);
    ui.set_automation_target_names(ModelRc::from(Rc::new(VecModel::from(
        choices
            .iter()
            .map(|choice| SharedString::from(choice.label.as_str()))
            .collect::<Vec<_>>(),
    ))));
    ui.set_automation_target_boolean(ModelRc::from(Rc::new(VecModel::from(
        choices
            .iter()
            .map(|choice| choice.boolean)
            .collect::<Vec<_>>(),
    ))));
    ui.set_automation_target_domain(ModelRc::from(Rc::new(VecModel::from(
        choices
            .iter()
            .map(|choice| SharedString::from(choice.domain.as_str()))
            .collect::<Vec<_>>(),
    ))));
    let choice_count = i32::try_from(choices.len()).unwrap_or(0);
    if ui.get_automation_create_target_index() >= choice_count {
        ui.set_automation_create_target_index(0);
    }
    if ui.get_automation_capture_target_index() >= choice_count {
        ui.set_automation_capture_target_index(0);
    }
    if ui.get_automation_edit_target_index() >= choice_count {
        ui.set_automation_edit_target_index(0);
    }
    *project_state.automation_channels.borrow_mut() = channels.to_vec();
    project_state.automation_targets_initialized.set(true);
}

struct AutomationTakeListModel {
    owner: Arc<IdePlaybackAutomation>,
    labels: Vec<String>,
}

impl Model for AutomationTakeListModel {
    type Data = AutomationTakeRow;

    fn row_count(&self) -> usize {
        self.owner.takes().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let take = self.owner.takes().get(row)?;
        let lane = take.lanes().first()?;
        let points: usize = take
            .lanes()
            .iter()
            .map(IdePlaybackAutomationLane::point_count)
            .sum();
        let editable = take.lanes().len() == 1;
        let target = if editable {
            self.labels.get(row)?.clone()
        } else {
            format!("{} lanes · view only", take.lanes().len())
        };
        let uniform = take
            .lanes()
            .iter()
            .all(|other| other.interpolation() == lane.interpolation());
        let interpolation = if uniform {
            match lane.interpolation() {
                AutomationInterpolation::Hold => "Hold",
                AutomationInterpolation::Linear => "Linear",
            }
        } else {
            "Mixed"
        };
        Some(AutomationTakeRow {
            id: take.id().into(),
            name: take.name().into(),
            enabled: take.enabled(),
            editable,
            range: format!("TICK {} to {}", take.start_tick(), take.end_tick()).into(),
            target: target.into(),
            interpolation: interpolation.into(),
            points: format!("{points} points").into(),
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &()
    }
}

fn automation_lane_label(target: AutomationTarget, choices: &[AutomationTargetChoice]) -> String {
    choices
        .iter()
        .find(|choice| choice.resolved == target)
        .map_or_else(
            || "Unresolved mixer target".to_owned(),
            |choice| choice.label.clone(),
        )
}

fn set_automation(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    let channels = mixer_channel_facts(observation.mixer_channels());
    set_automation_targets(ui, project_state, &channels);
    let owner = observation.automation();
    replace_automation_take_authority(
        ui,
        project_state,
        Arc::clone(owner),
        owner.takes().iter().map(|take| take.id().to_owned()),
    );
    let choices = automation_target_choices(&channels);
    let labels = owner
        .takes()
        .iter()
        .map(|take| {
            take.lanes().first().map_or_else(
                || "Unresolved mixer target".to_owned(),
                |lane| automation_lane_label(lane.target(), &choices),
            )
        })
        .collect();
    ui.set_automation_takes(ModelRc::new(AutomationTakeListModel {
        owner: Arc::clone(owner),
        labels,
    }));
}

fn replace_automation_take_authority(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    owner: Arc<IdePlaybackAutomation>,
    take_ids: impl IntoIterator<Item = String>,
) {
    let authority_replaced = project_state
        .automation
        .borrow()
        .as_ref()
        .is_none_or(|current| !Arc::ptr_eq(current, &owner));
    ui.set_automation_take_remove_confirming_id(SharedString::default());
    if authority_replaced {
        reset_automation_take_edit_form(ui);
    }
    *project_state.automation.borrow_mut() = Some(owner);
    *project_state.automation_take_ids.borrow_mut() = take_ids.into_iter().collect();
}

fn parse_automation_points(
    value: &str,
    boolean: bool,
) -> Result<Vec<AutomationPoint>, &'static str> {
    let mut points = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (tick, raw) = entry
            .split_once('=')
            .ok_or("Each automation point must be written as tick=value")?;
        let tick = tick
            .trim()
            .parse::<u64>()
            .map_err(|_| "Automation point ticks must be non-negative whole numbers")?;
        if tick > MAX_SAFE_TIMELINE_TICK {
            return Err("Automation point tick exceeds the exact project range");
        }
        let raw = raw.trim();
        let point = if boolean {
            match raw {
                "on" | "true" => AutomationValue::Boolean(true),
                "off" | "false" => AutomationValue::Boolean(false),
                _ => return Err("Switch automation values must be on or off"),
            }
        } else {
            AutomationValue::Integer(
                raw.parse::<i32>()
                    .map_err(|_| "Automation values must be whole millionths")?,
            )
        };
        points.push(AutomationPoint::new(tick, point));
    }
    if points.is_empty() {
        return Err("An automation take needs at least one point");
    }
    Ok(points)
}

/// Builds one authoring request from the exact values the form submitted.
///
/// The target index addresses `choices`, which the caller derives from the same
/// compiled mixer topology that populated the form. Boolean targets are forced
/// to `hold` here because the loader rejects any other interpolation for them.
fn automation_take_request(
    choices: &[AutomationTargetChoice],
    name: &str,
    target_index: i32,
    interpolation_index: i32,
    start: &str,
    end: &str,
    points: &str,
) -> Result<IdeAutomationTakeRequest, &'static str> {
    let choice = usize::try_from(target_index)
        .ok()
        .and_then(|index| choices.get(index))
        .ok_or("The selected automation target is unavailable")?;
    let interpolation = if choice.boolean || interpolation_index == 0 {
        AutomationInterpolation::Hold
    } else {
        AutomationInterpolation::Linear
    };
    let start_tick = parse_automation_tick(start)?;
    let end_tick = parse_automation_tick(end)?;
    if start_tick >= end_tick {
        return Err("The automation end tick must follow its start tick");
    }
    let points = parse_automation_points(points, choice.boolean)?;
    Ok(IdeAutomationTakeRequest {
        name: name.trim().to_owned(),
        start_tick,
        end_tick,
        target: choice.target.clone(),
        interpolation,
        points,
    })
}

/// Builds the only authoring input that live capture is allowed to choose:
/// a normalized take name and one target from the current compiled topology.
fn mixer_automation_capture_request(
    choices: &[AutomationTargetChoice],
    name: &str,
    target_index: i32,
) -> Result<(String, IdeAutomationTarget), &'static str> {
    let target = usize::try_from(target_index)
        .ok()
        .and_then(|index| choices.get(index))
        .ok_or("The selected automation target is unavailable")?
        .target
        .clone();
    let name = name.trim();
    if !(1..=120).contains(&name.chars().count()) {
        return Err("Automation take names must contain 1 through 120 characters");
    }
    Ok((name.to_owned(), target))
}

struct AutomationTakeEditForm {
    take_id: String,
    name: String,
    target_index: i32,
    interpolation_index: i32,
    start: String,
    end: String,
    points: String,
}

fn automation_take_edit_form(
    owner: &IdePlaybackAutomation,
    choices: &[AutomationTargetChoice],
    take_id: &str,
) -> Result<AutomationTakeEditForm, &'static str> {
    let take = owner
        .takes()
        .iter()
        .find(|take| take.id() == take_id)
        .ok_or("The selected automation take is no longer available")?;
    let [lane] = take.lanes() else {
        return Err("Only single-lane automation takes can be edited safely");
    };
    let target_index = choices
        .iter()
        .position(|choice| choice.resolved == lane.target())
        .ok_or("The automation take target is no longer available")?;
    let target_index = i32::try_from(target_index)
        .map_err(|_| "The automation target index is outside the supported range")?;
    let interpolation_index = match lane.interpolation() {
        AutomationInterpolation::Hold => 0,
        AutomationInterpolation::Linear => 1,
    };
    Ok(AutomationTakeEditForm {
        take_id: take.id().to_owned(),
        name: take.name().to_owned(),
        target_index,
        interpolation_index,
        start: take.start_tick().to_string(),
        end: take.end_tick().to_string(),
        points: format_automation_points(lane.points()),
    })
}

fn format_automation_points(points: &[AutomationPoint]) -> String {
    points
        .iter()
        .map(|point| {
            let value = match point.value() {
                AutomationValue::Integer(value) => value.to_string(),
                AutomationValue::Boolean(true) => "on".to_owned(),
                AutomationValue::Boolean(false) => "off".to_owned(),
            };
            format!("{}={value}", point.tick())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn automation_take_form_is_open(ui: &MainWindow) -> bool {
    ui.get_automation_capture_visible()
        || ui.get_automation_create_visible()
        || ui.get_automation_edit_visible()
}

fn automation_take_direct_action_is_current(ui: &MainWindow, take_id: &str) -> bool {
    ui.get_app_mode() == AppMode::Running
        && !automation_take_form_is_open(ui)
        && ui.get_automation_take_remove_confirming_id().is_empty()
        && !take_id.is_empty()
}

fn bind_automation_take_edit_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let project_state_for_begin = Rc::clone(project_state);
    actions.on_begin_edit_automation_take(move |take_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !automation_take_direct_action_is_current(&ui, take_id.as_str()) {
            ui.set_error_text("Automation take edit request is no longer current".into());
            return;
        }
        let owner = project_state_for_begin.automation.borrow().clone();
        let choices =
            automation_target_choices(&project_state_for_begin.automation_channels.borrow());
        let form = owner
            .as_deref()
            .ok_or("Automation take data is no longer available")
            .and_then(|owner| automation_take_edit_form(owner, &choices, take_id.as_str()));
        let form = match form {
            Ok(form) => form,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        ui.set_automation_edit_take_id(form.take_id.into());
        ui.set_automation_edit_name(form.name.into());
        ui.set_automation_edit_target_index(form.target_index);
        ui.set_automation_edit_interpolation_index(form.interpolation_index);
        ui.set_automation_edit_start(form.start.into());
        ui.set_automation_edit_end(form.end.into());
        ui.set_automation_edit_points(form.points.into());
        ui.set_automation_edit_visible(true);
        ui.set_error_text(SharedString::default());
    });

    let weak = ui.as_weak();
    let worker_for_edit = Rc::clone(worker);
    let project_state_for_edit = Rc::clone(project_state);
    actions.on_save_automation_take_edit(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_automation_edit_visible() {
            ui.set_error_text("Automation take edit request is no longer current".into());
            return;
        }
        let take_id = ui.get_automation_edit_take_id().to_string();
        if take_id.is_empty() {
            reset_automation_take_edit_form(&ui);
            ui.set_error_text("Automation take edit request is no longer current".into());
            return;
        }
        let owner = project_state_for_edit.automation.borrow().clone();
        let choices =
            automation_target_choices(&project_state_for_edit.automation_channels.borrow());
        if let Err(message) = owner
            .as_deref()
            .ok_or("Automation take data is no longer available")
            .and_then(|owner| automation_take_edit_form(owner, &choices, &take_id))
        {
            reset_automation_take_edit_form(&ui);
            ui.set_error_text(message.into());
            return;
        }
        let request = match automation_take_request(
            &choices,
            ui.get_automation_edit_name().as_str(),
            ui.get_automation_edit_target_index(),
            ui.get_automation_edit_interpolation_index(),
            ui.get_automation_edit_start().as_str(),
            ui.get_automation_edit_end().as_str(),
            ui.get_automation_edit_points().as_str(),
        ) {
            Ok(request) => request,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        submit_to_ui(
            &ui,
            &worker_for_edit,
            IdeWorkerCommand::EditAutomationTake { take_id, request },
        );
    });
}

fn bind_automation_take_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_take = Rc::clone(worker);
    let project_state_for_take = Rc::clone(project_state);
    actions.on_add_automation_take(
        move |name, target_index, interpolation_index, start, end, points| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running || !ui.get_automation_create_visible() {
                ui.set_error_text("Automation take request is no longer current".into());
                return;
            }
            let choices =
                automation_target_choices(&project_state_for_take.automation_channels.borrow());
            let request = match automation_take_request(
                &choices,
                name.as_str(),
                target_index,
                interpolation_index,
                start.as_str(),
                end.as_str(),
                points.as_str(),
            ) {
                Ok(request) => request,
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
            submit_to_ui(
                &ui,
                &worker_for_take,
                IdeWorkerCommand::AddAutomationTake { request },
            );
        },
    );
}

fn apply_automation_take_activation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AutomationTakeActivationChanged {
            revision,
            changed,
            enabled,
        } => {
            let state = if *enabled { "enabled" } else { "disabled" };
            ui.set_status_text(
                if *changed {
                    format!("Automation take {state} in project revision {revision}")
                } else {
                    format!("Automation take was already {state} in project revision {revision}")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AutomationTakeActivationCommittedWithWarning { warning } => {
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Automation take edit").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AutomationTakeActivationChanging { enabled } => {
            ui.set_app_mode(AppMode::ChangingAutomationTakeActivation);
            ui.set_status_text(
                if *enabled {
                    "Waiting for native output before enabling the automation take…"
                } else {
                    "Waiting for native output before disabling the automation take…"
                }
                .into(),
            );
        }
        _ => unreachable!("automation activation event dispatcher received another event"),
    }
}

fn apply_automation_take_edit_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AutomationTakeEdited {
            revision,
            changed,
            take_id: _,
            name,
        } => {
            reset_automation_take_edit_form(ui);
            ui.set_status_text(
                if *changed {
                    format!("Automation take {name} edited in project revision {revision}")
                } else {
                    format!(
                        "Automation take {name} was already current in project revision {revision}"
                    )
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AutomationTakeEditCommittedWithWarning { warning, .. } => {
            reset_automation_take_edit_form(ui);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Automation take edit").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AutomationTakeEditing { .. } => {
            ui.set_app_mode(AppMode::EditingAutomationTake);
            ui.set_status_text(
                "Waiting for native output before editing the automation take…".into(),
            );
        }
        _ => unreachable!("automation take edit event dispatcher received another event"),
    }
}

fn bind_automation_take_activation_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_activation = Rc::clone(worker);
    actions.on_set_automation_take_active(move |take_id, enabled| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !automation_take_direct_action_is_current(&ui, take_id.as_str()) {
            ui.set_error_text("Automation take edit request is no longer current".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_activation,
            IdeWorkerCommand::SetAutomationTakeActive {
                take_id: take_id.to_string(),
                enabled,
            },
        );
    });
}

fn bind_automation_take_removal_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let worker_for_submitter = Rc::clone(worker);
    bind_automation_take_removal_callback_with(
        ui,
        project_state,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker_for_submitter, command)),
    );
}

fn bind_automation_take_removal_callback_with(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    submitter: UiCommandSubmitter,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let project_state_for_removal = Rc::clone(project_state);
    actions.on_remove_automation_take(move |take_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let current = ui.get_automation_take_remove_confirming_id();
        let authoritative = project_state_for_removal
            .automation_take_ids
            .borrow()
            .iter()
            .any(|id| id == take_id.as_str());
        if ui.get_app_mode() != AppMode::Running
            || automation_take_form_is_open(&ui)
            || take_id.is_empty()
            || current != take_id
            || !authoritative
        {
            ui.set_automation_take_remove_confirming_id(SharedString::default());
            ui.set_error_text("Automation take removal request is no longer current".into());
            return;
        }
        ui.set_automation_take_remove_confirming_id(SharedString::default());
        submitter(
            &ui,
            IdeWorkerCommand::RemoveAutomationTake {
                take_id: take_id.to_string(),
            },
        );
    });
}

fn apply_automation_take_removal_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AutomationTakeRemoved {
            revision,
            take_id: _,
        } => {
            ui.set_automation_take_remove_confirming_id(SharedString::default());
            ui.set_status_text(
                format!("Automation take removed in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AutomationTakeRemovalCommittedWithWarning { warning, .. } => {
            ui.set_automation_take_remove_confirming_id(SharedString::default());
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Automation take removal").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AutomationTakeRemoving { .. } => {
            ui.set_automation_take_remove_confirming_id(SharedString::default());
            ui.set_app_mode(AppMode::RemovingAutomationTake);
            ui.set_status_text(
                "Waiting for native output before removing the automation take…".into(),
            );
        }
        _ => unreachable!("automation removal event dispatcher received another event"),
    }
}

fn parse_automation_tick(value: &str) -> Result<u64, &'static str> {
    parse_exact_arrangement_tick(
        value,
        "Automation ticks must be non-negative whole numbers",
        "Automation tick is outside the supported range",
        "Automation tick exceeds the exact project range",
    )
}

fn reset_automation_creation_form(ui: &MainWindow) {
    ui.set_automation_create_visible(false);
    ui.set_automation_create_name(SharedString::default());
    ui.set_automation_create_points(SharedString::default());
}

fn reset_mixer_automation_capture_form(ui: &MainWindow) {
    ui.set_automation_capture_visible(false);
    ui.set_automation_capture_name(SharedString::default());
    ui.set_automation_capture_target_index(0);
    ui.set_mixer_automation_capture_point_count(0);
    ui.set_mixer_automation_capture_maximum_point_count(0);
}

fn reset_automation_take_edit_form(ui: &MainWindow) {
    ui.set_automation_edit_visible(false);
    ui.set_automation_edit_take_id(SharedString::default());
    ui.set_automation_edit_name(SharedString::default());
    ui.set_automation_edit_target_index(0);
    ui.set_automation_edit_interpolation_index(0);
    ui.set_automation_edit_start(SharedString::default());
    ui.set_automation_edit_end(SharedString::default());
    ui.set_automation_edit_points(SharedString::default());
}

fn apply_automation_take_creation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AutomationTakeAdded {
            revision,
            take_id: _,
            name,
        } => {
            if ui.get_app_mode() == AppMode::SavingMixerAutomation {
                let point_count = ui.get_mixer_automation_capture_point_count();
                reset_mixer_automation_capture_form(ui);
                ui.set_app_mode(AppMode::Running);
                ui.set_status_text(
                    format!(
                        "Captured {point_count} committed mixer {} in project revision {revision}",
                        if point_count == 1 { "point" } else { "points" }
                    )
                    .into(),
                );
            } else {
                reset_automation_creation_form(ui);
                ui.set_status_text(
                    format!("Automation take {name} added in project revision {revision}").into(),
                );
            }
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AutomationTakeCreationCommittedWithWarning { name, warning } => {
            if ui.get_app_mode() == AppMode::SavingMixerAutomation {
                let point_count = ui.get_mixer_automation_capture_point_count();
                reset_mixer_automation_capture_form(ui);
                ui.set_app_mode(AppMode::Running);
                ui.set_status_text(
                    format!(
                        "Captured {point_count} committed mixer {} but project reopening needs attention",
                        if point_count == 1 { "point" } else { "points" }
                    )
                    .into(),
                );
            } else {
                reset_automation_creation_form(ui);
                ui.set_status_text(
                    committed_warning_status(ui.get_app_mode(), &format!("Automation take {name}"))
                        .into(),
                );
            }
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AutomationTakeAdding => {
            if ui.get_app_mode() == AppMode::SavingMixerAutomation {
                ui.set_status_text("Persisting the completed mixer automation capture…".into());
            } else {
                ui.set_app_mode(AppMode::AddingAutomationTake);
                ui.set_status_text(
                    "Waiting for native output before adding the automation take…".into(),
                );
            }
        }
        _ => unreachable!("automation take event dispatcher received another event"),
    }
}

const CONTROL_CV_DOMAIN: &str = "0-1000000";
const CONTROL_PAN_DOMAIN: &str = "0-1000000 (500000 = centre)";

/// Which transform a source admits, fixed by the version 1 binding domain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlSourceKind {
    Continuous,
    Edge,
}

#[derive(Clone, Debug, PartialEq)]
struct ControlSourceChoice {
    label: String,
    kind: ControlSourceKind,
    source: ControlSource,
}

/// The domain a target admits, fixed by `target_domain` in the contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlTargetDomain {
    Continuous,
    Boolean,
    Action,
}

#[derive(Clone, Debug, PartialEq)]
struct ControlTargetChoice {
    label: String,
    domain: ControlTargetDomain,
    range_hint: String,
    target: ControlTarget,
}

fn control_source_choices() -> Vec<ControlSourceChoice> {
    let mut choices = Vec::new();
    for index in 0..4_u8 {
        choices.push(ControlSourceChoice {
            label: format!("CV {}", index + 1),
            kind: ControlSourceKind::Continuous,
            source: ControlSource::CvInput { input_index: index },
        });
    }
    for (label, source) in [
        (
            "Clock rising",
            ControlSource::ClockInput { edge: Edge::Rising },
        ),
        (
            "Clock falling",
            ControlSource::ClockInput {
                edge: Edge::Falling,
            },
        ),
        (
            "Reset rising",
            ControlSource::ResetInput { edge: Edge::Rising },
        ),
        (
            "Reset falling",
            ControlSource::ResetInput {
                edge: Edge::Falling,
            },
        ),
    ] {
        choices.push(ControlSourceChoice {
            label: label.to_owned(),
            kind: ControlSourceKind::Edge,
            source,
        });
    }
    choices
}

fn scene_facts(scenes: &[IdeScene]) -> Vec<SceneFacts> {
    scenes
        .iter()
        .map(|scene| SceneFacts {
            scene_id: scene.id().to_owned(),
            name: scene.name().to_owned(),
        })
        .collect()
}

/// Solo is offered only on audio channels because `is_solo_source_track`
/// resolves to `is_audio_track`, so a solo binding on a group or return is
/// rejected by the compiler as `SoloTargetNotSource`.
fn control_target_choices(
    channels: &[MixerChannelFacts],
    scenes: &[SceneFacts],
) -> Vec<ControlTargetChoice> {
    let mut choices = vec![
        ControlTargetChoice {
            label: "Main gain".to_owned(),
            domain: ControlTargetDomain::Continuous,
            range_hint: CONTROL_CV_DOMAIN.to_owned(),
            target: ControlTarget::MixerMain {
                parameter: IdeControlMainParameter::Gain,
            },
        },
        ControlTargetChoice {
            label: "Main mute".to_owned(),
            domain: ControlTargetDomain::Boolean,
            range_hint: String::new(),
            target: ControlTarget::MixerMain {
                parameter: IdeControlMainParameter::Mute,
            },
        },
    ];
    for channel in channels {
        for (suffix, domain, hint, parameter) in [
            (
                "gain",
                ControlTargetDomain::Continuous,
                CONTROL_CV_DOMAIN,
                IdeControlChannelParameter::Gain,
            ),
            (
                "pan",
                ControlTargetDomain::Continuous,
                CONTROL_PAN_DOMAIN,
                IdeControlChannelParameter::Pan,
            ),
            (
                "mute",
                ControlTargetDomain::Boolean,
                "",
                IdeControlChannelParameter::Mute,
            ),
            (
                "solo",
                ControlTargetDomain::Boolean,
                "",
                IdeControlChannelParameter::Solo,
            ),
        ] {
            if parameter == IdeControlChannelParameter::Solo && !channel.audio {
                continue;
            }
            choices.push(ControlTargetChoice {
                label: format!("{} {suffix}", channel.name),
                domain,
                range_hint: hint.to_owned(),
                target: ControlTarget::MixerChannel {
                    track_id: channel.track_id.clone(),
                    parameter,
                },
            });
        }
    }
    for scene in scenes {
        choices.push(ControlTargetChoice {
            label: format!("Launch scene {}", scene.name),
            domain: ControlTargetDomain::Action,
            range_hint: String::new(),
            target: ControlTarget::Scene {
                scene_id: scene.scene_id.clone(),
                action: SceneAction::Launch,
            },
        });
    }
    for (label, action) in [
        ("Transport play", TransportAction::Play),
        ("Transport stop", TransportAction::Stop),
        ("Transport toggle", TransportAction::Toggle),
        ("Transport reset", TransportAction::Reset),
    ] {
        choices.push(ControlTargetChoice {
            label: label.to_owned(),
            domain: ControlTargetDomain::Action,
            range_hint: String::new(),
            target: ControlTarget::Transport { action },
        });
    }
    choices
}

/// A continuous source drives only continuous targets; an edge source drives
/// only Boolean and action targets. Both rules come from the contract, so a
/// combination the loader would reject is never offered.
const fn control_target_admits(kind: ControlSourceKind, domain: ControlTargetDomain) -> bool {
    match kind {
        ControlSourceKind::Continuous => matches!(domain, ControlTargetDomain::Continuous),
        ControlSourceKind::Edge => matches!(
            domain,
            ControlTargetDomain::Boolean | ControlTargetDomain::Action
        ),
    }
}

fn admitted_control_targets(
    kind: ControlSourceKind,
    targets: &[ControlTargetChoice],
) -> Vec<usize> {
    targets
        .iter()
        .enumerate()
        .filter(|(_, choice)| control_target_admits(kind, choice.domain))
        .map(|(index, _)| index)
        .collect()
}

/// Builds one insertion request from the form. The transform is derived, not
/// chosen: an action target forces `Invoke`, a Boolean target offers only
/// toggle or momentary, and a continuous target keeps the full CV input range.
fn control_binding_request(
    sources: &[ControlSourceChoice],
    targets: &[ControlTargetChoice],
    source_index: i32,
    target_index: i32,
    momentary: bool,
    output_min: &str,
    output_max: &str,
) -> Result<IdeControlBindingRequest, &'static str> {
    let source = usize::try_from(source_index)
        .ok()
        .and_then(|index| sources.get(index))
        .ok_or("The selected control source is unavailable")?;
    let target = usize::try_from(target_index)
        .ok()
        .and_then(|index| targets.get(index))
        .ok_or("The selected control target is unavailable")?;
    let admitted = admitted_control_targets(source.kind, targets);
    let selected =
        usize::try_from(target_index).map_err(|_| "The selected control target is unavailable")?;
    if !admitted.contains(&selected) {
        return Err("That source cannot drive that target");
    }
    let transform = match target.domain {
        ControlTargetDomain::Continuous => {
            let output_min = parse_control_millionths(output_min)?;
            let output_max = parse_control_millionths(output_max)?;
            ControlTransform::Linear {
                input_min: -1_000_000,
                input_max: 1_000_000,
                output_min,
                output_max,
            }
        }
        ControlTargetDomain::Boolean => ControlTransform::Trigger {
            behavior: if momentary {
                TriggerBehavior::Momentary
            } else {
                TriggerBehavior::Toggle
            },
        },
        ControlTargetDomain::Action => ControlTransform::Trigger {
            behavior: TriggerBehavior::Invoke,
        },
    };
    Ok(IdeControlBindingRequest {
        source: source.source.clone(),
        scope: ControlScope::Global,
        transform,
        target: target.target.clone(),
    })
}

fn set_control_mappings(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    let targets = control_target_choices(
        &mixer_channel_facts(observation.mixer_channels()),
        &scene_facts(observation.scenes()),
    );
    if *project_state.control_targets.borrow() != targets {
        let sources = control_source_choices();
        ui.set_control_source_names(ModelRc::from(Rc::new(VecModel::from(
            sources
                .iter()
                .map(|choice| SharedString::from(choice.label.as_str()))
                .collect::<Vec<_>>(),
        ))));
        ui.set_control_source_continuous(ModelRc::from(Rc::new(VecModel::from(
            sources
                .iter()
                .map(|choice| choice.kind == ControlSourceKind::Continuous)
                .collect::<Vec<_>>(),
        ))));
        ui.set_control_target_names(ModelRc::from(Rc::new(VecModel::from(
            targets
                .iter()
                .map(|choice| SharedString::from(choice.label.as_str()))
                .collect::<Vec<_>>(),
        ))));
        ui.set_control_target_continuous(ModelRc::from(Rc::new(VecModel::from(
            targets
                .iter()
                .map(|choice| choice.domain == ControlTargetDomain::Continuous)
                .collect::<Vec<_>>(),
        ))));
        ui.set_control_target_boolean(ModelRc::from(Rc::new(VecModel::from(
            targets
                .iter()
                .map(|choice| choice.domain == ControlTargetDomain::Boolean)
                .collect::<Vec<_>>(),
        ))));
        ui.set_control_target_range(ModelRc::from(Rc::new(VecModel::from(
            targets
                .iter()
                .map(|choice| SharedString::from(choice.range_hint.as_str()))
                .collect::<Vec<_>>(),
        ))));
        if ui.get_control_create_target_index() >= i32::try_from(targets.len()).unwrap_or(0) {
            ui.set_control_create_target_index(0);
        }
        *project_state.control_targets.borrow_mut() = targets;
    }
    ui.set_control_bindings(ModelRc::from(Rc::new(VecModel::from(
        observation
            .control_bindings()
            .iter()
            .map(|binding| ControlBindingRow {
                id: binding.id().into(),
                enabled: binding.enabled(),
                source: binding.source().into(),
                scope: binding.scope().into(),
                target: binding.target().into(),
                transform: binding.transform().into(),
            })
            .collect::<Vec<_>>(),
    ))));
}

fn apply_control_mapping_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ControlMappingEdited { revision, edit } => {
            ui.set_control_create_visible(false);
            ui.set_status_text(
                match edit {
                    IdeControlMappingEdit::Insert(_) => {
                        format!("Control binding added in project revision {revision}")
                    }
                    IdeControlMappingEdit::SetEnabled { enabled: true, .. } => {
                        format!("Control binding enabled in project revision {revision}")
                    }
                    IdeControlMappingEdit::SetEnabled { enabled: false, .. } => {
                        format!("Control binding disabled in project revision {revision}")
                    }
                    IdeControlMappingEdit::Remove { .. } => {
                        format!("Control binding removed in project revision {revision}")
                    }
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ControlMappingEditCommittedWithWarning { warning } => {
            ui.set_control_create_visible(false);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), "Control mapping edit").into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::ControlMappingEditing { .. } => {
            ui.set_app_mode(AppMode::EditingControlMapping);
            ui.set_status_text(
                "Waiting for native output before editing control mappings\u{2026}".into(),
            );
        }
        _ => unreachable!("control mapping event dispatcher received another event"),
    }
}

fn bind_control_mapping_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_add = Rc::clone(worker);
    let project_state_for_add = Rc::clone(project_state);
    actions.on_add_control_binding(
        move |source_index, target_index, momentary, output_min, output_max| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_app_mode() != AppMode::Running || !ui.get_control_create_visible() {
                ui.set_error_text("Control binding request is no longer current".into());
                return;
            }
            let request = match control_binding_request(
                &control_source_choices(),
                &project_state_for_add.control_targets.borrow(),
                source_index,
                target_index,
                momentary,
                output_min.as_str(),
                output_max.as_str(),
            ) {
                Ok(request) => request,
                Err(message) => {
                    ui.set_error_text(message.into());
                    return;
                }
            };
            submit_to_ui(
                &ui,
                &worker_for_add,
                IdeWorkerCommand::EditControlMapping {
                    edit: IdeControlMappingEdit::Insert(request),
                },
            );
        },
    );

    let weak = ui.as_weak();
    let worker_for_active = Rc::clone(worker);
    actions.on_set_control_binding_active(move |binding_id, enabled| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || binding_id.is_empty() {
            ui.set_error_text("Control binding request is no longer current".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_active,
            IdeWorkerCommand::EditControlMapping {
                edit: IdeControlMappingEdit::SetEnabled {
                    binding_id: binding_id.to_string(),
                    enabled,
                },
            },
        );
    });

    let weak = ui.as_weak();
    let worker_for_remove = Rc::clone(worker);
    actions.on_remove_control_binding(move |binding_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || binding_id.is_empty() {
            ui.set_error_text("Control binding request is no longer current".into());
            return;
        }
        submit_to_ui(
            &ui,
            &worker_for_remove,
            IdeWorkerCommand::EditControlMapping {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: binding_id.to_string(),
                },
            },
        );
    });
}

/// Describes one finished export without implying a measurement it does not
/// have. Material shorter than the EBU 400 ms gate has no integrated loudness,
/// and the summary says so rather than showing a placeholder number.
/// Export needs an end and something to render, so only a bounded, non-empty
/// extent can be exported. An unbounded project has no last frame to write.
const fn export_available(extent: IdePlaybackExtent) -> bool {
    matches!(extent, IdePlaybackExtent::Bounded { end_frame } if end_frame > 0)
}

fn export_summary(report: &OfflineExportReport) -> String {
    let seconds = u32::try_from(report.frame_count).map_or(f64::NAN, |frames| {
        f64::from(frames) / f64::from(report.sample_rate_hz)
    });
    if report.sample_peak <= 0.0 {
        return format!("{seconds:.2} s  ·  silent");
    }
    let loudness = report.integrated_lufs.map_or_else(
        || "integrated loudness needs at least 400 ms".to_owned(),
        |lufs| format!("{lufs:.1} LUFS integrated"),
    );
    format!(
        "{seconds:.2} s  ·  peak {:.1} dBFS  ·  true peak {:.1} dBTP  ·  {loudness}",
        linear_to_dbfs(report.sample_peak),
        linear_to_dbfs(report.true_peak),
    )
}

fn linear_to_dbfs(linear: f32) -> f32 {
    if linear > 0.0 {
        20.0 * linear.log10()
    } else {
        f32::NEG_INFINITY
    }
}

fn apply_project_export_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ProjectExporting => {
            ui.set_app_mode(AppMode::ExportingProject);
            ui.set_status_text("Rendering the project through the playback runtime…".into());
        }
        IdeWorkerEvent::ProjectExported { report } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_export_summary(export_summary(report).into());
            ui.set_status_text("Export complete".into());
            ui.set_error_text(SharedString::default());
        }
        _ => unreachable!("project export event dispatcher received another event"),
    }
}

fn track_stem_export_summary(report: &hyper_desktop_ide::OfflineTrackStemExportReport) -> String {
    let seconds = u32::try_from(report.frame_count).map_or(f64::NAN, |frames| {
        f64::from(frames) / f64::from(report.sample_rate_hz)
    });
    let container = match report.format {
        OfflineTrackStemExportFormat::Wav => "WAV",
        OfflineTrackStemExportFormat::Flac => "FLAC",
    };
    format!(
        "{} aligned {container} stems  ·  {seconds:.2} s",
        report.files.len()
    )
}

fn apply_track_stem_export_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::ProjectStemExporting => {
            ui.set_app_mode(AppMode::ExportingProject);
            ui.set_status_text(
                "Rendering aligned mixer stems through the playback runtime…".into(),
            );
        }
        IdeWorkerEvent::ProjectStemsExported { report } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_export_summary(track_stem_export_summary(report).into());
            ui.set_status_text("Mixer stems exported".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ProjectStemsExportedWithWarning { report, warning } => {
            ui.set_app_mode(AppMode::Running);
            ui.set_export_summary(track_stem_export_summary(report).into());
            ui.set_status_text(
                "Mixer stems exported, but filesystem durability needs attention".into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        _ => unreachable!("track-stem export event dispatcher received another event"),
    }
}

fn restoring_status_text(redo: bool, label: &str) -> String {
    if redo {
        format!("Reapplying the undone {label}…")
    } else {
        format!("Undoing the {label}…")
    }
}

fn restored_status_text(redo: bool, revision: u64) -> String {
    if redo {
        format!("Edit reapplied in project revision {revision}")
    } else {
        format!("Edit undone in project revision {revision}")
    }
}

fn restore_warning_text(redo: bool, label: &str, warning: &IdeResourceRestoreError) -> String {
    format!(
        "The {label} was {} but the project may need attention: {warning}",
        if redo { "reapplied" } else { "undone" }
    )
}

/// A restore can be offered only while the project is idle and the matching
/// stack holds an entry. The controls read this, and the callback refuses on
/// the same rule, so the button cannot be a different promise from the action.
fn restore_available(mode: AppMode, label: Option<&str>) -> bool {
    mode == AppMode::Running && label.is_some_and(|label| !label.is_empty())
}

fn apply_undo_state(ui: &MainWindow, undo_label: Option<&str>, redo_label: Option<&str>) {
    ui.set_undo_label(undo_label.unwrap_or_default().into());
    ui.set_redo_label(redo_label.unwrap_or_default().into());
    refresh_restore_availability(ui);
}

fn refresh_restore_availability(ui: &MainWindow) {
    let mode = ui.get_app_mode();
    let undo = ui.get_undo_label();
    let redo = ui.get_redo_label();
    ui.set_undo_available(restore_available(mode, Some(undo.as_str())));
    ui.set_redo_available(restore_available(mode, Some(redo.as_str())));
}

fn apply_restore_progress(ui: &MainWindow, redo: bool, label: &str) {
    ui.set_app_mode(AppMode::RestoringEdit);
    ui.set_status_text(restoring_status_text(redo, label).into());
    refresh_restore_availability(ui);
}

fn apply_restore_result(ui: &MainWindow, status: String, error: String) {
    ui.set_app_mode(AppMode::Running);
    ui.set_status_text(status.into());
    ui.set_error_text(error.into());
    refresh_restore_availability(ui);
}

/// Marking is possible only while the project is idle; engaging the loop also
/// needs both marks to form a usable range. The controls read these, and the
/// callbacks refuse on the same rules.
const fn loop_marking_available(mode: AppMode) -> bool {
    matches!(mode, AppMode::Running)
}

fn loop_toggle_available(mode: AppMode, marks: LoopMarks) -> bool {
    loop_marking_available(mode) && marks.range().is_some()
}

fn offline_export_range_from_frames(
    start_frame: usize,
    end_frame: usize,
) -> Option<OfflineExportRange> {
    OfflineExportRange::new(start_frame, end_frame).ok()
}

fn current_persisted_loop_export_range(state: &ProjectUiStateCell) -> Option<OfflineExportRange> {
    state
        .arrangement
        .borrow()
        .as_ref()
        .and_then(|binding| binding.owner.loop_range())
        .and_then(|range| offline_export_range_from_frames(range.start_frame(), range.end_frame()))
}

/// The marks a reopened project should show. The user's own marks win once they
/// have set any, so reopening seeds but never overwrites.
fn seeded_loop_marks(
    current: LoopMarks,
    persisted: Option<(usize, usize, bool)>,
) -> Option<(LoopMarks, bool)> {
    if current != LoopMarks::default() {
        return None;
    }
    let (start, end, enabled) = persisted?;
    Some((
        LoopMarks {
            start: Some(start),
            end: Some(end),
        },
        enabled,
    ))
}

fn seed_loop_marks(state: &ProjectUiStateCell, observation: &IdeRunningObservation) {
    let persisted = observation
        .arrangement()
        .loop_range()
        .map(|range| (range.start_frame(), range.end_frame(), range.enabled()));
    if let Some((marks, enabled)) = seeded_loop_marks(state.loop_marks.get(), persisted) {
        state.loop_marks.set(marks);
        state.loop_engaged.set(enabled);
    }
}

fn refresh_loop_controls(ui: &MainWindow, state: &ProjectUiStateCell, sample_rate_hz: u32) {
    state.sample_rate_hz.set(sample_rate_hz);
    let marks = state.loop_marks.get();
    let mode = ui.get_app_mode();
    ui.set_loop_summary(marks.summary(sample_rate_hz).into());
    ui.set_loop_marking_available(loop_marking_available(mode));
    ui.set_loop_toggle_available(loop_toggle_available(mode, marks));
    if marks.range().is_none() {
        ui.set_loop_engaged(false);
        state.loop_engaged.set(false);
    } else {
        ui.set_loop_engaged(state.loop_engaged.get());
    }
}

/// The tempo a stored value represents. Shown to at most two decimals because
/// the stored integer is exact and the beats-per-minute figure is derived.
/// Hyper does not time-stretch, so a placed audio clip pins the tempo: changing
/// it would leave the audio no longer fitting its authored duration and the
/// compiler refuses. The control is therefore offered only while the
/// arrangement is still empty, rather than accepting an edit that would fail.
/// The playhead's musical position, or empty when the timeline cannot place it.
/// Shown as bar|beat|tick, the notation both references use.
fn set_playhead(ui: &MainWindow, project_state: &ProjectUiStateCell, position_frame: usize) {
    ui.set_frame_position(position_frame.to_string().into());
    project_state.playhead_frame.set(position_frame);
    ui.set_musical_position(
        format_musical_position(&project_state.timeline.borrow(), position_frame).into(),
    );
}

fn format_musical_position(timeline: &ProjectTimeline, frame: usize) -> String {
    musical_position_at_frame(
        timeline.sample_rate_hz,
        &timeline.tempo_events,
        &timeline.meter_events,
        frame,
    )
    .map_or_else(
        |_| String::new(),
        |position| {
            format!(
                "{}|{}|{:03}",
                position.bar(),
                position.beat(),
                position.tick()
            )
        },
    )
}

/// The two maps the musical readout needs, cached because they change only when
/// the project does while the playhead moves every callback.
#[derive(Clone, Debug, Default)]
struct ProjectTimeline {
    project_id: String,
    sample_rate_hz: u32,
    tempo_events: Vec<TempoEvent>,
    meter_events: Vec<MeterEvent>,
    navigation_markers: Vec<IdeTimelineMarker>,
}

/// A typed time signature becomes a command only when the contract accepts it.
/// `4/4` and `6/8` are the shapes users type; anything else is reported.
fn meter_command(typed: &str) -> Result<IdeWorkerCommand, String> {
    let trimmed = typed.trim();
    let (numerator, denominator) = trimmed
        .split_once('/')
        .ok_or_else(|| format!("\"{trimmed}\" is not a time signature"))?;
    let numerator: u16 = numerator
        .trim()
        .parse()
        .map_err(|_| format!("\"{trimmed}\" is not a time signature"))?;
    let denominator: u16 = denominator
        .trim()
        .parse()
        .map_err(|_| format!("\"{trimmed}\" is not a time signature"))?;
    if meter_is_in_version_one_domain(numerator, denominator) {
        Ok(IdeWorkerCommand::SetMeter {
            numerator,
            denominator,
        })
    } else {
        Err(format!(
            "{trimmed} is not a time signature this project can hold"
        ))
    }
}

fn bind_meter_callback(ui: &MainWindow, worker: &WorkerCell) {
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    ui.global::<IdeActions>().on_set_meter(move |typed| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        match meter_command(typed.as_str()) {
            Ok(command) => submit_to_ui(&ui, &worker, command),
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn timeline_tick(typed: &str) -> Result<u64, String> {
    let trimmed = typed.trim();
    let tick: u64 = trimmed
        .parse()
        .map_err(|_| format!("\"{trimmed}\" is not a project tick"))?;
    if tick <= MAX_SAFE_TIMELINE_TICK {
        Ok(tick)
    } else {
        Err(format!(
            "{trimmed} is outside this project's timeline domain"
        ))
    }
}

fn timeline_meter(typed: &str) -> Result<(u16, u16), String> {
    match meter_command(typed)? {
        IdeWorkerCommand::SetMeter {
            numerator,
            denominator,
        } => Ok((numerator, denominator)),
        _ => unreachable!("meter_command constructs only a meter command"),
    }
}

fn timeline_tempo(typed: &str) -> Result<u32, String> {
    match tempo_command(typed)? {
        IdeWorkerCommand::SetTempo {
            microseconds_per_quarter,
        } => Ok(microseconds_per_quarter),
        _ => unreachable!("tempo_command constructs only a tempo command"),
    }
}

fn submit_timeline_map_edit(
    ui: &MainWindow,
    worker: &WorkerCell,
    edit: Result<TimelineMapEdit, String>,
) {
    match edit {
        Ok(edit) => submit_to_ui(ui, worker, IdeWorkerCommand::EditTimelineMap { edit }),
        Err(message) => ui.set_error_text(message.into()),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the tempo and meter edit callbacks form one explicit timeline-map registration table"
)]
fn bind_timeline_map_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let add_tempo_worker = Rc::clone(&worker);
    let update_tempo_worker = Rc::clone(&worker);
    let move_tempo_worker = Rc::clone(&worker);
    let remove_tempo_worker = Rc::clone(&worker);
    let add_meter_worker = Rc::clone(&worker);
    let update_meter_worker = Rc::clone(&worker);
    let move_meter_worker = Rc::clone(&worker);
    let remove_meter_worker = Rc::clone(&worker);
    actions.on_add_timeline_tempo(move |tick, bpm| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &add_tempo_worker,
            timeline_tick(tick.as_str()).and_then(|tick| {
                timeline_tempo(bpm.as_str()).map(|microseconds_per_quarter| {
                    TimelineMapEdit::AddTempo {
                        tick,
                        microseconds_per_quarter,
                    }
                })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_update_timeline_tempo(move |tick, bpm| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &update_tempo_worker,
            timeline_tick(tick.as_str()).and_then(|tick| {
                timeline_tempo(bpm.as_str()).map(|microseconds_per_quarter| {
                    TimelineMapEdit::UpdateTempo {
                        tick,
                        microseconds_per_quarter,
                    }
                })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_move_timeline_tempo(move |from_tick, to_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &move_tempo_worker,
            timeline_tick(from_tick.as_str()).and_then(|from_tick| {
                timeline_tick(to_tick.as_str())
                    .map(|to_tick| TimelineMapEdit::MoveTempo { from_tick, to_tick })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_remove_timeline_tempo(move |tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &remove_tempo_worker,
            timeline_tick(tick.as_str()).map(|tick| TimelineMapEdit::RemoveTempo { tick }),
        );
    });
    let weak = ui.as_weak();
    actions.on_add_timeline_meter(move |tick, signature| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &add_meter_worker,
            timeline_tick(tick.as_str()).and_then(|tick| {
                timeline_meter(signature.as_str()).map(|(numerator, denominator)| {
                    TimelineMapEdit::AddMeter {
                        tick,
                        numerator,
                        denominator,
                    }
                })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_update_timeline_meter(move |tick, signature| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &update_meter_worker,
            timeline_tick(tick.as_str()).and_then(|tick| {
                timeline_meter(signature.as_str()).map(|(numerator, denominator)| {
                    TimelineMapEdit::UpdateMeter {
                        tick,
                        numerator,
                        denominator,
                    }
                })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_move_timeline_meter(move |from_tick, to_tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &move_meter_worker,
            timeline_tick(from_tick.as_str()).and_then(|from_tick| {
                timeline_tick(to_tick.as_str())
                    .map(|to_tick| TimelineMapEdit::MoveMeter { from_tick, to_tick })
            }),
        );
    });
    let weak = ui.as_weak();
    actions.on_remove_timeline_meter(move |tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        submit_timeline_map_edit(
            &ui,
            &remove_meter_worker,
            timeline_tick(tick.as_str()).map(|tick| TimelineMapEdit::RemoveMeter { tick }),
        );
    });
}

fn normalize_timeline_marker_name(value: &str) -> Result<&str, &'static str> {
    let name = value.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err("Navigation marker name must contain 1 to 120 characters");
    }
    Ok(name)
}

fn timeline_marker_form_is_current(
    app_mode: AppMode,
    visible: bool,
    selected_marker_id: &str,
    requested_marker_id: &str,
) -> bool {
    app_mode == AppMode::Running
        && visible
        && !selected_marker_id.is_empty()
        && selected_marker_id == requested_marker_id
}

fn timeline_marker_locate_command(
    app_mode: AppMode,
    project_id: &str,
    index: i32,
    requested_marker_id: &str,
    current_marker_id: Option<&str>,
    play: bool,
) -> Result<IdeWorkerCommand, &'static str> {
    if app_mode != AppMode::Running || project_id.is_empty() {
        return Err("Navigation-marker locate is unavailable in the current state");
    }
    let index = usize::try_from(index)
        .map_err(|_| "The selected navigation marker is no longer available")?;
    if current_marker_id != Some(requested_marker_id) {
        return Err("The selected navigation marker is no longer available");
    }
    Ok(IdeWorkerCommand::LocateTimelineMarker {
        project_id: project_id.to_owned(),
        index,
        marker_id: requested_marker_id.to_owned(),
        state: if play {
            LocateState::Rolling
        } else {
            LocateState::Preserve
        },
    })
}

fn bind_timeline_marker_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    bind_timeline_marker_addition_callback(ui, worker);
    bind_timeline_marker_rename_callback(ui, worker, project_state);
    bind_timeline_marker_move_callback(ui, worker, project_state);
    bind_timeline_marker_removal_callback(ui, worker, project_state);
    bind_timeline_marker_locate_callback(ui, worker, project_state);
}

fn bind_timeline_marker_addition_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_addition = Rc::clone(worker);
    actions.on_add_timeline_marker(move |tick, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_timeline_marker_add_visible() {
            ui.set_error_text("Navigation-marker request is no longer current".into());
            return;
        }
        let tick = match timeline_tick(tick.as_str()) {
            Ok(tick) => tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let name = match normalize_timeline_marker_name(name.as_str()) {
            Ok(name) => name.to_owned(),
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        submit_to_ui(
            &ui,
            &worker_for_addition,
            IdeWorkerCommand::EditTimelineMarker {
                edit: IdeTimelineMarkerEdit::Add { tick, name },
            },
        );
    });
}

fn bind_timeline_marker_rename_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_rename = Rc::clone(worker);
    let timeline_for_rename = Rc::clone(project_state);
    actions.on_rename_timeline_marker(move |marker_id, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !timeline_marker_form_is_current(
            ui.get_app_mode(),
            ui.get_timeline_marker_rename_visible(),
            ui.get_timeline_marker_id().as_str(),
            marker_id.as_str(),
        ) {
            ui.set_error_text("Navigation-marker rename request is no longer current".into());
            return;
        }
        let Ok(source_tick) = timeline_tick(ui.get_timeline_marker_source_tick().as_str()) else {
            ui.set_error_text("Navigation-marker rename request is no longer current".into());
            return;
        };
        let name = match normalize_timeline_marker_name(name.as_str()) {
            Ok(name) => name.to_owned(),
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let timeline = timeline_for_rename.timeline.borrow();
        let Some(marker) = timeline
            .navigation_markers
            .iter()
            .find(|marker| marker.id() == marker_id.as_str())
        else {
            ui.set_error_text("The selected navigation marker is no longer available".into());
            return;
        };
        if marker.tick() != source_tick {
            ui.set_error_text(
                "The selected navigation marker has changed since it was opened".into(),
            );
            return;
        }
        if marker.name() == name {
            ui.set_status_text("Navigation marker already uses that name".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        drop(timeline);
        submit_to_ui(
            &ui,
            &worker_for_rename,
            IdeWorkerCommand::EditTimelineMarker {
                edit: IdeTimelineMarkerEdit::Rename {
                    marker_id: marker_id.to_string(),
                    name,
                },
            },
        );
    });
}

fn bind_timeline_marker_move_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_move = Rc::clone(worker);
    let timeline_for_move = Rc::clone(project_state);
    actions.on_move_timeline_marker(move |marker_id, tick| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !timeline_marker_form_is_current(
            ui.get_app_mode(),
            ui.get_timeline_marker_move_visible(),
            ui.get_timeline_marker_id().as_str(),
            marker_id.as_str(),
        ) {
            ui.set_error_text("Navigation-marker move request is no longer current".into());
            return;
        }
        let Ok(source_tick) = timeline_tick(ui.get_timeline_marker_source_tick().as_str()) else {
            ui.set_error_text("Navigation-marker move request is no longer current".into());
            return;
        };
        let tick = match timeline_tick(tick.as_str()) {
            Ok(tick) => tick,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        let timeline = timeline_for_move.timeline.borrow();
        let Some(marker) = timeline
            .navigation_markers
            .iter()
            .find(|marker| marker.id() == marker_id.as_str())
        else {
            ui.set_error_text("The selected navigation marker is no longer available".into());
            return;
        };
        if marker.tick() != source_tick {
            ui.set_error_text(
                "The selected navigation marker has changed since it was opened".into(),
            );
            return;
        }
        if marker.tick() == tick {
            ui.set_status_text("Navigation marker already uses that project tick".into());
            ui.set_error_text(SharedString::default());
            return;
        }
        drop(timeline);
        submit_to_ui(
            &ui,
            &worker_for_move,
            IdeWorkerCommand::EditTimelineMarker {
                edit: IdeTimelineMarkerEdit::Move {
                    marker_id: marker_id.to_string(),
                    tick,
                },
            },
        );
    });
}

fn bind_timeline_marker_removal_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_removal = Rc::clone(worker);
    let timeline_for_removal = Rc::clone(project_state);
    actions.on_remove_timeline_marker(move |marker_id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running
            || ui.get_timeline_marker_remove_confirming_id().as_str() != marker_id.as_str()
        {
            ui.set_error_text("Navigation-marker removal confirmation is no longer current".into());
            return;
        }
        let timeline = timeline_for_removal.timeline.borrow();
        if !timeline
            .navigation_markers
            .iter()
            .any(|marker| marker.id() == marker_id.as_str())
        {
            ui.set_error_text("The selected navigation marker is no longer available".into());
            return;
        }
        drop(timeline);
        submit_to_ui(
            &ui,
            &worker_for_removal,
            IdeWorkerCommand::EditTimelineMarker {
                edit: IdeTimelineMarkerEdit::Remove {
                    marker_id: marker_id.to_string(),
                },
            },
        );
    });
}

fn bind_timeline_marker_locate_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_locate = Rc::clone(worker);
    let timeline_for_locate = Rc::clone(project_state);
    actions.on_locate_timeline_marker(move |index, marker_id, play| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let timeline = timeline_for_locate.timeline.borrow();
        let current_marker_id = usize::try_from(index)
            .ok()
            .and_then(|index| timeline.navigation_markers.get(index))
            .map(IdeTimelineMarker::id);
        let command = match timeline_marker_locate_command(
            ui.get_app_mode(),
            timeline.project_id.as_str(),
            index,
            marker_id.as_str(),
            current_marker_id,
            play,
        ) {
            Ok(command) => command,
            Err(message) => {
                ui.set_error_text(message.into());
                return;
            }
        };
        drop(timeline);
        submit_to_ui(&ui, &worker_for_locate, command);
    });
}

fn reset_timeline_marker_forms(ui: &MainWindow) {
    ui.set_timeline_marker_add_visible(false);
    ui.set_timeline_marker_rename_visible(false);
    ui.set_timeline_marker_move_visible(false);
    ui.set_timeline_marker_id(SharedString::default());
    ui.set_timeline_marker_source_tick(SharedString::default());
    ui.set_timeline_marker_tick(SharedString::default());
    ui.set_timeline_marker_name(SharedString::default());
    ui.set_timeline_marker_remove_confirming_id(SharedString::default());
}

/// Saving the project writes the loop range alongside the mixer, because the
/// loop plan publishes both in one transaction. Marking a loop point is session
/// state and must never close the audio device on its own.
fn project_save_command(state: &ProjectUiStateCell) -> IdeWorkerCommand {
    let marks = state.loop_marks.get();
    if marks == LoopMarks::default() {
        IdeWorkerCommand::SaveProject
    } else {
        IdeWorkerCommand::SetArrangementLoopRange {
            range: marks.persisted(state.loop_engaged.get()),
        }
    }
}

/// A time signature reads numerator over denominator, in that order.
fn format_meter(numerator: u16, denominator: u16) -> String {
    format!("{numerator}/{denominator}")
}

/// Hyper does not time-stretch, so any audio bound to the timeline pins the
/// tempo. Placements are the obvious case; scene slots that play a clip are the
/// less obvious one, and they fail differently — a scene launch marker makes the
/// edit fail outright, while an interactive scene clip lets the edit succeed and
/// silently marks the clip unavailable. Both are refused here rather than
/// offering a control that breaks the project.
fn tempo_editable(placement_count: usize, scene_clip_count: usize) -> bool {
    placement_count == 0 && scene_clip_count == 0
}

/// How many scene slots play a clip, across every scene.
fn scene_clip_count(scenes: &[IdeScene]) -> usize {
    scenes
        .iter()
        .flat_map(IdeScene::track_slots)
        .filter(|slot| matches!(slot.action(), IdeSceneTrackAction::Play { .. }))
        .count()
}

fn set_tempo_and_loop(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    *project_state.timeline.borrow_mut() = ProjectTimeline {
        project_id: observation.project_id().to_owned(),
        sample_rate_hz: observation.sample_rate_hz(),
        tempo_events: observation.tempo_events().to_vec(),
        meter_events: observation.meter_events().to_vec(),
        navigation_markers: observation.timeline_markers().to_vec(),
    };
    let timeline = project_state.timeline.borrow();
    ui.set_timeline_tempo_markers(ModelRc::from(Rc::new(VecModel::from(
        timeline_tempo_marker_rows(&timeline),
    ))));
    ui.set_timeline_meter_markers(ModelRc::from(Rc::new(VecModel::from(
        timeline_meter_marker_rows(&timeline),
    ))));
    ui.set_timeline_navigation_markers(ModelRc::from(Rc::new(VecModel::from(
        timeline_navigation_marker_rows(&timeline),
    ))));
    drop(timeline);
    let editable = tempo_editable(
        observation.arrangement().placements().len(),
        scene_clip_count(observation.scenes()),
    );
    ui.set_tempo_bpm(format_tempo_bpm(observation.microseconds_per_quarter()).into());
    ui.set_tempo_available(editable);
    ui.set_meter_signature(
        observation
            .meter_events()
            .first()
            .map_or_else(String::new, |meter| {
                format_meter(meter.numerator, meter.denominator)
            })
            .into(),
    );
    ui.set_meter_available(true);
    ui.set_tempo_locked_reason(if editable {
        SharedString::default()
    } else {
        "TEMPO IS FIXED WHILE AUDIO IS PLACED OR IN A SCENE".into()
    });
    seed_loop_marks(project_state, observation);
    refresh_loop_controls(ui, project_state, observation.sample_rate_hz());
}

fn timeline_marker_position(meter_events: &[MeterEvent], tick: u64) -> SharedString {
    musical_position(meter_events, tick).map_or_else(
        |_| SharedString::default(),
        |position| {
            format!(
                "{}|{}|{:03}",
                position.bar(),
                position.beat(),
                position.tick()
            )
            .into()
        },
    )
}

fn timeline_tempo_marker_rows(timeline: &ProjectTimeline) -> Vec<TimelineTempoMarkerRow> {
    timeline
        .tempo_events
        .iter()
        .map(|event| TimelineTempoMarkerRow {
            tick: event.tick.to_string().into(),
            position: timeline_marker_position(&timeline.meter_events, event.tick),
            bpm: format_tempo_bpm(event.microseconds_per_quarter).into(),
            origin: event.tick == 0,
        })
        .collect()
}

fn timeline_meter_marker_rows(timeline: &ProjectTimeline) -> Vec<TimelineMeterMarkerRow> {
    timeline
        .meter_events
        .iter()
        .map(|event| TimelineMeterMarkerRow {
            tick: event.tick.to_string().into(),
            position: timeline_marker_position(&timeline.meter_events, event.tick),
            signature: format_meter(event.numerator, event.denominator).into(),
            origin: event.tick == 0,
        })
        .collect()
}

fn timeline_navigation_marker_rows(timeline: &ProjectTimeline) -> Vec<TimelineNavigationMarkerRow> {
    timeline
        .navigation_markers
        .iter()
        .enumerate()
        .map(|(index, marker)| TimelineNavigationMarkerRow {
            index: i32::try_from(index).unwrap_or(-1),
            id: marker.id().into(),
            tick: marker.tick().to_string().into(),
            position: timeline_marker_position(&timeline.meter_events, marker.tick()),
            name: marker.name().into(),
        })
        .collect()
}

fn format_tempo_bpm(microseconds_per_quarter: u32) -> String {
    format!(
        "{:.2}",
        bpm_for_microseconds_per_quarter(microseconds_per_quarter)
    )
    .trim_end_matches('0')
    .trim_end_matches('.')
    .to_owned()
}

/// A typed tempo becomes a command only when it converts to a value the
/// contract accepts. Anything else is reported rather than clamped.
fn tempo_command(typed: &str) -> Result<IdeWorkerCommand, String> {
    let trimmed = typed.trim();
    let bpm: f64 = trimmed
        .parse()
        .map_err(|_| format!("\"{trimmed}\" is not a tempo"))?;
    microseconds_per_quarter_for_bpm(bpm)
        .map(|microseconds_per_quarter| IdeWorkerCommand::SetTempo {
            microseconds_per_quarter,
        })
        .map_err(|source| source.to_string())
}

fn bind_tempo_callback(ui: &MainWindow, worker: &WorkerCell) {
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    ui.global::<IdeActions>().on_set_tempo(move |typed| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        match tempo_command(typed.as_str()) {
            Ok(command) => submit_to_ui(&ui, &worker, command),
            Err(message) => ui.set_error_text(message.into()),
        }
    });
}

fn bind_loop_callbacks(ui: &MainWindow, worker: &WorkerCell, state: &ProjectUiStateCell) {
    let actions = ui.global::<IdeActions>();
    let mark = |end: bool| {
        let weak = ui.as_weak();
        let worker = Rc::clone(worker);
        let state = Rc::clone(state);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if !loop_marking_available(ui.get_app_mode()) {
                return;
            }
            let frame = state.playhead_frame.get();
            let mut marks = state.loop_marks.get();
            if end {
                marks.end = Some(frame);
            } else {
                marks.start = Some(frame);
            }
            state.loop_marks.set(marks);
            ui.set_loop_summary(marks.summary(state.sample_rate_hz.get()).into());
            ui.set_loop_toggle_available(loop_toggle_available(ui.get_app_mode(), marks));
            submit_to_ui(
                &ui,
                &worker,
                IdeWorkerCommand::Transport(TransportCommand::SetLoopRange(marks.range())),
            );
            if marks.range().is_none() && ui.get_loop_engaged() {
                ui.set_loop_engaged(false);
                submit_to_ui(
                    &ui,
                    &worker,
                    IdeWorkerCommand::Transport(TransportCommand::SetLoopEnabled(false)),
                );
            }
        }
    };
    actions.on_loop_mark_in(mark(false));
    actions.on_loop_mark_out(mark(true));

    let weak = ui.as_weak();
    let toggle_worker = Rc::clone(worker);
    let toggle_state = Rc::clone(state);
    actions.on_loop_toggle(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !loop_toggle_available(ui.get_app_mode(), toggle_state.loop_marks.get()) {
            return;
        }
        let engaged = !ui.get_loop_engaged();
        ui.set_loop_engaged(engaged);
        toggle_state.loop_engaged.set(engaged);
        submit_to_ui(
            &ui,
            &toggle_worker,
            IdeWorkerCommand::Transport(TransportCommand::SetLoopEnabled(engaged)),
        );
    });

    let weak = ui.as_weak();
    let clear_worker = Rc::clone(worker);
    let clear_state = Rc::clone(state);
    actions.on_loop_clear(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        clear_state.loop_marks.set(LoopMarks::default());
        clear_state.loop_engaged.set(false);
        ui.set_loop_summary(SharedString::default());
        ui.set_loop_toggle_available(false);
        ui.set_loop_engaged(false);
        submit_to_ui(
            &ui,
            &clear_worker,
            IdeWorkerCommand::Transport(TransportCommand::SetLoopEnabled(false)),
        );
        submit_to_ui(
            &ui,
            &clear_worker,
            IdeWorkerCommand::Transport(TransportCommand::SetLoopRange(None)),
        );
    });
}

fn bind_undo_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let worker = Rc::clone(worker);
    bind_undo_callbacks_with(
        ui,
        Rc::new(move |ui, command| submit_to_ui(ui, &worker, command)),
    );
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "each generated callback retains the shared command submitter for the window lifetime"
)]
fn bind_undo_callbacks_with(ui: &MainWindow, submitter: UiCommandSubmitter) {
    let restore = |redo: bool| {
        let weak = ui.as_weak();
        let submitter = Rc::clone(&submitter);
        move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let label = if redo {
                ui.get_redo_label()
            } else {
                ui.get_undo_label()
            };
            if !restore_available(ui.get_app_mode(), Some(label.as_str())) {
                return;
            }
            submitter(
                &ui,
                if redo {
                    IdeWorkerCommand::Redo
                } else {
                    IdeWorkerCommand::Undo
                },
            );
        }
    };
    let actions = ui.global::<IdeActions>();
    actions.on_undo_edit(restore(false));
    actions.on_redo_edit(restore(true));
}

fn bind_project_export_callback(
    ui: &MainWindow,
    worker: &WorkerCell,
    project_state: &ProjectUiStateCell,
) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_export = Rc::clone(worker);
    actions.on_export_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        begin_project_export_destination_selection(&ui, &worker_for_export, None);
    });

    let weak = ui.as_weak();
    let worker_for_loop_export = Rc::clone(worker);
    let state_for_loop_export = Rc::clone(project_state);
    actions.on_export_loop_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Some(range) = current_persisted_loop_export_range(&state_for_loop_export) else {
            ui.set_error_text("Save a nonempty loop range before exporting it".into());
            return;
        };
        begin_project_export_destination_selection(&ui, &worker_for_loop_export, Some(range));
    });
}

fn begin_project_export_destination_selection(
    ui: &MainWindow,
    worker: &WorkerCell,
    range: Option<OfflineExportRange>,
) {
    if ui.get_app_mode() != AppMode::Running || !ui.get_export_available() {
        return;
    }
    let (title, file_name, waiting, cancelled) = if range.is_some() {
        (
            "Export persisted loop range",
            "loop-export.wav",
            "Waiting for loop export destination…",
            "Loop export cancelled",
        )
    } else {
        (
            "Export project audio",
            "export.wav",
            "Waiting for export destination…",
            "Export cancelled",
        )
    };
    ui.set_error_text(SharedString::default());
    ui.set_status_text(waiting.into());
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let spawn_result = slint::spawn_local(async move {
        let selection = rfd::AsyncFileDialog::new()
            .set_title(title)
            .set_file_name(file_name)
            .add_filter("WAV or FLAC", &["wav", "flac"])
            .save_file()
            .await;
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Some(selection) = selection else {
            ui.set_status_text(cancelled.into());
            return;
        };
        let path = selection.path().to_path_buf();
        ui.set_export_destination(path.display().to_string().into());
        ui.set_export_summary(SharedString::default());
        let command = match project_export_command(path, range) {
            Ok(command) => command,
            Err(error) => {
                ui.set_error_text(error.into());
                ui.set_status_text("Export destination must end in .wav or .flac".into());
                return;
            }
        };
        submit_to_ui(&ui, &worker, command);
    });
    if spawn_result.is_err() {
        ui.set_status_text("Export destination selection is unavailable".into());
    }
}

fn bind_project_stem_export_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_export = Rc::clone(worker);
    actions.on_export_track_stems(move |flac| {
        let weak = weak.clone();
        let worker = Rc::clone(&worker_for_export);
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_export_available() {
            return;
        }
        ui.set_error_text(SharedString::default());
        ui.set_status_text("Waiting for a new stem export folder…".into());
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title(if flac {
                    "Name a new aligned FLAC stem folder"
                } else {
                    "Name a new aligned WAV stem folder"
                })
                .set_file_name(if flac {
                    "project-flac-stems"
                } else {
                    "project-wav-stems"
                })
                .save_file()
                .await;
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(selection) = selection else {
                ui.set_status_text("Stem export cancelled".into());
                return;
            };
            let destination_directory = selection.path().to_path_buf();
            ui.set_export_destination(destination_directory.display().to_string().into());
            ui.set_export_summary(SharedString::default());
            let command = track_stem_export_command(destination_directory, flac);
            submit_to_ui(&ui, &worker, command);
        });
        if spawn_result.is_err() {
            ui.set_status_text("Stem export folder selection is unavailable".into());
        }
    });
}

fn project_export_command(
    destination_path: PathBuf,
    range: Option<OfflineExportRange>,
) -> Result<IdeWorkerCommand, String> {
    let format = IdeProjectExportFormat::from_destination(&destination_path)
        .map_err(|error| error.to_string())?;
    Ok(IdeWorkerCommand::ExportProject {
        destination_path,
        format,
        block_frames: EXPORT_BLOCK_FRAMES,
        range,
    })
}

fn track_stem_export_command(destination_directory: PathBuf, flac: bool) -> IdeWorkerCommand {
    IdeWorkerCommand::ExportTrackStems {
        destination_directory,
        format: if flac {
            IdeProjectExportFormat::Flac
        } else {
            IdeProjectExportFormat::Wav
        },
        block_frames: EXPORT_BLOCK_FRAMES,
    }
}

fn parse_control_millionths(value: &str) -> Result<i32, &'static str> {
    let parsed = value
        .trim()
        .parse::<i32>()
        .map_err(|_| "Control output values must be whole millionths")?;
    if (0..=1_000_000).contains(&parsed) {
        Ok(parsed)
    } else {
        Err("Control output values must be between 0 and 1000000")
    }
}

fn compact_name(name: &str, maximum_chars: usize) -> String {
    if name.chars().count() <= maximum_chars {
        return name.to_owned();
    }
    let mut display = name.chars().take(maximum_chars - 1).collect::<String>();
    display.push('…');
    display
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn set_mixer(
    ui: &MainWindow,
    channels: &[IdeMixerChannel],
    flat_mixer_strip_reorder_available: bool,
) {
    let current = ui.get_mixer_channels();
    let unchanged = current.row_count() == channels.len()
        && channels.iter().enumerate().all(|(index, expected)| {
            current.row_data(index).is_some_and(|actual| {
                let state = expected.state();
                actual.index == i32::try_from(index).unwrap_or(-1)
                    && actual.track_id.as_str() == expected.id()
                    && actual.name.as_str() == expected.name()
                    && actual.kind.as_str() == mixer_kind_name(expected.kind())
                    && actual.frozen == expected.frozen()
                    && actual.state_available == state.is_some()
                    && actual.gain_percent.to_bits()
                        == state.map_or(0.0, |state| state.gain * 100.0).to_bits()
                    && actual.pan_percent.to_bits()
                        == state.map_or(0.0, |state| state.pan * 100.0).to_bits()
                    && actual.muted == state.is_some_and(|state| state.muted)
                    && actual.solo == state.is_some_and(|state| state.solo)
                    && actual.solo_available == (expected.kind() == IdeMixerChannelKind::Audio)
                    && actual.renameable
                        == matches!(
                            expected.kind(),
                            IdeMixerChannelKind::Audio
                                | IdeMixerChannelKind::Group
                                | IdeMixerChannelKind::Return
                        )
                    && actual.removable == (expected.kind() == IdeMixerChannelKind::Audio)
                    && actual.can_move_earlier == (flat_mixer_strip_reorder_available && index > 0)
                    && actual.can_move_later
                        == (flat_mixer_strip_reorder_available && index + 1 < channels.len())
                    && actual.active == expected.active()
                    && actual.activation_toggleable
                        == (expected.kind() == IdeMixerChannelKind::Audio)
            })
        });
    if !unchanged {
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(mixer_rows(
            channels,
            flat_mixer_strip_reorder_available,
        )))));
    }
    let current_sends = ui.get_mixer_sends();
    if !mixer_sends_unchanged(&current_sends, channels) {
        ui.set_mixer_sends(ModelRc::from(Rc::new(VecModel::from(mixer_send_rows(
            channels,
        )))));
    }
    ui.set_return_send_candidates(ModelRc::from(Rc::new(VecModel::from(
        return_send_candidate_rows(channels),
    ))));
    ui.set_primary_route_candidates(ModelRc::from(Rc::new(VecModel::from(
        primary_route_candidate_rows(channels),
    ))));
}

fn mixer_sends_unchanged(sends: &ModelRc<MixerSendRow>, channels: &[IdeMixerChannel]) -> bool {
    if sends.row_count()
        != channels
            .iter()
            .map(|channel| channel.sends().len())
            .sum::<usize>()
    {
        return false;
    }
    let mut flat_index = 0;
    for (channel_index, channel) in channels.iter().enumerate() {
        for (send_index, expected) in channel.sends().iter().enumerate() {
            let Some(actual) = sends.row_data(flat_index) else {
                return false;
            };
            let state = expected.state();
            let tap = match expected.tap() {
                IdeMixerSendTap::PreFader => "PRE-FADER",
                IdeMixerSendTap::PostFader => "POST-FADER",
            };
            if actual.index != i32::try_from(flat_index).unwrap_or(-1)
                || actual.channel_index != i32::try_from(channel_index).unwrap_or(-1)
                || actual.send_index != i32::try_from(send_index).unwrap_or(-1)
                || actual.send_id.as_str() != expected.id()
                || actual.source_name.as_str() != channel.name()
                || actual.target_name.as_str() != expected.target_name()
                || actual.tap.as_str() != tap
                || actual.state_available != state.is_some()
                || actual.gain_percent.to_bits()
                    != state.map_or(0.0, |state| state.gain * 100.0).to_bits()
                || actual.pan_percent.to_bits()
                    != state.map_or(0.0, |state| state.pan * 100.0).to_bits()
                || actual.enabled != state.is_some_and(|state| state.enabled)
            {
                return false;
            }
            flat_index += 1;
        }
    }
    true
}

fn mode_after_closed_event(current: AppMode) -> AppMode {
    if current == AppMode::Selecting {
        AppMode::Selecting
    } else {
        AppMode::Closed
    }
}

fn apply_idle_running_event(ui: &MainWindow) {
    if ui.get_app_mode() != AppMode::PreparingRuntime {
        ui.set_app_mode(AppMode::Running);
        ui.set_status_text("Runtime evidence updated".into());
        ui.set_error_text(SharedString::default());
    }
}

fn mode_after_submission_failure(
    current: AppMode,
    command: IdeWorkerCommandKind,
    failure: UiSubmissionFailure,
) -> AppMode {
    match failure {
        UiSubmissionFailure::Disconnected => AppMode::Unavailable,
        UiSubmissionFailure::Full
            if current == AppMode::Creating && command == IdeWorkerCommandKind::CreateProject =>
        {
            AppMode::Closed
        }
        UiSubmissionFailure::Full
            if current == AppMode::Opening && command == IdeWorkerCommandKind::OpenProject =>
        {
            AppMode::Closed
        }
        UiSubmissionFailure::Full
            if current == AppMode::StartingOutputTest
                && command == IdeWorkerCommandKind::StartOutputTest =>
        {
            AppMode::Closed
        }
        UiSubmissionFailure::Full
            if current == AppMode::StoppingOutputTest
                && command == IdeWorkerCommandKind::StopOutputTest =>
        {
            AppMode::OutputTesting
        }
        UiSubmissionFailure::Full => current,
    }
}

fn meter_presentation(measurement_available: bool, dbfs: f32) -> (f32, String) {
    sample_peak_meter_presentation(measurement_available, dbfs, FINAL_OUTPUT_METER_FLOOR_DBFS)
}

fn input_meter_presentation(measurement_available: bool, dbfs: f32) -> (f32, String) {
    sample_peak_meter_presentation(measurement_available, dbfs, INPUT_METER_FLOOR_DBFS)
}

fn sample_peak_meter_presentation(
    measurement_available: bool,
    dbfs: f32,
    floor_dbfs: f32,
) -> (f32, String) {
    if !measurement_available || !dbfs.is_finite() {
        return (0.0, "— dBFS".to_owned());
    }
    let level = ((dbfs - floor_dbfs) / -floor_dbfs).clamp(0.0, 1.0);
    let reading = if dbfs <= floor_dbfs {
        format!("≤ {floor_dbfs:.1} dBFS")
    } else {
        format!("{dbfs:.1} dBFS")
    };
    (level, reading)
}

fn output_peak_hold_presentation(peak_hold_available: bool, dbfs: f32) -> (String, bool) {
    if !peak_hold_available || !dbfs.is_finite() {
        return ("hold —".to_owned(), false);
    }
    let reading = if dbfs <= FINAL_OUTPUT_METER_FLOOR_DBFS {
        format!("hold ≤ {FINAL_OUTPUT_METER_FLOOR_DBFS:.1}")
    } else {
        format!("hold {dbfs:.1}")
    };
    (reading, dbfs >= 0.0)
}

fn mixer_strip_meter_presentation(measurement_available: bool, dbfs: f32) -> (f32, String) {
    if !measurement_available || !dbfs.is_finite() {
        return (0.0, "—".to_owned());
    }
    let reading = if dbfs <= MIXER_STRIP_METER_FLOOR_DBFS {
        format!("≤ {MIXER_STRIP_METER_FLOOR_DBFS:.0}")
    } else {
        format!("{dbfs:.1}")
    };
    (
        ((dbfs - MIXER_STRIP_METER_FLOOR_DBFS) / -MIXER_STRIP_METER_FLOOR_DBFS).clamp(0.0, 1.0),
        reading,
    )
}

fn format_transport_clock(frame: u64, sample_rate_hz: u32) -> String {
    if sample_rate_hz == 0 {
        return "—".to_owned();
    }
    let total_milliseconds = (u128::from(frame) * 1_000) / u128::from(sample_rate_hz);
    let milliseconds = total_milliseconds % 1_000;
    let total_seconds = total_milliseconds / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let minutes = total_minutes % 60;
    let hours = total_minutes / 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

const fn return_to_start_command() -> TransportCommand {
    TransportCommand::Locate {
        frame: 0,
        state: LocateState::Preserve,
    }
}

fn native_failure_message(failure: IdeNativeCloseFailure) -> String {
    match failure {
        IdeNativeCloseFailure::RuntimeFailed => {
            "Native audio runtime failed while closing the project".to_owned()
        }
        IdeNativeCloseFailure::ThreadPanicked => {
            "Native audio thread panicked while closing the project".to_owned()
        }
    }
}

fn shutdown_worker(worker: DesktopIdeWorker) -> Result<(), DesktopUiShutdownError> {
    match worker.shutdown(INITIAL_SHUTDOWN_WAIT) {
        IdeWorkerShutdownOutcome::Stopped(result) => finish_shutdown(result),
        IdeWorkerShutdownOutcome::Pending(pending) => match pending.wait(FINAL_SHUTDOWN_WAIT) {
            IdeWorkerPendingShutdownWait::Stopped(result) => finish_shutdown(result),
            IdeWorkerPendingShutdownWait::TimedOut(_pending) => {
                Err(DesktopUiShutdownError::TimedOut)
            }
        },
    }
}

fn finish_shutdown(
    result: Result<IdeWorkerShutdownReport, hyper_desktop_ide::IdeWorkerJoinError>,
) -> Result<(), DesktopUiShutdownError> {
    let report = result.map_err(DesktopUiShutdownError::Join)?;
    if let Some(failure) = report.native_failure() {
        return Err(DesktopUiShutdownError::Native(failure));
    }
    match report.into_recording_abort() {
        Some(Err(source)) => Err(DesktopUiShutdownError::RecordingAbort(source)),
        Some(Ok(_)) | None => Ok(()),
    }
}

#[derive(Debug)]
enum DesktopUiShutdownError {
    TimedOut,
    Join(hyper_desktop_ide::IdeWorkerJoinError),
    Native(IdeNativeCloseFailure),
    RecordingAbort(hyper_desktop_ide::IdeRecordingAbortError),
}

impl fmt::Display for DesktopUiShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => {
                formatter.write_str("desktop IDE worker did not stop within 5 seconds")
            }
            Self::Join(source) => source.fmt(formatter),
            Self::Native(failure) => formatter.write_str(&native_failure_message(*failure)),
            Self::RecordingAbort(source) => {
                write!(
                    formatter,
                    "recording could not be aborted during shutdown: {source}"
                )
            }
        }
    }
}

impl Error for DesktopUiShutdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Join(source) => Some(source),
            Self::RecordingAbort(source) => Some(source),
            Self::TimedOut | Self::Native(_) => None,
        }
    }
}

fn finish_exit(
    run: Result<(), PlatformError>,
    shutdown: Result<(), DesktopUiShutdownError>,
) -> Result<(), DesktopUiExitError> {
    match (run, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(run), Ok(())) => Err(DesktopUiExitError::Run(Box::new(run))),
        (Ok(()), Err(shutdown)) => Err(DesktopUiExitError::Shutdown(shutdown)),
        (Err(run), Err(shutdown)) => Err(DesktopUiExitError::RunAndShutdown {
            run: Box::new(run),
            shutdown,
        }),
    }
}

#[derive(Debug)]
enum DesktopUiExitError {
    Run(Box<PlatformError>),
    Shutdown(DesktopUiShutdownError),
    RunAndShutdown {
        run: Box<PlatformError>,
        shutdown: DesktopUiShutdownError,
    },
}

impl fmt::Display for DesktopUiExitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Run(source) => source.fmt(formatter),
            Self::Shutdown(source) => source.fmt(formatter),
            Self::RunAndShutdown { run, shutdown } => {
                write!(
                    formatter,
                    "desktop UI failed: {run}; shutdown also failed: {shutdown}"
                )
            }
        }
    }
}

impl Error for DesktopUiExitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Run(source) => Some(source),
            Self::Shutdown(source)
            | Self::RunAndShutdown {
                shutdown: source, ..
            } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i_slint_backend_testing::ElementHandle;
    use slint::platform::{Key, PointerEventButton, WindowEvent};

    #[test]
    fn metronome_toggle_callback_submits_only_the_authoritative_next_state() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_metronome_toggle_callback_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_metronome_available(false);
        ui.global::<IdeActions>().invoke_metronome_toggle();
        assert!(submitted.borrow().is_empty());

        ui.set_metronome_available(true);
        ui.set_metronome_enabled(false);
        ui.global::<IdeActions>().invoke_metronome_toggle();
        ui.set_metronome_enabled(true);
        ui.global::<IdeActions>().invoke_metronome_toggle();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [
                    IdeWorkerCommand::Transport(TransportCommand::SetMetronomeEnabled(true)),
                    IdeWorkerCommand::Transport(TransportCommand::SetMetronomeEnabled(false)),
                ]
            ),
            "the generated callback must submit only the inverse of the worker-observed metronome state"
        );

        ui.set_app_mode(AppMode::Previewing);
        ui.global::<IdeActions>().invoke_metronome_toggle();
        assert_eq!(submitted.borrow().len(), 2);
    }

    #[test]
    fn metronome_gain_callback_submits_only_a_bounded_command_without_optimistic_ui_state() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_metronome_gain_callback_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_metronome_gain_percent(40.0);
        ui.set_metronome_available(false);
        ui.global::<IdeActions>().invoke_metronome_gain(75.25);
        assert!(submitted.borrow().is_empty());

        ui.set_metronome_available(true);
        ui.global::<IdeActions>().invoke_metronome_gain(75.25);
        assert_eq!(
            ui.get_metronome_gain_percent().to_bits(),
            40.0_f32.to_bits()
        );
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::Transport(TransportCommand::SetMetronomeGain(gain))]
                if *gain == MetronomeGain::from_millionths(752_500).unwrap()
        ));

        ui.global::<IdeActions>().invoke_metronome_gain(100.1);
        assert_eq!(submitted.borrow().len(), 1);
        assert_eq!(
            ui.get_error_text().as_str(),
            "Metronome click level is outside its supported range"
        );
    }

    #[test]
    fn metronome_observation_uses_committed_transport_state_without_meter_delivery() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let (_publisher, checkpoint) =
            hyper_mixer_runtime::playback_recovery_channel_with_initial_loop_and_metronome(
                None,
                None,
                None,
                false,
                hyper_mixer_runtime::InitialMetronomeState::prepared(
                    true,
                    MetronomeGain::from_millionths(725_000).unwrap(),
                ),
            );
        let observation = IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
            PlaybackRecoveryTransportState::Stopped,
            0,
        ))
        .with_transport_cycle_for_ui_test(checkpoint.observe_cycle());

        apply_metronome_observation(&ui, &observation);

        assert!(ui.get_metronome_available());
        assert!(ui.get_metronome_enabled());
        assert_eq!(
            ui.get_metronome_gain_percent().to_bits(),
            72.5_f32.to_bits()
        );
    }

    #[test]
    fn output_peak_hold_reset_callback_submits_once_only_after_worker_acceptance() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_left_reading("-7.5 dBFS".into());
        ui.set_right_reading("-0.4 dBFS".into());

        let submissions = Rc::new(RefCell::new(0_usize));
        let submissions_by_callback = Rc::clone(&submissions);
        let accepted = Rc::new(RefCell::new(false));
        let accepted_by_callback = Rc::clone(&accepted);
        bind_output_peak_hold_callback_with(&ui, move |_| {
            *submissions_by_callback.borrow_mut() += 1;
            *accepted_by_callback.borrow()
        });

        ui.set_app_mode(AppMode::Previewing);
        ui.set_output_peak_hold_available(true);
        ui.global::<IdeActions>().invoke_reset_output_peak_hold();
        assert_eq!(*submissions.borrow(), 0);
        assert!(!ui.get_output_peak_hold_reset_pending());

        ui.set_app_mode(AppMode::Running);
        ui.set_output_peak_hold_available(false);
        ui.global::<IdeActions>().invoke_reset_output_peak_hold();
        assert_eq!(*submissions.borrow(), 0);
        assert!(!ui.get_output_peak_hold_reset_pending());

        ui.set_output_peak_hold_available(true);
        ui.global::<IdeActions>().invoke_reset_output_peak_hold();
        assert_eq!(*submissions.borrow(), 1);
        assert!(!ui.get_output_peak_hold_reset_pending());

        *accepted.borrow_mut() = true;
        ui.global::<IdeActions>().invoke_reset_output_peak_hold();
        assert_eq!(*submissions.borrow(), 2);
        assert!(ui.get_output_peak_hold_reset_pending());
        assert_eq!(ui.get_left_reading(), "-7.5 dBFS");
        assert_eq!(ui.get_right_reading(), "-0.4 dBFS");

        ui.global::<IdeActions>().invoke_reset_output_peak_hold();
        assert_eq!(*submissions.borrow(), 2);
    }

    #[test]
    fn output_peak_hold_reset_event_clears_pending_without_replacing_project_ui() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let project_state = Rc::new(ProjectUiState::default());
        ui.set_app_mode(AppMode::Running);
        ui.set_project_name("Retained project".into());
        ui.set_track_count(3);
        ui.set_left_meter(0.75);
        ui.set_right_meter(0.5);
        ui.set_left_reading("-7.5 dBFS".into());
        ui.set_right_reading("-12.0 dBFS".into());
        ui.set_left_peak_reading("hold 0.0".into());
        ui.set_right_peak_reading("hold -3.0".into());
        ui.set_left_peak_overload_latched(true);
        ui.set_right_peak_overload_latched(false);
        ui.set_output_peak_hold_available(true);
        ui.set_output_peak_hold_reset_pending(true);
        ui.set_error_text("previous error".into());

        apply_event(
            &ui,
            &UiRoutes::default(),
            &project_state,
            IdeWorkerEvent::OutputPeakHoldReset(IdeOutputMeterObservation::empty()),
        );

        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert_eq!(ui.get_project_name(), "Retained project");
        assert_eq!(ui.get_track_count(), 3);
        assert_eq!(ui.get_left_meter().to_bits(), 0.0_f32.to_bits());
        assert_eq!(ui.get_right_meter().to_bits(), 0.0_f32.to_bits());
        assert_eq!(ui.get_left_reading(), "— dBFS");
        assert_eq!(ui.get_right_reading(), "— dBFS");
        assert_eq!(ui.get_left_peak_reading(), "hold —");
        assert_eq!(ui.get_right_peak_reading(), "hold —");
        assert!(!ui.get_left_peak_overload_latched());
        assert!(!ui.get_right_peak_overload_latched());
        assert!(!ui.get_output_peak_hold_available());
        assert!(!ui.get_output_peak_hold_reset_pending());
        assert_eq!(
            ui.get_status_text(),
            "Final-output sample-peak hold cleared"
        );
        assert!(ui.get_error_text().is_empty());
    }

    #[test]
    fn generated_arrangement_copy_submits_same_tick_overlap_to_transaction_authority() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_arrangement_visible(true);
        ui.set_arrangement_placements(ModelRc::from(Rc::new(VecModel::from(vec![
            ArrangementRow {
                index: 0,
                id: "placement-a".into(),
                track_name: "Audio".into(),
                clip_name: "Lead".into(),
                start_tick: "480".into(),
                clip_start_tick: "0".into(),
                duration_ticks: "960".into(),
                gain_edit: "0".into(),
                fade_in_edit: "0".into(),
                fade_out_edit: "0".into(),
                crossfade_target_id: SharedString::default(),
                crossfade_target_label: SharedString::default(),
                start_clock: "00:00.250".into(),
                end_clock: "00:00.750".into(),
                start_percent: 0.0,
                width_percent: 100.0,
                enabled: true,
                active: true,
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_arrangement_duplication_callback_with(
            &ui,
            |placement_id| {
                if placement_id == "placement-a" {
                    Ok(placement_id.to_owned())
                } else {
                    Err("The selected arrangement placement is no longer available")
                }
            },
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );

        ui.show().unwrap();
        let copy = ElementHandle::find_by_accessible_label(
            &ui,
            "Copy the exact source view of Lead on Audio to the entered start tick without ripple",
        )
        .next()
        .expect("the generated arrangement row exposes copy");
        assert_eq!(copy.accessible_enabled(), Some(true));
        copy.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::DuplicateArrangementPlacement {
                placement_id,
                start_tick: 480,
            }] if placement_id == "placement-a"
        ));
        assert!(ui.get_error_text().is_empty());
        ui.hide().unwrap();
    }

    #[test]
    fn generated_arrangement_fade_controls_expose_the_local_transaction_callback() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_arrangement_visible(true);
        ui.set_arrangement_placements(ModelRc::from(Rc::new(VecModel::from(vec![
            ArrangementRow {
                index: 0,
                id: "placement-a".into(),
                track_name: "Audio".into(),
                clip_name: "Lead".into(),
                start_tick: "480".into(),
                clip_start_tick: "0".into(),
                duration_ticks: "960".into(),
                gain_edit: "0".into(),
                fade_in_edit: "12".into(),
                fade_out_edit: "24".into(),
                crossfade_target_id: SharedString::default(),
                crossfade_target_label: SharedString::default(),
                start_clock: "00:00.250".into(),
                end_clock: "00:00.750".into(),
                start_percent: 0.0,
                width_percent: 100.0,
                enabled: true,
                active: true,
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        ui.global::<IdeActions>()
            .on_set_arrangement_placement_fades(move |placement_id, fade_in, fade_out| {
                submitted_by_callback.borrow_mut().push((
                    placement_id.to_string(),
                    fade_in.to_string(),
                    fade_out.to_string(),
                ));
            });

        ui.show().unwrap();
        assert!(
            ElementHandle::find_by_accessible_label(
                &ui,
                "Placement fade-in duration in ticks for Lead on Audio",
            )
            .next()
            .is_some(),
            "the generated arrangement row exposes a placement-local fade-in field"
        );
        assert!(
            ElementHandle::find_by_accessible_label(
                &ui,
                "Placement fade-out duration in ticks for Lead on Audio",
            )
            .next()
            .is_some(),
            "the generated arrangement row exposes a placement-local fade-out field"
        );
        ui.global::<IdeActions>()
            .invoke_set_arrangement_placement_fades("placement-a".into(), "16".into(), "32".into());
        assert_eq!(
            submitted.borrow().as_slice(),
            [("placement-a".to_owned(), "16".to_owned(), "32".to_owned())]
        );
        ui.hide().unwrap();
    }

    #[test]
    fn generated_arrangement_crossfade_control_submits_only_the_current_pair() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_arrangement_visible(true);
        ui.set_arrangement_placements(ModelRc::from(Rc::new(VecModel::from(vec![
            ArrangementRow {
                index: 0,
                id: "placement-a".into(),
                track_name: "Audio".into(),
                clip_name: "Lead".into(),
                start_tick: "480".into(),
                clip_start_tick: "0".into(),
                duration_ticks: "960".into(),
                gain_edit: "0".into(),
                fade_in_edit: "0".into(),
                fade_out_edit: "0".into(),
                crossfade_target_id: "placement-b".into(),
                crossfade_target_label: "Pad (480 ticks)".into(),
                start_clock: "00:00.250".into(),
                end_clock: "00:00.750".into(),
                start_percent: 0.0,
                width_percent: 100.0,
                enabled: true,
                active: true,
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_arrangement_crossfade_callback_with(
            &ui,
            |outgoing_placement_id, incoming_placement_id| {
                if outgoing_placement_id == "placement-a" && incoming_placement_id == "placement-b"
                {
                    Ok((
                        outgoing_placement_id.to_owned(),
                        incoming_placement_id.to_owned(),
                    ))
                } else {
                    Err("The selected crossfade target is no longer authoritative")
                }
            },
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );

        ui.show().unwrap();
        let crossfade = ElementHandle::find_by_accessible_label(
            &ui,
            "Apply a bounded linear crossfade from Lead on Audio to Pad (480 ticks)",
        )
        .next()
        .expect("the generated arrangement row exposes its bounded crossfade action");
        assert_eq!(crossfade.accessible_enabled(), Some(true));
        crossfade.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::ApplyArrangementPlacementCrossfade {
                outgoing_placement_id,
                incoming_placement_id,
            }] if outgoing_placement_id == "placement-a" && incoming_placement_id == "placement-b"
        ));

        ui.global::<IdeActions>()
            .invoke_apply_arrangement_placement_crossfade("placement-a".into(), "other".into());
        assert_eq!(submitted.borrow().len(), 1);
        assert_eq!(
            ui.get_error_text().as_str(),
            "The selected crossfade target is no longer authoritative"
        );

        ui.set_app_mode(AppMode::Previewing);
        ui.global::<IdeActions>()
            .invoke_apply_arrangement_placement_crossfade(
                "placement-a".into(),
                "placement-b".into(),
            );
        assert_eq!(submitted.borrow().len(), 1);
        ui.hide().unwrap();
    }

    fn audio_track_mixer_row(index: i32, track_id: &str) -> MixerChannelRow {
        MixerChannelRow {
            index,
            track_id: track_id.into(),
            name: "Audio channel".into(),
            display_name: "Audio".into(),
            kind: "AUDIO".into(),
            frozen: false,
            flattenable: false,
            state_available: true,
            gain_percent: 100.0,
            pan_percent: 0.0,
            muted: false,
            solo: false,
            meter_left_level: 0.0,
            meter_right_level: 0.0,
            meter_left_reading: "—".into(),
            meter_right_reading: "—".into(),
            solo_available: true,
            renameable: true,
            removable: true,
            can_move_earlier: index > 0,
            can_move_later: true,
            active: true,
            activation_toggleable: true,
        }
    }

    #[test]
    fn generated_track_freeze_control_follows_the_observed_frozen_state() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![
            audio_track_mixer_row(0, track_id),
        ]))));
        ui.show().unwrap();

        let freeze = ElementHandle::find_by_accessible_label(&ui, "Freeze Audio channel")
            .next()
            .expect("an unfrozen audio row exposes freeze");
        assert_eq!(freeze.accessible_enabled(), Some(true));
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Unfreeze Audio channel")
                .next()
                .is_none(),
            "an unfrozen audio row must not expose unfreeze"
        );
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Flatten Audio channel")
                .next()
                .is_none(),
            "an unfrozen audio row must not expose flatten"
        );
        freeze.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_track_freeze_visible());
        assert!(!ui.get_track_freeze_is_unfreeze());
        reset_audio_track_freeze_form(&ui);

        let mut frozen = audio_track_mixer_row(0, track_id);
        frozen.frozen = true;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![frozen]))));

        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Freeze Audio channel")
                .next()
                .is_none(),
            "a frozen audio row must not expose freeze"
        );
        let unfreeze = ElementHandle::find_by_accessible_label(&ui, "Unfreeze Audio channel")
            .next()
            .expect("a frozen audio row exposes unfreeze");
        assert_eq!(unfreeze.accessible_enabled(), Some(true));
        assert!(
            ElementHandle::find_by_accessible_label(&ui, "Flatten Audio channel")
                .next()
                .is_none(),
            "a frozen row without complete rendered authority must not expose flatten"
        );
        unfreeze.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_track_freeze_visible());
        assert!(ui.get_track_freeze_is_unfreeze());
        reset_audio_track_freeze_form(&ui);

        let mut flattenable = audio_track_mixer_row(0, track_id);
        flattenable.frozen = true;
        flattenable.flattenable = true;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![flattenable]))));
        let flatten = ElementHandle::find_by_accessible_label(&ui, "Flatten Audio channel")
            .next()
            .expect("a fully authoritative frozen audio row exposes flatten");
        assert_eq!(flatten.accessible_enabled(), Some(true));
        flatten.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_track_freeze_visible());
        assert!(ui.get_track_freeze_is_flatten());
        assert!(!ui.get_track_freeze_is_unfreeze());
        ui.hide().unwrap();
    }

    fn dispatch_ide_shortcut(ui: &MainWindow, text: &str, shift: bool, repeated: bool) {
        dispatch_ide_shortcut_with_modifiers(ui, text, shift, false, false, repeated);
    }

    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "the test mirrors independently dispatched keyboard modifier state"
    )]
    fn dispatch_ide_shortcut_with_modifiers(
        ui: &MainWindow,
        text: &str,
        shift: bool,
        alt: bool,
        meta: bool,
        repeated: bool,
    ) {
        ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Control.into(),
        });
        if shift {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Shift.into(),
            });
        }
        if alt {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Alt.into(),
            });
        }
        if meta {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Meta.into(),
            });
        }
        ui.window().dispatch_event(if repeated {
            WindowEvent::KeyPressRepeated { text: text.into() }
        } else {
            WindowEvent::KeyPressed { text: text.into() }
        });
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: text.into() });
        if shift {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Shift.into(),
            });
        }
        if alt {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Alt.into(),
            });
        }
        if meta {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Meta.into(),
            });
        }
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    }

    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "the test mirrors independently dispatched keyboard modifier state"
    )]
    fn dispatch_ide_space(
        ui: &MainWindow,
        control: bool,
        shift: bool,
        alt: bool,
        meta: bool,
        repeated: bool,
    ) {
        if control {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Control.into(),
            });
        }
        if shift {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Shift.into(),
            });
        }
        if alt {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Alt.into(),
            });
        }
        if meta {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: Key::Meta.into(),
            });
        }
        ui.window().dispatch_event(if repeated {
            WindowEvent::KeyPressRepeated {
                text: Key::Space.into(),
            }
        } else {
            WindowEvent::KeyPressed {
                text: Key::Space.into(),
            }
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Space.into(),
        });
        if meta {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Meta.into(),
            });
        }
        if alt {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Alt.into(),
            });
        }
        if shift {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Shift.into(),
            });
        }
        if control {
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: Key::Control.into(),
            });
        }
    }

    fn dispatch_ide_escape(ui: &MainWindow, repeated: bool) {
        ui.window().dispatch_event(if repeated {
            WindowEvent::KeyPressRepeated {
                text: Key::Escape.into(),
            }
        } else {
            WindowEvent::KeyPressed {
                text: Key::Escape.into(),
            }
        });
        ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Escape.into(),
        });
    }

    fn assert_undo_redo_shortcut_contract(ui: &MainWindow) {
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_undo_callbacks_with(
            ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_undo_label("clip gain".into());
        ui.set_redo_label("scene rename".into());
        refresh_restore_availability(ui);
        assert!(ui.get_undo_available());
        assert!(ui.get_redo_available());

        dispatch_ide_shortcut(ui, "z", false, false);
        dispatch_ide_shortcut(ui, "Z", true, false);
        dispatch_ide_shortcut(ui, "y", false, false);
        dispatch_ide_shortcut(ui, "z", false, true);
        assert_restore_shortcut_commands(
            &submitted,
            "the root maps only one non-repeating primary shortcut to each existing action",
        );

        ui.set_app_mode(AppMode::RestoringEdit);
        refresh_restore_availability(ui);
        dispatch_ide_shortcut(ui, "z", false, false);
        dispatch_ide_shortcut(ui, "Z", true, false);
        dispatch_ide_shortcut(ui, "y", false, false);
        assert_restore_shortcut_commands(
            &submitted,
            "pending durable work revokes all history shortcuts instead of queuing a second restore",
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_undo_label(SharedString::default());
        ui.set_redo_label(SharedString::default());
        refresh_restore_availability(ui);
        dispatch_ide_shortcut(ui, "z", false, false);
        dispatch_ide_shortcut(ui, "Z", true, false);
        dispatch_ide_shortcut(ui, "y", false, false);
        assert_restore_shortcut_commands(
            &submitted,
            "empty durable-history stacks remain inert even when the project is otherwise idle",
        );
    }

    fn assert_project_save_shortcut_contract(ui: &MainWindow) {
        let project_state: ProjectUiStateCell = Rc::new(ProjectUiState::default());
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_project_save_callback_with(
            ui,
            &project_state,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_clip_source_trim_visible(false);
        ui.set_mixer_main_state_available(true);
        dispatch_ide_shortcut(ui, "s", false, false);
        dispatch_ide_shortcut(ui, "s", false, true);
        dispatch_ide_shortcut(ui, "S", true, false);
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [IdeWorkerCommand::SaveProject]
            ),
            "only a non-repeating primary save shortcut submits the existing project save command"
        );

        dispatch_ide_shortcut(ui, "s", true, false);
        assert!(ui.get_save_project_as_visible());
        assert_eq!(
            ui.get_save_project_as_name(),
            format!("{} copy", ui.get_project_name())
        );
        ui.set_save_project_as_visible(false);
        dispatch_ide_shortcut_with_modifiers(ui, "s", true, true, false, false);
        dispatch_ide_shortcut_with_modifiers(ui, "s", true, false, true, false);
        assert!(
            !ui.get_save_project_as_visible(),
            "only the exact primary Shift+S shortcut may open Save As"
        );

        ui.set_app_mode(AppMode::Saving);
        dispatch_ide_shortcut(ui, "s", false, false);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_main_state_available(false);
        dispatch_ide_shortcut(ui, "s", false, false);
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [IdeWorkerCommand::SaveProject]
            ),
            "pending or unavailable committed mixer state cannot queue another save"
        );
    }

    fn assert_project_copy_callback_contract(ui: &MainWindow) {
        let worker: WorkerCell = Rc::new(RefCell::new(None));
        bind_project_copy_callback_with(ui, &worker, move |weak, _, project_name, switch_to| {
            let ui = weak.upgrade().unwrap();
            complete_project_copy_choice_with(
                &ui,
                project_name,
                switch_to,
                ProjectCopyChoice::Cancelled,
                |_| panic!("a cancelled Save As chooser must not submit a command"),
            );
        });
        ui.set_app_mode(AppMode::Running);
        ui.set_save_project_as_visible(true);
        ui.global::<IdeActions>()
            .invoke_save_project_as("Independent".into(), true);
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert_eq!(ui.get_status_text(), "Save As cancelled");

        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_choice = Rc::clone(&submitted);
        complete_project_copy_choice_with(
            ui,
            "Independent".to_owned(),
            true,
            ProjectCopyChoice::Selected(PathBuf::from("parent")),
            move |command| submitted_by_choice.borrow_mut().push(command),
        );
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::SaveProjectAs {
                parent,
                directory_name,
                project_name,
                switch_to,
            }] if parent == &PathBuf::from("parent")
                && directory_name == "Independent"
                && project_name == "Independent"
                && *switch_to
        ));

        bind_project_copy_callback_with(
            ui,
            &worker,
            move |weak, worker, project_name, switch_to| {
                let ui = weak.upgrade().unwrap();
                complete_project_copy_choice(
                    &ui,
                    &worker,
                    project_name,
                    switch_to,
                    ProjectCopyChoice::Selected(PathBuf::from("parent")),
                );
            },
        );
        ui.set_app_mode(AppMode::Running);
        ui.set_save_project_as_visible(true);
        ui.global::<IdeActions>()
            .invoke_save_project_as("Independent".into(), true);
        assert_eq!(ui.get_app_mode(), AppMode::Unavailable);
        assert!(ui.get_error_text().contains("no longer available"));
        ui.set_app_mode(AppMode::Saving);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_save_project_as("Ignored".into(), true);
        assert!(ui.get_error_text().is_empty());
        ui.set_save_project_as_visible(false);
        ui.set_app_mode(AppMode::Running);
    }

    fn assert_project_export_shortcut_contract(ui: &MainWindow) {
        let exports = Rc::new(Cell::new(0_usize));
        let exports_by_callback = Rc::clone(&exports);
        ui.global::<IdeActions>().on_export_project(move || {
            exports_by_callback.set(exports_by_callback.get() + 1);
        });

        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(true);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(false);
        dispatch_ide_shortcut(ui, "e", false, false);
        dispatch_ide_shortcut(ui, "e", false, true);
        dispatch_ide_shortcut(ui, "E", true, false);
        dispatch_ide_shortcut_with_modifiers(ui, "e", false, true, false, false);
        dispatch_ide_shortcut_with_modifiers(ui, "e", false, false, true, false);
        assert_eq!(
            exports.get(),
            1,
            "only the exact primary export shortcut reaches the existing export action"
        );

        ui.set_app_mode(AppMode::ExportingProject);
        dispatch_ide_shortcut(ui, "e", false, false);
        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(false);
        dispatch_ide_shortcut(ui, "e", false, false);
        ui.set_export_available(true);
        ui.set_new_track_visible(true);
        dispatch_ide_shortcut(ui, "e", false, false);
        ui.set_new_track_visible(false);
        assert_eq!(
            exports.get(),
            1,
            "pending export, unavailable playback extent, and local project forms keep export unavailable"
        );
    }

    #[test]
    fn export_picker_selection_builds_one_typed_command_or_refuses() {
        assert!(matches!(
            project_export_command(PathBuf::from("C:/Renders/mix.WAV"), None),
            Ok(IdeWorkerCommand::ExportProject {
                format: IdeProjectExportFormat::Wav,
                ..
            })
        ));
        assert!(matches!(
            project_export_command(PathBuf::from("C:/Renders/mix.flac"), None),
            Ok(IdeWorkerCommand::ExportProject {
                format: IdeProjectExportFormat::Flac,
                ..
            })
        ));
        let loop_range = OfflineExportRange::new(480, 960).unwrap();
        assert!(matches!(
            project_export_command(PathBuf::from("C:/Renders/loop.wav"), Some(loop_range)),
            Ok(IdeWorkerCommand::ExportProject {
                range: Some(observed),
                ..
            }) if observed == loop_range
        ));
        assert!(project_export_command(PathBuf::from("C:/Renders/mix.aiff"), None).is_err());
        assert!(matches!(
            track_stem_export_command(PathBuf::from("C:/Renders/Session stems"), false),
            IdeWorkerCommand::ExportTrackStems {
                format: IdeProjectExportFormat::Wav,
                ..
            }
        ));
        assert!(matches!(
            track_stem_export_command(PathBuf::from("C:/Renders/Session stems"), true),
            IdeWorkerCommand::ExportTrackStems {
                format: IdeProjectExportFormat::Flac,
                ..
            }
        ));
    }

    #[test]
    fn track_stem_export_buttons_preserve_format_and_running_authority() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(true);
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        ui.global::<IdeActions>()
            .on_export_track_stems(move |flac| submitted_by_callback.borrow_mut().push(flac));
        ui.show().unwrap();

        let wav = ElementHandle::find_by_accessible_label(
            &ui,
            "Export every mixer channel as aligned WAV stems",
        )
        .next()
        .expect("the WAV-stem action is visible for a running verified project");
        let flac = ElementHandle::find_by_accessible_label(
            &ui,
            "Export every mixer channel as aligned FLAC stems",
        )
        .next()
        .expect("the FLAC-stem action is visible for a running verified project");
        assert_eq!(wav.accessible_enabled(), Some(true));
        assert_eq!(flac.accessible_enabled(), Some(true));
        wav.mock_single_click(PointerEventButton::Left);
        flac.mock_single_click(PointerEventButton::Left);
        assert_eq!(submitted.borrow().as_slice(), &[false, true]);

        ui.set_export_available(false);
        assert_eq!(wav.accessible_enabled(), Some(false));
        assert_eq!(flac.accessible_enabled(), Some(false));
        wav.mock_single_click(PointerEventButton::Left);
        flac.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            submitted.borrow().as_slice(),
            &[false, true],
            "revoked export authority cannot enqueue a speculative stem request"
        );
        ui.hide().unwrap();
    }

    #[test]
    fn project_export_button_matches_its_supported_destination_formats() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(true);
        let exports = Rc::new(Cell::new(0_usize));
        let exports_by_callback = Rc::clone(&exports);
        ui.global::<IdeActions>().on_export_project(move || {
            exports_by_callback.set(exports_by_callback.get() + 1);
        });
        ui.show().unwrap();

        let export_project = ElementHandle::find_by_accessible_label(
            &ui,
            "Export the project mix as a WAV or FLAC file",
        )
        .next()
        .expect("the project export action describes every supported container");
        assert_eq!(export_project.accessible_enabled(), Some(true));
        export_project.mock_single_click(PointerEventButton::Left);
        assert_eq!(exports.get(), 1);

        ui.set_export_available(false);
        assert_eq!(export_project.accessible_enabled(), Some(false));
        export_project.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            exports.get(),
            1,
            "revoked export authority cannot enqueue a project export"
        );
        ui.hide().unwrap();
    }

    #[test]
    fn loop_export_button_follows_persisted_range_authority_not_live_marks() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(true);
        ui.set_loop_toggle_available(true);
        ui.set_loop_export_available(false);
        let exports = Rc::new(Cell::new(0_usize));
        let exports_by_callback = Rc::clone(&exports);
        ui.global::<IdeActions>().on_export_loop_project(move || {
            exports_by_callback.set(exports_by_callback.get() + 1);
        });
        ui.show().unwrap();

        let export_loop = ElementHandle::find_by_accessible_label(
            &ui,
            "Export the persisted loop range as a WAV or FLAC file",
        )
        .next()
        .expect("the persisted loop export action is visible for a running verified project");
        assert_eq!(export_loop.accessible_enabled(), Some(false));
        export_loop.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            exports.get(),
            0,
            "unsaved live marks cannot enable an export of a range they do not own"
        );

        ui.set_loop_toggle_available(false);
        ui.set_loop_export_available(true);
        assert_eq!(export_loop.accessible_enabled(), Some(true));
        export_loop.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            exports.get(),
            1,
            "a persisted range remains exportable even when the live marks are partial"
        );

        ui.set_loop_toggle_available(true);
        ui.set_loop_export_available(false);
        assert_eq!(export_loop.accessible_enabled(), Some(false));
        export_loop.mock_single_click(PointerEventButton::Left);
        assert_eq!(exports.get(), 1);
        ui.hide().unwrap();
    }

    fn assert_project_creation_shortcut_contract(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Closed);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(false);
        dispatch_ide_shortcut(ui, "n", false, false);
        assert!(
            ui.get_new_project_visible(),
            "the primary new-project shortcut opens the same closed-state form as the button"
        );

        ui.set_new_project_visible(false);
        dispatch_ide_shortcut(ui, "n", false, true);
        dispatch_ide_shortcut(ui, "N", true, false);
        dispatch_ide_shortcut_with_modifiers(ui, "n", false, true, false, false);
        dispatch_ide_shortcut_with_modifiers(ui, "n", false, false, true, false);
        assert!(
            !ui.get_new_project_visible(),
            "repeated and modified new-project chords remain reserved rather than reopening a form"
        );

        ui.set_app_mode(AppMode::Running);
        dispatch_ide_shortcut(ui, "n", false, false);
        assert!(
            !ui.get_new_project_visible(),
            "a running project cannot be replaced by a keyboard form request"
        );

        ui.set_app_mode(AppMode::Closed);
        ui.set_new_track_visible(true);
        dispatch_ide_shortcut(ui, "n", false, false);
        assert!(
            !ui.get_new_project_visible(),
            "the project form never interrupts an already-open track form"
        );
        ui.set_new_track_visible(false);
    }

    fn assert_project_open_shortcut_contract(ui: &MainWindow) {
        let opened = Rc::new(Cell::new(0_usize));
        let opened_by_callback = Rc::clone(&opened);
        ui.global::<IdeActions>().on_choose_project(move || {
            opened_by_callback.set(opened_by_callback.get() + 1);
        });

        ui.set_app_mode(AppMode::Closed);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(false);
        dispatch_ide_shortcut(ui, "o", false, false);
        assert_eq!(
            opened.get(),
            1,
            "the shortcut reaches the existing open action"
        );

        dispatch_ide_shortcut(ui, "o", false, true);
        dispatch_ide_shortcut(ui, "O", true, false);
        assert_eq!(
            opened.get(),
            1,
            "repeat and Shift must not reopen the picker"
        );

        ui.set_app_mode(AppMode::Running);
        dispatch_ide_shortcut(ui, "o", false, false);
        assert_eq!(
            opened.get(),
            1,
            "a running project cannot be replaced by a shortcut"
        );

        ui.set_app_mode(AppMode::Closed);
        ui.set_new_project_visible(true);
        dispatch_ide_shortcut(ui, "o", false, false);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(true);
        dispatch_ide_shortcut(ui, "o", false, false);
        assert_eq!(
            opened.get(),
            1,
            "a local project form keeps the picker entry point unavailable"
        );
        ui.set_new_track_visible(false);
    }

    fn assert_audio_track_creation_shortcut_contract(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_track_count(1);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(false);
        dispatch_ide_shortcut(ui, "t", false, false);
        assert!(
            ui.get_new_track_visible(),
            "the primary add-track shortcut opens the same bounded running-state form as the button"
        );

        ui.set_new_track_visible(false);
        dispatch_ide_shortcut(ui, "t", false, true);
        dispatch_ide_shortcut(ui, "T", true, false);
        assert!(
            !ui.get_new_track_visible(),
            "repeated and shifted add-track chords remain reserved rather than reopening a form"
        );

        ui.set_track_count(8);
        dispatch_ide_shortcut(ui, "t", false, false);
        assert!(
            !ui.get_new_track_visible(),
            "the bounded project track limit revokes the add-track shortcut"
        );

        ui.set_track_count(1);
        ui.set_app_mode(AppMode::Closed);
        dispatch_ide_shortcut(ui, "t", false, false);
        assert!(
            !ui.get_new_track_visible(),
            "a closed project cannot open an audio-track form"
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_new_project_visible(true);
        dispatch_ide_shortcut(ui, "t", false, false);
        assert!(
            !ui.get_new_track_visible(),
            "the audio-track form never interrupts an already-open project form"
        );
        ui.set_new_project_visible(false);
    }

    fn assert_project_close_shortcut_contract(ui: &MainWindow) {
        let closed = Rc::new(Cell::new(0_usize));
        let closed_by_callback = Rc::clone(&closed);
        ui.global::<IdeActions>().on_close_project(move || {
            closed_by_callback.set(closed_by_callback.get() + 1);
        });

        ui.set_new_project_visible(false);
        ui.set_new_track_visible(false);
        ui.set_app_mode(AppMode::Running);
        dispatch_ide_shortcut(ui, "w", false, false);
        dispatch_ide_shortcut(ui, "w", false, true);
        dispatch_ide_shortcut(ui, "W", true, false);
        dispatch_ide_shortcut_with_modifiers(ui, "w", false, true, false, false);
        dispatch_ide_shortcut_with_modifiers(ui, "w", false, false, true, false);
        assert_eq!(
            closed.get(),
            1,
            "only a non-repeating primary close shortcut reaches the existing close action"
        );

        ui.set_app_mode(AppMode::Previewing);
        dispatch_ide_shortcut(ui, "w", false, false);
        assert_eq!(
            closed.get(),
            2,
            "the previewing state shares the visible Close action authority"
        );

        ui.set_app_mode(AppMode::Closing);
        dispatch_ide_shortcut(ui, "w", false, false);
        ui.set_app_mode(AppMode::Running);
        ui.set_new_project_visible(true);
        dispatch_ide_shortcut(ui, "w", false, false);
        ui.set_new_project_visible(false);
        ui.set_new_track_visible(true);
        dispatch_ide_shortcut(ui, "w", false, false);
        assert_eq!(
            closed.get(),
            2,
            "pending work and local forms keep the close shortcut unavailable"
        );
        ui.set_new_track_visible(false);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one fixture verifies native and in-app close behavior against the same dirty mixer authority"
    )]
    fn assert_unsaved_mixer_close_contract(ui: &MainWindow) {
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_project_close_callback_with(
            ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        bind_window_close_callback(ui);

        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_unsaved_changes(false);
        ui.set_close_unsaved_mixer_confirming(false);
        let dirty_observation = IdeRunningObservation::for_ui_test(
            PlaybackRecoverySnapshot::for_test(PlaybackRecoveryTransportState::Stopped, 0),
        )
        .with_mixer_unsaved_changes(true);
        apply_event(
            ui,
            &UiRoutes::default(),
            &Rc::new(ProjectUiState::default()),
            IdeWorkerEvent::Running(dirty_observation),
        );
        assert!(
            ui.get_mixer_unsaved_changes(),
            "the generated mixer presentation must retain the worker's authoritative dirty observation"
        );
        ui.set_close_unsaved_mixer_confirming(false);
        ui.global::<IdeActions>().invoke_close_project();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [IdeWorkerCommand::CloseProject]
            ),
            "a clean running project may close through the existing worker command"
        );

        submitted.borrow_mut().clear();
        ui.set_mixer_unsaved_changes(true);
        ui.window()
            .try_dispatch_event(slint::platform::WindowEvent::CloseRequested)
            .expect("a rejected native close request must remain dispatchable");
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::CloseProject]
        ));
        apply_close_discard_required(ui);
        assert!(
            ui.get_close_unsaved_mixer_confirming(),
            "a native close request with a committed live-mixer difference must require an explicit close decision"
        );
        ui.global::<IdeActions>().invoke_close_project();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [IdeWorkerCommand::CloseProject]
            ),
            "repeating the ordinary close action cannot silently confirm discard"
        );
        ui.global::<IdeActions>().invoke_cancel_close_project();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [
                    IdeWorkerCommand::CloseProject,
                    IdeWorkerCommand::CancelCloseProject
                ]
            ),
            "keeping the project open must cancel the worker-owned discard authorization"
        );
        apply_close_discard_cancelled(ui);
        assert!(
            !ui.get_close_unsaved_mixer_confirming(),
            "only the worker cancellation outcome may withdraw the discard confirmation"
        );
        ui.global::<IdeActions>().invoke_close_project();
        apply_close_discard_required(ui);
        ui.global::<IdeActions>().invoke_confirm_close_project();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [
                    IdeWorkerCommand::CloseProject,
                    IdeWorkerCommand::CancelCloseProject,
                    IdeWorkerCommand::CloseProject,
                    IdeWorkerCommand::DiscardCloseProject
                ]
            ),
            "only the explicit discard confirmation may send close"
        );

        submitted.borrow_mut().clear();
        ui.set_app_mode(AppMode::Previewing);
        ui.set_close_unsaved_mixer_confirming(false);
        ui.global::<IdeActions>().invoke_close_project();
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::CloseProject]
        ));
        apply_close_discard_required(ui);
        assert!(
            ui.get_close_unsaved_mixer_confirming(),
            "take audition cannot bypass a pending live-mixer discard decision"
        );
        ui.global::<IdeActions>().invoke_confirm_close_project();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [
                    IdeWorkerCommand::CloseProject,
                    IdeWorkerCommand::DiscardCloseProject
                ]
            ),
            "the explicit discard path remains available while auditioning"
        );

        submitted.borrow_mut().clear();
        ui.set_app_mode(AppMode::Running);
        ui.set_close_unsaved_mixer_confirming(true);
        ui.set_mixer_unsaved_changes(false);
        let observation = IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
            PlaybackRecoveryTransportState::Stopped,
            0,
        ));
        apply_mixer_observation(ui, &observation);
        assert!(
            !ui.get_close_unsaved_mixer_confirming(),
            "a later clean recovery observation withdraws an obsolete discard confirmation"
        );
        assert_eq!(
            window_close_response(ui),
            CloseRequestResponse::KeepWindowShown,
            "a clean native close waits for the worker's fenced close decision"
        );

        submitted.borrow_mut().clear();
        ui.set_mixer_recovery_available(false);
        ui.global::<IdeActions>().invoke_restore_mixer_recovery();
        ui.global::<IdeActions>().invoke_discard_mixer_recovery();
        assert!(
            submitted.borrow().is_empty(),
            "recovery actions remain inert until the worker reports matching recovery data"
        );
        ui.set_mixer_recovery_available(true);
        ui.global::<IdeActions>().invoke_restore_mixer_recovery();
        ui.global::<IdeActions>().invoke_discard_mixer_recovery();
        assert!(
            matches!(
                submitted.borrow().as_slice(),
                [
                    IdeWorkerCommand::RestoreMixerRecovery,
                    IdeWorkerCommand::DiscardMixerRecovery
                ]
            ),
            "the recovery controls preserve the worker-owned explicit restore-or-discard choice"
        );
        ui.set_mixer_recovery_available(false);
    }

    fn assert_transport_space_shortcut_contract(ui: &MainWindow) {
        let starts = Rc::new(Cell::new(0_usize));
        let starts_by_callback = Rc::clone(&starts);
        ui.global::<IdeActions>().on_transport_start(move || {
            starts_by_callback.set(starts_by_callback.get() + 1);
        });
        let stops = Rc::new(Cell::new(0_usize));
        let stops_by_callback = Rc::clone(&stops);
        ui.global::<IdeActions>().on_transport_stop(move || {
            stops_by_callback.set(stops_by_callback.get() + 1);
        });

        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(false);
        ui.set_takes_visible(false);
        ui.set_arrangement_visible(false);
        ui.set_clips_visible(false);
        ui.set_loudness_visible(false);
        ui.set_scene_create_visible(false);
        ui.set_scene_rename_visible(false);
        ui.set_scene_duplicate_visible(false);
        ui.set_scene_slots_visible(false);
        apply_transport_recovery_availability(ui, PlaybackRecoveryTransportState::Stopped);
        assert!(ui.get_transport_start_available());
        assert!(!ui.get_transport_stop_available());
        dispatch_ide_space(ui, false, false, false, false, false);
        dispatch_ide_space(ui, false, false, false, false, true);
        assert_eq!(
            starts.get(),
            1,
            "Space reuses Start only from the current stopped recovery authority"
        );

        apply_transport_recovery_availability(ui, PlaybackRecoveryTransportState::Rolling);
        assert!(!ui.get_transport_start_available());
        assert!(ui.get_transport_stop_available());
        dispatch_ide_space(ui, false, false, false, false, false);
        assert_eq!(
            stops.get(),
            1,
            "Space reuses Stop only from the current rolling recovery authority"
        );

        dispatch_ide_space(ui, true, false, false, false, false);
        dispatch_ide_space(ui, false, true, false, false, false);
        dispatch_ide_space(ui, false, false, true, false, false);
        dispatch_ide_space(ui, false, false, false, true, false);
        ui.set_transport_stop_available(false);
        dispatch_ide_space(ui, false, false, false, false, false);
        ui.set_transport_start_available(true);
        ui.set_new_track_visible(true);
        dispatch_ide_space(ui, false, false, false, false, false);
        ui.set_new_track_visible(false);
        ui.set_clips_visible(true);
        dispatch_ide_space(ui, false, false, false, false, false);
        ui.set_clips_visible(false);
        ui.set_app_mode(AppMode::Previewing);
        dispatch_ide_space(ui, false, false, false, false, false);
        assert_eq!(starts.get(), 1);
        assert_eq!(
            stops.get(),
            1,
            "modifiers, revoked authority, local forms, hidden performance controls, and non-running modes remain inert"
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_transport_start_available(false);
    }

    fn assert_transport_running_observation_contract(ui: &MainWindow) {
        let starts = Rc::new(Cell::new(0_usize));
        let starts_by_callback = Rc::clone(&starts);
        ui.global::<IdeActions>().on_transport_start(move || {
            starts_by_callback.set(starts_by_callback.get() + 1);
        });
        let stops = Rc::new(Cell::new(0_usize));
        let stops_by_callback = Rc::clone(&stops);
        ui.global::<IdeActions>().on_transport_stop(move || {
            stops_by_callback.set(stops_by_callback.get() + 1);
        });

        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(false);
        ui.set_takes_visible(false);
        ui.set_arrangement_visible(false);
        ui.set_clips_visible(false);
        ui.set_loudness_visible(false);
        ui.set_scene_create_visible(false);
        ui.set_scene_rename_visible(false);
        ui.set_scene_duplicate_visible(false);
        ui.set_scene_slots_visible(false);
        ui.set_new_track_visible(false);
        let recording_routes = Rc::new(RefCell::new(Vec::new()));
        let take_routes = Rc::new(RefCell::new(Vec::new()));
        let audio_import_routes = Rc::new(RefCell::new(Vec::new()));
        let project_state = Rc::new(ProjectUiState::default());

        apply_running_event(
            ui,
            &recording_routes,
            &take_routes,
            &audio_import_routes,
            &project_state,
            &IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
                PlaybackRecoveryTransportState::Stopped,
                0,
            )),
        );
        assert!(ui.get_transport_start_available());
        assert!(!ui.get_transport_stop_available());
        dispatch_ide_space(ui, false, false, false, false, false);
        assert_eq!(
            starts.get(),
            1,
            "a stopped recovery observation enables the existing Start callback"
        );

        apply_running_event(
            ui,
            &recording_routes,
            &take_routes,
            &audio_import_routes,
            &project_state,
            &IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
                PlaybackRecoveryTransportState::Rolling,
                256,
            )),
        );
        assert!(!ui.get_transport_start_available());
        assert!(ui.get_transport_stop_available());
        dispatch_ide_space(ui, false, false, false, false, false);
        assert_eq!(
            stops.get(),
            1,
            "a rolling recovery observation enables the existing Stop callback"
        );

        ui.set_transport_stop_available(false);
    }

    fn assert_audio_import_shortcut_contract(ui: &MainWindow) {
        let selected = Rc::new(RefCell::new(Vec::new()));
        let selected_by_callback = Rc::clone(&selected);
        ui.global::<IdeActions>()
            .on_choose_audio_import(move |index| selected_by_callback.borrow_mut().push(index));

        ui.set_app_mode(AppMode::Running);
        ui.set_clips_visible(true);
        ui.set_clip_rename_visible(false);
        ui.set_clip_duplicate_visible(false);
        ui.set_clip_remove_visible(false);
        ui.set_clip_gain_visible(false);
        ui.set_clip_fades_visible(false);
        ui.set_clip_loop_visible(false);
        ui.set_clip_source_slip_visible(false);
        ui.set_clip_source_trim_visible(false);
        ui.set_add_arrangement_placement_visible(false);
        ui.set_audio_import_track_names(ModelRc::from(Rc::new(VecModel::from(vec![
            SharedString::from("Audio A"),
            SharedString::from("Audio B"),
        ]))));
        ui.set_selected_audio_import_track_index(1);

        dispatch_ide_shortcut(ui, "i", false, false);
        dispatch_ide_shortcut(ui, "i", false, true);
        dispatch_ide_shortcut(ui, "I", true, false);
        dispatch_ide_shortcut_with_modifiers(ui, "i", false, true, false, false);
        dispatch_ide_shortcut_with_modifiers(ui, "i", false, false, true, false);
        assert_eq!(
            selected.borrow().as_slice(),
            &[1],
            "only the exact primary import chord reaches the existing destination-aware action"
        );

        ui.set_selected_audio_import_track_index(-1);
        dispatch_ide_shortcut(ui, "i", false, false);
        ui.set_selected_audio_import_track_index(0);
        ui.set_clip_rename_visible(true);
        dispatch_ide_shortcut(ui, "i", false, false);
        ui.set_clip_rename_visible(false);
        ui.set_new_track_visible(true);
        dispatch_ide_shortcut(ui, "i", false, false);
        ui.set_new_track_visible(false);
        ui.set_app_mode(AppMode::ImportingAudio);
        dispatch_ide_shortcut(ui, "i", false, false);
        assert_eq!(
            selected.borrow().as_slice(),
            &[1],
            "invalid destinations, local forms, and pending imports keep the shortcut unavailable"
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_clips_visible(false);
        dispatch_ide_shortcut(ui, "i", false, false);
        assert_eq!(
            selected.borrow().as_slice(),
            &[1],
            "the shortcut stays contextual to the visible Audio Clips workspace"
        );
    }

    #[test]
    fn generated_track_reorder_buttons_emit_only_the_current_bounded_direction() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        let row = |index, track_id: &str, name: &str, earlier, later| MixerChannelRow {
            index,
            track_id: track_id.into(),
            name: name.into(),
            display_name: name.into(),
            kind: "AUDIO".into(),
            frozen: false,
            flattenable: false,
            state_available: true,
            gain_percent: 100.0,
            pan_percent: 0.0,
            muted: false,
            solo: false,
            meter_left_level: 0.0,
            meter_right_level: 0.0,
            meter_left_reading: "—".into(),
            meter_right_reading: "—".into(),
            solo_available: true,
            renameable: true,
            removable: true,
            can_move_earlier: earlier,
            can_move_later: later,
            active: true,
            activation_toggleable: true,
        };
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![
            row(
                0,
                "018f47a2-6b2d-7c11-8a44-001122334456",
                "Audio 1",
                false,
                true,
            ),
            row(
                1,
                "018f47a2-6b2d-7c11-8a44-001122334457",
                "Target",
                true,
                false,
            ),
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        ui.global::<IdeActions>()
            .on_reorder_audio_track(move |index, earlier| {
                submitted_by_callback.borrow_mut().push((index, earlier));
            });
        ui.show().unwrap();

        let move_target_earlier =
            ElementHandle::find_by_accessible_label(&ui, "Move Target earlier")
                .next()
                .expect("the non-edge earlier control is visible");
        assert_eq!(move_target_earlier.accessible_enabled(), Some(true));
        move_target_earlier.mock_single_click(PointerEventButton::Left);
        assert_eq!(submitted.borrow().as_slice(), &[(1, true)]);

        let move_first_earlier =
            ElementHandle::find_by_accessible_label(&ui, "Move Audio 1 earlier")
                .next()
                .expect("the first row exposes its disabled earlier control");
        let move_target_later = ElementHandle::find_by_accessible_label(&ui, "Move Target later")
            .next()
            .expect("the final row exposes its disabled later control");
        assert_eq!(move_first_earlier.accessible_enabled(), Some(false));
        assert_eq!(move_target_later.accessible_enabled(), Some(false));
        move_first_earlier.mock_single_click(PointerEventButton::Left);
        move_target_later.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            submitted.borrow().as_slice(),
            &[(1, true)],
            "disabled edge controls must not emit speculative reorder requests"
        );

        ui.set_app_mode(AppMode::Closed);
        assert_eq!(move_target_earlier.accessible_enabled(), Some(false));
        move_target_earlier.mock_single_click(PointerEventButton::Left);
        assert_eq!(submitted.borrow().as_slice(), &[(1, true)]);
    }

    #[test]
    fn generated_audio_track_duplication_form_uses_the_current_audio_row() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        ui.set_track_count(1);
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![
            MixerChannelRow {
                index: 0,
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".into(),
                name: "Audio 1".into(),
                display_name: "Audio 1".into(),
                kind: "AUDIO".into(),
                frozen: false,
                flattenable: false,
                state_available: true,
                gain_percent: 100.0,
                pan_percent: 0.0,
                muted: false,
                solo: false,
                meter_left_level: 0.0,
                meter_right_level: 0.0,
                meter_left_reading: "—".into(),
                meter_right_reading: "—".into(),
                solo_available: true,
                renameable: true,
                removable: true,
                can_move_earlier: false,
                can_move_later: false,
                active: true,
                activation_toggleable: true,
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        ui.global::<IdeActions>()
            .on_duplicate_mixer_strip(move |index, name| {
                submitted_by_callback
                    .borrow_mut()
                    .push((index, name.to_string()));
            });
        ui.show().unwrap();

        let duplicate = ElementHandle::find_by_accessible_label(&ui, "Duplicate Audio 1")
            .next()
            .expect("the current audio row exposes duplication");
        assert_eq!(duplicate.accessible_enabled(), Some(true));
        duplicate.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_duplicate_track_visible());
        assert_eq!(ui.get_duplicate_track_index(), 0);
        assert_eq!(ui.get_duplicate_track_source_name(), "Audio 1");

        ui.set_duplicate_track_name("Audio 1 copy".into());
        let confirm = ElementHandle::find_by_accessible_label(&ui, "Confirm duplicate mixer strip")
            .next()
            .expect("the visible form exposes one confirmation action");
        assert_eq!(confirm.accessible_enabled(), Some(true));
        confirm.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            submitted.borrow().as_slice(),
            &[(0, "Audio 1 copy".to_owned())],
            "the generated row and form must forward the source row and chosen name exactly"
        );
    }

    #[test]
    fn mixer_strip_duplication_lifecycle_uses_generic_status_text() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);

        apply_audio_track_duplication_event(&ui, &IdeWorkerEvent::AudioTrackDuplicating);
        assert_eq!(ui.get_app_mode(), AppMode::DuplicatingTrack);
        assert_eq!(
            ui.get_status_text(),
            "Waiting for native output before duplicating the mixer strip…"
        );

        apply_audio_track_duplication_event(
            &ui,
            &IdeWorkerEvent::AudioTrackDuplicated {
                revision: 7,
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
            },
        );
        assert_eq!(
            ui.get_status_text(),
            "Mixer strip duplicated in project revision 7"
        );
    }

    #[test]
    fn routing_bus_form_exposes_only_selected_audio_sources_to_its_callback() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_track_count(2);
        ui.set_routing_bus_create_visible(true);
        ui.set_routing_bus_name("Drums".into());
        ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(vec![
            RoutingBusSourceRow {
                index: 0,
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".into(),
                name: "Audio 1".into(),
                selected: false,
            },
        ]))));
        let selected = Rc::new(RefCell::new(Vec::new()));
        let selected_by_callback = Rc::clone(&selected);
        ui.global::<IdeActions>()
            .on_toggle_routing_bus_source(move |index| {
                selected_by_callback.borrow_mut().push(index);
            });
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        ui.global::<IdeActions>()
            .on_add_routing_bus(move |kind, name| {
                submitted_by_callback
                    .borrow_mut()
                    .push((kind, name.to_string()));
            });
        ui.show().unwrap();

        let source = ElementHandle::find_by_accessible_label(&ui, "Select audio source Audio 1")
            .next()
            .expect("the form exposes its only valid audio source");
        assert_eq!(source.accessible_enabled(), Some(true));
        source.mock_single_click(PointerEventButton::Left);
        assert_eq!(selected.borrow().as_slice(), &[0]);

        let mut sources = ui.get_routing_bus_sources().iter().collect::<Vec<_>>();
        sources[0].selected = true;
        ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(sources))));
        ui.set_routing_bus_selected_source_count(1);
        let submit = ElementHandle::find_by_accessible_label(&ui, "Create routing bus")
            .next()
            .expect("a selected source enables one bus-creation action");
        assert_eq!(submit.accessible_enabled(), Some(true));
        submit.mock_single_click(PointerEventButton::Left);
        assert_eq!(submitted.borrow().as_slice(), &[(0, "Drums".to_owned())]);

        apply_routing_bus_event(
            &ui,
            &IdeWorkerEvent::RoutingBusAdded {
                revision: 3,
                track_id: "018f47a2-6b2d-7c11-8a44-001122334499".to_owned(),
            },
        );
        assert!(!ui.get_routing_bus_create_visible());
        assert!(ui.get_status_text().contains("project revision 3"));
    }

    #[test]
    fn routing_bus_removal_requires_confirmation_for_the_current_bus() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        let mut routing_bus = audio_track_mixer_row(0, track_id);
        routing_bus.name = "Verb".into();
        routing_bus.display_name = "Verb".into();
        routing_bus.kind = "RETURN".into();
        routing_bus.renameable = false;
        routing_bus.removable = false;
        routing_bus.activation_toggleable = false;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![routing_bus]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_routing_bus_removal_callback_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.show().unwrap();

        let remove = ElementHandle::find_by_accessible_label(&ui, "Remove routing bus Verb")
            .next()
            .expect("the current Return bus exposes removal");
        assert_eq!(remove.accessible_enabled(), Some(true));
        remove.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_routing_bus_removal_visible());
        assert_eq!(ui.get_routing_bus_removal_id(), track_id);

        let confirm =
            ElementHandle::find_by_accessible_label(&ui, "Confirm removal of routing bus Verb")
                .next()
                .expect("the confirmation exposes exactly one removal action");
        assert_eq!(confirm.accessible_enabled(), Some(true));
        confirm.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::RemoveRoutingBus { track_id: submitted_track_id }]
                if submitted_track_id == track_id
        ));

        apply_routing_bus_event(
            &ui,
            &IdeWorkerEvent::RoutingBusRemoved {
                revision: 7,
                track_id: track_id.to_owned(),
            },
        );
        assert!(!ui.get_routing_bus_removal_visible());
        assert!(ui.get_routing_bus_removal_id().is_empty());
        assert!(ui.get_status_text().contains("project revision 7"));
        ui.global::<IdeActions>()
            .invoke_remove_routing_bus(track_id.into());
        assert_eq!(submitted.borrow().len(), 1);
        assert!(
            ui.get_error_text()
                .contains("removal confirmation is no longer current")
        );
        ui.hide().unwrap();
    }

    #[test]
    fn generated_routing_bus_rename_form_submits_the_current_return_bus() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        let mut routing_bus = audio_track_mixer_row(0, track_id);
        routing_bus.name = "Verb".into();
        routing_bus.display_name = "Verb".into();
        routing_bus.kind = "RETURN".into();
        routing_bus.removable = false;
        routing_bus.activation_toggleable = false;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![routing_bus]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_routing_bus_rename_callback_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.show().unwrap();

        let rename = ElementHandle::find_by_accessible_label(&ui, "Rename")
            .next()
            .expect("the current Return bus exposes renaming");
        assert_eq!(rename.accessible_enabled(), Some(true));
        rename.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_rename_track_visible());
        assert_eq!(ui.get_rename_track_index(), 0);
        assert_eq!(ui.get_rename_track_original_name(), "Verb");
        assert_eq!(ui.get_rename_track_kind(), "RETURN");

        ui.set_rename_track_name("Reverb".into());
        let confirm = ElementHandle::find_by_accessible_label(&ui, "Rename routing bus")
            .next()
            .expect("the visible routing-bus form exposes one confirmation action");
        assert_eq!(confirm.accessible_enabled(), Some(true));
        confirm.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::RenameRoutingBus { track_id: submitted_track_id, name }]
                if submitted_track_id == track_id && name == "Reverb"
        ));
        ui.hide().unwrap();
    }

    #[test]
    fn routing_bus_sources_follow_audio_channels_without_losing_current_selection() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        let channels = vec![
            IdeMixerChannel::for_test(
                "018f47a2-6b2d-7c11-8a44-001122334456",
                "Audio 1",
                IdeMixerChannelKind::Audio,
            ),
            IdeMixerChannel::for_test(
                "018f47a2-6b2d-7c11-8a44-001122334457",
                "Drums",
                IdeMixerChannelKind::Group,
            ),
            IdeMixerChannel::for_test(
                "018f47a2-6b2d-7c11-8a44-001122334458",
                "Verb",
                IdeMixerChannelKind::Return,
            ),
        ];
        set_routing_bus_sources(&ui, &channels);
        assert!(matches!(
            ui.get_routing_bus_sources().iter().collect::<Vec<_>>().as_slice(),
            [source] if source.track_id == "018f47a2-6b2d-7c11-8a44-001122334456"
                && source.name == "Audio 1" && !source.selected
        ));

        let mut selected = ui.get_routing_bus_sources().iter().collect::<Vec<_>>();
        selected[0].selected = true;
        ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(selected))));
        ui.set_routing_bus_selected_source_count(1);
        set_routing_bus_sources(&ui, &channels);
        assert!(ui.get_routing_bus_sources().row_data(0).unwrap().selected);
        assert_eq!(ui.get_routing_bus_selected_source_count(), 1);

        let mut changed = channels;
        changed.push(IdeMixerChannel::for_test(
            "018f47a2-6b2d-7c11-8a44-001122334459",
            "Audio 2",
            IdeMixerChannelKind::Audio,
        ));
        set_routing_bus_sources(&ui, &changed);
        assert_eq!(ui.get_routing_bus_sources().row_count(), 2);
        assert!(
            ui.get_routing_bus_sources()
                .iter()
                .all(|source| !source.selected)
        );
        assert_eq!(ui.get_routing_bus_selected_source_count(), 0);
    }

    #[test]
    fn beginning_a_routing_bus_form_clears_previous_source_selection() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_track_count(2);
        ui.set_routing_bus_kind_index(1);
        ui.set_routing_bus_name("Old return".into());
        ui.set_routing_bus_sources(ModelRc::from(Rc::new(VecModel::from(vec![
            RoutingBusSourceRow {
                index: 0,
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".into(),
                name: "Audio 1".into(),
                selected: true,
            },
        ]))));
        ui.set_routing_bus_selected_source_count(1);
        let weak = ui.as_weak();
        ui.global::<IdeActions>()
            .on_begin_routing_bus_creation(move || {
                if let Some(ui) = weak.upgrade() {
                    begin_routing_bus_creation(&ui);
                }
            });
        ui.show().unwrap();
        ElementHandle::find_by_accessible_label(&ui, "New routing bus")
            .next()
            .expect("the current project exposes routing-bus creation")
            .mock_single_click(PointerEventButton::Left);

        assert!(ui.get_routing_bus_create_visible());
        assert_eq!(ui.get_routing_bus_kind_index(), 0);
        assert_eq!(ui.get_routing_bus_name(), "Group");
        assert_eq!(ui.get_routing_bus_selected_source_count(), 0);
        assert!(
            ui.get_routing_bus_sources()
                .iter()
                .all(|source| !source.selected)
        );
    }

    #[test]
    fn focused_line_edits_consume_project_global_chords() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_export_available(true);
        ui.set_mixer_main_state_available(true);
        ui.set_scene_rename_visible(true);
        ui.show().unwrap();

        let mut matches = ElementHandle::find_by_element_id(&ui, "MainWindow::scene-rename-input");
        let editor = matches
            .next()
            .expect("the visible scene editor must expose its LineEdit");
        assert!(matches.next().is_none(), "the scene editor id is unique");
        editor.mock_single_click(PointerEventButton::Left);

        let closed = Rc::new(Cell::new(0_usize));
        let closed_by_callback = Rc::clone(&closed);
        ui.global::<IdeActions>().on_close_project(move || {
            closed_by_callback.set(closed_by_callback.get() + 1);
        });
        let exports = Rc::new(Cell::new(0_usize));
        let exports_by_callback = Rc::clone(&exports);
        ui.global::<IdeActions>().on_export_project(move || {
            exports_by_callback.set(exports_by_callback.get() + 1);
        });
        dispatch_ide_shortcut(&ui, "w", false, false);
        dispatch_ide_shortcut(&ui, "e", false, false);
        dispatch_ide_shortcut(&ui, "s", true, false);
        assert_eq!(
            closed.get(),
            0,
            "a focused LineEdit owns the primary close chord instead of abandoning its editor"
        );
        assert_eq!(
            exports.get(),
            0,
            "a focused LineEdit owns the primary export chord instead of opening an unrelated native picker"
        );
        assert!(
            !ui.get_save_project_as_visible(),
            "a focused local editor keeps Save As unavailable instead of opening a second project form"
        );

        ui.set_scene_rename_visible(false);
        let mut save_as_entries =
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-entry");
        let save_as_entry = save_as_entries
            .next()
            .expect("dismissing the editor restores the generated Save As entry button");
        assert!(
            save_as_entries.next().is_none(),
            "the restored Save As entry button id remains unique"
        );
        save_as_entry.mock_single_click(PointerEventButton::Left);
        assert!(
            ui.get_save_project_as_visible(),
            "Save As becomes available again after the local editor is dismissed"
        );
    }

    #[test]
    fn rename_buttons_submit_only_the_current_project_name() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_main_state_available(true);
        ui.set_project_name("Source".into());
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_project_rename_callback_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.show().unwrap();

        let mut entries =
            ElementHandle::find_by_element_id(&ui, "MainWindow::rename-project-entry");
        let entry = entries
            .next()
            .expect("the generated Rename Project entry button must be present");
        assert!(
            entries.next().is_none(),
            "the Rename Project entry id is unique"
        );
        entry.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_rename_project_visible());
        assert_eq!(ui.get_rename_project_name(), "Source");
        assert!(
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-entry")
                .next()
                .is_none(),
            "the active Rename Project form owns the project surface"
        );

        let mut cancels =
            ElementHandle::find_by_element_id(&ui, "MainWindow::rename-project-cancel");
        let cancel = cancels
            .next()
            .expect("the visible Rename Project form must expose its cancel button");
        assert!(
            cancels.next().is_none(),
            "the Rename Project cancel id is unique"
        );
        cancel.mock_single_click(PointerEventButton::Left);
        assert!(!ui.get_rename_project_visible());

        let entry = ElementHandle::find_by_element_id(&ui, "MainWindow::rename-project-entry")
            .next()
            .expect("cancelling restores the generated Rename Project entry button");
        entry.mock_single_click(PointerEventButton::Left);

        ui.set_rename_project_name("Renamed session".into());
        let mut submits =
            ElementHandle::find_by_element_id(&ui, "MainWindow::rename-project-submit");
        let submit = submits
            .next()
            .expect("the visible Rename Project form must expose its submission button");
        assert!(
            submits.next().is_none(),
            "the Rename Project submission id is unique"
        );
        submit.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::RenameProject { project_name }] if project_name == "Renamed session"
        ));

        ui.set_app_mode(AppMode::Saving);
        ui.global::<IdeActions>()
            .invoke_rename_project("Ignored while pending".into());
        assert_eq!(
            submitted.borrow().len(),
            1,
            "pending rename state is submission-inert"
        );
        ui.set_app_mode(AppMode::Running);

        apply_event(
            &ui,
            &UiRoutes::default(),
            &Rc::new(ProjectUiState::default()),
            IdeWorkerEvent::ProjectRenamed {
                revision: 2,
                changed: true,
                mixer_changed: false,
                name: "Renamed session".to_owned(),
            },
        );
        assert!(!ui.get_rename_project_visible());
        assert!(ui.get_status_text().contains("Renamed session"));

        apply_event(
            &ui,
            &UiRoutes::default(),
            &Rc::new(ProjectUiState::default()),
            IdeWorkerEvent::ProjectRenamed {
                revision: 3,
                changed: false,
                mixer_changed: true,
                name: "Renamed session".to_owned(),
            },
        );
        assert!(
            ui.get_status_text()
                .contains("live mixer changes were saved")
        );
    }

    #[test]
    fn save_as_buttons_use_the_generated_callback_route() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_main_state_available(true);
        ui.set_project_name("Source".into());
        ui.show().unwrap();

        let mut entries =
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-entry");
        let entry = entries
            .next()
            .expect("the generated Save As entry button must be present");
        assert!(
            entries.next().is_none(),
            "the Save As entry button id is unique"
        );
        entry.mock_single_click(PointerEventButton::Left);
        assert!(ui.get_save_project_as_visible());
        assert_eq!(ui.get_save_project_as_name(), "Source copy");

        let mut cancels =
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-cancel");
        let cancel = cancels
            .next()
            .expect("the generated Save As cancel button must be present");
        assert!(
            cancels.next().is_none(),
            "the Save As cancel button id is unique"
        );
        cancel.mock_single_click(PointerEventButton::Left);
        assert!(
            !ui.get_save_project_as_visible(),
            "the generated cancel button closes only the local Save As form"
        );

        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_choice = Rc::clone(&submitted);
        let worker: WorkerCell = Rc::new(RefCell::new(None));
        bind_project_copy_callback_with(&ui, &worker, move |weak, _, project_name, switch_to| {
            let ui = weak.upgrade().unwrap();
            complete_project_copy_choice_with(
                &ui,
                project_name,
                switch_to,
                ProjectCopyChoice::Selected(PathBuf::from("parent")),
                |command| submitted_by_choice.borrow_mut().push(command),
            );
        });
        let mut restored_entries =
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-entry");
        let restored_entry = restored_entries
            .next()
            .expect("the generated Save As entry button returns after cancellation");
        assert!(
            restored_entries.next().is_none(),
            "the restored Save As entry button id is unique"
        );
        restored_entry.mock_single_click(PointerEventButton::Left);
        let mut submits =
            ElementHandle::find_by_element_id(&ui, "MainWindow::save-project-as-submit");
        let submit = submits
            .next()
            .expect("the generated Save As submission button must be present");
        assert!(
            submits.next().is_none(),
            "the Save As submission button id is unique"
        );
        ui.set_save_project_as_switch_to(false);
        submit.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::SaveProjectAs {
                parent,
                directory_name,
                project_name,
                switch_to,
            }] if parent == &PathBuf::from("parent")
                && directory_name == "Source copy"
                && project_name == "Source copy"
                && !switch_to
        ));
        submit.mock_single_click(PointerEventButton::Left);
        assert_eq!(
            submitted.borrow().len(),
            1,
            "the generated submission button stays inert while Save As is pending"
        );
    }

    fn assert_form_cancellation_shortcut_contract(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Closed);
        ui.set_new_project_visible(true);
        ui.set_new_track_visible(false);
        dispatch_ide_escape(ui, false);
        assert!(
            !ui.get_new_project_visible(),
            "Escape cancels the local closed-project form before a native picker is started"
        );

        ui.set_new_project_visible(true);
        ui.set_app_mode(AppMode::Selecting);
        dispatch_ide_escape(ui, false);
        assert!(
            ui.get_new_project_visible(),
            "Escape cannot dismiss a project form while native folder selection owns the workflow"
        );
        ui.set_new_project_visible(false);

        ui.set_app_mode(AppMode::Running);
        ui.set_new_track_visible(true);
        dispatch_ide_escape(ui, false);
        assert!(
            !ui.get_new_track_visible(),
            "Escape cancels the local running-project audio-track form"
        );

        ui.set_new_track_visible(true);
        dispatch_ide_escape(ui, true);
        assert!(
            ui.get_new_track_visible(),
            "repeated Escape does not reopen or repeatedly mutate an audio-track form"
        );

        ui.set_app_mode(AppMode::AddingTrack);
        dispatch_ide_escape(ui, false);
        assert!(
            ui.get_new_track_visible(),
            "Escape cannot dismiss an audio-track form while durable creation is in flight"
        );
        ui.set_new_track_visible(false);
    }

    fn assert_restore_shortcut_commands(
        submitted: &Rc<RefCell<Vec<IdeWorkerCommand>>>,
        message: &str,
    ) {
        let commands = submitted.borrow();
        assert_eq!(commands.len(), 3, "{message}");
        assert!(matches!(commands[0], IdeWorkerCommand::Undo), "{message}");
        assert!(matches!(commands[1], IdeWorkerCommand::Redo), "{message}");
        assert!(matches!(commands[2], IdeWorkerCommand::Redo), "{message}");
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one generated-window fixture validates the complete native-device interaction contract"
    )]
    fn assert_native_filter_window_contract(ui: &MainWindow) {
        let channels = [
            IdeMixerChannel::for_test("track-a", "Audio", IdeMixerChannelKind::Audio),
            IdeMixerChannel::for_test("group-a", "Group", IdeMixerChannelKind::Group),
            IdeMixerChannel::for_test("return-a", "Return", IdeMixerChannelKind::Return),
        ];
        set_native_devices(ui, &[], &channels);
        let targets = ui.get_native_filter_track_names();
        assert_eq!(targets.row_count(), 3);
        assert_eq!(targets.row_data(0).unwrap(), "Audio");
        assert_eq!(targets.row_data(1).unwrap(), "Group");
        assert_eq!(targets.row_data(2).unwrap(), "Return");

        let project_state = Rc::new(ProjectUiState::default());
        project_state
            .native_filter_tracks
            .borrow_mut()
            .push("track-a".to_owned());
        ui.set_native_devices(ModelRc::from(Rc::new(VecModel::from(vec![
            NativeDeviceRow {
                index: 0,
                chain_id: "chain-a".into(),
                device_instance_id: "device-a".into(),
                track_id: "track-a".into(),
                track_name: "Audio".into(),
                enabled: true,
                can_move_earlier: false,
                can_move_later: true,
                can_duplicate: true,
                kind_index: 0,
                kind_name: "FILTER".into(),
                detail: "1250 Hz · Q 0.707".into(),
                mode_index: 0,
                mode_name: "LOW-PASS".into(),
                cutoff_millihertz: "1250000".into(),
                cutoff_label: "1250 Hz".into(),
                resonance_millionths: "707000".into(),
                resonance_label: "Q 0.707".into(),
                threshold_millidecibels: "".into(),
                ratio_millionths: "".into(),
                attack_microseconds: "".into(),
                release_microseconds: "".into(),
                makeup_millidecibels: "".into(),
            },
            NativeDeviceRow {
                index: 1,
                chain_id: "chain-a".into(),
                device_instance_id: "device-b".into(),
                track_id: "track-a".into(),
                track_name: "Audio".into(),
                enabled: true,
                can_move_earlier: true,
                can_move_later: false,
                can_duplicate: true,
                kind_index: 1,
                kind_name: "COMPRESSOR".into(),
                detail: "-18 dB · 4:1".into(),
                mode_index: 0,
                mode_name: "".into(),
                cutoff_millihertz: "".into(),
                cutoff_label: "".into(),
                resonance_millionths: "".into(),
                resonance_label: "".into(),
                threshold_millidecibels: "-18000".into(),
                ratio_millionths: "4000000".into(),
                attack_microseconds: "10000".into(),
                release_microseconds: "100000".into(),
                makeup_millidecibels: "0".into(),
            },
            NativeDeviceRow {
                index: 2,
                chain_id: "chain-b".into(),
                device_instance_id: "device-c".into(),
                track_id: "track-b".into(),
                track_name: "Bass".into(),
                enabled: true,
                can_move_earlier: false,
                can_move_later: false,
                can_duplicate: true,
                kind_index: 0,
                kind_name: "FILTER".into(),
                detail: "800 Hz · Q 1.0".into(),
                mode_index: 1,
                mode_name: "HIGH-PASS".into(),
                cutoff_millihertz: "800000".into(),
                cutoff_label: "800 Hz".into(),
                resonance_millionths: "1000000".into(),
                resonance_label: "Q 1.0".into(),
                threshold_millidecibels: "".into(),
                ratio_millionths: "".into(),
                attack_microseconds: "".into(),
                release_microseconds: "".into(),
                makeup_millidecibels: "".into(),
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_native_filter_callbacks_with(
            ui,
            &project_state,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.set_app_mode(AppMode::Running);

        let actions = ui.global::<IdeActions>();
        observe_native_device_copy_authority(ui, &project_state, 7);
        actions.invoke_paste_native_device(0);
        assert_eq!(
            ui.get_error_text(),
            "Copy a native device before choosing a paste target"
        );
        actions.invoke_add_native_filter(0, 1, "80000".into(), "2000000".into());
        actions.invoke_update_native_filter(0, false, 1, "90000".into(), "1500000".into());
        actions.invoke_remove_native_filter(0);
        actions.invoke_add_native_compressor(
            0,
            "-24000".into(),
            "2000000".into(),
            "5000".into(),
            "80000".into(),
            "3000".into(),
        );
        actions.invoke_update_native_compressor(
            1,
            false,
            "-12000".into(),
            "6000000".into(),
            "2000".into(),
            "60000".into(),
            "1000".into(),
        );
        actions.invoke_move_native_device(0, 1);
        actions.invoke_duplicate_native_device(1);
        actions.invoke_copy_native_device(1);
        assert!(ui.get_native_device_copy_available());
        assert_eq!(
            ui.get_status_text(),
            "Native device selected. Choose a target in another chain and paste after it."
        );
        observe_native_device_copy_authority(ui, &project_state, 7);
        assert!(ui.get_native_device_copy_available());
        actions.invoke_paste_native_device(0);
        assert_eq!(submitted.borrow().len(), 7);
        assert_eq!(
            ui.get_error_text(),
            "Choose a paste target in another native device chain"
        );
        actions.invoke_paste_native_device(2);
        actions.invoke_move_native_device(0, -1);
        actions.invoke_add_native_filter(0, 0, "19999".into(), "707000".into());
        assert!(ui.get_error_text().contains("Cutoff"));
        actions.invoke_remove_native_filter(-1);

        let commands = submitted.borrow();
        assert_eq!(commands.len(), 8);
        assert!(matches!(
            &commands[0],
            IdeWorkerCommand::EditNativeFilter {
                edit: IdeNativeFilterEdit::Add {
                    track_id,
                    mode: NativeFilterMode::HighPass,
                    cutoff_millihertz: 80_000,
                    resonance_millionths: 2_000_000,
                },
            } if track_id == "track-a"
        ));
        assert!(matches!(
            &commands[1],
            IdeWorkerCommand::EditNativeFilter {
                edit: IdeNativeFilterEdit::Update {
                    chain_id,
                    device_instance_id,
                    enabled: false,
                    mode: NativeFilterMode::HighPass,
                    cutoff_millihertz: 90_000,
                    resonance_millionths: 1_500_000,
                },
            } if chain_id == "chain-a" && device_instance_id == "device-a"
        ));
        assert!(matches!(
            &commands[2],
            IdeWorkerCommand::EditNativeFilter {
                edit: IdeNativeFilterEdit::Remove {
                    chain_id,
                    device_instance_id,
                },
            } if chain_id == "chain-a" && device_instance_id == "device-a"
        ));
        assert!(matches!(
            &commands[3],
            IdeWorkerCommand::EditNativeFilter { edit: IdeNativeFilterEdit::AddCompressor { track_id, threshold_millidecibels: -24_000, ratio_millionths: 2_000_000, attack_microseconds: 5_000, release_microseconds: 80_000, makeup_millidecibels: 3_000 } } if track_id == "track-a"
        ));
        assert!(matches!(
            &commands[4],
            IdeWorkerCommand::EditNativeFilter { edit: IdeNativeFilterEdit::UpdateCompressor { chain_id, device_instance_id, enabled: false, threshold_millidecibels: -12_000, ratio_millionths: 6_000_000, attack_microseconds: 2_000, release_microseconds: 60_000, makeup_millidecibels: 1_000 } } if chain_id == "chain-a" && device_instance_id == "device-b"
        ));
        assert!(matches!(
            &commands[5],
            IdeWorkerCommand::EditNativeFilter { edit: IdeNativeFilterEdit::Move { chain_id, device_instance_id, destination_index: 1 } } if chain_id == "chain-a" && device_instance_id == "device-a"
        ));
        assert!(matches!(
            &commands[6],
            IdeWorkerCommand::EditNativeFilter { edit: IdeNativeFilterEdit::Duplicate { chain_id, device_instance_id } } if chain_id == "chain-a" && device_instance_id == "device-b"
        ));
        assert!(matches!(
            &commands[7],
            IdeWorkerCommand::EditNativeFilter { edit: IdeNativeFilterEdit::CopyTo { source_chain_id, source_device_instance_id, target_chain_id, target_device_instance_id } }
                if source_chain_id == "chain-a"
                    && source_device_instance_id == "device-b"
                    && target_chain_id == "chain-b"
                    && target_device_instance_id == "device-c"
        ));
        drop(commands);
        assert!(ui.get_error_text().contains("no longer available"));

        let mut constrained_rows = (0..ui.get_native_devices().row_count())
            .filter_map(|index| ui.get_native_devices().row_data(index))
            .collect::<Vec<_>>();
        constrained_rows[2].can_duplicate = false;
        ui.set_native_devices(ModelRc::from(Rc::new(VecModel::from(constrained_rows))));
        actions.invoke_duplicate_native_device(2);
        actions.invoke_paste_native_device(2);
        assert_eq!(submitted.borrow().len(), 8);
        assert_eq!(
            ui.get_error_text(),
            "The selected native device chain cannot contain another device"
        );
        observe_native_device_copy_authority(ui, &project_state, 8);
        assert!(!ui.get_native_device_copy_available());
        actions.invoke_paste_native_device(2);
        assert_eq!(submitted.borrow().len(), 8);
        assert_eq!(
            ui.get_error_text(),
            "Copy a native device before choosing a paste target"
        );
        assert_native_filter_pending_modes();

        apply_native_filter_event(
            ui,
            &IdeWorkerEvent::NativeFilterEdited {
                revision: 4,
                changed: true,
                edit: IdeNativeFilterEdit::Remove {
                    chain_id: "chain-a".to_owned(),
                    device_instance_id: "device-a".to_owned(),
                },
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(ui.get_status_text().contains("revision 4"));
        assert!(ui.get_error_text().is_empty());

        assert_native_filter_committed_warning(ui);
    }

    fn assert_automation_take_removal_window_contract(ui: &MainWindow) {
        let take_id = "018f47a2-6b2d-7c11-8a44-0011223344a0";
        let project_state = Rc::new(ProjectUiState::default());
        project_state
            .automation_take_ids
            .borrow_mut()
            .push(take_id.to_owned());
        ui.set_automation_takes(ModelRc::from(Rc::new(VecModel::from(vec![
            AutomationTakeRow {
                id: take_id.into(),
                name: "Main fade".into(),
                enabled: true,
                editable: true,
                range: "TICK 0 to 960".into(),
                target: "Main gain".into(),
                interpolation: "Linear".into(),
                points: "2 points".into(),
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_automation_take_removal_callback_with(
            ui,
            &project_state,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        let actions = ui.global::<IdeActions>();
        ui.set_app_mode(AppMode::Running);
        ui.set_automation_take_remove_confirming_id(take_id.into());
        actions.invoke_remove_automation_take(take_id.into());
        assert!(ui.get_automation_take_remove_confirming_id().is_empty());
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::RemoveAutomationTake { take_id: observed }]
                if observed == take_id
        ));

        ui.set_automation_take_remove_confirming_id(take_id.into());
        ui.set_automation_take_remove_confirming_id(SharedString::default());
        assert!(ui.get_automation_take_remove_confirming_id().is_empty());
        actions.invoke_remove_automation_take(take_id.into());
        assert_eq!(submitted.borrow().len(), 1);
        ui.set_automation_take_remove_confirming_id(take_id.into());
        actions.invoke_remove_automation_take(take_id.into());
        assert_eq!(submitted.borrow().len(), 2);

        ui.set_automation_take_remove_confirming_id("stale".into());
        actions.invoke_remove_automation_take(take_id.into());
        ui.set_app_mode(AppMode::Closed);
        ui.set_automation_take_remove_confirming_id(take_id.into());
        actions.invoke_remove_automation_take(take_id.into());
        ui.set_app_mode(AppMode::Running);
        project_state.automation_take_ids.borrow_mut().clear();
        ui.set_automation_take_remove_confirming_id(take_id.into());
        actions.invoke_remove_automation_take(take_id.into());
        assert_eq!(submitted.borrow().len(), 2);
        assert!(ui.get_error_text().contains("no longer current"));

        apply_automation_take_removal_event(
            ui,
            &IdeWorkerEvent::AutomationTakeRemoving {
                take_id: take_id.to_owned(),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::RemovingAutomationTake);
        apply_automation_take_removal_event(
            ui,
            &IdeWorkerEvent::AutomationTakeRemoved {
                revision: 7,
                take_id: take_id.to_owned(),
            },
        );
        assert!(ui.get_status_text().contains("revision 7"));
        assert!(ui.get_error_text().is_empty());
        apply_automation_take_removal_event(
            ui,
            &IdeWorkerEvent::AutomationTakeRemovalCommittedWithWarning {
                take_id: take_id.to_owned(),
                warning: Box::new(IdeAutomationTakeEditError::TakeUnavailable),
            },
        );
        assert!(ui.get_status_text().contains("committed"));
        assert!(ui.get_error_text().contains("unavailable"));
    }

    fn assert_native_filter_committed_warning(ui: &MainWindow) {
        ui.set_native_filter_add_visible(true);
        ui.set_native_filter_edit_visible(true);
        ui.set_native_filter_edit_index(0);
        ui.set_native_filter_remove_confirming_index(0);
        apply_native_filter_event(
            ui,
            &IdeWorkerEvent::NativeFilterEditCommittedWithWarning {
                edit: IdeNativeFilterEdit::Remove {
                    chain_id: "chain-a".to_owned(),
                    device_instance_id: "device-a".to_owned(),
                },
                warning: Box::new(hyper_desktop_ide::IdeNativeFilterEditError::InvalidIdentity),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_native_filter_add_visible());
        assert!(!ui.get_native_filter_edit_visible());
        assert_eq!(ui.get_native_filter_edit_index(), -1);
        assert_eq!(ui.get_native_filter_remove_confirming_index(), -1);
        assert!(ui.get_status_text().contains("committed"));
        assert!(ui.get_error_text().contains("saved"));
    }

    fn assert_native_filter_pending_modes() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::EditNativeFilter,
            ),
            AppMode::EditingNativeFilter
        );
        assert_eq!(
            mode_after_failure(
                AppMode::EditingNativeFilter,
                IdeWorkerCommandKind::EditNativeFilter,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn next_audio_track_name_uses_the_reopened_structural_count() {
        assert_eq!(next_audio_track_name(2).as_str(), "Audio 3");
        assert_eq!(next_audio_track_name(8).as_str(), "Audio 9");
    }

    #[test]
    fn desktop_limits_match_the_fixed_version_one_playback_domain() {
        let config = application_config().unwrap();
        let limits = config.project_limits();
        assert_eq!(config.maximum_block_frames(), 4_096);
        assert_eq!(limits.audio.max_frames(), 43_200_000);
        assert_eq!(limits.session.media_cache.max_assets, 4_096);
        assert!(
            limits.session.media_cache.max_decoded_sample_bytes
                >= limits.audio.max_decoded_sample_bytes()
        );
    }

    #[test]
    fn recent_project_presentation_routes_by_retained_path() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ui-history-{}-{nonce}",
            std::process::id()
        ));
        let config = fixture.join("config");
        let project = fixture.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let history = ProjectHistory::at(&config);
        let projects = history.remember("Daily Session", &project).unwrap();

        let routes: RecentProjectRoutes = Rc::new(RefCell::new(Vec::new()));
        let presentation = recent_project_presentation(&projects);
        *routes.borrow_mut() = presentation.routes.clone();

        assert_eq!(presentation.names, [SharedString::from("Daily Session")]);
        assert_eq!(
            presentation.display_names,
            [SharedString::from("Daily Session")]
        );
        assert_eq!(presentation.paths.len(), 1);
        assert!(recent_project_command(&routes, -1).is_none());
        assert!(recent_project_command(&routes, 1).is_none());
        let Some(IdeWorkerCommand::OpenRecentProject(path)) = recent_project_command(&routes, 0)
        else {
            panic!("the retained recent-project path must be routed");
        };
        assert_eq!(path, projects[0].path());

        std::fs::remove_dir(project).unwrap();
        std::fs::remove_file(history.document_path()).unwrap();
        std::fs::remove_file(config.join("recent-projects.lock")).unwrap();
        std::fs::remove_dir(config).unwrap();
        std::fs::remove_dir(fixture).unwrap();
    }

    #[test]
    fn meter_presentation_is_bounded_and_preserves_unavailable_state() {
        assert_eq!(meter_presentation(false, 0.0), (0.0, "— dBFS".to_owned()));
        assert_eq!(
            meter_presentation(true, f32::NAN),
            (0.0, "— dBFS".to_owned())
        );
        assert_eq!(
            meter_presentation(true, FINAL_OUTPUT_METER_FLOOR_DBFS),
            (0.0, "≤ -60.0 dBFS".to_owned())
        );
        assert_eq!(meter_presentation(true, 0.0), (1.0, "0.0 dBFS".to_owned()));
        assert!((meter_presentation(true, 6.0).0 - 1.0).abs() <= f32::EPSILON);

        assert_eq!(
            output_peak_hold_presentation(false, 0.0),
            ("hold —".to_owned(), false)
        );
        assert_eq!(
            output_peak_hold_presentation(true, f32::NAN),
            ("hold —".to_owned(), false)
        );
        assert_eq!(
            output_peak_hold_presentation(true, FINAL_OUTPUT_METER_FLOOR_DBFS),
            ("hold ≤ -60.0".to_owned(), false)
        );
        assert_eq!(
            output_peak_hold_presentation(true, 0.0),
            ("hold 0.0".to_owned(), true)
        );
    }

    #[test]
    fn accepted_input_meter_binding_presents_raw_capture_values_and_clears_with_monitoring() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let observation = IdeInputMeterObservation::display_for_ui_test(Some(0.5), Some(0.25));

        apply_input_meter_observation(&ui, &observation);
        assert!((ui.get_input_meter_left_level() - 0.899_657).abs() < 0.000_1);
        assert!((ui.get_input_meter_right_level() - 0.799_313).abs() < 0.000_1);
        assert_eq!(ui.get_input_meter_left_reading().as_str(), "-6.0 dBFS");
        assert_eq!(ui.get_input_meter_right_reading().as_str(), "-12.0 dBFS");

        reset_monitoring(&ui);
        assert_eq!(ui.get_input_meter_left_level().to_bits(), 0.0_f32.to_bits());
        assert_eq!(
            ui.get_input_meter_right_level().to_bits(),
            0.0_f32.to_bits()
        );
        assert_eq!(ui.get_input_meter_left_reading().as_str(), "— dBFS");
        assert_eq!(ui.get_input_meter_right_reading().as_str(), "— dBFS");
    }

    #[test]
    fn mixer_strip_meter_presentation_preserves_decaying_sample_peak_values() {
        assert_eq!(
            mixer_strip_meter_presentation(false, 0.0),
            (0.0, "—".to_owned())
        );
        assert_eq!(
            mixer_strip_meter_presentation(true, f32::NAN),
            (0.0, "—".to_owned())
        );
        assert_eq!(
            mixer_strip_meter_presentation(true, -30.0),
            (0.5, "-30.0".to_owned())
        );
        assert_eq!(
            mixer_strip_meter_presentation(true, MIXER_STRIP_METER_FLOOR_DBFS),
            (0.0, "≤ -60".to_owned())
        );
        assert_eq!(
            mixer_strip_meter_presentation(true, 0.0),
            (1.0, "0.0".to_owned())
        );
        assert_eq!(
            mixer_strip_meter_presentation(true, 6.0),
            (1.0, "6.0".to_owned())
        );
    }

    #[test]
    fn missing_strip_meter_snapshot_clears_readings_without_changing_mixer_controls() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let mut row = audio_track_mixer_row(0, "track-a");
        row.gain_percent = 125.0;
        row.pan_percent = -25.0;
        row.meter_left_level = 0.75;
        row.meter_right_level = 0.5;
        row.meter_left_reading = "-2.5".into();
        row.meter_right_reading = "-6.0".into();
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![row]))));

        apply_mixer_strip_meter_observation(
            &ui,
            &IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
                PlaybackRecoveryTransportState::Stopped,
                0,
            )),
        );

        let row = ui.get_mixer_channels().row_data(0).unwrap();
        assert_eq!(row.gain_percent.to_bits(), 125.0_f32.to_bits());
        assert_eq!(row.pan_percent.to_bits(), (-25.0_f32).to_bits());
        assert_eq!(row.meter_left_level.to_bits(), 0.0_f32.to_bits());
        assert_eq!(row.meter_right_level.to_bits(), 0.0_f32.to_bits());
        assert_eq!(row.meter_left_reading.as_str(), "—");
        assert_eq!(row.meter_right_reading.as_str(), "—");
    }

    #[test]
    fn unavailable_loudness_values_retain_their_units() {
        assert_eq!(format_loudness(None, "LUFS"), "— LUFS");
        assert_eq!(format_loudness(None, "LU"), "— LU");
        assert_eq!(format_maximum_true_peak(None), "— dBTP");
    }

    #[test]
    fn transport_clock_uses_authoritative_frames_without_rounding_ahead() {
        assert_eq!(format_transport_clock(0, 48_000), "00:00:00.000");
        assert_eq!(format_transport_clock(71_999, 48_000), "00:00:01.499");
        assert_eq!(format_transport_clock(72_000, 48_000), "00:00:01.500");
        assert_eq!(format_transport_clock(172_800_000, 48_000), "01:00:00.000");
        assert_eq!(format_transport_clock(1, 0), "—");
    }

    #[test]
    fn normalized_seek_maps_exact_endpoints_and_nearest_frames() {
        assert_eq!(seek_percent_to_frame(0.0, 43_200_000), Ok(0));
        assert_eq!(seek_percent_to_frame(50.0, 43_200_000), Ok(21_600_000));
        assert_eq!(seek_percent_to_frame(100.0, 43_200_000), Ok(43_200_000));
        assert_eq!(seek_percent_to_frame(33.3333, 3), Ok(1));
        assert!(seek_percent_to_frame(f32::NAN, 100).is_err());
        assert!(seek_percent_to_frame(100.1, 100).is_err());
    }

    #[test]
    fn transport_progress_is_bounded_by_the_compiled_endpoint() {
        assert_eq!(frame_progress_percent(0, 0).to_bits(), 0.0_f32.to_bits());
        assert_eq!(
            frame_progress_percent(25, 100).to_bits(),
            25.0_f32.to_bits()
        );
        assert_eq!(
            frame_progress_percent(150, 100).to_bits(),
            100.0_f32.to_bits()
        );
    }

    #[test]
    fn return_to_start_preserves_the_observed_transport_state() {
        assert_eq!(
            return_to_start_command(),
            TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            }
        );
    }

    #[test]
    fn scene_creation_requires_an_explicit_valid_launch_timing() {
        assert!(scene_capacity_available(MAX_IDE_SCENES - 1));
        assert!(!scene_capacity_available(MAX_IDE_SCENES));
        assert!(matches!(
            scene_creation_command(" Verse ", 0, "ignored"),
            Ok(IdeWorkerCommand::AddScene {
                name,
                quantization: IdeSceneLaunchQuantization::Immediate,
            }) if name == "Verse"
        ));
        assert!(matches!(
            scene_creation_command("Grid", 1, " 960 "),
            Ok(IdeWorkerCommand::AddScene {
                name,
                quantization: IdeSceneLaunchQuantization::Grid { quantum_ticks: 960 },
            }) if name == "Grid"
        ));
        assert!(scene_creation_command("", 0, "").is_err());
        assert!(scene_creation_command(&"s".repeat(121), 0, "").is_err());
        assert!(scene_creation_command("Grid", 1, "0").is_err());
        assert!(scene_creation_command("Grid", 1, "1.5").is_err());
        assert!(scene_creation_command("Grid", 2, "960").is_err());
    }

    #[test]
    fn scene_slot_selection_never_invents_a_clip() {
        assert_eq!(
            selected_scene_slot_edit(0, -1, ["clip-a"].into_iter()),
            Ok(IdeSceneSlotEdit::Continue)
        );
        assert_eq!(
            selected_scene_slot_edit(1, -1, ["clip-a"].into_iter()),
            Ok(IdeSceneSlotEdit::Stop)
        );
        assert_eq!(
            selected_scene_slot_edit(2, 1, ["clip-a", "clip-b"].into_iter()),
            Ok(IdeSceneSlotEdit::Play {
                clip_id: "clip-b".to_owned(),
            })
        );
        assert!(selected_scene_slot_edit(2, -1, ["clip-a"].into_iter()).is_err());
        assert!(selected_scene_slot_edit(2, 1, ["clip-a"].into_iter()).is_err());
        assert!(selected_scene_slot_edit(3, 0, ["clip-a"].into_iter()).is_err());
    }

    #[test]
    fn scene_slot_no_op_detection_compares_the_exact_action() {
        assert!(scene_slot_action_matches(
            &IdeSceneTrackAction::Continue,
            &IdeSceneSlotEdit::Continue
        ));
        assert!(scene_slot_action_matches(
            &IdeSceneTrackAction::Stop,
            &IdeSceneSlotEdit::Stop
        ));
        assert!(scene_slot_action_matches(
            &IdeSceneTrackAction::Play {
                clip_id: "clip-a".to_owned(),
            },
            &IdeSceneSlotEdit::Play {
                clip_id: "clip-a".to_owned(),
            }
        ));
        assert!(!scene_slot_action_matches(
            &IdeSceneTrackAction::Play {
                clip_id: "clip-a".to_owned(),
            },
            &IdeSceneSlotEdit::Play {
                clip_id: "clip-b".to_owned(),
            }
        ));
    }

    #[test]
    fn scene_availability_is_presented_without_overstating_launchability() {
        assert_eq!(
            scene_availability_presentation(IdeSceneAvailability::Ready),
            ("Ready", true)
        );
        assert_eq!(
            scene_availability_presentation(IdeSceneAvailability::Disabled),
            ("Disabled", false)
        );
        assert_eq!(
            scene_availability_presentation(IdeSceneAvailability::ConsolidationRequired),
            ("Consolidation required", false)
        );
        assert_eq!(
            scene_availability_presentation(IdeSceneAvailability::NotPrepared),
            ("Audio unavailable", false)
        );
    }

    #[test]
    fn scene_runtime_presentation_distinguishes_queued_and_current_state() {
        assert_eq!(
            scene_runtime_presentation(1, Some(1), Some(48_000), Some(0), 48_000),
            (true, false, "Queued for 00:00:01.000".to_owned())
        );
        assert_eq!(
            scene_runtime_presentation(0, Some(0), Some(96_000), Some(0), 48_000),
            (true, true, "Current · queued for 00:00:02.000".to_owned())
        );
        assert_eq!(
            scene_runtime_presentation(2, None, None, Some(2), 48_000),
            (false, true, "Current".to_owned())
        );
        assert_eq!(
            scene_runtime_presentation(3, None, None, None, 48_000),
            (false, false, String::new())
        );
    }

    #[test]
    fn scene_launch_evidence_is_retained_only_within_one_playback_session() {
        let observed = IdeSceneLaunchState::default();
        let mut evidence = SceneLaunchEvidence::default();

        assert!(evidence.observe(7, Some(observed), false));
        assert!(!evidence.observe(7, None, false));
        assert_eq!(evidence.state, Some(observed));

        assert!(evidence.observe(8, None, false));
        assert_eq!(evidence.state, None);
        assert_eq!(evidence.playback_session_generation, Some(8));
    }

    #[test]
    fn scene_content_change_clears_unreplaced_launch_evidence() {
        let observed = IdeSceneLaunchState::default();
        let mut evidence = SceneLaunchEvidence::default();

        assert!(evidence.observe(11, Some(observed), false));
        assert!(evidence.observe(11, None, true));
        assert_eq!(evidence.state, None);
    }

    #[test]
    fn scene_display_names_are_unicode_safe_and_bounded() {
        assert_eq!(scene_display_name("Verse"), "Verse");
        let display = scene_display_name("Ambient interlude with instruments");
        assert_eq!(display.chars().count(), MAX_VISIBLE_SCENE_NAME_CHARS);
        assert!(display.ends_with('…'));
        let unicode = scene_display_name("Élévation très longue scène musicale");
        assert_eq!(unicode.chars().count(), MAX_VISIBLE_SCENE_NAME_CHARS);
        assert!(unicode.ends_with('…'));
    }

    #[test]
    fn scene_launch_requires_current_ready_running_row() {
        let scenes = ModelRc::from(Rc::new(VecModel::from(vec![
            SceneRow {
                index: 0,
                id: "scene-a".into(),
                name: "Verse".into(),
                display_name: "Verse".into(),
                availability: "Ready".into(),
                runtime_state: "".into(),
                active: true,
                removable: true,
                launchable: true,
                queued: false,
                current: false,
            },
            SceneRow {
                index: 1,
                id: "scene-b".into(),
                name: "Break".into(),
                display_name: "Break".into(),
                availability: "Disabled".into(),
                runtime_state: "".into(),
                active: false,
                removable: false,
                launchable: false,
                queued: false,
                current: false,
            },
        ])));

        assert_eq!(validate_scene_launch(AppMode::Running, 0, &scenes), Ok(0));
        assert_eq!(
            validate_scene_launch(AppMode::CapturingScenePerformance, 0, &scenes),
            Ok(0)
        );
        assert_eq!(
            validate_scene_launch(AppMode::Closed, 0, &scenes),
            Err(SceneLaunchFailure::NotReady)
        );
        assert_eq!(
            validate_scene_launch(AppMode::Running, 1, &scenes),
            Err(SceneLaunchFailure::NotReady)
        );
        assert_eq!(
            validate_scene_launch(AppMode::Running, 2, &scenes),
            Err(SceneLaunchFailure::StaleSelection)
        );
        assert_eq!(
            validate_scene_launch(AppMode::Running, -1, &scenes),
            Err(SceneLaunchFailure::InvalidSelection)
        );
    }

    #[test]
    fn scene_performance_capture_ui_lifecycle_reports_committed_boundaries() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();

        apply_scene_performance_capture_event(
            &ui,
            &IdeWorkerEvent::ScenePerformanceCaptureStarted {
                maximum_launch_count: 4,
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::CapturingScenePerformance);
        assert_eq!(ui.get_scene_performance_capture_launch_count(), 0);
        assert!(ui.get_status_text().contains("transport start is queued"));

        apply_scene_performance_capture_event(
            &ui,
            &IdeWorkerEvent::ScenePerformanceCaptureProgress { launch_count: 1 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::CapturingScenePerformance);
        assert_eq!(ui.get_scene_performance_capture_launch_count(), 1);
        assert!(ui.get_status_text().contains("1 committed grid launch"));

        apply_scene_performance_capture_event(
            &ui,
            &IdeWorkerEvent::ScenePerformanceCaptureStopping { launch_count: 1 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::StoppingScenePerformance);
        assert!(ui.get_status_text().contains("stopped audio cycle"));

        apply_scene_performance_capture_event(
            &ui,
            &IdeWorkerEvent::ScenePerformanceCaptureSaving { launch_count: 1 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::SavingScenePerformance);
        assert!(ui.get_status_text().contains("one atomic arrangement edit"));

        apply_scene_performance_capture_event(
            &ui,
            &IdeWorkerEvent::ScenePerformanceCaptureSaved {
                revision: 7,
                launch_count: 1,
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert_eq!(ui.get_scene_performance_capture_launch_count(), 0);
        assert!(ui.get_status_text().contains("project revision 7"));

        assert_eq!(
            scene_performance_capture_submission_mode(AppMode::Running),
            AppMode::CapturingScenePerformance
        );
        assert_eq!(
            scene_performance_capture_submission_mode(AppMode::CapturingScenePerformance),
            AppMode::StoppingScenePerformance
        );
        assert_eq!(
            mode_after_failure(
                AppMode::StoppingScenePerformance,
                IdeWorkerCommandKind::CaptureScenePerformance,
            ),
            AppMode::CapturingScenePerformance
        );
    }

    #[test]
    fn mixer_automation_capture_ui_lifecycle_preserves_generic_take_save_boundaries() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_automation_capture_visible(true);
        ui.set_automation_capture_name("Live main gain".into());

        apply_mixer_automation_capture_event(
            &ui,
            &IdeWorkerEvent::MixerAutomationCaptureStarted {
                maximum_point_count: 4,
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::CapturingMixerAutomation);
        assert_eq!(ui.get_mixer_automation_capture_point_count(), 0);
        assert_eq!(ui.get_mixer_automation_capture_maximum_point_count(), 4);
        assert!(ui.get_status_text().contains("transport start is queued"));

        apply_mixer_automation_capture_event(
            &ui,
            &IdeWorkerEvent::MixerAutomationCaptureProgress { point_count: 2 },
        );
        assert_eq!(ui.get_mixer_automation_capture_point_count(), 2);
        assert!(ui.get_status_text().contains("2 committed points"));

        apply_mixer_automation_capture_event(
            &ui,
            &IdeWorkerEvent::MixerAutomationCaptureStopping { point_count: 2 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::StoppingMixerAutomation);
        assert!(ui.get_status_text().contains("stopped audio cycle"));

        apply_mixer_automation_capture_event(
            &ui,
            &IdeWorkerEvent::MixerAutomationCaptureSaving { point_count: 2 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::SavingMixerAutomation);
        apply_automation_take_creation_event(&ui, &IdeWorkerEvent::AutomationTakeAdding);
        assert_eq!(
            ui.get_app_mode(),
            AppMode::SavingMixerAutomation,
            "the generic mutation event must not erase capture ownership"
        );
        assert!(ui.get_status_text().contains("Persisting"));

        apply_automation_take_creation_event(
            &ui,
            &IdeWorkerEvent::AutomationTakeAdded {
                revision: 9,
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
                name: "Live main gain".to_owned(),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_automation_capture_visible());
        assert!(ui.get_automation_capture_name().is_empty());
        assert!(ui.get_status_text().contains("project revision 9"));

        assert_eq!(
            mixer_automation_capture_submission_mode(AppMode::Running),
            AppMode::CapturingMixerAutomation
        );
        assert_eq!(
            mixer_automation_capture_submission_mode(AppMode::CapturingMixerAutomation),
            AppMode::StoppingMixerAutomation
        );
        assert_eq!(
            mode_after_failure(
                AppMode::StoppingMixerAutomation,
                IdeWorkerCommandKind::CaptureMixerAutomation,
            ),
            AppMode::CapturingMixerAutomation
        );
        assert_eq!(
            mode_after_failure(
                AppMode::SavingMixerAutomation,
                IdeWorkerCommandKind::AddAutomationTake,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn scene_rename_command_normalizes_name_and_preserves_identity() {
        assert!(matches!(
            scene_rename_command("scene-a", "Intro", " Verse A "),
            Ok(Some(IdeWorkerCommand::RenameScene { scene_id, name }))
                if scene_id == "scene-a" && name == "Verse A"
        ));
        assert!(matches!(
            scene_rename_command("scene-a", "Intro", " Intro "),
            Ok(None)
        ));
        assert!(scene_rename_command("scene-a", "Intro", "   ").is_err());
        assert!(scene_rename_command("scene-a", "Intro", &"é".repeat(121)).is_err());
        assert!(scene_rename_command("scene-a", "Intro", &"é".repeat(120)).is_ok());
        assert_eq!(
            mode_after_successful_submission(AppMode::Running, IdeWorkerCommandKind::RenameScene),
            AppMode::RenamingScene
        );
        assert_eq!(
            mode_after_failure(AppMode::RenamingScene, IdeWorkerCommandKind::RenameScene),
            AppMode::Running
        );
    }

    #[test]
    fn a_typed_tempo_becomes_a_command_only_when_the_contract_accepts_it() {
        assert!(matches!(
            tempo_command("120"),
            Ok(IdeWorkerCommand::SetTempo {
                microseconds_per_quarter: 500_000
            })
        ));
        assert!(
            matches!(
                tempo_command("  140.5  "),
                Ok(IdeWorkerCommand::SetTempo {
                    microseconds_per_quarter: 427_046
                })
            ),
            "surrounding space is trimmed and the figure rounds to the stored integer"
        );

        for refused in ["", "fast", "0", "-120", "0.001"] {
            assert!(
                tempo_command(refused).is_err(),
                "{refused:?} is reported rather than clamped"
            );
        }
        assert_eq!(
            tempo_command("abc").unwrap_err(),
            "\"abc\" is not a tempo",
            "and the message names what was typed"
        );
    }

    #[test]
    fn the_musical_readout_names_the_bar_and_beat_under_the_playhead() {
        let timeline = ProjectTimeline {
            sample_rate_hz: 48_000,
            tempo_events: vec![TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_000,
            }],
            meter_events: vec![MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            }],
            ..ProjectTimeline::default()
        };
        assert_eq!(format_musical_position(&timeline, 0), "1|1|000");
        assert_eq!(
            format_musical_position(&timeline, 24_000),
            "1|2|000",
            "one beat at 120 bpm is 24000 frames at 48 kHz"
        );
        assert_eq!(format_musical_position(&timeline, 96_000), "2|1|000");
        assert!(
            format_musical_position(&timeline, 25_000).starts_with("1|2|"),
            "a frame between beats still names the beat it is inside"
        );

        assert_eq!(
            format_musical_position(&ProjectTimeline::default(), 0),
            "",
            "a project whose timeline is not loaded shows nothing rather than a wrong bar"
        );
    }

    #[test]
    fn a_typed_time_signature_becomes_a_command_only_when_the_contract_accepts_it() {
        assert!(matches!(
            meter_command("3/4"),
            Ok(IdeWorkerCommand::SetMeter {
                numerator: 3,
                denominator: 4
            })
        ));
        assert!(
            matches!(
                meter_command("  6 / 8 "),
                Ok(IdeWorkerCommand::SetMeter {
                    numerator: 6,
                    denominator: 8
                })
            ),
            "surrounding space is trimmed on both sides of the slash"
        );

        for refused in ["", "4", "4-4", "0/4", "256/4", "4/3", "4/0", "4/512", "x/y"] {
            assert!(
                meter_command(refused).is_err(),
                "{refused:?} is reported rather than accepted"
            );
        }
        assert_eq!(
            meter_command("4-4").unwrap_err(),
            "\"4-4\" is not a time signature"
        );
        assert_eq!(
            meter_command("4/3").unwrap_err(),
            "4/3 is not a time signature this project can hold",
            "a well-formed but unsupported denominator says so specifically"
        );
    }

    #[test]
    fn timeline_marker_inputs_preserve_the_project_domain_and_marker_kind() {
        assert_eq!(timeline_tick(" 7680 "), Ok(7_680));
        assert!(timeline_tick("-1").is_err());
        assert!(timeline_tick("9007199254740992").is_err());
        assert!(timeline_tick("bar five").is_err());

        assert!(matches!(timeline_tempo("120"), Ok(500_000)));
        assert!(matches!(timeline_meter("6/8"), Ok((6, 8))));
    }

    #[test]
    fn navigation_marker_controls_preserve_current_identity_and_transport_intent() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::EditTimelineMarker,
            ),
            AppMode::Saving
        );
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::EditTimelineMarker),
            AppMode::Running
        );
        assert_eq!(
            command_status(IdeWorkerCommandKind::EditTimelineMarker),
            "Preserving mixer state, saving the navigation marker, and reopening project..."
        );
        assert_eq!(
            command_status(IdeWorkerCommandKind::LocateTimelineMarker),
            "Navigation-marker locate queued…"
        );
        assert_eq!(normalize_timeline_marker_name("  Opening  "), Ok("Opening"));
        assert!(normalize_timeline_marker_name(" ").is_err());
        assert!(normalize_timeline_marker_name(&"x".repeat(121)).is_err());
        assert!(timeline_marker_form_is_current(
            AppMode::Running,
            true,
            "marker-a",
            "marker-a"
        ));
        assert!(!timeline_marker_form_is_current(
            AppMode::Saving,
            true,
            "marker-a",
            "marker-a"
        ));

        let locate = timeline_marker_locate_command(
            AppMode::Running,
            "project-a",
            2,
            "marker-a",
            Some("marker-a"),
            false,
        )
        .expect("the current marker must locate without starting playback");
        assert!(matches!(
            locate,
            IdeWorkerCommand::LocateTimelineMarker {
                project_id,
                index: 2,
                marker_id,
                state: LocateState::Preserve,
            } if project_id == "project-a" && marker_id == "marker-a"
        ));
        assert!(matches!(
            timeline_marker_locate_command(
                AppMode::Running,
                "project-a",
                2,
                "marker-a",
                Some("marker-a"),
                true,
            ),
            Ok(IdeWorkerCommand::LocateTimelineMarker {
                state: LocateState::Rolling,
                ..
            })
        ));
        assert!(
            timeline_marker_locate_command(
                AppMode::Running,
                "project-a",
                2,
                "marker-a",
                Some("other-marker"),
                false,
            )
            .is_err()
        );
        assert!(
            timeline_marker_locate_command(
                AppMode::Running,
                "project-a",
                -1,
                "marker-a",
                Some("marker-a"),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn committed_navigation_marker_warning_revokes_all_marker_form_state() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::Saving);
        ui.set_timeline_marker_add_visible(true);
        ui.set_timeline_marker_rename_visible(true);
        ui.set_timeline_marker_move_visible(true);
        ui.set_timeline_marker_id("marker-a".into());
        ui.set_timeline_marker_source_tick("1920".into());
        ui.set_timeline_marker_tick("3840".into());
        ui.set_timeline_marker_name("Verse".into());
        ui.set_timeline_marker_remove_confirming_id("marker-a".into());

        apply_event(
            &ui,
            &UiRoutes::default(),
            &Rc::new(ProjectUiState::default()),
            IdeWorkerEvent::TimelineMarkerCommittedWithWarning {
                warning: Box::new(IdeTempoEditError::TimelineResourceMissing),
            },
        );

        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_timeline_marker_add_visible());
        assert!(!ui.get_timeline_marker_rename_visible());
        assert!(!ui.get_timeline_marker_move_visible());
        assert!(ui.get_timeline_marker_id().is_empty());
        assert!(ui.get_timeline_marker_source_tick().is_empty());
        assert!(ui.get_timeline_marker_tick().is_empty());
        assert!(ui.get_timeline_marker_name().is_empty());
        assert!(ui.get_timeline_marker_remove_confirming_id().is_empty());
        assert!(ui.get_status_text().is_empty());
        assert!(ui.get_error_text().contains("navigation marker was saved"));
    }

    #[test]
    fn navigation_marker_removal_confirmation_closes_timeline_map_forms() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::Running);
        ui.set_timeline_map_visible(true);
        ui.set_timeline_navigation_markers(ModelRc::from(Rc::new(VecModel::from(vec![
            TimelineNavigationMarkerRow {
                index: 0,
                id: "marker-a".into(),
                tick: "1920".into(),
                position: "1|2|000".into(),
                name: "Verse".into(),
            },
        ]))));
        ui.set_timeline_tempo_add_visible(true);
        ui.set_timeline_tempo_edit_visible(true);
        ui.set_timeline_tempo_move_visible(true);
        ui.set_timeline_meter_add_visible(true);
        ui.set_timeline_meter_edit_visible(true);
        ui.set_timeline_meter_move_visible(true);
        ui.show().unwrap();

        let remove = ElementHandle::find_by_accessible_label(&ui, "Remove navigation marker Verse")
            .next()
            .expect("the rendered navigation marker exposes removal");
        assert_eq!(remove.accessible_enabled(), Some(true));
        remove.mock_single_click(PointerEventButton::Left);

        assert!(!ui.get_timeline_tempo_add_visible());
        assert!(!ui.get_timeline_tempo_edit_visible());
        assert!(!ui.get_timeline_tempo_move_visible());
        assert!(!ui.get_timeline_meter_add_visible());
        assert!(!ui.get_timeline_meter_edit_visible());
        assert!(!ui.get_timeline_meter_move_visible());
        assert_eq!(ui.get_timeline_marker_remove_confirming_id(), "marker-a");
        ui.hide().unwrap();
    }

    #[test]
    fn timeline_marker_rows_show_the_stored_maps_in_musical_order() {
        let timeline = ProjectTimeline {
            sample_rate_hz: 48_000,
            tempo_events: vec![
                TempoEvent {
                    tick: 0,
                    microseconds_per_quarter: 500_000,
                },
                TempoEvent {
                    tick: 7_680,
                    microseconds_per_quarter: 400_000,
                },
            ],
            meter_events: vec![
                MeterEvent {
                    tick: 0,
                    numerator: 4,
                    denominator: 4,
                },
                MeterEvent {
                    tick: 7_680,
                    numerator: 3,
                    denominator: 4,
                },
            ],
            ..ProjectTimeline::default()
        };
        let tempos = timeline_tempo_marker_rows(&timeline);
        assert_eq!(tempos.len(), 2);
        assert!(tempos[0].origin);
        assert_eq!(tempos[1].tick, "7680");
        assert_eq!(tempos[1].position, "2|1|000");
        assert_eq!(tempos[1].bpm, "150");

        let meters = timeline_meter_marker_rows(&timeline);
        assert_eq!(meters.len(), 2);
        assert!(meters[0].origin);
        assert_eq!(meters[1].signature, "3/4");
        assert_eq!(meters[1].position, "2|1|000");
    }

    /// The seam the review found untested: saving must write the marks the
    /// session holds, and marking must never be a durable transaction.
    #[test]
    fn saving_writes_the_loop_the_session_holds_and_marking_does_not() {
        let state: ProjectUiStateCell = Rc::new(ProjectUiState::default());
        assert!(
            matches!(project_save_command(&state), IdeWorkerCommand::SaveProject),
            "a project with no marks saves the mixer alone"
        );

        state.loop_marks.set(LoopMarks {
            start: Some(48_000),
            end: Some(96_000),
        });
        state.loop_engaged.set(true);
        assert!(
            matches!(
                project_save_command(&state),
                IdeWorkerCommand::SetArrangementLoopRange {
                    range: Some(IdeLoopRangeFrames {
                        start_frame: 48_000,
                        end_frame: 96_000,
                        enabled: true,
                    })
                }
            ),
            "and a marked loop is written with the mixer in one transaction"
        );

        state.loop_engaged.set(false);
        assert!(
            matches!(
                project_save_command(&state),
                IdeWorkerCommand::SetArrangementLoopRange {
                    range: Some(IdeLoopRangeFrames { enabled: false, .. })
                }
            ),
            "a disengaged loop is saved as disengaged"
        );

        state.loop_marks.set(LoopMarks {
            start: Some(48_000),
            end: None,
        });
        assert!(
            matches!(
                project_save_command(&state),
                IdeWorkerCommand::SetArrangementLoopRange { range: None }
            ),
            "one mark clears any stored range rather than writing half of one"
        );
    }

    #[test]
    fn a_time_signature_reads_numerator_over_denominator() {
        assert_eq!(format_meter(6, 4), "6/4");
        assert_eq!(format_meter(4, 6), "4/6");
        assert_ne!(
            format_meter(6, 4),
            format_meter(4, 6),
            "a 6/4 project must not display as 4/6"
        );
        assert_eq!(format_meter(3, 4), "3/4");
        assert_eq!(format_meter(6, 8), "6/8");
    }

    #[test]
    fn the_tempo_control_closes_once_any_audio_is_bound_to_the_timeline() {
        assert!(
            tempo_editable(0, 0),
            "a project with no audio on the timeline can still choose its tempo"
        );
        assert!(
            !tempo_editable(1, 0),
            "one placed clip pins the tempo, because Hyper does not time-stretch"
        );
        assert!(
            !tempo_editable(0, 1),
            "and so does one scene slot that plays a clip, which the compiler              either refuses outright or silently marks unavailable"
        );
        assert!(!tempo_editable(9, 9));
    }

    #[test]
    fn only_scene_slots_that_play_a_clip_pin_the_tempo() {
        use hyper_desktop_ide::IdeSceneTrackSlot;

        assert_eq!(scene_clip_count(&[]), 0);

        let scene = |slots: Vec<IdeSceneTrackSlot>| IdeScene::for_test("scene", "Scene", slots);
        assert_eq!(
            scene_clip_count(&[scene(vec![
                IdeSceneTrackSlot::for_test("t1", "Track 1", IdeSceneTrackAction::Stop),
                IdeSceneTrackSlot::for_test("t2", "Track 2", IdeSceneTrackAction::Continue),
            ])]),
            0,
            "stop and continue slots carry no audio, so they do not pin the tempo"
        );
        assert_eq!(
            scene_clip_count(&[scene(vec![
                IdeSceneTrackSlot::for_test("t1", "Track 1", IdeSceneTrackAction::Stop),
                IdeSceneTrackSlot::for_test(
                    "t2",
                    "Track 2",
                    IdeSceneTrackAction::Play {
                        clip_id: "clip".to_owned()
                    }
                ),
            ])]),
            1,
            "a slot that plays a clip does"
        );
        assert_eq!(
            scene_clip_count(&[
                scene(vec![IdeSceneTrackSlot::for_test(
                    "t1",
                    "Track 1",
                    IdeSceneTrackAction::Play {
                        clip_id: "a".to_owned()
                    }
                )]),
                scene(vec![IdeSceneTrackSlot::for_test(
                    "t2",
                    "Track 2",
                    IdeSceneTrackAction::Play {
                        clip_id: "b".to_owned()
                    }
                )]),
            ]),
            2,
            "and they are counted across every scene, not just the first"
        );
    }

    #[test]
    fn the_displayed_tempo_comes_from_the_stored_integer() {
        assert_eq!(format_tempo_bpm(500_000), "120");
        assert_eq!(format_tempo_bpm(1_000_000), "60");
        assert_eq!(
            format_tempo_bpm(427_046),
            "140.5",
            "a tempo that does not divide evenly still reads back as what was asked"
        );
        assert_eq!(
            format_tempo_bpm(499_999),
            "120",
            "and a neighbouring integer rounds to the same two decimals"
        );
        assert_ne!(
            format_tempo_bpm(400_000),
            format_tempo_bpm(500_000),
            "different stored tempos display differently"
        );
    }

    #[test]
    fn loop_marks_only_become_a_range_once_both_exist_and_the_end_is_later() {
        let none = LoopMarks::default();
        assert!(none.range().is_none());
        assert_eq!(none.summary(48_000), "");

        let in_only = LoopMarks {
            start: Some(48_000),
            end: None,
        };
        assert!(in_only.range().is_none(), "one mark is not a range");
        assert!(in_only.summary(48_000).starts_with("LOOP IN"));

        let out_only = LoopMarks {
            start: None,
            end: Some(96_000),
        };
        assert!(out_only.range().is_none());
        assert!(out_only.summary(48_000).starts_with("LOOP OUT"));

        let inverted = LoopMarks {
            start: Some(96_000),
            end: Some(48_000),
        };
        assert!(
            inverted.range().is_none(),
            "an end before the start is refused rather than swapped"
        );
        assert!(
            inverted.summary(48_000).contains("IS NOT AFTER IN"),
            "and says so instead of showing a range that will not engage: {}",
            inverted.summary(48_000)
        );

        let empty = LoopMarks {
            start: Some(48_000),
            end: Some(48_000),
        };
        assert!(empty.range().is_none(), "a zero-length range is refused");
        assert!(
            empty.summary(48_000).contains("IS NOT AFTER IN"),
            "and is never shown as a loop the transport would run: {}",
            empty.summary(48_000)
        );

        let usable = LoopMarks {
            start: Some(48_000),
            end: Some(96_000),
        };
        let range = usable.range().expect("both marks, end after start");
        assert_eq!(range.start_frame(), 48_000);
        assert_eq!(range.end_frame(), 96_000);
        assert!(usable.summary(48_000).contains('→'));
    }

    #[test]
    fn offline_export_range_from_frames_accepts_only_a_nonempty_range() {
        assert_eq!(
            offline_export_range_from_frames(480, 960),
            Some(OfflineExportRange::new(480, 960).unwrap())
        );
        assert_eq!(offline_export_range_from_frames(960, 960), None);
    }

    #[test]
    fn marks_persist_as_frames_and_carry_whether_the_loop_was_engaged() {
        let usable = LoopMarks {
            start: Some(48_000),
            end: Some(96_000),
        };
        assert_eq!(
            usable.persisted(true),
            Some(IdeLoopRangeFrames {
                start_frame: 48_000,
                end_frame: 96_000,
                enabled: true,
            }),
            "the durable form is frames; the application converts to ticks"
        );
        assert_eq!(
            usable.persisted(false).map(|range| range.enabled),
            Some(false),
            "and carries whether the loop was engaged, not only where it was"
        );

        for unusable in [
            LoopMarks::default(),
            LoopMarks {
                start: Some(48_000),
                end: None,
            },
            LoopMarks {
                start: Some(96_000),
                end: Some(48_000),
            },
        ] {
            assert_eq!(
                unusable.persisted(true),
                None,
                "a range the transport would refuse is never written"
            );
        }
    }

    #[test]
    fn reopening_seeds_the_saved_loop_but_never_overwrites_the_users_marks() {
        assert_eq!(
            seeded_loop_marks(LoopMarks::default(), Some((48_000, 96_000, true))),
            Some((
                LoopMarks {
                    start: Some(48_000),
                    end: Some(96_000)
                },
                true
            )),
            "a saved loop comes back with the project"
        );
        assert_eq!(
            seeded_loop_marks(LoopMarks::default(), Some((0, 1_920, false)))
                .map(|(_, enabled)| enabled),
            Some(false),
            "including whether it was engaged"
        );
        assert_eq!(
            seeded_loop_marks(LoopMarks::default(), None),
            None,
            "a project with no saved loop seeds nothing"
        );

        let user_marks = LoopMarks {
            start: Some(10),
            end: None,
        };
        assert_eq!(
            seeded_loop_marks(user_marks, Some((48_000, 96_000, true))),
            None,
            "and a mark the user already set is never overwritten by an observation"
        );
    }

    #[test]
    fn the_loop_controls_follow_the_mode_and_the_marks() {
        let usable = LoopMarks {
            start: Some(0),
            end: Some(100),
        };
        let partial = LoopMarks {
            start: Some(0),
            end: None,
        };

        assert!(loop_marking_available(AppMode::Running));
        assert!(
            !loop_marking_available(AppMode::Saving),
            "marking is refused while a project mutation is in flight"
        );
        assert!(!loop_marking_available(AppMode::RestoringEdit));

        assert!(loop_toggle_available(AppMode::Running, usable));
        assert!(
            !loop_toggle_available(AppMode::Running, partial),
            "a single mark cannot engage a loop"
        );
        assert!(
            !loop_toggle_available(AppMode::Running, LoopMarks::default()),
            "and neither can no marks at all"
        );
        assert!(
            !loop_toggle_available(AppMode::Saving, usable),
            "a usable range still waits for the project to be idle"
        );
    }

    #[test]
    fn undo_and_redo_use_one_pending_mode_and_name_the_direction() {
        for command in [IdeWorkerCommandKind::Undo, IdeWorkerCommandKind::Redo] {
            assert_eq!(
                mode_after_successful_submission(AppMode::Running, command),
                AppMode::RestoringEdit
            );
            assert_eq!(
                mode_after_failure(AppMode::RestoringEdit, command),
                AppMode::Running
            );
            assert_eq!(
                mode_after_failure(AppMode::Saving, command),
                AppMode::Saving,
                "a failure that did not come from a restore leaves the mode alone"
            );
        }

        assert_eq!(
            restoring_status_text(false, "clip gain"),
            "Undoing the clip gain…"
        );
        assert_eq!(
            restoring_status_text(true, "clip gain"),
            "Reapplying the undone clip gain…"
        );
        assert_eq!(
            restored_status_text(false, 7),
            "Edit undone in project revision 7"
        );
        assert_eq!(
            restored_status_text(true, 8),
            "Edit reapplied in project revision 8"
        );

        assert!(restore_available(AppMode::Running, Some("clip gain")));
        assert!(
            !restore_available(AppMode::Running, Some("")),
            "an empty stack offers nothing, whatever the mode"
        );
        assert!(!restore_available(AppMode::Running, None));
        assert!(
            !restore_available(AppMode::RestoringEdit, Some("clip gain")),
            "a restore in flight cannot start another"
        );
        assert!(
            !restore_available(AppMode::Saving, Some("clip gain")),
            "and neither can any other pending project mutation"
        );

        let warning = restore_warning_text(
            false,
            "scene rename",
            &IdeResourceRestoreError::ResourceMissing,
        );
        assert!(
            warning.starts_with("The scene rename was undone but the project may need attention:"),
            "a committed restore that then failed says so rather than reporting plain failure: \
             {warning}"
        );
        assert!(
            restore_warning_text(true, "clip loop", &IdeResourceRestoreError::ResourceMissing)
                .contains("was reapplied but"),
            "and names the direction it went"
        );
    }

    #[test]
    fn scene_duplication_command_normalizes_name_and_uses_a_dedicated_pending_mode() {
        assert!(matches!(
            scene_duplication_command("scene-a", " Verse copy "),
            Ok(IdeWorkerCommand::DuplicateScene { scene_id, name })
                if scene_id == "scene-a" && name == "Verse copy"
        ));
        assert!(scene_duplication_command("", "Copy").is_err());
        assert!(scene_duplication_command("scene-a", "   ").is_err());
        assert!(scene_duplication_command("scene-a", &"x".repeat(121)).is_err());
        let unicode_boundary = "\u{00e9}".repeat(120);
        assert!(matches!(
            scene_duplication_command("scene-a", &unicode_boundary),
            Ok(IdeWorkerCommand::DuplicateScene { scene_id, name })
                if scene_id == "scene-a" && name == unicode_boundary
        ));
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::DuplicateScene,
            ),
            AppMode::DuplicatingScene
        );
        assert_eq!(
            mode_after_failure(
                AppMode::DuplicatingScene,
                IdeWorkerCommandKind::DuplicateScene,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn scene_activation_command_preserves_identity_and_rejects_stale_requests() {
        assert!(matches!(
            scene_activation_command("scene-a", true, false),
            Ok(Some(IdeWorkerCommand::SetSceneActive { scene_id, active: false }))
                if scene_id == "scene-a"
        ));
        assert!(matches!(
            scene_activation_command("scene-a", false, false),
            Ok(None)
        ));
        assert!(scene_activation_command("", true, false).is_err());
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetSceneActive,
            ),
            AppMode::ChangingSceneActivation
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingSceneActivation,
                IdeWorkerCommandKind::SetSceneActive,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn mixer_channel_validation_requires_current_observed_rows() {
        let track_a = "018f47a2-6b2d-7c11-8a44-001122334456";
        let channels = ModelRc::from(Rc::new(VecModel::from(vec![audio_track_mixer_row(
            0, track_a,
        )])));
        assert!(validate_mixer_channel(AppMode::Running, 0, &channels).is_ok());
        assert_eq!(
            validate_mixer_channel(AppMode::Closed, 0, &channels).unwrap_err(),
            MixerControlFailure::StateUnavailable
        );
        assert_eq!(
            validate_mixer_channel(AppMode::Running, 1, &channels).unwrap_err(),
            MixerControlFailure::StaleSelection
        );
        assert_eq!(
            validate_mixer_channel(AppMode::Running, -1, &channels).unwrap_err(),
            MixerControlFailure::InvalidSelection
        );
    }

    #[test]
    fn audio_track_commands_require_authoritative_rows() {
        let track_a = "018f47a2-6b2d-7c11-8a44-001122334456";
        let track_b = "018f47a2-6b2d-7c11-8a44-001122334457";
        let reordered = ModelRc::from(Rc::new(VecModel::from(vec![
            audio_track_mixer_row(0, track_b),
            audio_track_mixer_row(1, track_a),
        ])));
        assert!(matches!(
            mixer_strip_reorder_command(AppMode::Running, 1, true, &reordered),
            Ok(IdeWorkerCommand::ReorderMixerStrips { track_id, destination_index })
                if track_id == track_a && destination_index == 0
        ));
        assert!(matches!(
            mixer_strip_reorder_command(AppMode::Running, 0, true, &reordered),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert!(matches!(
            mixer_strip_reorder_command(AppMode::Closed, 1, true, &reordered),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::ReorderMixerStrips,
            ),
            AppMode::Saving
        );
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::ReorderMixerStrips),
            AppMode::Running
        );
        let mut group = audio_track_mixer_row(1, "018f47a2-6b2d-7c11-8a44-001122334458");
        group.name = "Drums".into();
        group.kind = "GROUP".into();
        group.solo_available = false;
        group.removable = false;
        group.activation_toggleable = false;
        let mut returned = audio_track_mixer_row(2, "018f47a2-6b2d-7c11-8a44-001122334459");
        returned.name = "Verb".into();
        returned.kind = "RETURN".into();
        returned.solo_available = false;
        returned.removable = false;
        returned.can_move_later = false;
        returned.activation_toggleable = false;
        let mixed = ModelRc::from(Rc::new(VecModel::from(vec![
            audio_track_mixer_row(0, track_b),
            group,
            returned,
        ])));
        assert!(matches!(
            mixer_strip_reorder_command(AppMode::Running, 1, true, &mixed),
            Ok(IdeWorkerCommand::ReorderMixerStrips { track_id, destination_index })
                if track_id == "018f47a2-6b2d-7c11-8a44-001122334458" && destination_index == 0
        ));
        assert!(matches!(
            mixer_strip_reorder_command(AppMode::Running, 2, false, &mixed),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert!(matches!(
            mixer_strip_duplication_command(AppMode::Running, 1, "Drums copy", 3, &mixed),
            Ok(IdeWorkerCommand::DuplicateAudioTrack { track_id, name })
                if track_id == "018f47a2-6b2d-7c11-8a44-001122334458" && name == "Drums copy"
        ));
        assert!(matches!(
            mixer_strip_duplication_command(
                AppMode::Running,
                1,
                " Target copy ",
                2,
                &reordered,
            ),
            Ok(IdeWorkerCommand::DuplicateAudioTrack { track_id, name })
                if track_id == track_a && name == "Target copy"
        ));
        assert!(matches!(
            mixer_strip_duplication_command(AppMode::Running, 1, "Copy", 8, &reordered),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert!(matches!(
            mixer_strip_duplication_command(AppMode::Running, 1, "   ", 2, &reordered),
            Err(MixerControlFailure::InvalidValue)
        ));
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::DuplicateAudioTrack,
            ),
            AppMode::DuplicatingTrack
        );
        assert_eq!(
            mode_after_failure(
                AppMode::DuplicatingTrack,
                IdeWorkerCommandKind::DuplicateAudioTrack,
            ),
            AppMode::Running
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one authoritative mixer model exercises related removal, rename, and mode transitions"
    )]
    fn audio_track_removal_requires_authoritative_rows() {
        let track_a = "018f47a2-6b2d-7c11-8a44-001122334456";
        let track_b = "018f47a2-6b2d-7c11-8a44-001122334457";
        let reordered = ModelRc::from(Rc::new(VecModel::from(vec![
            audio_track_mixer_row(0, track_b),
            audio_track_mixer_row(1, track_a),
        ])));
        assert_eq!(
            validate_audio_track_removal(AppMode::Running, track_a, &reordered).unwrap(),
            track_a
        );
        assert_eq!(
            validate_audio_track_removal(
                AppMode::Running,
                "018f47a2-6b2d-7c11-8a44-001122334499",
                &reordered,
            ),
            Err(MixerControlFailure::StaleSelection)
        );
        assert_eq!(
            validate_audio_track_content_removal(AppMode::Running, track_a, &reordered).unwrap(),
            track_a
        );
        assert_eq!(
            validate_audio_track_content_removal(AppMode::Closed, track_a, &reordered),
            Err(MixerControlFailure::StateUnavailable)
        );
        let mut routing_bus = audio_track_mixer_row(0, track_a);
        routing_bus.kind = "GROUP".into();
        routing_bus.renameable = true;
        routing_bus.removable = false;
        routing_bus.activation_toggleable = false;
        let routing_bus = ModelRc::from(Rc::new(VecModel::from(vec![routing_bus])));
        assert_eq!(
            validate_routing_bus_removal(AppMode::Running, track_a, &routing_bus).unwrap(),
            track_a
        );
        assert_eq!(
            validate_routing_bus_removal(AppMode::Closed, track_a, &routing_bus),
            Err(MixerControlFailure::StateUnavailable)
        );
        assert_eq!(
            validate_routing_bus_removal(AppMode::Running, track_a, &reordered),
            Err(MixerControlFailure::StateUnavailable)
        );
        assert!(matches!(
            routing_bus_rename_command(AppMode::Running, 0, " Percussion ", &routing_bus),
            Ok(IdeWorkerCommand::RenameRoutingBus { track_id, name })
                if track_id == track_a && name == "Percussion"
        ));
        assert!(matches!(
            routing_bus_rename_command(AppMode::Closed, 0, "Percussion", &routing_bus),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert!(matches!(
            routing_bus_rename_command(AppMode::Running, 0, "   ", &routing_bus),
            Err(MixerControlFailure::InvalidValue)
        ));
        assert!(matches!(
            routing_bus_rename_command(AppMode::Running, 0, "Percussion", &reordered),
            Err(MixerControlFailure::StateUnavailable)
        ));
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::RenameRoutingBus
            ),
            AppMode::RenamingTrack
        );
        assert_eq!(
            mode_after_failure(
                AppMode::RenamingTrack,
                IdeWorkerCommandKind::RenameRoutingBus
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::RemoveRoutingBus
            ),
            AppMode::Saving
        );
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::RemoveRoutingBus),
            AppMode::Running
        );
        let mut non_audio = audio_track_mixer_row(0, track_a);
        non_audio.activation_toggleable = false;
        let non_audio = ModelRc::from(Rc::new(VecModel::from(vec![non_audio])));
        assert_eq!(
            validate_audio_track_content_removal(AppMode::Running, track_a, &non_audio),
            Err(MixerControlFailure::StateUnavailable)
        );
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::RemoveAudioTrackContent,
            ),
            AppMode::RemovingTrack
        );
        assert_eq!(
            mode_after_failure(
                AppMode::RemovingTrack,
                IdeWorkerCommandKind::RemoveAudioTrackContent,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn mixer_value_domains_reject_invalid_values() {
        assert_eq!(gain_percent_to_millionths(0.0), Ok(0));
        assert_eq!(gain_percent_to_millionths(125.5), Ok(1_255_000));
        assert_eq!(gain_percent_to_millionths(200.0), Ok(2_000_000));
        assert_eq!(
            gain_percent_to_millionths(200.1),
            Err(MixerControlFailure::InvalidValue)
        );
        assert_eq!(
            gain_percent_to_millionths(f32::NAN),
            Err(MixerControlFailure::InvalidValue)
        );
        assert_eq!(pan_percent_to_millionths(-100.0), Ok(-1_000_000));
        assert_eq!(pan_percent_to_millionths(25.25), Ok(252_500));
        assert_eq!(pan_percent_to_millionths(100.0), Ok(1_000_000));
        assert_eq!(
            pan_percent_to_millionths(-100.1),
            Err(MixerControlFailure::InvalidValue)
        );
    }

    #[test]
    fn send_controls_require_current_observed_rows() {
        let sends = ModelRc::from(Rc::new(VecModel::from(vec![MixerSendRow {
            index: 0,
            channel_index: 2,
            send_index: 1,
            send_id: "018f47a2-6b2d-7c11-8a44-00112233446a".into(),
            source_name: "Drums".into(),
            target_name: "Reverb".into(),
            tap: "POST-FADER".into(),
            state_available: true,
            gain_percent: 50.0,
            pan_percent: -25.0,
            enabled: true,
        }])));
        let row = validate_mixer_send(AppMode::Running, 0, &sends).unwrap();
        assert_eq!(row.channel_index, 2);
        assert_eq!(row.send_index, 1);
        assert_eq!(
            validate_mixer_send(AppMode::Closed, 0, &sends).unwrap_err(),
            MixerControlFailure::StateUnavailable
        );
        assert_eq!(
            validate_mixer_send(AppMode::Running, 1, &sends).unwrap_err(),
            MixerControlFailure::StaleSelection
        );
        assert_eq!(
            validate_mixer_send(AppMode::Running, -1, &sends).unwrap_err(),
            MixerControlFailure::InvalidSelection
        );
    }

    #[test]
    fn return_send_actions_forward_only_current_generated_rows() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        ui.set_mixer_sends_visible(true);
        ui.set_return_send_candidates(ModelRc::from(Rc::new(VecModel::from(vec![
            ReturnSendCandidateRow {
                source_track_id: "018f47a2-6b2d-7c11-8a44-001122334456".into(),
                source_name: "Drums".into(),
                return_track_id: "018f47a2-6b2d-7c11-8a44-001122334457".into(),
                return_name: "Reverb".into(),
            },
        ]))));
        ui.set_mixer_sends(ModelRc::from(Rc::new(VecModel::from(vec![MixerSendRow {
            index: 0,
            channel_index: 0,
            send_index: 0,
            send_id: "018f47a2-6b2d-7c11-8a44-001122334458".into(),
            source_name: "Drums".into(),
            target_name: "Reverb".into(),
            tap: "POST-FADER".into(),
            state_available: true,
            gain_percent: 100.0,
            pan_percent: 0.0,
            enabled: true,
        }]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_return_send_callbacks_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.show().unwrap();

        let add = ElementHandle::find_by_accessible_label(&ui, "Add send from Drums to Reverb")
            .next()
            .expect("the generated return-send candidate is reachable");
        assert_eq!(add.accessible_enabled(), Some(true));
        add.mock_single_click(PointerEventButton::Left);
        let remove =
            ElementHandle::find_by_accessible_label(&ui, "Remove send from Drums to Reverb")
                .next()
                .expect("the generated return-send removal is reachable");
        assert_eq!(remove.accessible_enabled(), Some(true));
        remove.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [
                IdeWorkerCommand::AddReturnSend {
                    source_track_id,
                    return_track_id,
                },
                IdeWorkerCommand::RemoveReturnSend { send_id },
            ] if source_track_id == "018f47a2-6b2d-7c11-8a44-001122334456"
                && return_track_id == "018f47a2-6b2d-7c11-8a44-001122334457"
                && send_id == "018f47a2-6b2d-7c11-8a44-001122334458"
        ));

        let actions = ui.global::<IdeActions>();
        actions.invoke_add_return_send(
            "018f47a2-6b2d-7c11-8a44-001122334499".into(),
            "018f47a2-6b2d-7c11-8a44-001122334457".into(),
        );
        actions.invoke_remove_return_send("018f47a2-6b2d-7c11-8a44-001122334499".into());
        assert_eq!(
            submitted.borrow().len(),
            2,
            "stale return-send identities must not enqueue a mutation"
        );
        assert_eq!(
            ui.get_error_text(),
            "The selected return send is no longer available"
        );
        ui.hide().unwrap();
    }

    #[test]
    fn primary_route_actions_forward_only_current_generated_rows() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        ui.set_mixer_sends_visible(true);
        ui.set_primary_route_candidates(ModelRc::from(Rc::new(VecModel::from(vec![
            PrimaryRouteCandidateRow {
                source_track_id: "018f47a2-6b2d-7c11-8a44-001122334456".into(),
                source_name: "Drums".into(),
                target_group_track_id: "018f47a2-6b2d-7c11-8a44-001122334457".into(),
                target_name: "Music".into(),
                targets_main: false,
            },
            PrimaryRouteCandidateRow {
                source_track_id: "018f47a2-6b2d-7c11-8a44-001122334458".into(),
                source_name: "Bass".into(),
                target_group_track_id: SharedString::default(),
                target_name: "Main".into(),
                targets_main: true,
            },
        ]))));
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_audio_primary_route_callbacks_with(
            &ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        ui.show().unwrap();

        let route = ElementHandle::find_by_accessible_label(&ui, "Route Drums to Music")
            .next()
            .expect("the generated primary-route candidate is reachable");
        assert_eq!(route.accessible_enabled(), Some(true));
        route.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::SetAudioPrimaryRoute {
                source_track_id,
                target_group_track_id: Some(target_group_track_id),
            }] if source_track_id == "018f47a2-6b2d-7c11-8a44-001122334456"
                && target_group_track_id == "018f47a2-6b2d-7c11-8a44-001122334457"
        ));

        let main = ElementHandle::find_by_accessible_label(&ui, "Route Bass to Main")
            .next()
            .expect("the generated Main-route candidate is reachable");
        assert_eq!(main.accessible_enabled(), Some(true));
        main.mock_single_click(PointerEventButton::Left);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [
                IdeWorkerCommand::SetAudioPrimaryRoute { .. },
                IdeWorkerCommand::SetAudioPrimaryRoute {
                    source_track_id,
                    target_group_track_id: None,
                },
            ] if source_track_id == "018f47a2-6b2d-7c11-8a44-001122334458"
        ));

        ui.global::<IdeActions>().invoke_set_audio_primary_route(
            "018f47a2-6b2d-7c11-8a44-001122334499".into(),
            "018f47a2-6b2d-7c11-8a44-001122334457".into(),
            false,
        );
        assert_eq!(
            submitted.borrow().len(),
            2,
            "stale primary-route identities must not enqueue a mutation"
        );
        assert_eq!(
            ui.get_error_text(),
            "The selected primary route is no longer available"
        );
        ui.hide().unwrap();
    }

    #[test]
    fn mixer_display_names_are_unicode_safe_and_bounded() {
        assert_eq!(compact_name("Drums", MAX_VISIBLE_MIXER_NAME_CHARS), "Drums");
        let display = compact_name("Élévation musicale", MAX_VISIBLE_MIXER_NAME_CHARS);
        assert_eq!(display.chars().count(), MAX_VISIBLE_MIXER_NAME_CHARS);
        assert!(display.ends_with('…'));
    }

    #[test]
    fn command_failures_restore_only_the_state_the_command_consumed() {
        assert_eq!(
            mode_after_failure(AppMode::Discovering, IdeWorkerCommandKind::Initialize),
            AppMode::Unavailable
        );
        assert_eq!(
            mode_after_failure(AppMode::Opening, IdeWorkerCommandKind::OpenProject),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_failure(AppMode::Creating, IdeWorkerCommandKind::CreateProject),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_failure(AppMode::Closing, IdeWorkerCommandKind::CloseProject),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Closed, IdeWorkerCommandKind::CloseProject),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_failure(AppMode::Running, IdeWorkerCommandKind::OpenProject),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Running, IdeWorkerCommandKind::Transport),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Running, IdeWorkerCommandKind::Mixer),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::SaveProject),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::AddingTrack, IdeWorkerCommandKind::AddAudioTrack,),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::RemovingTrack,
                IdeWorkerCommandKind::RemoveAudioTrack,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::RenamingTrack,
                IdeWorkerCommandKind::RenameAudioTrack,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingTrackActivation,
                IdeWorkerCommandKind::SetAudioTrackActive,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::PreparingRecording,
                IdeWorkerCommandKind::StartRecording,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::FinalizingRecording,
                IdeWorkerCommandKind::StopRecording,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::FinalizingRecording,
                IdeWorkerCommandKind::AbortRecording,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Opening, IdeWorkerCommandKind::Observe),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::StartingOutputTest,
                IdeWorkerCommandKind::StartOutputTest,
            ),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_failure(
                AppMode::StoppingOutputTest,
                IdeWorkerCommandKind::StopOutputTest,
            ),
            AppMode::OutputTesting
        );
    }

    #[test]
    fn runtime_preparation_has_a_dedicated_recoverable_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::PreparingRuntime,
                IdeWorkerCommandKind::PrepareRuntime,
            ),
            AppMode::PreparingRuntime
        );
        assert_eq!(
            mode_after_failure(
                AppMode::PreparingRuntime,
                IdeWorkerCommandKind::PrepareRuntime,
            ),
            AppMode::Running
        );
        assert!(command_status(IdeWorkerCommandKind::PrepareRuntime).contains("Runtime bundle"));
        assert_eq!(encode_sha256(&[0xab; 32]), "ab".repeat(32));
    }

    #[test]
    fn arrangement_activation_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingArrangementActivation,
                IdeWorkerCommandKind::SetArrangementPlacementActive,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_edit_input_preserves_the_exact_project_tick_domain() {
        assert_eq!(parse_arrangement_start_tick(" 1920 "), Ok(1_920));
        assert_eq!(
            parse_arrangement_start_tick("9007199254740991"),
            Ok(MAX_SAFE_TIMELINE_TICK)
        );
        assert!(parse_arrangement_start_tick("").is_err());
        assert!(parse_arrangement_start_tick("-1").is_err());
        assert!(parse_arrangement_start_tick("1.5").is_err());
        assert!(parse_arrangement_start_tick("9007199254740992").is_err());
        assert!(parse_arrangement_start_tick("18446744073709551616").is_err());
        assert_eq!(parse_arrangement_scene_launch_tick(" 960 "), Ok(960));
        assert_eq!(
            parse_arrangement_scene_launch_tick("9007199254740991"),
            Ok(MAX_SAFE_TIMELINE_TICK)
        );
        assert!(parse_arrangement_scene_launch_tick("").is_err());
        assert!(parse_arrangement_scene_launch_tick("-1").is_err());
        assert!(parse_arrangement_scene_launch_tick("1.5").is_err());
        assert!(parse_arrangement_scene_launch_tick("9007199254740992").is_err());
    }

    #[test]
    fn arrangement_move_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::MovingArrangementPlacement,
                IdeWorkerCommandKind::MoveArrangementPlacement,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_duration_input_preserves_the_exact_positive_project_tick_domain() {
        assert_eq!(parse_arrangement_duration_ticks(" 1920 "), Ok(1_920));
        assert_eq!(
            parse_arrangement_duration_ticks("9007199254740991"),
            Ok(MAX_SAFE_TIMELINE_TICK)
        );
        assert!(parse_arrangement_duration_ticks("").is_err());
        assert!(parse_arrangement_duration_ticks("0").is_err());
        assert!(parse_arrangement_duration_ticks("-1").is_err());
        assert!(parse_arrangement_duration_ticks("1.5").is_err());
        assert!(parse_arrangement_duration_ticks("9007199254740992").is_err());
        assert!(parse_arrangement_duration_ticks("18446744073709551616").is_err());
    }

    #[test]
    fn arrangement_duration_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::TrimmingArrangementPlacement,
                IdeWorkerCommandKind::SetArrangementPlacementDuration,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_placement_gain_input_accepts_finite_offsets_only() {
        assert_eq!(parse_arrangement_placement_gain(" -12.5 "), Ok(-12.5));
        assert_eq!(parse_arrangement_placement_gain("0"), Ok(0.0));
        assert_eq!(
            parse_arrangement_placement_gain("6.020599913279624"),
            Ok(6.020_599_913_279_624)
        );
        assert!(parse_arrangement_placement_gain("NaN").is_err());
        assert!(parse_arrangement_placement_gain("inf").is_err());
        assert!(parse_arrangement_placement_gain("").is_err());
        assert!(parse_arrangement_placement_gain("not gain").is_err());
    }

    #[test]
    fn arrangement_placement_gain_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetArrangementPlacementGain,
            ),
            AppMode::ChangingArrangementPlacementGain
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingArrangementPlacementGain,
                IdeWorkerCommandKind::SetArrangementPlacementGain,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_placement_fade_input_and_submission_stay_in_the_exact_local_domain() {
        assert_eq!(parse_arrangement_placement_fades(" 1 ", "3", 4), Ok((1, 3)));
        assert_eq!(parse_arrangement_placement_fades("0", "0", 4), Ok((0, 0)));
        assert!(parse_arrangement_placement_fades("", "0", 4).is_err());
        assert!(parse_arrangement_placement_fades("1.5", "0", 4).is_err());
        assert!(parse_arrangement_placement_fades("0", "5", 4).is_err());
        assert!(parse_arrangement_placement_fades("3", "2", 4).is_err());
        assert!(parse_arrangement_placement_fades("9007199254740992", "0", u64::MAX).is_err());
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetArrangementPlacementFades,
            ),
            AppMode::ChangingArrangementPlacementFades
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingArrangementPlacementFades,
                IdeWorkerCommandKind::SetArrangementPlacementFades,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_crossfade_submission_has_a_dedicated_pending_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade,
            ),
            AppMode::ChangingArrangementPlacementCrossfade
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingArrangementPlacementCrossfade,
                IdeWorkerCommandKind::ApplyArrangementPlacementCrossfade,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_front_trim_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::TrimmingArrangementPlacement,
                IdeWorkerCommandKind::TrimArrangementPlacementFront,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_removal_requires_the_current_explicit_confirmation() {
        assert!(arrangement_removal_confirmation_is_current(
            AppMode::Running,
            true,
            "placement-a",
            "placement-a",
        ));
        assert!(!arrangement_removal_confirmation_is_current(
            AppMode::RemovingArrangementPlacement,
            true,
            "placement-a",
            "placement-a",
        ));
        assert!(!arrangement_removal_confirmation_is_current(
            AppMode::Running,
            false,
            "placement-a",
            "placement-a",
        ));
        assert!(!arrangement_removal_confirmation_is_current(
            AppMode::Running,
            true,
            "placement-a",
            "placement-b",
        ));
        assert!(!arrangement_removal_confirmation_is_current(
            AppMode::Running,
            true,
            "",
            "",
        ));
    }

    #[test]
    fn arrangement_removal_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::RemovingArrangementPlacement,
                IdeWorkerCommandKind::RemoveArrangementPlacement,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_addition_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::AddingArrangementPlacement,
                IdeWorkerCommandKind::AddArrangementPlacement,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_scene_launch_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::AddArrangementSceneLaunch,
            ),
            AppMode::AddingArrangementSceneLaunch
        );
        assert_eq!(
            mode_after_failure(
                AppMode::AddingArrangementSceneLaunch,
                IdeWorkerCommandKind::AddArrangementSceneLaunch,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::MoveArrangementSceneLaunch,
            ),
            AppMode::MovingArrangementSceneLaunch
        );
        assert_eq!(
            mode_after_failure(
                AppMode::MovingArrangementSceneLaunch,
                IdeWorkerCommandKind::MoveArrangementSceneLaunch,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
            ),
            AppMode::ChangingArrangementSceneLaunchScene
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingArrangementSceneLaunchScene,
                IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
            ),
            AppMode::RemovingArrangementSceneLaunch
        );
        assert_eq!(
            mode_after_failure(
                AppMode::RemovingArrangementSceneLaunch,
                IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_scene_launch_edit_requires_the_open_form_identity() {
        assert!(arrangement_scene_launch_form_is_current(
            AppMode::Running,
            true,
            "marker-a",
            "marker-a",
        ));
        assert!(!arrangement_scene_launch_form_is_current(
            AppMode::MovingArrangementSceneLaunch,
            true,
            "marker-a",
            "marker-a",
        ));
        assert!(!arrangement_scene_launch_form_is_current(
            AppMode::Running,
            false,
            "marker-a",
            "marker-a",
        ));
        assert!(!arrangement_scene_launch_form_is_current(
            AppMode::Running,
            true,
            "marker-a",
            "marker-b",
        ));
        assert!(!arrangement_scene_launch_form_is_current(
            AppMode::Running,
            true,
            "",
            "",
        ));
    }

    #[test]
    fn arrangement_scene_launch_navigation_builds_the_exact_user_intent() {
        assert!(matches!(
            arrangement_scene_launch_locate_command(
                AppMode::Running,
                "project-a",
                2,
                "marker-a",
                Some("marker-a"),
                false,
            ),
            Ok(IdeWorkerCommand::LocateSceneLaunch {
                project_id,
                index: 2,
                launch_id,
                state: LocateState::Preserve,
            }) if project_id == "project-a" && launch_id == "marker-a"
        ));
        assert!(matches!(
            arrangement_scene_launch_locate_command(
                AppMode::Running,
                "project-a",
                2,
                "marker-a",
                Some("marker-a"),
                true,
            ),
            Ok(IdeWorkerCommand::LocateSceneLaunch {
                state: LocateState::Rolling,
                ..
            })
        ));
        assert!(
            arrangement_scene_launch_locate_command(
                AppMode::MovingArrangementSceneLaunch,
                "project-a",
                2,
                "marker-a",
                Some("marker-a"),
                false,
            )
            .is_err()
        );
        assert!(
            arrangement_scene_launch_locate_command(
                AppMode::Running,
                "project-a",
                -1,
                "marker-a",
                None,
                false,
            )
            .is_err()
        );
        assert!(
            arrangement_scene_launch_locate_command(
                AppMode::Running,
                "project-a",
                2,
                "marker-a",
                Some("marker-b"),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn arrangement_scene_launch_reassignment_requires_refreshed_authority() {
        assert!(arrangement_scene_launch_scene_authority_is_current(
            960,
            960,
            0,
            0,
            Some("scene-a"),
            "scene-a",
        ));
        for current in [
            arrangement_scene_launch_scene_authority_is_current(
                961,
                960,
                0,
                0,
                Some("scene-a"),
                "scene-a",
            ),
            arrangement_scene_launch_scene_authority_is_current(
                960,
                960,
                1,
                0,
                Some("scene-a"),
                "scene-a",
            ),
            arrangement_scene_launch_scene_authority_is_current(
                960,
                960,
                0,
                0,
                Some("scene-b"),
                "scene-a",
            ),
            arrangement_scene_launch_scene_authority_is_current(960, 960, 0, 0, None, "scene-a"),
        ] {
            assert!(!current);
        }

        assert!(matches!(
            arrangement_scene_launch_scene_command("marker-a", 0, 1, Some("scene-b")),
            Ok(Some(IdeWorkerCommand::SetArrangementSceneLaunchScene {
                launch_id,
                scene_id,
            })) if launch_id == "marker-a" && scene_id == "scene-b"
        ));
        assert!(matches!(
            arrangement_scene_launch_scene_command("marker-a", 0, 0, Some("scene-a")),
            Ok(None)
        ));
        assert!(arrangement_scene_launch_scene_command("marker-a", 0, 1, None).is_err());
    }

    fn assert_arrangement_scene_launch_edit_events(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_move_arrangement_scene_launch_visible(true);
        ui.set_move_arrangement_scene_launch_id("marker-a".into());
        ui.set_move_arrangement_scene_launch_name("Verse".into());
        ui.set_move_arrangement_scene_launch_original_tick("960".into());
        ui.set_move_arrangement_scene_launch_tick("1920".into());

        apply_arrangement_scene_launch_move_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchMoving { tick: 1_920 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::MovingArrangementSceneLaunch);
        assert!(ui.get_move_arrangement_scene_launch_visible());

        apply_arrangement_scene_launch_move_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchMoved {
                revision: 3,
                launch_id: "marker-a".to_owned(),
                tick: 1_920,
            },
        );
        assert!(!ui.get_move_arrangement_scene_launch_visible());
        assert!(ui.get_move_arrangement_scene_launch_id().is_empty());
        assert!(ui.get_move_arrangement_scene_launch_tick().is_empty());
        assert!(ui.get_status_text().contains("revision 3"));

        ui.set_app_mode(AppMode::Running);
        ui.set_set_arrangement_scene_launch_scene_visible(true);
        ui.set_set_arrangement_scene_launch_scene_id("marker-a".into());
        ui.set_set_arrangement_scene_launch_scene_name("Verse".into());
        ui.set_set_arrangement_scene_launch_scene_current_id("scene-a".into());
        ui.set_set_arrangement_scene_launch_scene_current_index(0);
        ui.set_set_arrangement_scene_launch_scene_tick("1920".into());
        ui.set_set_arrangement_scene_launch_scene_target_index(1);

        apply_arrangement_scene_launch_scene_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchSceneChanging,
        );
        assert_eq!(
            ui.get_app_mode(),
            AppMode::ChangingArrangementSceneLaunchScene
        );
        assert!(ui.get_set_arrangement_scene_launch_scene_visible());

        apply_arrangement_scene_launch_scene_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchSceneChanged {
                revision: 4,
                launch_id: "marker-a".to_owned(),
                scene_id: "scene-b".to_owned(),
            },
        );
        assert!(!ui.get_set_arrangement_scene_launch_scene_visible());
        assert!(ui.get_set_arrangement_scene_launch_scene_id().is_empty());
        assert_eq!(
            ui.get_set_arrangement_scene_launch_scene_current_index(),
            -1
        );
        assert_eq!(ui.get_set_arrangement_scene_launch_scene_target_index(), -1);
        assert!(ui.get_status_text().contains("revision 4"));

        ui.set_app_mode(AppMode::Running);
        ui.set_remove_arrangement_scene_launch_visible(true);
        ui.set_remove_arrangement_scene_launch_id("marker-a".into());
        ui.set_remove_arrangement_scene_launch_name("Verse".into());
        ui.set_remove_arrangement_scene_launch_tick("1920".into());

        apply_arrangement_scene_launch_removal_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchRemoving,
        );
        assert_eq!(ui.get_app_mode(), AppMode::RemovingArrangementSceneLaunch);
        assert!(ui.get_remove_arrangement_scene_launch_visible());

        apply_arrangement_scene_launch_removal_event(
            ui,
            &IdeWorkerEvent::ArrangementSceneLaunchRemoved {
                revision: 5,
                launch_id: "marker-a".to_owned(),
            },
        );
        assert!(!ui.get_remove_arrangement_scene_launch_visible());
        assert!(ui.get_remove_arrangement_scene_launch_id().is_empty());
        assert!(ui.get_remove_arrangement_scene_launch_tick().is_empty());
        assert!(ui.get_status_text().contains("revision 5"));
    }

    fn assert_arrangement_scene_launch_committed_warnings(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_move_arrangement_scene_launch_visible(true);
        ui.set_move_arrangement_scene_launch_id("marker-a".into());
        ui.set_move_arrangement_scene_launch_name("Verse".into());
        ui.set_move_arrangement_scene_launch_original_tick("960".into());
        ui.set_move_arrangement_scene_launch_tick("1920".into());
        apply_arrangement_placement_committed_warning(
            ui,
            &IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
                command: IdeWorkerCommandKind::MoveArrangementSceneLaunch,
                warning: Box::new(IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity),
            },
        );
        assert!(!ui.get_move_arrangement_scene_launch_visible());
        assert!(ui.get_move_arrangement_scene_launch_id().is_empty());
        assert!(ui.get_move_arrangement_scene_launch_tick().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Running);
        ui.set_set_arrangement_scene_launch_scene_visible(true);
        ui.set_set_arrangement_scene_launch_scene_id("marker-a".into());
        ui.set_set_arrangement_scene_launch_scene_name("Verse".into());
        ui.set_set_arrangement_scene_launch_scene_current_id("scene-a".into());
        ui.set_set_arrangement_scene_launch_scene_current_index(0);
        ui.set_set_arrangement_scene_launch_scene_tick("1920".into());
        ui.set_set_arrangement_scene_launch_scene_target_index(1);
        apply_arrangement_placement_committed_warning(
            ui,
            &IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetArrangementSceneLaunchScene,
                warning: Box::new(IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity),
            },
        );
        assert!(!ui.get_set_arrangement_scene_launch_scene_visible());
        assert!(ui.get_set_arrangement_scene_launch_scene_id().is_empty());
        assert_eq!(
            ui.get_set_arrangement_scene_launch_scene_current_index(),
            -1
        );
        assert_eq!(ui.get_set_arrangement_scene_launch_scene_target_index(), -1);
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Closed);
        ui.set_remove_arrangement_scene_launch_visible(true);
        ui.set_remove_arrangement_scene_launch_id("marker-a".into());
        ui.set_remove_arrangement_scene_launch_name("Verse".into());
        ui.set_remove_arrangement_scene_launch_tick("1920".into());
        apply_arrangement_placement_committed_warning(
            ui,
            &IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
                command: IdeWorkerCommandKind::RemoveArrangementSceneLaunch,
                warning: Box::new(IdeArrangementPlacementEditError::InvalidSceneLaunchIdentity),
            },
        );
        assert!(!ui.get_remove_arrangement_scene_launch_visible());
        assert!(ui.get_remove_arrangement_scene_launch_id().is_empty());
        assert!(ui.get_remove_arrangement_scene_launch_tick().is_empty());
        assert!(
            ui.get_status_text()
                .contains("committed, but the project could not be reopened")
        );
        assert!(!ui.get_error_text().is_empty());
    }

    #[test]
    fn automation_take_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::AddAutomationTake,
            ),
            AppMode::AddingAutomationTake
        );
        assert_eq!(
            mode_after_failure(
                AppMode::AddingAutomationTake,
                IdeWorkerCommandKind::AddAutomationTake,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn automation_points_parse_only_inside_their_declared_value_kind() {
        assert_eq!(
            parse_automation_points("0=1000000, 3840=0", false),
            Ok(vec![
                AutomationPoint::new(0, AutomationValue::Integer(1_000_000)),
                AutomationPoint::new(3_840, AutomationValue::Integer(0)),
            ])
        );
        assert_eq!(
            parse_automation_points(" 0 = off , 1920 = on ", true),
            Ok(vec![
                AutomationPoint::new(0, AutomationValue::Boolean(false)),
                AutomationPoint::new(1_920, AutomationValue::Boolean(true)),
            ])
        );
        assert!(parse_automation_points("0=on", false).is_err());
        assert!(parse_automation_points("0=1000000", true).is_err());
        assert!(parse_automation_points("1000000", false).is_err());
        assert!(parse_automation_points("-1=0", false).is_err());
        assert!(parse_automation_points("   ", false).is_err());
    }

    #[test]
    fn automation_point_formatting_round_trips_the_supported_editor_syntax() {
        let continuous = [
            AutomationPoint::new(0, AutomationValue::Integer(1_000_000)),
            AutomationPoint::new(960, AutomationValue::Integer(-250_000)),
        ];
        let switches = [
            AutomationPoint::new(0, AutomationValue::Boolean(false)),
            AutomationPoint::new(960, AutomationValue::Boolean(true)),
        ];
        assert_eq!(
            format_automation_points(&continuous),
            "0=1000000, 960=-250000"
        );
        assert_eq!(
            parse_automation_points(&format_automation_points(&continuous), false),
            Ok(continuous.to_vec())
        );
        assert_eq!(format_automation_points(&switches), "0=off, 960=on");
        assert_eq!(
            parse_automation_points(&format_automation_points(&switches), true),
            Ok(switches.to_vec())
        );
    }

    #[test]
    fn automation_take_activation_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetAutomationTakeActive,
            ),
            AppMode::ChangingAutomationTakeActivation
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingAutomationTakeActivation,
                IdeWorkerCommandKind::SetAutomationTakeActive,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn automation_take_edit_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::EditAutomationTake,
            ),
            AppMode::EditingAutomationTake
        );
        assert_eq!(
            mode_after_failure(
                AppMode::EditingAutomationTake,
                IdeWorkerCommandKind::EditAutomationTake,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn generated_automation_take_actions_close_while_an_edit_form_is_open() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        let first_id = "018f47a2-6b2d-7c11-8a44-0011223344a0";
        ui.set_app_mode(AppMode::Running);
        ui.set_mixer_visible(true);
        ui.set_mixer_automation_visible(true);
        ui.set_automation_takes(ModelRc::from(Rc::new(VecModel::from(vec![
            AutomationTakeRow {
                id: first_id.into(),
                name: "First".into(),
                enabled: true,
                editable: true,
                range: "TICK 0 to 960".into(),
                target: "Main gain".into(),
                interpolation: "Linear".into(),
                points: "2 points".into(),
            },
            AutomationTakeRow {
                id: "018f47a2-6b2d-7c11-8a44-0011223344a1".into(),
                name: "Second".into(),
                enabled: true,
                editable: true,
                range: "TICK 960 to 1920".into(),
                target: "Main gain".into(),
                interpolation: "Linear".into(),
                points: "2 points".into(),
            },
        ]))));

        ui.show().unwrap();
        let first_edit = ElementHandle::find_by_accessible_label(&ui, "Edit First automation take")
            .next()
            .expect("the first generated take exposes edit");
        let second_edit =
            ElementHandle::find_by_accessible_label(&ui, "Edit Second automation take")
                .next()
                .expect("the second generated take exposes edit");
        let activation =
            ElementHandle::find_by_accessible_label(&ui, "Disable First automation take")
                .next()
                .expect("the first generated take exposes activation");
        let removal = ElementHandle::find_by_accessible_label(&ui, "Remove First automation take")
            .next()
            .expect("the first generated take exposes removal");
        let channels = ElementHandle::find_by_accessible_label(
            &ui,
            "Return from automation to mixer channels",
        )
        .next()
        .expect("the automation panel exposes its channel navigation");
        let performance =
            ElementHandle::find_by_accessible_label(&ui, "Return from automation to performance")
                .next()
                .expect("the automation panel exposes its performance navigation");
        assert_eq!(first_edit.accessible_enabled(), Some(true));
        assert_eq!(second_edit.accessible_enabled(), Some(true));
        assert_eq!(activation.accessible_enabled(), Some(true));
        assert_eq!(removal.accessible_enabled(), Some(true));
        assert_eq!(channels.accessible_enabled(), Some(true));
        assert_eq!(performance.accessible_enabled(), Some(true));
        assert!(automation_take_direct_action_is_current(&ui, first_id));

        ui.set_automation_edit_visible(true);
        assert!(!automation_take_direct_action_is_current(&ui, first_id));
        assert_eq!(first_edit.accessible_enabled(), Some(false));
        assert_eq!(second_edit.accessible_enabled(), Some(false));
        assert_eq!(activation.accessible_enabled(), Some(false));
        assert_eq!(removal.accessible_enabled(), Some(false));
        assert_eq!(channels.accessible_enabled(), Some(false));
        assert_eq!(performance.accessible_enabled(), Some(false));

        ui.set_automation_edit_visible(false);
        ui.set_automation_create_visible(true);
        assert!(!automation_take_direct_action_is_current(&ui, first_id));
        assert_eq!(first_edit.accessible_enabled(), Some(false));
        assert_eq!(activation.accessible_enabled(), Some(false));
        assert_eq!(removal.accessible_enabled(), Some(false));
        assert_eq!(channels.accessible_enabled(), Some(false));
        assert_eq!(performance.accessible_enabled(), Some(false));

        ui.set_automation_create_visible(false);
        ui.set_automation_take_remove_confirming_id(first_id.into());
        assert!(!automation_take_direct_action_is_current(&ui, first_id));
        assert_eq!(first_edit.accessible_enabled(), Some(false));
        assert_eq!(activation.accessible_enabled(), Some(false));
        assert_eq!(channels.accessible_enabled(), Some(false));
        assert_eq!(performance.accessible_enabled(), Some(false));
        ui.hide().unwrap();
    }

    fn automation_channel_fixture() -> Vec<MixerChannelFacts> {
        vec![
            MixerChannelFacts {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                name: "Drums".to_owned(),
                audio: true,
                sends: vec![MixerSendFacts {
                    send_id: "018f47a2-6b2d-7c11-8a44-001122334469".to_owned(),
                    target_name: "Reverb".to_owned(),
                }],
            },
            MixerChannelFacts {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334472".to_owned(),
                name: "Reverb".to_owned(),
                audio: false,
                sends: Vec::new(),
            },
        ]
    }

    fn control_scene_fixture() -> Vec<SceneFacts> {
        vec![SceneFacts {
            scene_id: "018f47a2-6b2d-7c11-8a44-001122334461".to_owned(),
            name: "Verse".to_owned(),
        }]
    }

    #[test]
    fn control_target_choices_offer_only_targets_the_compiler_accepts() {
        let choices =
            control_target_choices(&automation_channel_fixture(), &control_scene_fixture());
        let labels: Vec<&str> = choices.iter().map(|choice| choice.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Main gain",
                "Main mute",
                "Drums gain",
                "Drums pan",
                "Drums mute",
                "Drums solo",
                "Reverb gain",
                "Reverb pan",
                "Reverb mute",
                "Launch scene Verse",
                "Transport play",
                "Transport stop",
                "Transport toggle",
                "Transport reset",
            ],
            "solo is offered on the audio channel and withheld from the return"
        );
        let drums_gain = &choices[2];
        assert_eq!(
            drums_gain.target,
            ControlTarget::MixerChannel {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                parameter: IdeControlChannelParameter::Gain,
            }
        );
        assert_eq!(drums_gain.domain, ControlTargetDomain::Continuous);
        let drums_pan = &choices[3];
        assert_eq!(
            drums_pan.target,
            ControlTarget::MixerChannel {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                parameter: IdeControlChannelParameter::Pan,
            }
        );
        assert_eq!(choices[4].domain, ControlTargetDomain::Boolean);
        assert_eq!(choices[9].domain, ControlTargetDomain::Action);
        assert_eq!(
            choices[9].target,
            ControlTarget::Scene {
                scene_id: "018f47a2-6b2d-7c11-8a44-001122334461".to_owned(),
                action: SceneAction::Launch,
            }
        );
    }

    fn assert_control_targets_repopulate_after_a_project_reset(ui: &MainWindow) {
        let recording_routes: RecordingRoutes = Rc::new(RefCell::new(Vec::new()));
        let take_routes: TakeRoutes = Rc::new(RefCell::new(Vec::new()));
        let project_state = Rc::new(ProjectUiState::default());
        let targets =
            control_target_choices(&automation_channel_fixture(), &control_scene_fixture());
        *project_state.control_targets.borrow_mut() = targets.clone();

        reset_project(ui, &recording_routes, &take_routes, &project_state);
        assert!(
            project_state.control_targets.borrow().is_empty(),
            "the control target cache must not survive a project reset"
        );
    }

    fn assert_control_mapping_events_report_durable_outcomes(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_control_create_visible(true);
        apply_control_mapping_event(
            ui,
            &IdeWorkerEvent::ControlMappingEdited {
                revision: 7,
                edit: IdeControlMappingEdit::Insert(IdeControlBindingRequest {
                    source: ControlSource::CvInput { input_index: 0 },
                    scope: ControlScope::Global,
                    transform: ControlTransform::Linear {
                        input_min: -1_000_000,
                        input_max: 1_000_000,
                        output_min: 0,
                        output_max: 1_000_000,
                    },
                    target: ControlTarget::MixerMain {
                        parameter: IdeControlMainParameter::Gain,
                    },
                }),
            },
        );
        assert!(!ui.get_control_create_visible());
        assert!(ui.get_status_text().contains("added in project revision 7"));
        assert!(ui.get_error_text().is_empty());

        for (edit, expected) in [
            (
                IdeControlMappingEdit::SetEnabled {
                    binding_id: "b".to_owned(),
                    enabled: false,
                },
                "disabled in project revision 8",
            ),
            (
                IdeControlMappingEdit::SetEnabled {
                    binding_id: "b".to_owned(),
                    enabled: true,
                },
                "enabled in project revision 8",
            ),
            (
                IdeControlMappingEdit::Remove {
                    binding_id: "b".to_owned(),
                },
                "removed in project revision 8",
            ),
        ] {
            apply_control_mapping_event(
                ui,
                &IdeWorkerEvent::ControlMappingEdited { revision: 8, edit },
            );
            assert!(
                ui.get_status_text().contains(expected),
                "expected {expected}, got {}",
                ui.get_status_text()
            );
        }

        apply_control_mapping_event(
            ui,
            &IdeWorkerEvent::ControlMappingEditing {
                edit: IdeControlMappingEdit::Remove {
                    binding_id: "b".to_owned(),
                },
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::EditingControlMapping);
    }

    #[test]
    fn only_a_bounded_non_empty_project_can_be_exported() {
        assert!(export_available(IdePlaybackExtent::Bounded {
            end_frame: 1
        }));
        assert!(!export_available(IdePlaybackExtent::Bounded {
            end_frame: 0
        }));
        assert!(!export_available(IdePlaybackExtent::Unbounded));
    }

    #[test]
    fn export_summary_reports_measurements_and_declines_to_invent_loudness() {
        let gated = export_summary(&OfflineExportReport {
            frame_count: 48_000,
            sample_rate_hz: 48_000,
            sample_peak: 0.5,
            true_peak: 0.5,
            integrated_lufs: Some(-14.2),
        });
        assert!(gated.contains("1.00 s"), "{gated}");
        assert!(gated.contains("-6.0 dBFS"), "{gated}");
        assert!(gated.contains("-6.0 dBTP"), "{gated}");
        assert!(gated.contains("-14.2 LUFS integrated"), "{gated}");

        let ungated = export_summary(&OfflineExportReport {
            frame_count: 100,
            sample_rate_hz: 48_000,
            sample_peak: 0.25,
            true_peak: 0.3,
            integrated_lufs: None,
        });
        assert!(
            ungated.contains("integrated loudness needs at least 400 ms"),
            "short material states why there is no value: {ungated}"
        );
        assert!(
            !ungated.contains("LUFS integrated"),
            "no placeholder loudness number is shown: {ungated}"
        );
        assert!(ungated.contains("true peak -10.5 dBTP"), "{ungated}");

        let silent = export_summary(&OfflineExportReport {
            frame_count: 48_000,
            sample_rate_hz: 48_000,
            sample_peak: 0.0,
            true_peak: 0.0,
            integrated_lufs: None,
        });
        assert_eq!(
            silent, "1.00 s  ·  silent",
            "silence is reported as silence, not as material too short to gate"
        );
        assert!(!silent.contains("400 ms"), "{silent}");
        assert!(!silent.contains("-inf"), "{silent}");
    }

    #[test]
    fn control_binding_requests_follow_the_contract_domain_rules() {
        let sources = control_source_choices();
        assert_eq!(
            sources.len(),
            8,
            "four CV inputs plus clock and reset edges"
        );
        let targets = vec![
            ControlTargetChoice {
                label: "Main gain".to_owned(),
                domain: ControlTargetDomain::Continuous,
                range_hint: CONTROL_CV_DOMAIN.to_owned(),
                target: ControlTarget::MixerMain {
                    parameter: IdeControlMainParameter::Gain,
                },
            },
            ControlTargetChoice {
                label: "Main mute".to_owned(),
                domain: ControlTargetDomain::Boolean,
                range_hint: String::new(),
                target: ControlTarget::MixerMain {
                    parameter: IdeControlMainParameter::Mute,
                },
            },
            ControlTargetChoice {
                label: "Transport play".to_owned(),
                domain: ControlTargetDomain::Action,
                range_hint: String::new(),
                target: ControlTarget::Transport {
                    action: TransportAction::Play,
                },
            },
        ];

        let cv = control_binding_request(&sources, &targets, 0, 0, false, "0", "1000000").unwrap();
        assert_eq!(cv.source, ControlSource::CvInput { input_index: 0 });
        assert_eq!(
            cv.transform,
            ControlTransform::Linear {
                input_min: -1_000_000,
                input_max: 1_000_000,
                output_min: 0,
                output_max: 1_000_000,
            },
            "a CV source keeps the full normalized input range"
        );

        let toggled = control_binding_request(&sources, &targets, 4, 1, false, "", "").unwrap();
        assert_eq!(
            toggled.transform,
            ControlTransform::Trigger {
                behavior: TriggerBehavior::Toggle
            }
        );
        let momentary = control_binding_request(&sources, &targets, 4, 1, true, "", "").unwrap();
        assert_eq!(
            momentary.transform,
            ControlTransform::Trigger {
                behavior: TriggerBehavior::Momentary
            }
        );

        let action = control_binding_request(&sources, &targets, 6, 2, true, "", "").unwrap();
        assert_eq!(
            action.transform,
            ControlTransform::Trigger {
                behavior: TriggerBehavior::Invoke
            },
            "an action target forces invoke regardless of the momentary control"
        );

        assert!(
            control_binding_request(&sources, &targets, 0, 1, false, "0", "1000000").is_err(),
            "a CV source may not drive a switch target"
        );
        assert!(
            control_binding_request(&sources, &targets, 4, 0, false, "0", "1000000").is_err(),
            "an edge source may not drive a value target"
        );
        assert!(control_binding_request(&sources, &targets, 0, 0, false, "0", "1000001").is_err());
        assert!(control_binding_request(&sources, &targets, 0, 0, false, "-1", "10").is_err());
        assert!(control_binding_request(&sources, &targets, 99, 0, false, "0", "10").is_err());
        assert!(control_binding_request(&sources, &targets, -1, 0, false, "0", "10").is_err());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one table pins every offered automation target against its compiled address"
    )]
    fn automation_target_choices_match_the_compiled_mixer_addresses() {
        let choices = automation_target_choices(&automation_channel_fixture());
        let table: Vec<(&str, bool, AutomationTarget)> = choices
            .iter()
            .map(|choice| (choice.label.as_str(), choice.boolean, choice.resolved))
            .collect();
        assert_eq!(
            table,
            vec![
                (
                    "Main gain",
                    false,
                    AutomationTarget::Main {
                        parameter: AutomationMainParameter::Gain
                    }
                ),
                (
                    "Main mute",
                    true,
                    AutomationTarget::Main {
                        parameter: AutomationMainParameter::Mute
                    }
                ),
                (
                    "Drums gain",
                    false,
                    AutomationTarget::Channel {
                        slot: 0,
                        parameter: AutomationChannelParameter::Gain
                    }
                ),
                (
                    "Drums pan",
                    false,
                    AutomationTarget::Channel {
                        slot: 0,
                        parameter: AutomationChannelParameter::Pan
                    }
                ),
                (
                    "Drums mute",
                    true,
                    AutomationTarget::Channel {
                        slot: 0,
                        parameter: AutomationChannelParameter::Mute
                    }
                ),
                (
                    "Drums solo",
                    true,
                    AutomationTarget::Channel {
                        slot: 0,
                        parameter: AutomationChannelParameter::Solo
                    }
                ),
                (
                    "Drums to Reverb send gain",
                    false,
                    AutomationTarget::Send {
                        slot: 0,
                        send: 0,
                        parameter: AutomationSendParameter::Gain
                    }
                ),
                (
                    "Drums to Reverb send pan",
                    false,
                    AutomationTarget::Send {
                        slot: 0,
                        send: 0,
                        parameter: AutomationSendParameter::Pan
                    }
                ),
                (
                    "Drums to Reverb send enabled",
                    true,
                    AutomationTarget::Send {
                        slot: 0,
                        send: 0,
                        parameter: AutomationSendParameter::Enabled
                    }
                ),
                (
                    "Reverb gain",
                    false,
                    AutomationTarget::Channel {
                        slot: 1,
                        parameter: AutomationChannelParameter::Gain
                    }
                ),
                (
                    "Reverb pan",
                    false,
                    AutomationTarget::Channel {
                        slot: 1,
                        parameter: AutomationChannelParameter::Pan
                    }
                ),
                (
                    "Reverb mute",
                    true,
                    AutomationTarget::Channel {
                        slot: 1,
                        parameter: AutomationChannelParameter::Mute
                    }
                ),
            ]
        );
        assert_eq!(choices[3].domain, AUTOMATION_PAN_DOMAIN);
        assert_eq!(choices[4].domain, AUTOMATION_SWITCH_DOMAIN);
        assert_eq!(
            automation_lane_label(
                AutomationTarget::Send {
                    slot: 0,
                    send: 0,
                    parameter: AutomationSendParameter::Enabled,
                },
                &choices
            ),
            "Drums to Reverb send enabled"
        );
        assert_eq!(
            automation_lane_label(
                AutomationTarget::Channel {
                    slot: 7,
                    parameter: AutomationChannelParameter::Gain,
                },
                &choices
            ),
            "Unresolved mixer target"
        );
    }

    fn assert_automation_targets_repopulate_after_a_project_reset(ui: &MainWindow) {
        let recording_routes: RecordingRoutes = Rc::new(RefCell::new(Vec::new()));
        let take_routes: TakeRoutes = Rc::new(RefCell::new(Vec::new()));
        let project_state = Rc::new(ProjectUiState::default());
        let channels = automation_channel_fixture();
        let empty: [MixerChannelFacts; 0] = [];

        set_automation_targets(ui, &project_state, &empty);
        assert_eq!(ui.get_automation_target_names().row_count(), 2);
        assert_eq!(ui.get_automation_target_boolean().row_count(), 2);
        assert_eq!(ui.get_automation_target_domain().row_count(), 2);

        set_automation_targets(ui, &project_state, &channels);
        assert_eq!(ui.get_automation_target_names().row_count(), 12);

        reset_project(ui, &recording_routes, &take_routes, &project_state);
        assert_eq!(ui.get_automation_target_names().row_count(), 0);

        set_automation_targets(ui, &project_state, &channels);
        assert_eq!(ui.get_automation_target_names().row_count(), 12);
        assert_eq!(ui.get_automation_target_boolean().row_count(), 12);
        assert_eq!(ui.get_automation_target_domain().row_count(), 12);
    }

    #[test]
    fn automation_take_requests_coerce_interpolation_and_reject_invalid_forms() {
        let choices = automation_target_choices(&automation_channel_fixture());
        let linear =
            automation_take_request(&choices, " Fade ", 2, 1, "0", "1920", "0=1000000").unwrap();
        assert_eq!(linear.name, "Fade");
        assert_eq!(linear.interpolation, AutomationInterpolation::Linear);
        assert_eq!(
            linear.target,
            IdeAutomationTarget::Channel {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                parameter: AutomationChannelParameter::Gain,
            }
        );
        assert_eq!((linear.start_tick, linear.end_tick), (0, 1_920));

        let held =
            automation_take_request(&choices, "Hold", 2, 0, "0", "1920", "0=1000000").unwrap();
        assert_eq!(held.interpolation, AutomationInterpolation::Hold);

        let switch = automation_take_request(&choices, "Mute", 4, 1, "0", "1920", "0=on").unwrap();
        assert_eq!(
            switch.interpolation,
            AutomationInterpolation::Hold,
            "a boolean target may not request linear interpolation"
        );
        assert_eq!(
            switch.points,
            vec![AutomationPoint::new(0, AutomationValue::Boolean(true))]
        );
        assert_eq!(
            switch.target,
            IdeAutomationTarget::Channel {
                track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
                parameter: AutomationChannelParameter::Mute,
            }
        );

        let send = automation_take_request(&choices, "Send", 8, 1, "0", "1920", "0=off").unwrap();
        assert_eq!(
            send.target,
            IdeAutomationTarget::Send {
                send_id: "018f47a2-6b2d-7c11-8a44-001122334469".to_owned(),
                parameter: AutomationSendParameter::Enabled,
            }
        );

        assert!(automation_take_request(&choices, "Out", 13, 0, "0", "1920", "0=0").is_err());
        assert!(automation_take_request(&choices, "Out", -1, 0, "0", "1920", "0=0").is_err());
        assert!(automation_take_request(&choices, "Empty", 2, 0, "1920", "1920", "0=0").is_err());
        assert!(automation_take_request(&choices, "Reversed", 2, 0, "1920", "0", "0=0").is_err());
        assert!(automation_take_request(&choices, "Switch", 4, 0, "0", "1920", "0=5").is_err());
    }

    #[test]
    fn mixer_automation_capture_request_normalizes_name_and_requires_current_target() {
        let choices = automation_target_choices(&automation_channel_fixture());
        let (name, target) =
            mixer_automation_capture_request(&choices, "  Live main gain  ", 0).unwrap();
        assert_eq!(name, "Live main gain");
        assert_eq!(
            target,
            IdeAutomationTarget::Main {
                parameter: AutomationMainParameter::Gain,
            }
        );
        assert!(mixer_automation_capture_request(&choices, "", 0).is_err());
        assert!(mixer_automation_capture_request(&choices, "Valid", -1).is_err());
        assert!(mixer_automation_capture_request(&choices, "Valid", 100).is_err());
    }

    fn assert_automation_take_events_report_durable_outcomes(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_automation_create_visible(true);
        ui.set_automation_create_name("Fade".into());
        ui.set_automation_create_points("0=1000000".into());

        apply_automation_take_creation_event(
            ui,
            &IdeWorkerEvent::AutomationTakeAdded {
                revision: 4,
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
                name: "Fade".to_owned(),
            },
        );
        assert!(!ui.get_automation_create_visible());
        assert!(ui.get_automation_create_name().is_empty());
        assert!(ui.get_automation_create_points().is_empty());
        assert!(ui.get_status_text().contains("project revision 4"));
        assert!(ui.get_error_text().is_empty());

        apply_automation_take_creation_event(ui, &IdeWorkerEvent::AutomationTakeAdding);
        assert_eq!(ui.get_app_mode(), AppMode::AddingAutomationTake);

        apply_automation_take_activation_event(
            ui,
            &IdeWorkerEvent::AutomationTakeActivationChanged {
                revision: 5,
                changed: true,
                enabled: false,
            },
        );
        assert!(
            ui.get_status_text()
                .contains("disabled in project revision 5")
        );
        assert!(!ui.get_status_text().contains("already"));

        apply_automation_take_activation_event(
            ui,
            &IdeWorkerEvent::AutomationTakeActivationChanged {
                revision: 5,
                changed: false,
                enabled: true,
            },
        );
        assert!(
            ui.get_status_text()
                .contains("was already enabled in project revision 5")
        );

        apply_automation_take_activation_event(
            ui,
            &IdeWorkerEvent::AutomationTakeActivationChanging { enabled: true },
        );
        assert_eq!(ui.get_app_mode(), AppMode::ChangingAutomationTakeActivation);

        ui.set_app_mode(AppMode::Running);
        ui.set_automation_edit_visible(true);
        ui.set_automation_edit_take_id("018f47a2-6b2d-7c11-8a44-0011223344a0".into());
        ui.set_automation_edit_name("Main mute".into());
        ui.set_automation_edit_points("0=off, 960=on".into());
        apply_automation_take_edit_event(
            ui,
            &IdeWorkerEvent::AutomationTakeEdited {
                revision: 6,
                changed: true,
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
                name: "Main mute".to_owned(),
            },
        );
        assert!(!ui.get_automation_edit_visible());
        assert!(ui.get_automation_edit_take_id().is_empty());
        assert!(ui.get_automation_edit_name().is_empty());
        assert!(ui.get_automation_edit_points().is_empty());
        assert!(
            ui.get_status_text()
                .contains("Main mute edited in project revision 6")
        );

        apply_automation_take_edit_event(
            ui,
            &IdeWorkerEvent::AutomationTakeEditing {
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::EditingAutomationTake);
        apply_automation_take_edit_event(
            ui,
            &IdeWorkerEvent::AutomationTakeEditCommittedWithWarning {
                take_id: "018f47a2-6b2d-7c11-8a44-0011223344a0".to_owned(),
                warning: Box::new(IdeAutomationTakeEditError::TakeUnavailable),
            },
        );
        assert!(ui.get_status_text().contains("committed"));
        assert!(ui.get_error_text().contains("unavailable"));
    }

    #[test]
    fn arrangement_duplication_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::DuplicateArrangementPlacement,
            ),
            AppMode::DuplicatingArrangementPlacement
        );
        assert_eq!(
            mode_after_failure(
                AppMode::DuplicatingArrangementPlacement,
                IdeWorkerCommandKind::DuplicateArrangementPlacement,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn arrangement_split_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SplitArrangementPlacement,
            ),
            AppMode::SplittingArrangementPlacement
        );
        assert_eq!(
            mode_after_failure(
                AppMode::SplittingArrangementPlacement,
                IdeWorkerCommandKind::SplitArrangementPlacement,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_gain_input_matches_native_execution_domain() {
        assert_eq!(parse_audio_clip_gain(" -12.5 "), Ok(-12.5));
        assert_eq!(parse_audio_clip_gain("0"), Ok(0.0));
        assert!(parse_audio_clip_gain("6.020599913279624").is_ok());
        assert!(parse_audio_clip_gain("6.1").is_err());
        assert!(parse_audio_clip_gain("NaN").is_err());
        assert!(parse_audio_clip_gain("inf").is_err());
        assert!(parse_audio_clip_gain("").is_err());
        assert!(parse_audio_clip_gain("not gain").is_err());
    }

    #[test]
    fn audio_clip_fade_input_matches_the_exact_clip_domain() {
        assert_eq!(parse_audio_clip_fades(" 1 ", "3", 4), Ok((1, 3)));
        assert_eq!(parse_audio_clip_fades("0", "0", 4), Ok((0, 0)));
        assert!(parse_audio_clip_fades("", "0", 4).is_err());
        assert!(parse_audio_clip_fades("1.5", "0", 4).is_err());
        assert!(parse_audio_clip_fades("0", "5", 4).is_err());
        assert!(parse_audio_clip_fades("3", "2", 4).is_err());
        assert!(parse_audio_clip_fades("9007199254740992", "0", u64::MAX).is_err());
    }

    #[test]
    fn audio_clip_loop_input_matches_the_exact_clip_domain() {
        assert_eq!(parse_audio_clip_loop(" 1 ", "3", 4), Ok((1, 3)));
        assert_eq!(parse_audio_clip_loop("0", "4", 4), Ok((0, 4)));
        assert!(parse_audio_clip_loop("", "4", 4).is_err());
        assert!(parse_audio_clip_loop("1.5", "4", 4).is_err());
        assert!(parse_audio_clip_loop("0", "0", 4).is_err());
        assert!(parse_audio_clip_loop("3", "3", 4).is_err());
        assert!(parse_audio_clip_loop("3", "2", 4).is_err());
        assert!(parse_audio_clip_loop("0", "5", 4).is_err());
        assert!(parse_audio_clip_loop("0", "9007199254740992", u64::MAX).is_err());
    }

    #[test]
    fn audio_clip_source_start_input_matches_the_exact_frame_domain() {
        assert_eq!(parse_audio_clip_source_start(" 25 "), Ok(25));
        assert_eq!(parse_audio_clip_source_start("0"), Ok(0));
        assert_eq!(
            parse_audio_clip_source_start("9007199254740991"),
            Ok(MAX_SAFE_TIMELINE_TICK)
        );
        assert!(parse_audio_clip_source_start("").is_err());
        assert!(parse_audio_clip_source_start("-1").is_err());
        assert!(parse_audio_clip_source_start("1.5").is_err());
        assert!(parse_audio_clip_source_start("9007199254740992").is_err());
        assert!(parse_audio_clip_source_start("18446744073709551616").is_err());
    }

    #[test]
    fn audio_clip_source_frame_count_requires_a_positive_exact_frame_domain() {
        assert_eq!(parse_audio_clip_source_frame_count(" 25 "), Ok(25));
        assert_eq!(
            parse_audio_clip_source_frame_count("9007199254740991"),
            Ok(MAX_SAFE_TIMELINE_TICK)
        );
        assert!(parse_audio_clip_source_frame_count("0").is_err());
        assert!(parse_audio_clip_source_frame_count("").is_err());
        assert!(parse_audio_clip_source_frame_count("-1").is_err());
        assert!(parse_audio_clip_source_frame_count("1.5").is_err());
        assert!(parse_audio_clip_source_frame_count("9007199254740992").is_err());
    }

    #[test]
    fn audio_clip_rename_command_normalizes_name_and_preserves_identity() {
        assert!(matches!(
            audio_clip_rename_command("clip-a", "Verse", " Lead vocal "),
            Ok(Some(IdeWorkerCommand::RenameAudioClip { clip_id, name }))
                if clip_id == "clip-a" && name == "Lead vocal"
        ));
        assert!(matches!(
            audio_clip_rename_command("clip-a", "Verse", " Verse "),
            Ok(None)
        ));
        assert!(audio_clip_rename_command("clip-a", "Verse", "").is_err());
        assert!(audio_clip_rename_command("clip-a", "Verse", "   ").is_err());
        assert!(audio_clip_rename_command("clip-a", "Verse", &"x".repeat(121)).is_err());
        let unicode_boundary = "\u{00e9}".repeat(120);
        assert!(matches!(
            audio_clip_rename_command("clip-a", "Verse", &unicode_boundary),
            Ok(Some(IdeWorkerCommand::RenameAudioClip { clip_id, name }))
                if clip_id == "clip-a" && name == unicode_boundary
        ));
        assert!(audio_clip_rename_command("clip-a", "Verse", &"\u{00e9}".repeat(121)).is_err());
    }

    #[test]
    fn audio_clip_duplication_command_normalizes_name_and_preserves_source_identity() {
        assert!(matches!(
            audio_clip_duplication_command("clip-a", " Verse copy "),
            Ok(IdeWorkerCommand::DuplicateAudioClip { clip_id, name })
                if clip_id == "clip-a" && name == "Verse copy"
        ));
        assert!(audio_clip_duplication_command("clip-a", "").is_err());
        assert!(audio_clip_duplication_command("clip-a", "   ").is_err());
        assert!(audio_clip_duplication_command("clip-a", &"x".repeat(121)).is_err());
        let unicode_boundary = "\u{00e9}".repeat(120);
        assert!(matches!(
            audio_clip_duplication_command("clip-a", &unicode_boundary),
            Ok(IdeWorkerCommand::DuplicateAudioClip { clip_id, name })
                if clip_id == "clip-a" && name == unicode_boundary
        ));
    }

    #[test]
    fn audio_clip_rename_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(AppMode::RenamingClip, IdeWorkerCommandKind::RenameAudioClip),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_duplication_failure_keeps_the_form_recoverable() {
        assert_eq!(
            mode_after_failure(
                AppMode::DuplicatingClip,
                IdeWorkerCommandKind::DuplicateAudioClip,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_duplication_submission_enters_pending_mode_immediately() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::DuplicateAudioClip,
            ),
            AppMode::DuplicatingClip
        );
        assert_eq!(
            mode_after_successful_submission(AppMode::Running, IdeWorkerCommandKind::Transport),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_removal_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::RemoveAudioClip,
            ),
            AppMode::RemovingClip
        );
        assert_eq!(
            mode_after_failure(AppMode::RemovingClip, IdeWorkerCommandKind::RemoveAudioClip),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_gain_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipGain,
                IdeWorkerCommandKind::SetAudioClipGain,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_fade_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetAudioClipFades,
            ),
            AppMode::ChangingClipFades
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipFades,
                IdeWorkerCommandKind::SetAudioClipFades,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_loop_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetAudioClipLoop,
            ),
            AppMode::ChangingClipLoop
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipLoop,
                IdeWorkerCommandKind::SetAudioClipLoop,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_reverse_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetAudioClipReverse,
            ),
            AppMode::ChangingClipReverse
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipReverse,
                IdeWorkerCommandKind::SetAudioClipReverse,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_source_slip_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::SetAudioClipSourceStart,
            ),
            AppMode::ChangingClipSource
        );
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipSource,
                IdeWorkerCommandKind::SetAudioClipSourceStart,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn audio_clip_source_trim_submission_and_failure_use_a_dedicated_mode() {
        assert_eq!(
            mode_after_successful_submission(
                AppMode::Running,
                IdeWorkerCommandKind::TrimAudioClipSource,
            ),
            AppMode::TrimmingClipSource
        );
        assert_eq!(
            mode_after_failure(
                AppMode::TrimmingClipSource,
                IdeWorkerCommandKind::TrimAudioClipSource,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn committed_audio_clip_edit_warning_presentation_identifies_the_matching_form() {
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::RenameAudioClip),
            Some((AudioClipEditWarningForm::Rename, "Audio-clip rename"))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipActive),
            Some((AudioClipEditWarningForm::None, "Audio-clip activation edit"))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipGain),
            Some((AudioClipEditWarningForm::Gain, "Audio-clip gain edit"))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipFades),
            Some((AudioClipEditWarningForm::Fades, "Audio-clip fade edit"))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipLoop),
            Some((AudioClipEditWarningForm::Loop, "Audio-clip loop edit"))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipReverse),
            Some((
                AudioClipEditWarningForm::Reverse,
                "Audio-clip direction edit"
            ))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SetAudioClipSourceStart),
            Some((
                AudioClipEditWarningForm::SourceSlip,
                "Audio-clip source-slip edit"
            ))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::TrimAudioClipSource),
            Some((
                AudioClipEditWarningForm::SourceTrim,
                "Audio-clip source-trim edit"
            ))
        );
        assert_eq!(
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SaveProject),
            None
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the single generated-window regression preserves related reopen and warning authority ordering"
    )]
    fn committed_audio_clip_edit_warnings_preserve_reopen_authority() {
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        assert_eq!(
            usize::try_from(ui.get_max_scene_count()).unwrap(),
            MAX_IDE_SCENES
        );
        assert_window_contracts(&ui);
        assert_undo_redo_shortcut_contract(&ui);
        assert_project_save_shortcut_contract(&ui);
        assert_project_copy_callback_contract(&ui);
        assert_project_export_shortcut_contract(&ui);
        assert_project_creation_shortcut_contract(&ui);
        assert_project_open_shortcut_contract(&ui);
        assert_audio_track_creation_shortcut_contract(&ui);
        assert_project_close_shortcut_contract(&ui);
        assert_unsaved_mixer_close_contract(&ui);
        assert_transport_space_shortcut_contract(&ui);
        assert_transport_running_observation_contract(&ui);
        assert_audio_import_shortcut_contract(&ui);
        assert_form_cancellation_shortcut_contract(&ui);
        ui.set_app_mode(AppMode::PreparingRuntime);
        ui.set_status_text("Choose a Runtime destination".into());
        ui.set_error_text("retained message".into());
        apply_idle_running_event(&ui);
        assert_eq!(ui.get_app_mode(), AppMode::PreparingRuntime);
        assert_eq!(ui.get_status_text(), "Choose a Runtime destination");
        assert_eq!(ui.get_error_text(), "retained message");

        ui.set_app_mode(AppMode::Running);
        apply_idle_running_event(&ui);
        assert_eq!(ui.get_status_text(), "Runtime evidence updated");
        assert!(ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Running);
        ui.set_clip_fades_visible(true);
        ui.set_clip_fades_id("clip-a".into());
        ui.set_clip_fades_name("Clip A".into());
        ui.set_clip_fades_duration("4".into());
        ui.set_clip_fades_original_in("0".into());
        ui.set_clip_fades_original_out("0".into());
        ui.set_clip_fades_in("1".into());
        ui.set_clip_fades_out("2".into());

        apply_audio_clip_edit_committed_warning(
            &ui,
            &IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipFades,
                warning: IdeAudioClipEditCommittedWarning::Fades(Box::new(
                    IdeAudioClipFadeEditError::InvalidClipIdentity,
                )),
            },
        );

        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_clip_fades_visible());
        assert!(ui.get_clip_fades_id().is_empty());
        assert!(ui.get_clip_fades_in().is_empty());
        assert!(ui.get_clip_fades_out().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Closed);
        ui.set_clip_loop_visible(true);
        ui.set_clip_loop_id("clip-a".into());
        ui.set_clip_loop_name("Clip A".into());
        ui.set_clip_loop_duration("4".into());
        ui.set_clip_loop_original_enabled(false);
        ui.set_clip_loop_original_start("0".into());
        ui.set_clip_loop_original_end("4".into());
        ui.set_clip_loop_enabled(true);
        ui.set_clip_loop_start("1".into());
        ui.set_clip_loop_end("3".into());

        apply_audio_clip_edit_committed_warning(
            &ui,
            &IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipLoop,
                warning: IdeAudioClipEditCommittedWarning::Loop(Box::new(
                    IdeAudioClipLoopEditError::InvalidClipIdentity,
                )),
            },
        );

        assert_eq!(ui.get_app_mode(), AppMode::Closed);
        assert!(!ui.get_clip_loop_visible());
        assert!(ui.get_clip_loop_id().is_empty());
        assert!(ui.get_clip_loop_start().is_empty());
        assert!(ui.get_clip_loop_end().is_empty());
        assert!(
            ui.get_status_text()
                .contains("committed, but the project could not be reopened")
        );
        assert!(!ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Running);
        apply_audio_clip_edit_committed_warning(
            &ui,
            &IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipReverse,
                warning: IdeAudioClipEditCommittedWarning::Reverse(Box::new(
                    IdeAudioClipReverseEditError::InvalidClipIdentity,
                )),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());

        assert_committed_audio_clip_source_slip_warning(&ui);
        assert_committed_audio_clip_source_trim_warning(&ui);
        assert_arrangement_scene_launch_edit_events(&ui);
        assert_arrangement_scene_launch_committed_warnings(&ui);
        assert_scene_authoring_forms(&ui);
        assert_scene_removal_events_reset_form(&ui);
        assert_scene_duplication_events_reset_form(&ui);
        assert_scene_rename_success_resets_form(&ui);
        assert_scene_activation_events_preserve_state(&ui);
        assert_automation_take_events_report_durable_outcomes(&ui);
        assert_automation_targets_repopulate_after_a_project_reset(&ui);
        assert_control_targets_repopulate_after_a_project_reset(&ui);
        assert_control_mapping_events_report_durable_outcomes(&ui);
    }

    fn assert_window_contracts(ui: &MainWindow) {
        assert_recent_project_window_contract(ui);
        assert_audio_import_window_contract(ui);
        assert_audio_track_content_removal_window_contract(ui);
        assert_audio_track_freeze_window_contract(ui);
        assert_audio_track_flatten_window_contract(ui);
        assert_audio_clip_removal_window_contract(ui);
        assert_audio_clip_source_trim_window_contract(ui);
        assert_unused_sample_cleanup_window_contract(ui);
        assert_native_filter_window_contract(ui);
        assert_automation_take_removal_window_contract(ui);
    }

    fn assert_recent_project_window_contract(ui: &MainWindow) {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let fixture = std::env::temp_dir().join(format!(
            "hyper-desktop-ui-window-history-{}-{nonce}",
            std::process::id()
        ));
        let config = fixture.join("config");
        let first = fixture.join("first");
        let second = fixture.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let history = ProjectHistory::at(&config);
        history
            .remember(
                "First project with a deliberately long visible name",
                &first,
            )
            .unwrap();
        let projects = history.remember("Second", &second).unwrap();
        let routes = UiRoutes::default();
        let project_state = Rc::new(ProjectUiState::default());

        ui.set_project_history_warning("stale warning".into());
        apply_recent_projects(ui, &routes.recent_projects, &projects);
        assert!(ui.get_project_history_warning().is_empty());
        let names = ui.get_recent_project_names();
        let display_names = ui.get_recent_project_display_names();
        let paths = ui.get_recent_project_paths();
        assert_eq!(
            (
                names.row_count(),
                display_names.row_count(),
                paths.row_count()
            ),
            (2, 2, 2)
        );
        for (index, project) in projects.iter().enumerate() {
            assert_eq!(names.row_data(index).unwrap(), project.name());
            assert_eq!(
                paths.row_data(index).unwrap(),
                project.path().display().to_string()
            );
            assert_eq!(routes.recent_projects.borrow()[index], project.path());
        }

        let bad_config = fixture.join("bad-config");
        std::fs::write(&bad_config, b"not a directory").unwrap();
        let warning = ProjectHistory::at(&bad_config).load().unwrap_err();
        apply_event(
            ui,
            &routes,
            &project_state,
            IdeWorkerEvent::ProjectHistoryWarning(warning),
        );
        assert_eq!(ui.get_recent_project_names().row_count(), 2);
        assert!(!ui.get_project_history_warning().is_empty());

        let worker: WorkerCell = Rc::new(RefCell::new(None));
        bind_recent_project_callback(ui, &worker, &routes.recent_projects);
        ui.set_app_mode(AppMode::Closed);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_open_recent_project(1);
        assert_eq!(
            ui.get_project_location(),
            projects[1].path().to_string_lossy()
        );
        assert!(!ui.get_error_text().is_empty());

        routes.recent_projects.borrow_mut().truncate(1);
        ui.set_app_mode(AppMode::Closed);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_open_recent_project(1);
        assert!(
            ui.get_error_text()
                .contains("selection is no longer available")
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_project_location("retained".into());
        ui.global::<IdeActions>().invoke_open_recent_project(0);
        assert_eq!(ui.get_project_location(), "retained");

        std::fs::remove_dir_all(fixture).unwrap();
    }

    fn assert_audio_import_window_contract(ui: &MainWindow) {
        let routes: AudioImportRoutes = Rc::default();
        let channels = [
            IdeMixerChannel::for_test("track-a", "Audio A", IdeMixerChannelKind::Audio),
            IdeMixerChannel::for_test("bus-a", "Group A", IdeMixerChannelKind::Group),
            IdeMixerChannel::for_test("track-b", "Audio B", IdeMixerChannelKind::Audio),
        ];
        set_audio_import_targets(ui, &routes, &channels);
        let names = ui.get_audio_import_track_names();
        assert_eq!(names.row_count(), 2);
        assert_eq!(names.row_data(0).unwrap(), "Audio A");
        assert_eq!(names.row_data(1).unwrap(), "Audio B");
        assert_eq!(routes.borrow().as_slice(), ["track-a", "track-b"]);

        assert_audio_import_callback_contract(ui, &routes);
        assert_audio_import_event_contract(ui, &routes);
    }

    fn assert_audio_track_content_removal_window_contract(ui: &MainWindow) {
        let worker: WorkerCell = Rc::new(RefCell::new(None));
        bind_audio_track_content_removal_callback(ui, &worker);
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![
            MixerChannelRow {
                index: 0,
                track_id: track_id.into(),
                name: "Lead".into(),
                display_name: "Lead".into(),
                kind: "AUDIO".into(),
                frozen: false,
                flattenable: false,
                state_available: true,
                gain_percent: 100.0,
                pan_percent: 0.0,
                muted: false,
                solo: false,
                meter_left_level: 0.0,
                meter_right_level: 0.0,
                meter_left_reading: "—".into(),
                meter_right_reading: "—".into(),
                solo_available: true,
                renameable: true,
                removable: false,
                can_move_earlier: false,
                can_move_later: false,
                active: true,
                activation_toggleable: true,
            },
        ]))));
        ui.set_app_mode(AppMode::Running);
        ui.set_remove_track_content_visible(false);
        ui.set_remove_track_content_id(SharedString::default());
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_remove_audio_track_content(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("confirmation is no longer current")
        );

        ui.set_remove_track_content_visible(true);
        ui.set_remove_track_content_id(track_id.into());
        ui.set_remove_track_content_name("Lead".into());
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_remove_audio_track_content(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("worker is no longer available")
        );

        apply_audio_track_content_removal_event(ui, &IdeWorkerEvent::AudioTrackContentRemoving);
        assert_eq!(ui.get_app_mode(), AppMode::RemovingTrack);
        apply_audio_track_content_removal_event(
            ui,
            &IdeWorkerEvent::AudioTrackContentRemoved {
                revision: 9,
                track_id: track_id.to_owned(),
            },
        );
        assert!(!ui.get_remove_track_content_visible());
        assert!(ui.get_remove_track_content_id().is_empty());
        assert!(ui.get_status_text().contains("revision 9"));
        assert!(ui.get_error_text().is_empty());
    }

    fn assert_audio_track_freeze_window_contract(ui: &MainWindow) {
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_audio_track_freeze_callbacks_with(
            ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![
            audio_track_mixer_row(0, track_id),
        ]))));
        ui.set_app_mode(AppMode::Running);
        reset_audio_track_freeze_form(ui);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_freeze_audio_track(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("freeze confirmation is no longer current")
        );
        assert!(submitted.borrow().is_empty());

        ui.set_track_freeze_visible(true);
        ui.set_track_freeze_id(track_id.into());
        ui.set_track_freeze_name("Lead".into());
        ui.set_track_freeze_is_unfreeze(false);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_freeze_audio_track(track_id.into());
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::FreezeAudioTrack { track_id: submitted_track_id }]
                if submitted_track_id == track_id
        ));
        assert!(ui.get_error_text().is_empty());

        ui.set_track_freeze_is_unfreeze(true);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_freeze_audio_track(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("freeze confirmation is no longer current")
        );
        assert_eq!(submitted.borrow().len(), 1);

        let mut frozen = audio_track_mixer_row(0, track_id);
        frozen.frozen = true;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![frozen]))));
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_unfreeze_audio_track(track_id.into());
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::FreezeAudioTrack { track_id: frozen_track_id }, IdeWorkerCommand::UnfreezeAudioTrack { track_id: unfrozen_track_id }]
                if frozen_track_id == track_id && unfrozen_track_id == track_id
        ));
        assert!(ui.get_error_text().is_empty());

        ui.set_track_freeze_is_unfreeze(false);
        ui.set_track_freeze_is_flatten(true);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_freeze_audio_track(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("freeze confirmation is no longer current")
        );
        assert_eq!(submitted.borrow().len(), 2);
        reset_audio_track_freeze_form(ui);

        apply_audio_track_freeze_event(ui, &IdeWorkerEvent::AudioTrackFreezing);
        assert_eq!(ui.get_app_mode(), AppMode::FreezingTrack);
        apply_audio_track_freeze_event(
            ui,
            &IdeWorkerEvent::AudioTrackFrozen {
                revision: 9,
                track_id: track_id.to_owned(),
            },
        );
        assert!(!ui.get_track_freeze_visible());
        assert!(ui.get_track_freeze_id().is_empty());
        assert!(ui.get_status_text().contains("revision 9"));
        assert!(ui.get_error_text().is_empty());

        ui.set_track_freeze_visible(true);
        ui.set_track_freeze_id(track_id.into());
        ui.set_track_freeze_name("Lead".into());
        ui.set_track_freeze_is_unfreeze(true);
        apply_audio_track_unfreeze_event(ui, &IdeWorkerEvent::AudioTrackUnfreezing);
        assert_eq!(ui.get_app_mode(), AppMode::UnfreezingTrack);
        apply_audio_track_unfreeze_event(
            ui,
            &IdeWorkerEvent::AudioTrackUnfrozen {
                revision: 10,
                track_id: track_id.to_owned(),
            },
        );
        assert!(!ui.get_track_freeze_visible());
        assert!(ui.get_track_freeze_id().is_empty());
        assert!(ui.get_status_text().contains("revision 10"));
        assert!(ui.get_error_text().is_empty());
    }

    fn assert_audio_track_flatten_window_contract(ui: &MainWindow) {
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_audio_track_freeze_callbacks_with(
            ui,
            Rc::new(move |_, command| submitted_by_callback.borrow_mut().push(command)),
        );
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        let mut frozen = audio_track_mixer_row(0, track_id);
        frozen.frozen = true;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![frozen]))));
        ui.set_app_mode(AppMode::Running);
        reset_audio_track_freeze_form(ui);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_flatten_audio_track(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("flatten confirmation is no longer current")
        );
        assert!(submitted.borrow().is_empty());

        ui.set_track_freeze_visible(true);
        ui.set_track_freeze_id(track_id.into());
        ui.set_track_freeze_name("Lead".into());
        ui.set_track_freeze_is_unfreeze(false);
        ui.set_track_freeze_is_flatten(true);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_flatten_audio_track(track_id.into());
        assert!(
            ui.get_error_text()
                .contains("no longer has complete freeze authority")
        );
        assert!(submitted.borrow().is_empty());

        let mut flattenable = audio_track_mixer_row(0, track_id);
        flattenable.frozen = true;
        flattenable.flattenable = true;
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(vec![flattenable]))));
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_flatten_audio_track(track_id.into());
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::FlattenAudioTrack {
                track_id: flattened_track_id
            }] if flattened_track_id == track_id
        ));
        assert!(ui.get_error_text().is_empty());

        apply_audio_track_flatten_event(ui, &IdeWorkerEvent::AudioTrackFlattening);
        assert_eq!(ui.get_app_mode(), AppMode::FlatteningTrack);
        apply_audio_track_flatten_event(
            ui,
            &IdeWorkerEvent::AudioTrackFlattened {
                revision: 11,
                track_id: track_id.to_owned(),
            },
        );
        assert!(!ui.get_track_freeze_visible());
        assert!(ui.get_track_freeze_id().is_empty());
        assert!(ui.get_status_text().contains("revision 11"));
        assert!(ui.get_error_text().is_empty());
    }

    fn assert_audio_clip_removal_window_contract(ui: &MainWindow) {
        let worker: WorkerCell = Rc::new(RefCell::new(None));
        let project_state = Rc::new(ProjectUiState::default());
        bind_audio_clip_removal_callback(ui, &worker, &project_state);

        ui.set_app_mode(AppMode::Running);
        ui.set_clip_remove_visible(true);
        ui.set_clip_remove_id("clip-a".into());
        ui.set_clip_remove_name("Unused clip".into());
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_remove_audio_clip("stale-clip".into());
        assert!(ui.get_error_text().contains("no longer current"));
        assert!(ui.get_clip_remove_visible());

        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>()
            .invoke_remove_audio_clip("clip-a".into());
        assert!(ui.get_error_text().contains("no longer available"));
        assert!(ui.get_clip_remove_visible());

        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_audio_clip_removal_callback_with(
            ui,
            |clip_id| (clip_id == "clip-a").then(|| clip_id.to_owned()),
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );
        ui.set_error_text(SharedString::default());
        ui.set_app_mode(AppMode::Closed);
        ui.global::<IdeActions>()
            .invoke_remove_audio_clip("clip-a".into());
        assert!(submitted.borrow().is_empty());
        assert!(ui.get_error_text().contains("no longer current"));

        ui.set_error_text(SharedString::default());
        ui.set_app_mode(AppMode::Running);
        ui.set_clip_remove_visible(false);
        ui.global::<IdeActions>()
            .invoke_remove_audio_clip("clip-a".into());
        assert!(submitted.borrow().is_empty());
        assert!(ui.get_error_text().contains("no longer current"));

        ui.set_error_text(SharedString::default());
        ui.set_clip_remove_visible(true);
        ui.global::<IdeActions>()
            .invoke_remove_audio_clip("clip-a".into());
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::RemoveAudioClip { clip_id }] if clip_id == "clip-a"
        ));
        assert!(ui.get_error_text().is_empty());

        apply_audio_clip_removal_event(ui, &IdeWorkerEvent::AudioClipRemoving);
        assert_eq!(ui.get_app_mode(), AppMode::RemovingClip);
        assert!(ui.get_clip_remove_visible());
        apply_audio_clip_removal_event(
            ui,
            &IdeWorkerEvent::AudioClipRemoved {
                revision: 9,
                clip_id: "clip-a".to_owned(),
            },
        );
        assert!(!ui.get_clip_remove_visible());
        assert!(ui.get_clip_remove_id().is_empty());
        assert!(ui.get_status_text().contains("source media was retained"));
        assert!(ui.get_error_text().is_empty());
    }

    fn assert_audio_clip_source_trim_window_contract(ui: &MainWindow) {
        let available = Rc::new(Cell::new(true));
        let available_for_callback = Rc::clone(&available);
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_audio_clip_source_trim_callback_with(
            ui,
            move |clip_id| {
                (available_for_callback.get() && clip_id == "clip-a")
                    .then(|| ("clip-a".to_owned(), 0, 100))
            },
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_clip_source_trim_visible(true);
        ui.set_clip_source_trim_id("clip-a".into());
        ui.set_clip_source_trim_name("Clip A".into());
        ui.set_clip_source_trim_original_start("0".into());
        ui.set_clip_source_trim_original_frame_count("100".into());
        ui.set_clip_source_trim_start("25".into());
        ui.set_clip_source_trim_frame_count("50".into());
        ui.set_error_text(SharedString::default());

        ui.global::<IdeActions>().invoke_trim_audio_clip_source(
            "stale-clip".into(),
            "25".into(),
            "50".into(),
        );
        assert!(submitted.borrow().is_empty());
        assert!(ui.get_error_text().contains("no longer available"));
        assert!(ui.get_clip_source_trim_visible());
        assert_eq!(ui.get_clip_source_trim_start(), "25");
        assert_eq!(ui.get_clip_source_trim_frame_count(), "50");

        available.set(false);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_trim_audio_clip_source(
            "clip-a".into(),
            "25".into(),
            "50".into(),
        );
        assert!(submitted.borrow().is_empty());
        assert!(ui.get_error_text().contains("no longer available"));
        assert!(ui.get_clip_source_trim_visible());
        assert_eq!(ui.get_clip_source_trim_start(), "25");
        assert_eq!(ui.get_clip_source_trim_frame_count(), "50");

        available.set(true);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_trim_audio_clip_source(
            "clip-a".into(),
            "25".into(),
            "50".into(),
        );
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::TrimAudioClipSource {
                clip_id,
                source_start_frame: 25,
                source_frame_count: 50,
            }] if clip_id == "clip-a"
        ));
        assert!(ui.get_error_text().is_empty());
        assert!(ui.get_clip_source_trim_visible());
        assert_eq!(ui.get_clip_source_trim_id(), "clip-a");
        assert_eq!(ui.get_clip_source_trim_start(), "25");
        assert_eq!(ui.get_clip_source_trim_frame_count(), "50");
    }

    fn assert_unused_sample_cleanup_window_contract(ui: &MainWindow) {
        let confirmation = IdeUnusedSampleCleanupSummary::new([7; 32], 9, 2, 4_096);
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        bind_unused_sample_cleanup_callback_with(
            ui,
            move || Some(confirmation),
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );

        ui.set_app_mode(AppMode::Closed);
        ui.set_unused_sample_cleanup_confirming(true);
        ui.global::<IdeActions>().invoke_clean_unused_samples();
        assert!(submitted.borrow().is_empty());

        ui.set_app_mode(AppMode::Running);
        ui.set_unused_sample_assets(2);
        ui.set_unused_sample_bytes("4096 bytes".into());
        ui.set_unused_sample_cleanup_confirming(false);
        ui.global::<IdeActions>().invoke_clean_unused_samples();
        assert!(submitted.borrow().is_empty());
        assert!(ui.get_error_text().contains("no longer current"));

        bind_unused_sample_cleanup_callback_with(
            ui,
            || None,
            |_, _| panic!("missing cleanup authority must not submit"),
        );
        ui.set_unused_sample_cleanup_confirming(true);
        ui.global::<IdeActions>().invoke_clean_unused_samples();
        assert!(ui.get_error_text().contains("authority is unavailable"));

        bind_unused_sample_cleanup_callback_with(
            ui,
            || Some(IdeUnusedSampleCleanupSummary::new([7; 32], 9, 0, 0)),
            |_, _| panic!("empty cleanup authority must not submit"),
        );
        ui.global::<IdeActions>().invoke_clean_unused_samples();
        assert!(ui.get_error_text().contains("no unused sample media"));

        let submitted_by_callback = Rc::clone(&submitted);
        bind_unused_sample_cleanup_callback_with(
            ui,
            move || Some(confirmation),
            move |_, command| submitted_by_callback.borrow_mut().push(command),
        );
        ui.set_error_text(SharedString::default());
        ui.set_unused_sample_cleanup_confirming(true);
        ui.global::<IdeActions>().invoke_clean_unused_samples();
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::CleanUnusedSamples { confirmation: submitted }]
                if *submitted == confirmation
        ));

        apply_unused_sample_cleanup_event(
            ui,
            &IdeWorkerEvent::UnusedSampleCleanupStarting {
                assets: 2,
                bytes: 4_096,
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::CleaningSamples);
        apply_unused_sample_cleanup_event(
            ui,
            &IdeWorkerEvent::UnusedSamplesCleaned {
                revision: 10,
                assets: 2,
                bytes: 4_096,
            },
        );
        assert!(!ui.get_unused_sample_cleanup_confirming());
        assert!(
            ui.get_status_text()
                .contains("external originals were retained")
        );
    }

    fn assert_audio_import_callback_contract(ui: &MainWindow, routes: &AudioImportRoutes) {
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let submitted_by_callback = Rc::clone(&submitted);
        let worker: WorkerCell = Rc::new(RefCell::new(None));
        bind_audio_import_callback_with(ui, &worker, routes, move |weak, _, track_id| {
            let ui = weak.upgrade().unwrap();
            complete_audio_import_choice_with(
                &ui,
                track_id,
                AudioImportChoice::Selected(vec![
                    PathBuf::from("kick.wav"),
                    PathBuf::from("snare.flac"),
                ]),
                |command| submitted_by_callback.borrow_mut().push(command),
            );
        });
        ui.set_project_history_warning("retained history warning".into());
        ui.set_app_mode(AppMode::Running);
        ui.set_error_text("stale error".into());
        ui.global::<IdeActions>().invoke_choose_audio_import(1);
        assert!(matches!(
            submitted.borrow().as_slice(),
            [IdeWorkerCommand::ImportAudio { paths, track_id }]
                if paths == &[PathBuf::from("kick.wav"), PathBuf::from("snare.flac")]
                    && track_id == "track-b"
        ));
        assert_eq!(ui.get_app_mode(), AppMode::ImportingAudio);
        assert!(ui.get_error_text().is_empty());
        assert_eq!(ui.get_project_history_warning(), "retained history warning");

        ui.set_app_mode(AppMode::Closed);
        ui.global::<IdeActions>().invoke_choose_audio_import(0);
        assert_eq!(
            submitted.borrow().len(),
            1,
            "Closed mode owns no import action"
        );

        ui.set_app_mode(AppMode::Running);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_choose_audio_import(-1);
        assert!(ui.get_error_text().contains("no longer available"));
        assert_eq!(submitted.borrow().len(), 1);

        routes.borrow_mut().truncate(1);
        ui.set_error_text(SharedString::default());
        ui.global::<IdeActions>().invoke_choose_audio_import(1);
        assert!(ui.get_error_text().contains("no longer available"));
        assert_eq!(submitted.borrow().len(), 1);
        assert_eq!(ui.get_project_history_warning(), "retained history warning");

        *routes.borrow_mut() = vec!["track-a".to_owned(), "track-b".to_owned()];
        ui.set_selected_audio_import_track_index(1);
        set_audio_import_targets(
            ui,
            routes,
            &[
                IdeMixerChannel::for_test("track-b", "Audio B", IdeMixerChannelKind::Audio),
                IdeMixerChannel::for_test("track-a", "Audio A", IdeMixerChannelKind::Audio),
            ],
        );
        assert_eq!(ui.get_selected_audio_import_track_index(), 0);

        bind_audio_import_callback_with(ui, &worker, routes, move |weak, _, track_id| {
            let ui = weak.upgrade().unwrap();
            complete_audio_import_choice_with(&ui, track_id, AudioImportChoice::Cancelled, |_| {
                panic!("a cancelled chooser must not submit a command")
            });
        });
        ui.set_app_mode(AppMode::Running);
        ui.global::<IdeActions>().invoke_choose_audio_import(0);
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert_eq!(ui.get_status_text(), "Audio import cancelled");

        bind_audio_import_callback_with(ui, &worker, routes, move |weak, worker, track_id| {
            let ui = weak.upgrade().unwrap();
            complete_audio_import_choice(
                &ui,
                &worker,
                track_id,
                AudioImportChoice::Selected(vec![PathBuf::from("kick.wav")]),
            );
        });
        ui.set_app_mode(AppMode::Running);
        ui.global::<IdeActions>().invoke_choose_audio_import(0);
        assert_eq!(ui.get_app_mode(), AppMode::Unavailable);
        assert!(ui.get_error_text().contains("no longer available"));
    }

    fn assert_audio_import_event_contract(ui: &MainWindow, routes: &AudioImportRoutes) {
        let ui_routes = UiRoutes {
            audio_imports: Rc::clone(routes),
            ..UiRoutes::default()
        };
        let project_state = Rc::new(ProjectUiState::default());
        ui.set_project_history_warning("retained history warning".into());
        apply_event(
            ui,
            &ui_routes,
            &project_state,
            IdeWorkerEvent::AudioImporting { files: 2 },
        );
        assert_eq!(ui.get_app_mode(), AppMode::ImportingAudio);
        assert_eq!(ui.get_status_text(), "Importing 2 audio files…");
        assert!(ui.get_error_text().is_empty());

        apply_event(
            ui,
            &ui_routes,
            &project_state,
            IdeWorkerEvent::AudioImported(IdeAudioImportBatchOutcome::for_test(
                2,
                7,
                vec!["clip-a".to_owned(), "clip-b".to_owned()],
            )),
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert_eq!(
            ui.get_status_text(),
            "Imported 2 audio files in project revision 7"
        );
        assert!(ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::ImportingAudio);
        apply_event(
            ui,
            &ui_routes,
            &project_state,
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::ImportAudio,
                reason: IdeWorkerCommandFailure::AudioImport(Box::new(
                    IdeAudioImportBatchFailure::for_test(
                        1,
                        2,
                        PathBuf::from("snare.flac"),
                        IdeAudioImportError::UnsupportedContainer,
                    ),
                )),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(ui.get_error_text().contains("imported 1 of 2"));
        assert_eq!(ui.get_project_history_warning(), "retained history warning");
    }

    fn assert_committed_audio_clip_source_slip_warning(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_clip_source_slip_visible(true);
        ui.set_clip_source_slip_id("clip-a".into());
        ui.set_clip_source_slip_name("Clip A".into());
        ui.set_clip_source_slip_frame_count("100".into());
        ui.set_clip_source_slip_original("0".into());
        ui.set_clip_source_slip_start("25".into());

        apply_audio_clip_edit_committed_warning(
            ui,
            &IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipSourceStart,
                warning: IdeAudioClipEditCommittedWarning::SourceSlip(Box::new(
                    IdeAudioClipSourceSlipEditError::InvalidClipIdentity,
                )),
            },
        );

        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_clip_source_slip_visible());
        assert!(ui.get_clip_source_slip_id().is_empty());
        assert!(ui.get_clip_source_slip_start().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_committed_audio_clip_source_trim_warning(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_clip_source_trim_visible(true);
        ui.set_clip_source_trim_id("clip-a".into());
        ui.set_clip_source_trim_name("Clip A".into());
        ui.set_clip_source_trim_original_start("0".into());
        ui.set_clip_source_trim_original_frame_count("100".into());
        ui.set_clip_source_trim_start("25".into());
        ui.set_clip_source_trim_frame_count("50".into());

        apply_audio_clip_edit_committed_warning(
            ui,
            &IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::TrimAudioClipSource,
                warning: IdeAudioClipEditCommittedWarning::SourceTrim(Box::new(
                    IdeAudioClipSourceTrimEditError::InvalidClipIdentity,
                )),
            },
        );

        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(!ui.get_clip_source_trim_visible());
        assert!(ui.get_clip_source_trim_id().is_empty());
        assert!(ui.get_clip_source_trim_start().is_empty());
        assert!(ui.get_clip_source_trim_frame_count().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_scene_authoring_forms(ui: &MainWindow) {
        let worker = Rc::new(RefCell::new(None));
        let project_state = Rc::new(ProjectUiState::default());
        bind_scene_callbacks(ui, &worker, &project_state);
        ui.set_app_mode(AppMode::Running);
        ui.global::<IdeActions>().invoke_begin_add_scene();
        assert!(ui.get_scene_create_visible());
        assert_eq!(ui.get_scene_create_name().as_str(), "Scene 1");
        assert_eq!(ui.get_scene_create_quantization_index(), 0);
        assert!(ui.get_scene_create_quantum_ticks().is_empty());

        ui.set_scene_create_name("Verse".into());
        ui.set_scene_create_quantization_index(1);
        ui.set_scene_create_quantum_ticks("960".into());
        apply_scene_creation_event(
            ui,
            &IdeWorkerEvent::SceneAdded {
                revision: 2,
                scene_id: "scene-verse".to_owned(),
                name: "Verse".to_owned(),
            },
        );
        assert!(!ui.get_scene_create_visible());
        assert!(ui.get_scene_create_name().is_empty());
        assert_eq!(ui.get_scene_create_quantization_index(), 0);
        assert!(ui.get_scene_create_quantum_ticks().is_empty());

        ui.set_scene_create_visible(true);
        ui.set_scene_create_name("Bridge".into());
        ui.set_scene_create_quantization_index(1);
        ui.set_scene_create_quantum_ticks("1920".into());
        apply_scene_creation_event(
            ui,
            &IdeWorkerEvent::SceneCreationCommittedWithWarning {
                name: "Bridge".to_owned(),
                warning: Box::new(IdeSceneCreationError::InvalidName),
            },
        );
        assert!(!ui.get_scene_create_visible());
        assert!(ui.get_scene_create_name().is_empty());
        assert_eq!(ui.get_scene_create_quantization_index(), 0);
        assert!(ui.get_scene_create_quantum_ticks().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());

        ui.set_scene_slots_visible(true);
        apply_scene_slot_event(
            ui,
            &IdeWorkerEvent::SceneSlotChanged {
                revision: 3,
                changed: true,
                edit: IdeSceneSlotEdit::Stop,
            },
        );
        assert!(ui.get_scene_slots_visible());
        assert!(ui.get_status_text().contains("project revision 3"));
        assert!(ui.get_error_text().is_empty());

        ui.set_scene_slots_scene_id("scene-verse".into());
        ui.set_scene_slots_scene_name("Verse".into());
        ui.set_selected_scene_slot_index(0);
        ui.set_selected_scene_slot_track_id("track-a".into());
        ui.set_selected_scene_slot_track_name("Track A".into());
        apply_scene_slot_event(
            ui,
            &IdeWorkerEvent::SceneSlotEditCommittedWithWarning {
                edit: IdeSceneSlotEdit::Stop,
                warning: Box::new(IdeSceneSlotEditError::InvalidSceneIdentity),
            },
        );
        assert!(!ui.get_scene_slots_visible());
        assert!(ui.get_scene_slots_scene_id().is_empty());
        assert!(ui.get_scene_slots_scene_name().is_empty());
        assert_eq!(ui.get_selected_scene_slot_index(), -1);
        assert!(ui.get_selected_scene_slot_track_id().is_empty());
        assert!(ui.get_selected_scene_slot_track_name().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_scene_removal_events_reset_form(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_scene_remove_visible(true);
        ui.set_scene_remove_id("scene-a".into());
        ui.set_scene_remove_name("Intro".into());
        apply_scene_removal_event(ui, &IdeWorkerEvent::SceneRemoving);
        assert_eq!(ui.get_app_mode(), AppMode::RemovingScene);
        assert!(ui.get_scene_remove_visible());
        assert!(ui.get_status_text().contains("removing the scene"));

        ui.set_app_mode(AppMode::Running);
        apply_scene_removal_event(
            ui,
            &IdeWorkerEvent::SceneRemoved {
                revision: 4,
                scene_id: "scene-a".to_owned(),
            },
        );
        assert!(!ui.get_scene_remove_visible());
        assert!(ui.get_scene_remove_id().is_empty());
        assert!(ui.get_scene_remove_name().is_empty());
        assert!(ui.get_status_text().contains("project revision 4"));
        assert!(ui.get_error_text().is_empty());

        ui.set_scene_remove_visible(true);
        ui.set_scene_remove_id("scene-a".into());
        ui.set_scene_remove_name("Intro".into());
        apply_scene_removal_event(
            ui,
            &IdeWorkerEvent::SceneRemovalCommittedWithWarning {
                warning: Box::new(IdeSceneRemovalError::InvalidSceneIdentity),
            },
        );
        assert!(!ui.get_scene_remove_visible());
        assert!(ui.get_scene_remove_id().is_empty());
        assert!(ui.get_scene_remove_name().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_scene_rename_success_resets_form(ui: &MainWindow) {
        ui.set_scene_rename_visible(true);
        ui.set_scene_rename_id("scene-a".into());
        ui.set_scene_rename_original("Intro".into());
        ui.set_scene_rename_name("Verse A".into());
        apply_scene_rename_event(
            ui,
            &IdeWorkerEvent::SceneRenamed {
                revision: 4,
                changed: true,
                name: "Verse A".to_owned(),
            },
        );
        assert!(!ui.get_scene_rename_visible());
        assert!(ui.get_scene_rename_id().is_empty());
        assert!(ui.get_scene_rename_name().is_empty());
        assert!(ui.get_status_text().contains("project revision 4"));
        assert!(ui.get_error_text().is_empty());

        ui.set_scene_rename_visible(true);
        ui.set_scene_rename_id("scene-a".into());
        ui.set_scene_rename_original("Intro".into());
        ui.set_scene_rename_name("Verse A".into());
        apply_scene_rename_event(
            ui,
            &IdeWorkerEvent::SceneRenameCommittedWithWarning {
                name: "Verse A".to_owned(),
                warning: Box::new(IdeSceneRenameError::InvalidName),
            },
        );
        assert!(!ui.get_scene_rename_visible());
        assert!(ui.get_scene_rename_id().is_empty());
        assert!(ui.get_scene_rename_name().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_scene_duplication_events_reset_form(ui: &MainWindow) {
        ui.set_app_mode(AppMode::Running);
        ui.set_scene_duplicate_visible(true);
        ui.set_scene_duplicate_id("scene-a".into());
        ui.set_scene_duplicate_source_name("Intro".into());
        ui.set_scene_duplicate_name("Intro copy".into());
        apply_scene_duplication_event(ui, &IdeWorkerEvent::SceneDuplicating);
        assert_eq!(ui.get_app_mode(), AppMode::DuplicatingScene);
        assert!(ui.get_scene_duplicate_visible());
        assert_eq!(ui.get_scene_duplicate_id().as_str(), "scene-a");
        assert!(ui.get_status_text().contains("duplicating the scene"));
        assert!(ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Running);
        apply_scene_duplication_event(
            ui,
            &IdeWorkerEvent::SceneDuplicated {
                revision: 5,
                scene_id: "scene-copy".to_owned(),
                name: "Intro copy".to_owned(),
            },
        );
        assert!(!ui.get_scene_duplicate_visible());
        assert!(ui.get_scene_duplicate_id().is_empty());
        assert!(ui.get_scene_duplicate_source_name().is_empty());
        assert!(ui.get_scene_duplicate_name().is_empty());
        assert!(ui.get_status_text().contains("project revision 5"));
        assert!(
            ui.get_status_text()
                .contains("arrangement launches were unchanged")
        );
        assert!(ui.get_error_text().is_empty());

        ui.set_scene_duplicate_visible(true);
        ui.set_scene_duplicate_id("scene-a".into());
        ui.set_scene_duplicate_source_name("Intro".into());
        ui.set_scene_duplicate_name("Intro copy".into());
        apply_scene_duplication_event(
            ui,
            &IdeWorkerEvent::SceneDuplicationCommittedWithWarning {
                name: "Intro copy".to_owned(),
                warning: Box::new(IdeSceneDuplicationError::InvalidName),
            },
        );
        assert!(!ui.get_scene_duplicate_visible());
        assert!(ui.get_scene_duplicate_id().is_empty());
        assert!(ui.get_scene_duplicate_source_name().is_empty());
        assert!(ui.get_scene_duplicate_name().is_empty());
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    fn assert_scene_activation_events_preserve_state(ui: &MainWindow) {
        apply_scene_activation_event(
            ui,
            &IdeWorkerEvent::SceneActivationChanging { active: false },
        );
        assert_eq!(ui.get_app_mode(), AppMode::ChangingSceneActivation);
        assert!(ui.get_status_text().contains("disabling"));
        assert!(ui.get_error_text().is_empty());

        ui.set_app_mode(AppMode::Running);
        apply_scene_activation_event(
            ui,
            &IdeWorkerEvent::SceneActivationChanged {
                revision: 5,
                changed: true,
                active: false,
            },
        );
        assert!(ui.get_status_text().contains("disabled"));
        assert!(ui.get_status_text().contains("project revision 5"));
        assert!(ui.get_error_text().is_empty());

        apply_scene_activation_event(
            ui,
            &IdeWorkerEvent::SceneActivationCommittedWithWarning {
                active: true,
                warning: Box::new(IdeSceneRenameError::InvalidSceneIdentity),
            },
        );
        assert_eq!(ui.get_app_mode(), AppMode::Running);
        assert!(ui.get_status_text().contains("committed and reopened"));
        assert!(!ui.get_error_text().is_empty());
    }

    #[test]
    fn audio_clip_activation_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::ChangingClipActivation,
                IdeWorkerCommandKind::SetAudioClipActive,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn take_routes_keep_recording_creation_separate_from_asset_preview() {
        let routes = Rc::new(RefCell::new(vec![TakeRoute {
            recording_id: "recording-a".to_owned(),
            asset_id: "asset-a".to_owned(),
        }]));

        assert_eq!(
            take_route(&routes, 0),
            Some(TakeRoute {
                recording_id: "recording-a".to_owned(),
                asset_id: "asset-a".to_owned(),
            })
        );
        assert_eq!(take_route(&routes, -1), None);
        assert_eq!(take_route(&routes, 1), None);
        assert!(matches!(
            take_clip_creation_command(&routes, 0, "Lead take"),
            Some(IdeWorkerCommand::CreateAudioClipFromTake {
                recording_id,
                name,
            }) if recording_id == "recording-a" && name == "Lead take"
        ));
        assert!(matches!(
            take_clip_placement_command(&routes, 0, "Lead take"),
            Some(IdeWorkerCommand::CreateAndPlaceAudioClipFromTake {
                recording_id,
                name,
            }) if recording_id == "recording-a" && name == "Lead take"
        ));
        assert!(matches!(
            take_discard_command(&routes, 0),
            Some(IdeWorkerCommand::DiscardRecordingTake { recording_id })
                if recording_id == "recording-a"
        ));
        assert!(take_discard_command(&routes, -1).is_none());
    }

    #[test]
    fn recorded_take_rows_preserve_discard_protection_and_reset_confirmation_on_reopen() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = MainWindow::new().unwrap();
        let routes: TakeRoutes = Rc::new(RefCell::new(Vec::new()));
        let observation = IdeRunningObservation::for_ui_test(PlaybackRecoverySnapshot::for_test(
            PlaybackRecoveryTransportState::Stopped,
            0,
        ))
        .with_recording_takes_for_ui_test(vec![
            (
                "recording-discardable",
                "asset-discardable",
                "track-a",
                "Audio A",
                true,
            ),
            (
                "recording-protected",
                "asset-protected",
                "track-b",
                "Audio B",
                false,
            ),
        ]);

        set_recording_takes(&ui, &routes, &observation);
        let rows = ui.get_recording_takes();
        assert_eq!(rows.row_count(), 2);
        assert_eq!(rows.row_data(0).unwrap().label, "Audio A");
        assert!(rows.row_data(0).unwrap().discardable);
        assert!(!rows.row_data(1).unwrap().discardable);
        assert!(matches!(
            take_discard_command(&routes, 0),
            Some(IdeWorkerCommand::DiscardRecordingTake { recording_id })
                if recording_id == "recording-discardable"
        ));
        assert_eq!(
            take_route(&routes, 1).unwrap().recording_id,
            "recording-protected"
        );

        ui.set_take_discard_confirming_index(0);
        set_recording_takes(&ui, &routes, &observation);
        assert_eq!(ui.get_take_discard_confirming_index(), -1);
    }

    #[test]
    fn recording_clip_creation_failure_keeps_the_form_recoverable() {
        assert_eq!(
            mode_after_failure(
                AppMode::CreatingClip,
                IdeWorkerCommandKind::CreateAudioClipFromTake,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::CreatingClip,
                IdeWorkerCommandKind::CreateAndPlaceAudioClipFromTake,
            ),
            AppMode::Running
        );
    }

    #[test]
    fn recording_take_discard_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::DiscardRecordingTake),
            AppMode::Running
        );
    }

    #[test]
    fn recording_take_rename_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(AppMode::Saving, IdeWorkerCommandKind::RenameRecordingTake),
            AppMode::Running
        );
    }

    #[test]
    fn submission_failures_restore_only_rejected_operation_state() {
        assert_eq!(
            mode_after_submission_failure(
                AppMode::Opening,
                IdeWorkerCommandKind::OpenProject,
                UiSubmissionFailure::Full,
            ),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::Creating,
                IdeWorkerCommandKind::CreateProject,
                UiSubmissionFailure::Full,
            ),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::Running,
                IdeWorkerCommandKind::Transport,
                UiSubmissionFailure::Full,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::Opening,
                IdeWorkerCommandKind::OpenProject,
                UiSubmissionFailure::Disconnected,
            ),
            AppMode::Unavailable
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::Creating,
                IdeWorkerCommandKind::CreateProject,
                UiSubmissionFailure::Disconnected,
            ),
            AppMode::Unavailable
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::StartingOutputTest,
                IdeWorkerCommandKind::StartOutputTest,
                UiSubmissionFailure::Full,
            ),
            AppMode::Closed
        );
        assert_eq!(
            mode_after_submission_failure(
                AppMode::StoppingOutputTest,
                IdeWorkerCommandKind::StopOutputTest,
                UiSubmissionFailure::Full,
            ),
            AppMode::OutputTesting
        );
    }

    #[test]
    fn closed_worker_event_does_not_complete_an_active_project_dialog() {
        assert_eq!(
            mode_after_closed_event(AppMode::Selecting),
            AppMode::Selecting
        );
        assert_eq!(mode_after_closed_event(AppMode::Opening), AppMode::Closed);
        assert_eq!(mode_after_closed_event(AppMode::Running), AppMode::Closed);
    }

    #[test]
    fn unavailable_routed_output_is_not_presented_as_system_default() {
        let inventory = IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap();
        let presentation = output_presentation(
            &inventory,
            &IdeOutputSelection::Routed("route-missing".to_owned()),
        );
        assert_eq!(presentation.selected, 2);
        assert_eq!(presentation.routes[0], None);
        assert_eq!(
            presentation.routes[presentation.selected].as_deref(),
            Some("route-missing")
        );
        assert_eq!(
            presentation.names[presentation.selected],
            SharedString::from("Unavailable selected output")
        );
    }

    #[test]
    fn input_cue_presentation_keeps_off_and_stale_routes_explicit() {
        let inventory = IdeAudioInputInventory::try_new(&[("input-a", "Input A")]).unwrap();
        let disabled = input_presentation(Some(&inventory), &IdeInputSelection::Disabled);
        assert_eq!(disabled.selected, 0);
        assert_eq!(disabled.routes[0], InputRoute::Disabled);
        assert_eq!(disabled.routes[1], InputRoute::SystemDefault);
        assert_eq!(disabled.routes[2], InputRoute::Routed("input-a".to_owned()));

        let unavailable = input_presentation(
            Some(&inventory),
            &IdeInputSelection::Routed("input-missing".to_owned()),
        );
        assert_eq!(unavailable.selected, 3);
        assert_eq!(
            unavailable.routes[unavailable.selected],
            InputRoute::Routed("input-missing".to_owned())
        );
        assert_eq!(
            unavailable.names[unavailable.selected],
            SharedString::from("Unavailable selected input")
        );

        let undiscovered = input_presentation(None, &IdeInputSelection::Disabled);
        assert_eq!(undiscovered.routes, [InputRoute::Disabled]);
        assert_eq!(undiscovered.selected, 0);
    }

    #[test]
    fn input_cue_gain_conversion_rejects_nonfinite_and_above_unity() {
        assert_eq!(monitor_percent_to_millionths(0.0), Ok(0));
        assert_eq!(monitor_percent_to_millionths(25.5), Ok(255_000));
        assert_eq!(monitor_percent_to_millionths(100.0), Ok(1_000_000));
        assert!(monitor_percent_to_millionths(100.1).is_err());
        assert!(monitor_percent_to_millionths(f32::NAN).is_err());
    }

    #[test]
    fn take_audition_gain_conversion_rejects_nonfinite_and_above_unity() {
        assert_eq!(preview_percent_to_millionths(0.0), Ok(0));
        assert_eq!(preview_percent_to_millionths(25.5), Ok(255_000));
        assert_eq!(preview_percent_to_millionths(100.0), Ok(1_000_000));
        assert!(preview_percent_to_millionths(100.1).is_err());
        assert!(preview_percent_to_millionths(f32::NAN).is_err());
    }

    #[test]
    fn audition_preparation_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::PreparingPreview,
                IdeWorkerCommandKind::StartAudioPreview,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(
                AppMode::PreparingPreview,
                IdeWorkerCommandKind::StartAudioClipPreview,
            ),
            AppMode::Running
        );
        assert_eq!(
            mode_after_failure(AppMode::Previewing, IdeWorkerCommandKind::StopAudioPreview,),
            AppMode::Previewing
        );
    }

    #[test]
    fn compound_exit_preserves_run_and_shutdown_failures() {
        let result = finish_exit(
            Err(PlatformError::Unsupported),
            Err(DesktopUiShutdownError::TimedOut),
        );
        let Err(DesktopUiExitError::RunAndShutdown { run, shutdown }) = result else {
            panic!("compound failures must remain distinct");
        };
        assert!(matches!(*run, PlatformError::Unsupported));
        assert!(matches!(shutdown, DesktopUiShutdownError::TimedOut));
    }
}
