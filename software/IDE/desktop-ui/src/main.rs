#![deny(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use hyper_desktop_ide::{
    DesktopAudioPreviewState, DesktopIdeWorker, IdeApplicationConfig, IdeAudioClip,
    IdeAudioInputInventory, IdeAudioOutputInventory, IdeAudioPreviewControlCommand,
    IdeInputSelection, IdeLoudnessCommand, IdeLoudnessDisplayUnit, IdeLoudnessMeasurementState,
    IdeLoudnessPresentationConfig, IdeLoudnessScale, IdeLoudnessScaleRegion,
    IdeMaximumTruePeakDisplay, IdeMixerChannel, IdeMixerChannelKind, IdeMixerCommand,
    IdeMixerSendTap, IdeMonitoringCommand, IdeNativeCloseFailure, IdeOutputSelection,
    IdeOutputTestState, IdePlaybackArrangement, IdePlaybackExtent, IdeRecordingAbortOutcome,
    IdeRecordingCaptureKind, IdeRecordingImportOutcome, IdeRecordingTake, IdeRunningObservation,
    IdeScaledLoudnessValue, IdeScene, IdeSceneAvailability, IdeWorkerCommand, IdeWorkerCommandKind,
    IdeWorkerEvent, IdeWorkerPendingShutdownWait, IdeWorkerSendError, IdeWorkerShutdownOutcome,
    IdeWorkerShutdownReport, LocateState, PlaybackRecoveryTransportState, TransportCommand,
    desktop_ide_application_config, ide_audio_clip_gain_is_supported,
};
#[cfg(test)]
use hyper_desktop_ide::{IdeAudioClipEditCommittedWarning, IdeAudioClipFadeEditError};
use slint::platform::PlatformError;
use slint::{
    ComponentHandle, Model, ModelRc, ModelTracker, SharedString, Timer, TimerMode, VecModel,
};

slint::include_modules!();

const WORKER_COMMAND_CAPACITY: usize = 16;
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
    arrangement: RefCell<Option<ArrangementBinding>>,
    audio_clips: RefCell<Vec<IdeAudioClip>>,
}

struct ArrangementBinding {
    project_id: String,
    owner: Arc<IdePlaybackArrangement>,
    sample_rate_hz: u32,
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

