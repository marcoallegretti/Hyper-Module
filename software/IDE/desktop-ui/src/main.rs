#![deny(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use hyper_desktop_ide::{
    AutomationChannelParameter, AutomationInterpolation, AutomationMainParameter, AutomationPoint,
    AutomationSendParameter, AutomationTarget, AutomationValue, DesktopAudioPreviewState,
    DesktopIdeWorker, IdeApplicationConfig, IdeAudioClip,
    IdeAudioInputInventory, IdeAudioOutputInventory, IdeAudioPreviewControlCommand,
    IdeInputSelection, IdeLoudnessCommand, IdeLoudnessDisplayUnit, IdeLoudnessMeasurementState,
    IdeLoudnessPresentationConfig, IdeLoudnessScale, IdeLoudnessScaleRegion,
    IdeMaximumTruePeakDisplay, IdeMixerChannel, IdeMixerChannelKind, IdeMixerCommand,
    IdeMixerSendTap, IdeMonitoringCommand, IdeNativeCloseFailure, IdeOutputSelection,
    ControlScope, ControlSource, ControlTarget, ControlTransform, Edge,
    IdeControlBindingRequest, IdeControlChannelParameter, IdeControlMainParameter,
    IdeControlMappingEdit, IdeLoopRangeFrames, IdeResourceRestoreError, SceneAction,
    TransportAction, TriggerBehavior,
    IdeAutomationTakeRequest, IdeAutomationTarget, IdeOutputTestState, IdePlaybackArrangement,
    OfflineExportReport,
    IdePlaybackAutomation, IdePlaybackAutomationLane, IdePlaybackExtent, IdeRecordingAbortOutcome,
    IdeRecordingCaptureKind, IdeRecordingImportOutcome, IdeRecordingTake, IdeRunningObservation,
    IdeScaledLoudnessValue, IdeScene, IdeSceneAvailability, IdeSceneLaunchQuantization,
    IdeSceneLaunchState, IdeSceneSlotEdit, IdeSceneTrackAction, IdeWorkerCommand,
    IdeWorkerCommandKind, IdeWorkerEvent, IdeWorkerPendingShutdownWait, IdeWorkerSendError,
    IdeWorkerShutdownOutcome, IdeWorkerShutdownReport, LocateState, MAX_IDE_SCENES,
    PlaybackRecoveryTransportState, TransportCommand, TransportLoopRange,
    desktop_ide_application_config,
    ide_audio_clip_gain_is_supported,
};
#[cfg(test)]
use hyper_desktop_ide::{
    IdeArrangementPlacementEditError, IdeAudioClipEditCommittedWarning, IdeAudioClipFadeEditError,
    IdeAudioClipLoopEditError, IdeAudioClipReverseEditError, IdeAudioClipSourceSlipEditError,
    IdeSceneCreationError, IdeSceneDuplicationError, IdeSceneRenameError, IdeSceneSlotEditError,
};
use slint::platform::PlatformError;
use slint::{
    ComponentHandle, Model, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel,
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
const MAX_SAFE_TIMELINE_TICK: u64 = 9_007_199_254_740_991;

type WorkerCell = Rc<RefCell<Option<DesktopIdeWorker>>>;
type OutputRoutes = Rc<RefCell<Vec<Option<String>>>>;
type InputRoutes = Rc<RefCell<Vec<InputRoute>>>;
type RecordingRoutes = Rc<RefCell<Vec<usize>>>;
type TakeRoutes = Rc<RefCell<Vec<TakeRoute>>>;
type ProjectUiStateCell = Rc<ProjectUiState>;

#[derive(Default)]
struct ProjectUiState {
    playback_end_frame: RefCell<Option<usize>>,
    playhead_frame: Cell<usize>,
    loop_marks: Cell<LoopMarks>,
    loop_engaged: Cell<bool>,
    sample_rate_hz: Cell<u32>,
    arrangement: RefCell<Option<ArrangementBinding>>,
    audio_clips: RefCell<Vec<IdeAudioClip>>,
    scenes: RefCell<Vec<IdeScene>>,
    scene_launch_evidence: RefCell<SceneLaunchEvidence>,
    automation_channels: RefCell<Vec<MixerChannelFacts>>,
    control_targets: RefCell<Vec<ControlTargetChoice>>,
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
            (Some(start), None) => format!("LOOP IN {}", format_transport_clock_frames(start, sample_rate_hz)),
            (None, Some(end)) => format!("LOOP OUT {}", format_transport_clock_frames(end, sample_rate_hz)),
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

fn main() {
    if let Err(error) = run() {
        eprintln!("desktop IDE failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let ui = MainWindow::new()?;
    configure_ui_contracts(&ui);
    let worker = DesktopIdeWorker::start(
        application_config()?,
        WORKER_COMMAND_CAPACITY,
        WORKER_EVENT_CAPACITY,
    )?;
    let worker = Rc::new(RefCell::new(Some(worker)));
    let output_routes = Rc::new(RefCell::new(Vec::new()));
    let input_routes = Rc::new(RefCell::new(Vec::new()));
    let recording_routes = Rc::new(RefCell::new(Vec::new()));
    let take_routes = Rc::new(RefCell::new(Vec::new()));
    let project_state = Rc::new(ProjectUiState::default());

    bind_callbacks(
        &ui,
        &worker,
        &output_routes,
        &input_routes,
        &recording_routes,
        &take_routes,
        &project_state,
    );
    let timer = start_event_pump(
        &ui,
        &worker,
        &output_routes,
        &input_routes,
        &recording_routes,
        &take_routes,
        &project_state,
    );
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
    output_routes: &OutputRoutes,
    input_routes: &InputRoutes,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
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
    let routes_for_selection = Rc::clone(output_routes);
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

    bind_input_selection_callbacks(ui, worker, input_routes);

    bind_project_callbacks(ui, worker);

    bind_playback_callbacks(ui, worker, project_state);
    bind_monitoring_callbacks(ui, worker);
    bind_recording_callbacks(ui, worker, recording_routes);
    bind_audio_preview_callbacks(ui, worker, take_routes);
}

fn bind_project_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    bind_project_creation_callback(ui, worker);
    bind_project_open_callback(ui, worker);
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
    bind_scene_callbacks(ui, worker, project_state);

    let weak = ui.as_weak();
    let worker_for_save = Rc::clone(worker);
    actions.on_save_project(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || !ui.get_mixer_main_state_available() {
            ui.set_error_text("Committed mixer state is not available to save".into());
            return;
        }
        submit_to_ui(&ui, &worker_for_save, IdeWorkerCommand::SaveProject);
    });

    bind_audio_track_callback(ui, worker);

    let weak = ui.as_weak();
    let worker_for_close = Rc::clone(worker);
    actions.on_close_project(move || {
        submit(&weak, &worker_for_close, IdeWorkerCommand::CloseProject);
    });

    bind_mixer_callbacks(ui, worker);
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
}

fn bind_audio_track_callback(ui: &MainWindow, worker: &WorkerCell) {
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

    bind_audio_track_removal_callback(ui, worker);
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
    bind_automation_take_callback(ui, worker, project_state);
    bind_automation_take_activation_callback(ui, worker);
    bind_control_mapping_callbacks(ui, worker, project_state);
    bind_project_export_callback(ui, worker);
    bind_undo_callbacks(ui, worker);
    bind_loop_callbacks(ui, worker, project_state);
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
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_duplication = Rc::clone(worker);
    let project_state_for_duplication = Rc::clone(project_state);
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
        let state = project_state_for_duplication.arrangement.borrow();
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
            ui.set_error_text(
                "The copied placement would overlap its source at the same start tick".into(),
            );
            return;
        }
        let owned_placement_id = placement.id().to_owned();
        drop(state);
        submit_to_ui(
            &ui,
            &worker_for_duplication,
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

    bind_arrangement_placement_addition_callback(ui, worker, project_state);
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
    output_routes: &OutputRoutes,
    input_routes: &InputRoutes,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    project_state: &ProjectUiStateCell,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let output_routes = Rc::clone(output_routes);
    let input_routes = Rc::clone(input_routes);
    let recording_routes = Rc::clone(recording_routes);
    let take_routes = Rc::clone(take_routes);
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
                    apply_event(
                        &ui,
                        &output_routes,
                        &input_routes,
                        &recording_routes,
                        &take_routes,
                        &project_state,
                        event,
                    );
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
            AppMode::Running | AppMode::Previewing | AppMode::Recording | AppMode::OutputTesting
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
    let command_kind = command.kind();
    let worker_ref = worker.borrow();
    let Some(worker) = worker_ref.as_ref() else {
        ui.set_app_mode(mode_after_submission_failure(
            ui.get_app_mode(),
            command_kind,
            UiSubmissionFailure::Disconnected,
        ));
        ui.set_error_text("IDE worker is no longer available".into());
        return;
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
        }
    }
}

fn mode_after_successful_submission(current: AppMode, command: IdeWorkerCommandKind) -> AppMode {
    match command {
        IdeWorkerCommandKind::StartOutputTest => AppMode::StartingOutputTest,
        IdeWorkerCommandKind::StopOutputTest => AppMode::StoppingOutputTest,
        IdeWorkerCommandKind::CreateProject => AppMode::Creating,
        IdeWorkerCommandKind::SaveProject
        | IdeWorkerCommandKind::AddScene
        | IdeWorkerCommandKind::SetSceneSlot => AppMode::Saving,
        IdeWorkerCommandKind::AddAudioTrack => AppMode::AddingTrack,
        IdeWorkerCommandKind::RemoveAudioTrack => AppMode::RemovingTrack,
        IdeWorkerCommandKind::RenameAudioTrack => AppMode::RenamingTrack,
        IdeWorkerCommandKind::SetAudioTrackActive => AppMode::ChangingTrackActivation,
        IdeWorkerCommandKind::SetArrangementPlacementActive => {
            AppMode::ChangingArrangementActivation
        }
        IdeWorkerCommandKind::MoveArrangementPlacement => AppMode::MovingArrangementPlacement,
        IdeWorkerCommandKind::SetArrangementPlacementDuration
        | IdeWorkerCommandKind::TrimArrangementPlacementFront => {
            AppMode::TrimmingArrangementPlacement
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement => AppMode::RemovingArrangementPlacement,
        IdeWorkerCommandKind::AddArrangementPlacement => AppMode::AddingArrangementPlacement,
        IdeWorkerCommandKind::AddArrangementSceneLaunch => AppMode::AddingArrangementSceneLaunch,
        IdeWorkerCommandKind::AddAutomationTake => AppMode::AddingAutomationTake,
        IdeWorkerCommandKind::SetAutomationTakeActive => {
            AppMode::ChangingAutomationTakeActivation
        }
        IdeWorkerCommandKind::EditControlMapping => AppMode::EditingControlMapping,
        IdeWorkerCommandKind::ExportProject => AppMode::ExportingProject,
        IdeWorkerCommandKind::Undo | IdeWorkerCommandKind::Redo => AppMode::RestoringEdit,
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => AppMode::MovingArrangementSceneLaunch,
        IdeWorkerCommandKind::SetArrangementSceneLaunchScene => {
            AppMode::ChangingArrangementSceneLaunchScene
        }
        IdeWorkerCommandKind::RemoveArrangementSceneLaunch => {
            AppMode::RemovingArrangementSceneLaunch
        }
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            AppMode::DuplicatingArrangementPlacement
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => AppMode::SplittingArrangementPlacement,
        IdeWorkerCommandKind::RenameAudioClip => AppMode::RenamingClip,
        IdeWorkerCommandKind::DuplicateAudioClip => AppMode::DuplicatingClip,
        IdeWorkerCommandKind::SetAudioClipActive => AppMode::ChangingClipActivation,
        IdeWorkerCommandKind::SetAudioClipGain => AppMode::ChangingClipGain,
        IdeWorkerCommandKind::SetAudioClipFades => AppMode::ChangingClipFades,
        IdeWorkerCommandKind::SetAudioClipLoop => AppMode::ChangingClipLoop,
        IdeWorkerCommandKind::SetAudioClipReverse => AppMode::ChangingClipReverse,
        IdeWorkerCommandKind::SetAudioClipSourceStart => AppMode::ChangingClipSource,
        IdeWorkerCommandKind::RenameScene => AppMode::RenamingScene,
        IdeWorkerCommandKind::DuplicateScene => AppMode::DuplicatingScene,
        IdeWorkerCommandKind::SetSceneActive => AppMode::ChangingSceneActivation,
        IdeWorkerCommandKind::CreateAudioClipFromTake => AppMode::CreatingClip,
        IdeWorkerCommandKind::StartRecording => AppMode::PreparingRecording,
        IdeWorkerCommandKind::StopRecording | IdeWorkerCommandKind::AbortRecording => {
            AppMode::FinalizingRecording
        }
        IdeWorkerCommandKind::StartAudioPreview => AppMode::PreparingPreview,
        _ => current,
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
        IdeWorkerCommandKind::Loudness => "Loudness measurement command queued…",
        IdeWorkerCommandKind::Mixer => "Mixer command queued…",
        IdeWorkerCommandKind::Monitoring => "Applying input cue control…",
        IdeWorkerCommandKind::SaveProject => "Saving mixer state and reopening project…",
        IdeWorkerCommandKind::AddArrangementSceneLaunch => {
            "Preserving mixer state, adding the scene marker, and reopening project..."
        }
        IdeWorkerCommandKind::MoveArrangementSceneLaunch => {
            "Preserving mixer state, moving the scene marker, and reopening project..."
        }
        IdeWorkerCommandKind::SetArrangementLoopRange => {
            "Preserving mixer state, saving the loop range, and reopening project..."
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
        IdeWorkerCommandKind::RemoveAudioTrack => {
            "Preserving mixer state, removing the empty audio track, and reopening project…"
        }
        IdeWorkerCommandKind::RenameAudioTrack => {
            "Preserving mixer state, renaming the audio track, and reopening project…"
        }
        IdeWorkerCommandKind::SetAudioTrackActive => {
            "Preserving mixer state, updating the audio track, and reopening project…"
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
        IdeWorkerCommandKind::AddAutomationTake => {
            "Preserving mixer state, adding the automation take, and reopening project..."
        }
        IdeWorkerCommandKind::SetAutomationTakeActive => {
            "Preserving mixer state, editing the automation take, and reopening project..."
        }
        IdeWorkerCommandKind::EditControlMapping => {
            "Preserving mixer state, editing control mappings, and reopening project..."
        }
        IdeWorkerCommandKind::ExportProject => {
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
        IdeWorkerCommandKind::RenameScene => "Renaming the scene and reopening project…",
        IdeWorkerCommandKind::SetSceneActive => "Updating scene activation and reopening project…",
        IdeWorkerCommandKind::CreateAudioClipFromTake => {
            "Preserving mixer state, creating the reusable audio clip, and reopening project…"
        }
        IdeWorkerCommandKind::StartRecording => {
            "Stopping playback and preparing an exclusive monitored capture…"
        }
        IdeWorkerCommandKind::StopRecording => "Verifying and importing the recorded take…",
        IdeWorkerCommandKind::AbortRecording => "Aborting capture and retaining recovery data…",
        IdeWorkerCommandKind::StartAudioPreview => {
            "Stopping transport and preparing dry take audition…"
        }
        IdeWorkerCommandKind::StopAudioPreview => "Stopping take audition…",
        IdeWorkerCommandKind::AudioPreviewControl => "Applying take audition control…",
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
    output_routes: &OutputRoutes,
    input_routes: &InputRoutes,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    project_state: &ProjectUiStateCell,
    event: IdeWorkerEvent,
) {
    match event {
        IdeWorkerEvent::Closed {
            snapshot,
            native_failure,
            input_failure,
        } => {
            apply_output_inventory(ui, output_routes, snapshot.outputs(), snapshot.selection());
            apply_input_inventory(
                ui,
                input_routes,
                snapshot.inputs(),
                snapshot.input_selection(),
            );
            reset_project(ui, recording_routes, take_routes, project_state);
            ui.set_output_test_confirming(false);
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
            recording_routes,
            take_routes,
            project_state,
            &observation,
        ),
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
                    TransportCommand::SetLoopRange(Some(_)) => {
                        "Loop range queued for the audio runtime"
                    }
                    TransportCommand::SetLoopRange(None) => {
                        "Loop range cleared for the audio runtime"
                    }
                    TransportCommand::SetLoopEnabled(true) => {
                        "Loop queued for the audio runtime"
                    }
                    TransportCommand::SetLoopEnabled(false) => {
                        "Loop release queued for the audio runtime"
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
        IdeWorkerEvent::MonitoringApplied(_) => {
            ui.set_status_text("Input cue control applied".into());
            ui.set_error_text(SharedString::default());
        }
        event @ IdeWorkerEvent::ProjectSaved { .. } => apply_project_saved_event(ui, &event),
        event @ (IdeWorkerEvent::AudioTrackAdded { .. } | IdeWorkerEvent::AudioTrackAdding) => {
            apply_audio_track_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackRemoved { .. } | IdeWorkerEvent::AudioTrackRemoving) => {
            apply_audio_track_removal_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackRenamed { .. } | IdeWorkerEvent::AudioTrackRenaming) => {
            apply_audio_track_rename_event(ui, &event);
        }
        event @ (IdeWorkerEvent::AudioTrackActivationChanged { .. }
        | IdeWorkerEvent::AudioTrackActivationChanging { .. }) => {
            apply_audio_track_activation_event(ui, &event);
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
        event @ (IdeWorkerEvent::SceneAdded { .. }
        | IdeWorkerEvent::SceneCreationCommittedWithWarning { .. }
        | IdeWorkerEvent::SceneAdding) => apply_scene_creation_event(ui, &event),
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
        event @ (IdeWorkerEvent::ControlMappingEdited { .. }
        | IdeWorkerEvent::ControlMappingEditCommittedWithWarning { .. }
        | IdeWorkerEvent::ControlMappingEditing { .. }) => {
            apply_control_mapping_event(ui, &event);
        }
        event @ (IdeWorkerEvent::ProjectExporting | IdeWorkerEvent::ProjectExported { .. }) => {
            apply_project_export_event(ui, &event);
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
        | IdeWorkerEvent::AudioClipCreatingFromTake) => {
            apply_recording_clip_creation_event(ui, &event);
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
        event @ (IdeWorkerEvent::AudioPreviewPreparing
        | IdeWorkerEvent::AudioPreviewStopRequested
        | IdeWorkerEvent::AudioPreviewControlApplied(_)) => apply_audio_preview_event(ui, &event),
        IdeWorkerEvent::Saving => {
            ui.set_app_mode(AppMode::Saving);
            ui.set_status_text("Waiting for native output before atomic save…".into());
        }
        IdeWorkerEvent::Closing => {
            ui.set_app_mode(AppMode::Closing);
            ui.set_status_text("Waiting for native output to close…".into());
        }
        IdeWorkerEvent::CommandFailed { command, reason } => {
            ui.set_app_mode(mode_after_failure(ui.get_app_mode(), command));
            ui.set_error_text(format!("{}: {reason}", command_status(command)).into());
        }
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

fn apply_audio_track_rename_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AudioTrackRenamed { revision, changed } => {
            ui.set_rename_track_visible(false);
            ui.set_rename_track_index(-1);
            ui.set_rename_track_original_name(SharedString::default());
            ui.set_rename_track_name(SharedString::default());
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioClipEditWarningForm {
    None,
    Rename,
    Gain,
    Fades,
    Loop,
    Reverse,
    SourceSlip,
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
        _ => unreachable!("recording clip creation dispatcher received another event"),
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

fn apply_running_event(
    ui: &MainWindow,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    project_state: &ProjectUiStateCell,
    observation: &IdeRunningObservation,
) {
    ui.set_new_project_visible(false);
    ui.set_project_name(observation.project_name().into());
    ui.set_project_id(observation.project_id().into());
    ui.set_sample_rate(format!("{} Hz", observation.sample_rate_hz()).into());
    ui.set_scene_count(observation.scene_count().to_string().into());
    ui.set_track_count(i32::try_from(observation.track_count()).unwrap_or(8));
    if !ui.get_new_track_visible() {
        ui.set_new_track_name(next_audio_track_name(ui.get_track_count()));
    }
    let scenes_changed = set_observed_scenes(ui, project_state, observation);
    apply_mixer_observation(ui, observation);
    set_recording_targets(ui, recording_routes, observation.mixer_channels());
    set_recording_takes(ui, take_routes, observation);
    set_playback_extent(ui, project_state, observation);
    seed_loop_marks(project_state, observation);
    refresh_loop_controls(ui, project_state, observation.sample_rate_hz());
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
    ui.set_frame_position(position_frame.to_string().into());
    project_state.playhead_frame.set(position_frame);
    ui.set_transport_clock(
        format_transport_clock(
            u64::try_from(position_frame).unwrap_or(u64::MAX),
            observation.sample_rate_hz(),
        )
        .into(),
    );
    ui.set_transport_state(
        if observation.preview().state().is_active() {
            "Auditioning take · transport stopped"
        } else {
            match observation.recovery().state() {
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
            ui.set_status_text("Dry take audition · input cue temporarily suppressed".into());
            ui.set_error_text(SharedString::default());
        }
        DesktopAudioPreviewState::Complete => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Take audition complete".into());
            ui.set_error_text(SharedString::default());
        }
        DesktopAudioPreviewState::Failed => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Take audition stopped".into());
            ui.set_error_text("The audio callback could not render this recorded take".into());
        }
        DesktopAudioPreviewState::Idle => {
            ui.set_app_mode(AppMode::Running);
            ui.set_status_text("Runtime evidence updated".into());
            ui.set_error_text(SharedString::default());
        }
    }
}

fn next_audio_track_name(track_count: i32) -> SharedString {
    format!("Audio {}", track_count.saturating_add(1)).into()
}

fn apply_output_meter_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    let (left_level, left_reading, right_level, right_reading) = observation
        .meter()
        .snapshot()
        .map_or((0.0, "—".to_owned(), 0.0, "—".to_owned()), |snapshot| {
            let meter = snapshot.meter();
            let (left_level, left_reading) = meter_presentation(meter.left().sample_peak());
            let (right_level, right_reading) = meter_presentation(meter.right().sample_peak());
            (left_level, left_reading, right_level, right_reading)
        });
    ui.set_left_meter(left_level);
    ui.set_right_meter(right_level);
    ui.set_left_reading(left_reading.into());
    ui.set_right_reading(right_reading.into());
    apply_loudness_observation(ui, observation);
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
        });
    }
    *take_routes.borrow_mut() = routes;
    ui.set_recording_takes(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_preview_active(preview.state().is_active());
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
        Some(ArrangementRow {
            index: i32::try_from(row).ok()?,
            id: placement.id().into(),
            track_name: track_name.into(),
            clip_name: clip_name.into(),
            start_tick: placement.start_tick().to_string().into(),
            clip_start_tick: placement.clip_start_tick().to_string().into(),
            duration_ticks: placement.duration_ticks().to_string().into(),
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
}

fn set_audio_clips(
    ui: &MainWindow,
    project_state: &ProjectUiStateCell,
    clips: &[IdeAudioClip],
) -> bool {
    if project_state.audio_clips.borrow().as_slice() == clips {
        return false;
    }
    let rows: Vec<ClipRow> = clips
        .iter()
        .map(|clip| ClipRow {
            id: clip.id().into(),
            name: clip.name().into(),
            track_name: clip.track_name().into(),
            active: clip.active(),
            gain_label: format!("{:+.2} dB", clip.gain_decibels()).into(),
            gain_edit: format_audio_clip_gain(clip.gain_decibels()).into(),
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

fn format_audio_clip_gain(gain_decibels: f64) -> String {
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
    ui.set_status_text("Capturing a monitored take; project playback remains paused".into());
    ui.set_error_text(SharedString::default());
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

fn apply_mixer_observation(ui: &MainWindow, observation: &IdeRunningObservation) {
    set_mixer(ui, observation.mixer_channels());
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

fn reset_project(
    ui: &MainWindow,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    project_state: &ProjectUiStateCell,
) {
    ui.set_project_name("No project open".into());
    ui.set_project_id("—".into());
    ui.set_sample_rate("—".into());
    ui.set_scene_count("—".into());
    ui.set_track_count(0);
    reset_scene_creation_form(ui);
    ui.set_new_track_visible(false);
    ui.set_new_track_name("Audio 2".into());
    ui.set_rename_track_visible(false);
    ui.set_remove_track_visible(false);
    ui.set_remove_track_id(SharedString::default());
    ui.set_remove_track_name(SharedString::default());
    ui.set_rename_track_index(-1);
    ui.set_rename_track_original_name(SharedString::default());
    ui.set_rename_track_name(SharedString::default());
    ui.set_scenes(ModelRc::default());
    ui.set_scene_names(ModelRc::default());
    reset_scene_rename_form(ui);
    reset_scene_duplication_form(ui);
    reset_scene_slots_form(ui);
    ui.set_mixer_visible(false);
    ui.set_mixer_sends_visible(false);
    ui.set_mixer_channels(ModelRc::default());
    ui.set_mixer_sends(ModelRc::default());
    ui.set_mixer_main_state_available(false);
    ui.set_mixer_main_gain_percent(0.0);
    ui.set_mixer_main_muted(false);
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
    ui.set_clip_gain_visible(false);
    ui.set_clip_gain_id(SharedString::default());
    ui.set_clip_gain_name(SharedString::default());
    ui.set_clip_gain_original(SharedString::default());
    ui.set_clip_gain_value(SharedString::default());
    reset_audio_clip_fade_form(ui);
    reset_audio_clip_loop_form(ui);
    reset_audio_clip_source_slip_form(ui);
    ui.set_audio_clips(ModelRc::default());
    ui.set_loudness_visible(false);
    ui.set_loudness_reset_confirming(false);
    ui.set_arrangement_span_clock("—".into());
    ui.set_preview_active(false);
    ui.set_preview_gain_percent(25.0);
    ui.set_preview_muted(false);
    ui.set_preview_state("Idle".into());
    ui.set_preview_progress("—".into());
    *project_state.playback_end_frame.borrow_mut() = None;
    *project_state.arrangement.borrow_mut() = None;
    project_state.audio_clips.borrow_mut().clear();
    project_state.scenes.borrow_mut().clear();
    project_state.automation_channels.borrow_mut().clear();
    project_state.control_targets.borrow_mut().clear();
    project_state.scene_launch_evidence.borrow_mut().clear();
    ui.set_transport_extent_bounded(false);
    ui.set_transport_seek_available(false);
    ui.set_transport_progress_percent(0.0);
    ui.set_transport_end_clock("—".into());
    ui.set_transport_state("Stopped".into());
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
    reset_loudness_observation(ui);
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
    reset_automation_creation_form(ui);
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
    ui.set_undo_label(SharedString::default());
    ui.set_redo_label(SharedString::default());
    ui.set_undo_available(false);
    ui.set_redo_available(false);
    ui.set_loop_summary(SharedString::default());
    ui.set_loop_marking_available(false);
    ui.set_loop_toggle_available(false);
    ui.set_loop_engaged(false);
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
        | IdeWorkerCommandKind::AddScene
        | IdeWorkerCommandKind::SetSceneSlot
            if current == AppMode::Saving =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::AddAudioTrack if current == AppMode::AddingTrack => AppMode::Running,
        IdeWorkerCommandKind::RemoveAudioTrack if current == AppMode::RemovingTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::RenameAudioTrack if current == AppMode::RenamingTrack => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetAudioTrackActive
            if current == AppMode::ChangingTrackActivation =>
        {
            AppMode::Running
        }
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
        IdeWorkerCommandKind::SetAutomationTakeActive
            if current == AppMode::ChangingAutomationTakeActivation =>
        {
            AppMode::Running
        }
        IdeWorkerCommandKind::EditControlMapping if current == AppMode::EditingControlMapping => {
            AppMode::Running
        }
        IdeWorkerCommandKind::ExportProject if current == AppMode::ExportingProject => {
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
        IdeWorkerCommandKind::RenameScene if current == AppMode::RenamingScene => AppMode::Running,
        IdeWorkerCommandKind::DuplicateScene if current == AppMode::DuplicatingScene => {
            AppMode::Running
        }
        IdeWorkerCommandKind::SetSceneActive if current == AppMode::ChangingSceneActivation => {
            AppMode::Running
        }
        IdeWorkerCommandKind::CreateAudioClipFromTake if current == AppMode::CreatingClip => {
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
        IdeWorkerCommandKind::StartAudioPreview if current == AppMode::PreparingPreview => {
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
    if mode != AppMode::Running || !scene.launchable {
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

fn mixer_rows(channels: &[IdeMixerChannel]) -> Vec<MixerChannelRow> {
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
                state_available: state.is_some(),
                gain_percent: state.map_or(0.0, |state| state.gain * 100.0),
                pan_percent: state.map_or(0.0, |state| state.pan * 100.0),
                muted: state.is_some_and(|state| state.muted),
                solo: state.is_some_and(|state| state.solo),
                solo_available: channel.kind() == IdeMixerChannelKind::Audio,
                renameable: channel.kind() == IdeMixerChannelKind::Audio,
                removable: channel.kind() == IdeMixerChannelKind::Audio,
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
            (
                "solo",
                AUTOMATION_SWITCH_DOMAIN,
                true,
                AutomationChannelParameter::Solo,
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
    if project_state.automation_channels.borrow().as_slice() == channels {
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
        choices.iter().map(|choice| choice.boolean).collect::<Vec<_>>(),
    ))));
    ui.set_automation_target_domain(ModelRc::from(Rc::new(VecModel::from(
        choices
            .iter()
            .map(|choice| SharedString::from(choice.domain.as_str()))
            .collect::<Vec<_>>(),
    ))));
    if ui.get_automation_create_target_index() >= i32::try_from(choices.len()).unwrap_or(0) {
        ui.set_automation_create_target_index(0);
    }
    *project_state.automation_channels.borrow_mut() = channels.to_vec();
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
        let target = if take.lanes().len() == 1 {
            self.labels.get(row)?.clone()
        } else {
            format!("{} lanes", take.lanes().len())
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

fn bind_automation_take_activation_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_activation = Rc::clone(worker);
    actions.on_set_automation_take_active(move |take_id, enabled| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running || take_id.is_empty() {
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

fn apply_automation_take_creation_event(ui: &MainWindow, event: &IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::AutomationTakeAdded {
            revision,
            take_id: _,
            name,
        } => {
            reset_automation_creation_form(ui);
            ui.set_status_text(
                format!("Automation take {name} added in project revision {revision}").into(),
            );
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::AutomationTakeCreationCommittedWithWarning { name, warning } => {
            reset_automation_creation_form(ui);
            ui.set_status_text(
                committed_warning_status(ui.get_app_mode(), &format!("Automation take {name}"))
                    .into(),
            );
            ui.set_error_text(warning.to_string().into());
        }
        IdeWorkerEvent::AutomationTakeAdding => {
            ui.set_app_mode(AppMode::AddingAutomationTake);
            ui.set_status_text(
                "Waiting for native output before adding the automation take…".into(),
            );
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
        ("Clock rising", ControlSource::ClockInput { edge: Edge::Rising }),
        (
            "Clock falling",
            ControlSource::ClockInput {
                edge: Edge::Falling,
            },
        ),
        ("Reset rising", ControlSource::ResetInput { edge: Edge::Rising }),
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
    let selected = usize::try_from(target_index).map_err(|_| "The selected control target is unavailable")?;
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
            submit_to_ui(
                &ui,
                &worker,
                IdeWorkerCommand::SetArrangementLoopRange {
                    range: marks.persisted(state.loop_engaged.get()),
                },
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
        submit_to_ui(
            &ui,
            &toggle_worker,
            IdeWorkerCommand::SetArrangementLoopRange {
                range: toggle_state.loop_marks.get().persisted(engaged),
            },
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
        submit_to_ui(
            &ui,
            &clear_worker,
            IdeWorkerCommand::SetArrangementLoopRange { range: None },
        );
    });
}

fn bind_undo_callbacks(ui: &MainWindow, worker: &WorkerCell) {
    let restore = |redo: bool| {
        let weak = ui.as_weak();
        let worker = Rc::clone(worker);
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
            submit_to_ui(
                &ui,
                &worker,
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

fn bind_project_export_callback(ui: &MainWindow, worker: &WorkerCell) {
    let actions = ui.global::<IdeActions>();
    let weak = ui.as_weak();
    let worker_for_export = Rc::clone(worker);
    actions.on_export_project(move || {
        let weak = weak.clone();
        let worker = Rc::clone(&worker_for_export);
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        ui.set_error_text(SharedString::default());
        ui.set_status_text("Waiting for export destination…".into());
        let spawn_result = slint::spawn_local(async move {
            let selection = rfd::AsyncFileDialog::new()
                .set_title("Export project audio")
                .set_file_name("export.wav")
                .add_filter("WAV", &["wav"])
                .save_file()
                .await;
            let Some(ui) = weak.upgrade() else {
                return;
            };
            let Some(selection) = selection else {
                ui.set_status_text("Export cancelled".into());
                return;
            };
            let path = selection.path().to_path_buf();
            ui.set_export_destination(path.display().to_string().into());
            ui.set_export_summary(SharedString::default());
            submit_to_ui(
                &ui,
                &worker,
                IdeWorkerCommand::ExportProject {
                    wav_path: path,
                    block_frames: EXPORT_BLOCK_FRAMES,
                },
            );
        });
        if spawn_result.is_err() {
            ui.set_status_text("Export destination selection is unavailable".into());
        }
    });
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

fn set_mixer(ui: &MainWindow, channels: &[IdeMixerChannel]) {
    let current = ui.get_mixer_channels();
    let unchanged = current.row_count() == channels.len()
        && channels.iter().enumerate().all(|(index, expected)| {
            current.row_data(index).is_some_and(|actual| {
                let state = expected.state();
                actual.index == i32::try_from(index).unwrap_or(-1)
                    && actual.track_id.as_str() == expected.id()
                    && actual.name.as_str() == expected.name()
                    && actual.kind.as_str() == mixer_kind_name(expected.kind())
                    && actual.state_available == state.is_some()
                    && actual.gain_percent.to_bits()
                        == state.map_or(0.0, |state| state.gain * 100.0).to_bits()
                    && actual.pan_percent.to_bits()
                        == state.map_or(0.0, |state| state.pan * 100.0).to_bits()
                    && actual.muted == state.is_some_and(|state| state.muted)
                    && actual.solo == state.is_some_and(|state| state.solo)
                    && actual.solo_available == (expected.kind() == IdeMixerChannelKind::Audio)
                    && actual.renameable == (expected.kind() == IdeMixerChannelKind::Audio)
                    && actual.removable == (expected.kind() == IdeMixerChannelKind::Audio)
                    && actual.active == expected.active()
                    && actual.activation_toggleable
                        == (expected.kind() == IdeMixerChannelKind::Audio)
            })
        });
    if !unchanged {
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(mixer_rows(channels)))));
    }
    let current_sends = ui.get_mixer_sends();
    if !mixer_sends_unchanged(&current_sends, channels) {
        ui.set_mixer_sends(ModelRc::from(Rc::new(VecModel::from(mixer_send_rows(
            channels,
        )))));
    }
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

fn meter_presentation(sample_peak: Option<f32>) -> (f32, String) {
    let Some(sample_peak) = sample_peak.filter(|value| value.is_finite() && *value >= 0.0) else {
        return (0.0, "—".to_owned());
    };
    let dbfs = if sample_peak == 0.0 {
        -60.0
    } else {
        (20.0 * sample_peak.log10()).max(-60.0)
    };
    (
        ((dbfs + 60.0) / 60.0).clamp(0.0, 1.0),
        format!("{dbfs:.1} dB"),
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
    fn meter_presentation_is_bounded_and_preserves_unavailable_state() {
        assert_eq!(meter_presentation(None), (0.0, "—".to_owned()));
        assert_eq!(meter_presentation(Some(f32::NAN)), (0.0, "—".to_owned()));
        assert_eq!(meter_presentation(Some(0.0)), (0.0, "-60.0 dB".to_owned()));
        assert_eq!(meter_presentation(Some(1.0)), (1.0, "0.0 dB".to_owned()));
        assert!((meter_presentation(Some(2.0)).0 - 1.0).abs() <= f32::EPSILON);
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
                launchable: false,
                queued: false,
                current: false,
            },
        ])));

        assert_eq!(validate_scene_launch(AppMode::Running, 0, &scenes), Ok(0));
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
    fn mixer_controls_require_current_observed_rows_and_bounded_values() {
        let row = |index, track_id: &str| MixerChannelRow {
            index,
            track_id: track_id.into(),
            name: "Audio channel".into(),
            display_name: "Audio".into(),
            kind: "AUDIO".into(),
            state_available: true,
            gain_percent: 100.0,
            pan_percent: 0.0,
            muted: false,
            solo: false,
            solo_available: true,
            renameable: true,
            removable: true,
            active: true,
            activation_toggleable: true,
        };
        let track_a = "018f47a2-6b2d-7c11-8a44-001122334456";
        let track_b = "018f47a2-6b2d-7c11-8a44-001122334457";
        let channels = ModelRc::from(Rc::new(VecModel::from(vec![row(0, track_a)])));
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
        let reordered = ModelRc::from(Rc::new(VecModel::from(vec![
            row(0, track_b),
            row(1, track_a),
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
        let choices = control_target_choices(&automation_channel_fixture(), &control_scene_fixture());
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
        let targets = control_target_choices(&automation_channel_fixture(), &control_scene_fixture());
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
        assert!(export_available(IdePlaybackExtent::Bounded { end_frame: 1 }));
        assert!(!export_available(IdePlaybackExtent::Bounded { end_frame: 0 }));
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
        assert_eq!(sources.len(), 8, "four CV inputs plus clock and reset edges");
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
                (
                    "Reverb solo",
                    true,
                    AutomationTarget::Channel {
                        slot: 1,
                        parameter: AutomationChannelParameter::Solo
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

        set_automation_targets(ui, &project_state, &channels);
        assert_eq!(ui.get_automation_target_names().row_count(), 13);

        reset_project(ui, &recording_routes, &take_routes, &project_state);
        assert_eq!(ui.get_automation_target_names().row_count(), 0);

        set_automation_targets(ui, &project_state, &channels);
        assert_eq!(ui.get_automation_target_names().row_count(), 13);
        assert_eq!(ui.get_automation_target_boolean().row_count(), 13);
        assert_eq!(ui.get_automation_target_domain().row_count(), 13);
    }

    #[test]
    fn automation_take_requests_coerce_interpolation_and_reject_invalid_forms() {
        let choices = automation_target_choices(&automation_channel_fixture());
        let linear = automation_take_request(&choices, " Fade ", 2, 1, "0", "1920", "0=1000000")
            .unwrap();
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

        let held = automation_take_request(&choices, "Hold", 2, 0, "0", "1920", "0=1000000")
            .unwrap();
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
        assert!(ui.get_status_text().contains("disabled in project revision 5"));
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
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SaveProject),
            None
        );
    }

    #[test]
    fn committed_audio_clip_edit_warnings_preserve_reopen_authority() {
        let ui = MainWindow::new().unwrap();
        configure_ui_contracts(&ui);
        assert_eq!(
            usize::try_from(ui.get_max_scene_count()).unwrap(),
            MAX_IDE_SCENES
        );
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
        assert_arrangement_scene_launch_edit_events(&ui);
        assert_arrangement_scene_launch_committed_warnings(&ui);
        assert_scene_authoring_forms(&ui);
        assert_scene_duplication_events_reset_form(&ui);
        assert_scene_rename_success_resets_form(&ui);
        assert_scene_activation_events_preserve_state(&ui);
        assert_automation_take_events_report_durable_outcomes(&ui);
        assert_automation_targets_repopulate_after_a_project_reset(&ui);
        assert_control_targets_repopulate_after_a_project_reset(&ui);
        assert_control_mapping_events_report_durable_outcomes(&ui);
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
    fn preview_preparation_failure_restores_running_mode() {
        assert_eq!(
            mode_after_failure(
                AppMode::PreparingPreview,
                IdeWorkerCommandKind::StartAudioPreview,
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