    bind_arrangement_placement_addition_callback(ui, worker, project_state);
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

fn parse_arrangement_start_tick(value: &str) -> Result<u64, &'static str> {
    parse_exact_arrangement_tick(
        value,
        "Arrangement start tick must be a non-negative whole number",
        "Arrangement start tick is outside the supported range",
        "Arrangement start tick exceeds the exact project range",
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
        IdeWorkerCommandKind::SaveProject => AppMode::Saving,
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
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            AppMode::DuplicatingArrangementPlacement
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => AppMode::SplittingArrangementPlacement,
        IdeWorkerCommandKind::RenameAudioClip => AppMode::RenamingClip,
        IdeWorkerCommandKind::DuplicateAudioClip => AppMode::DuplicatingClip,
        IdeWorkerCommandKind::SetAudioClipActive => AppMode::ChangingClipActivation,
        IdeWorkerCommandKind::SetAudioClipGain => AppMode::ChangingClipGain,
        IdeWorkerCommandKind::SetAudioClipFades => AppMode::ChangingClipFades,
        IdeWorkerCommandKind::CreateAudioClipFromTake => AppMode::CreatingClip,
        IdeWorkerCommandKind::StartRecording => AppMode::PreparingRecording,
        IdeWorkerCommandKind::StopRecording | IdeWorkerCommandKind::AbortRecording => {
            AppMode::FinalizingRecording
        }
        IdeWorkerCommandKind::StartAudioPreview => AppMode::PreparingPreview,
        _ => current,
    }
}

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
        IdeWorkerCommandKind::Loudness => "Loudness measurement command queued…",
        IdeWorkerCommandKind::Mixer => "Mixer command queued…",
        IdeWorkerCommandKind::Monitoring => "Applying input cue control…",
        IdeWorkerCommandKind::SaveProject => "Saving mixer state and reopening project…",
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
    let status = match command {
        IdeWorkerCommandKind::AddArrangementPlacement => {
            reset_arrangement_placement_addition_form(ui);
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement addition warning must retain its identity");
            format!(
                "Arrangement placement {placement_id} committed and reopened with incomplete transaction cleanup"
            )
        }
        IdeWorkerCommandKind::DuplicateArrangementPlacement => {
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement duplication warning must retain its identity");
            format!(
                "Copied arrangement placement {placement_id} committed and reopened with incomplete transaction cleanup"
            )
        }
        IdeWorkerCommandKind::SplitArrangementPlacement => {
            let placement_id = warning
                .committed_created_placement_id()
                .expect("committed placement split warning must retain its right-half identity");
            format!(
                "Split arrangement placement right half {placement_id} committed and reopened with incomplete transaction cleanup"
            )
        }
        IdeWorkerCommandKind::RemoveArrangementPlacement => {
            reset_arrangement_placement_removal_confirmation(ui);
            "Arrangement placement removal committed and reopened with incomplete transaction cleanup"
                .to_owned()
        }
        _ => {
            "Arrangement placement edit committed and reopened with incomplete transaction cleanup"
                .to_owned()
        }
    };
    ui.set_status_text(status.into());
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
                format!(
                    "Reusable audio clip {name} ({clip_id}) committed and reopened with incomplete transaction cleanup"
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
        AudioClipEditWarningForm::None => {}
        AudioClipEditWarningForm::Rename => reset_audio_clip_rename_form(ui),
        AudioClipEditWarningForm::Gain => reset_audio_clip_gain_form(ui),
        AudioClipEditWarningForm::Fades => reset_audio_clip_fade_form(ui),
    }
    ui.set_app_mode(AppMode::Running);
    ui.set_status_text(
        format!("{action} committed and reopened with a project recovery warning").into(),
    );
    ui.set_error_text(warning.to_string().into());
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
    set_scenes(ui, observation.scenes());
    apply_mixer_observation(ui, observation);
    set_recording_targets(ui, recording_routes, observation.mixer_channels());
    set_recording_takes(ui, take_routes, observation);
    set_playback_extent(ui, project_state, observation);
    set_arrangement(ui, project_state, observation);
    set_audio_clips(ui, project_state, observation.audio_clips());
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
            ui.set_transport_end_clock("—".into());
            ui.set_transport_progress_percent(0.0);
        }
    }
}

struct ArrangementListModel {
    owner: Arc<IdePlaybackArrangement>,
    sample_rate_hz: u32,
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
    let unchanged = project_state
        .arrangement
        .borrow()
        .as_ref()
        .is_some_and(|current| {
            current.project_id == observation.project_id()
                && current.sample_rate_hz == observation.sample_rate_hz()
                && Arc::ptr_eq(&current.owner, owner)
        });
    if unchanged {
        return;
    }
    ui.set_arrangement_placements(ModelRc::new(ArrangementListModel {
        owner: Arc::clone(owner),
        sample_rate_hz: observation.sample_rate_hz(),
    }));
    ui.set_arrangement_span_clock(
        format_frame_clock(owner.end_frame(), observation.sample_rate_hz()).into(),
    );
    *project_state.arrangement.borrow_mut() = Some(ArrangementBinding {
        project_id: observation.project_id().to_owned(),
        owner: Arc::clone(owner),
        sample_rate_hz: observation.sample_rate_hz(),
    });
}

fn set_audio_clips(ui: &MainWindow, project_state: &ProjectUiStateCell, clips: &[IdeAudioClip]) {
    if project_state.audio_clips.borrow().as_slice() == clips {
        return;
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
            fade_label: format!(
                "{} in / {} out",
                clip.fade_in_ticks(),
                clip.fade_out_ticks()
            )
            .into(),
            fade_in_edit: clip.fade_in_ticks().to_string().into(),
            fade_out_edit: clip.fade_out_ticks().to_string().into(),
            duration_ticks: clip.duration_ticks().to_string().into(),
        })
        .collect();
    ui.set_audio_clips(ModelRc::from(Rc::new(VecModel::from(rows))));
    *project_state.audio_clips.borrow_mut() = clips.to_vec();
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
    ui.set_arrangement_visible(false);
    ui.set_remove_arrangement_placement_visible(false);
    ui.set_remove_arrangement_placement_id(SharedString::default());
    ui.set_remove_arrangement_placement_clip_name(SharedString::default());
    ui.set_remove_arrangement_placement_track_name(SharedString::default());
    ui.set_add_arrangement_placement_visible(false);
    ui.set_add_arrangement_placement_clip_id(SharedString::default());
    ui.set_add_arrangement_placement_clip_name(SharedString::default());
    ui.set_add_arrangement_placement_track_name(SharedString::default());
    ui.set_add_arrangement_placement_duration_ticks(SharedString::default());
    ui.set_add_arrangement_placement_start_tick("0".into());
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
    ui.set_audio_clips(ModelRc::default());
    ui.set_loudness_visible(false);
    ui.set_loudness_reset_confirming(false);
    ui.set_arrangement_placements(ModelRc::default());
    ui.set_arrangement_span_clock("—".into());
    ui.set_preview_active(false);
    ui.set_preview_gain_percent(25.0);
    ui.set_preview_muted(false);
    ui.set_preview_state("Idle".into());
    ui.set_preview_progress("—".into());
    *project_state.playback_end_frame.borrow_mut() = None;
    *project_state.arrangement.borrow_mut() = None;
    project_state.audio_clips.borrow_mut().clear();
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

fn reset_monitoring(ui: &MainWindow) {
    ui.set_monitoring_active(false);
    ui.set_monitoring_gain_percent(0.0);
    ui.set_monitoring_muted(false);
    ui.set_monitoring_state("Off".into());
    ui.set_monitoring_frame_count("0".into());
    ui.set_monitoring_drop_count("0".into());
    ui.set_monitoring_underflow_count("0".into());
}

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
        IdeWorkerCommandKind::SaveProject if current == AppMode::Saving => AppMode::Running,
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

fn scene_rows(scenes: &[IdeScene]) -> Vec<SceneRow> {
    scenes
        .iter()
        .enumerate()
        .map(|(index, scene)| {
            let (availability, launchable) = scene_availability_presentation(scene.availability());
            SceneRow {
                index: i32::try_from(index).unwrap_or(-1),
                name: scene.name().into(),
                display_name: scene_display_name(scene.name()).into(),
                availability: availability.into(),
                launchable,
            }
        })
        .collect()
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

fn set_scenes(ui: &MainWindow, scenes: &[IdeScene]) {
    let current = ui.get_scenes();
    let unchanged = current.row_count() == scenes.len()
        && scenes.iter().enumerate().all(|(index, expected)| {
            current.row_data(index).is_some_and(|actual| {
                let (availability, launchable) =
                    scene_availability_presentation(expected.availability());
                actual.index == i32::try_from(index).unwrap_or(-1)
                    && actual.name.as_str() == expected.name()
                    && actual.availability.as_str() == availability
                    && actual.launchable == launchable
            })
        });
    if !unchanged {
        ui.set_scenes(ModelRc::from(Rc::new(VecModel::from(scene_rows(scenes)))));
    }
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
                name: "Verse".into(),
                display_name: "Verse".into(),
                availability: "Ready".into(),
                launchable: true,
            },
            SceneRow {
                index: 1,
                name: "Break".into(),
                display_name: "Break".into(),
                availability: "Disabled".into(),
                launchable: false,
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
    fn arrangement_move_input_preserves_the_exact_project_tick_domain() {
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
            audio_clip_edit_warning_presentation(IdeWorkerCommandKind::SaveProject),
            None
        );
    }

    #[test]
    fn committed_audio_clip_fade_warning_restores_reopened_ui_authority() {
        let ui = MainWindow::new().unwrap();
        ui.set_app_mode(AppMode::ChangingClipFades);
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
