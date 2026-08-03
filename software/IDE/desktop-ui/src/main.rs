#![deny(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use hyper_desktop_ide::{
    DesktopIdeWorker, IdeApplicationConfig, IdeAudioOutputInventory, IdeMixerChannel,
    IdeMixerChannelKind, IdeMixerCommand, IdeNativeCloseFailure, IdeOutputSelection,
    IdeRunningObservation, IdeScene, IdeSceneAvailability, IdeWorkerCommand, IdeWorkerCommandKind,
    IdeWorkerEvent, IdeWorkerPendingShutdownWait, IdeWorkerSendError, IdeWorkerShutdownOutcome,
    IdeWorkerShutdownReport, LocateState, PlaybackRecoveryTransportState, TransportCommand,
    desktop_ide_application_config,
};
use slint::platform::PlatformError;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

const WORKER_COMMAND_CAPACITY: usize = 16;
const WORKER_EVENT_CAPACITY: usize = 16;
const UI_POLL_INTERVAL: Duration = Duration::from_millis(50);
const INITIAL_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const FINAL_SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
const MAX_VISIBLE_SCENE_NAME_CHARS: usize = 20;
const MAX_VISIBLE_MIXER_NAME_CHARS: usize = 10;

type WorkerCell = Rc<RefCell<Option<DesktopIdeWorker>>>;
type OutputRoutes = Rc<RefCell<Vec<Option<String>>>>;

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

    bind_callbacks(&ui, &worker, &output_routes);
    let timer = start_event_pump(&ui, &worker, &output_routes);
    let run_result = ui.run();
    timer.stop();

    let shutdown_result = worker.borrow_mut().take().map_or(Ok(()), shutdown_worker);
    finish_exit(run_result, shutdown_result)?;
    Ok(())
}

fn application_config() -> Result<IdeApplicationConfig, Box<dyn Error>> {
    Ok(desktop_ide_application_config()?)
}

fn bind_callbacks(ui: &MainWindow, worker: &WorkerCell, output_routes: &OutputRoutes) {
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

    bind_playback_callbacks(ui, worker);
}

fn bind_playback_callbacks(ui: &MainWindow, worker: &WorkerCell) {
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
    let worker_for_close = Rc::clone(worker);
    actions.on_close_project(move || {
        submit(&weak, &worker_for_close, IdeWorkerCommand::CloseProject);
    });

    bind_mixer_callbacks(ui, worker);
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
}

fn start_event_pump(ui: &MainWindow, worker: &WorkerCell, output_routes: &OutputRoutes) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let output_routes = Rc::clone(output_routes);
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
                    apply_event(&ui, &output_routes, event);
                }
                Ok(None) => break,
                Err(error) => {
                    ui.set_app_mode(AppMode::Unavailable);
                    ui.set_error_text(error.to_string().into());
                    return;
                }
            }
        }
        if ui.get_app_mode() != AppMode::Running {
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

fn command_status(command: IdeWorkerCommandKind) -> &'static str {
    match command {
        IdeWorkerCommandKind::Initialize => "Discovering desktop audio outputs…",
        IdeWorkerCommandKind::RetryDiscovery => "Retrying desktop audio discovery…",
        IdeWorkerCommandKind::RefreshOutputs => "Refreshing desktop audio outputs…",
        IdeWorkerCommandKind::SelectSystemDefault | IdeWorkerCommandKind::SelectOutput => {
            "Applying audio output selection…"
        }
        IdeWorkerCommandKind::OpenProject => "Recovering and verifying project…",
        IdeWorkerCommandKind::Transport => "Transport command queued…",
        IdeWorkerCommandKind::Mixer => "Mixer command queued…",
        IdeWorkerCommandKind::Observe => "Refreshing runtime evidence…",
        IdeWorkerCommandKind::CloseProject => "Closing project and native output…",
        IdeWorkerCommandKind::Shutdown => "Shutting down…",
    }
}

fn apply_event(ui: &MainWindow, output_routes: &OutputRoutes, event: IdeWorkerEvent) {
    match event {
        IdeWorkerEvent::Closed {
            snapshot,
            native_failure,
        } => {
            apply_output_inventory(ui, output_routes, snapshot.outputs(), snapshot.selection());
            reset_project(ui);
            let mode = mode_after_closed_event(ui.get_app_mode());
            ui.set_app_mode(mode);
            if mode == AppMode::Closed {
                ui.set_status_text("Ready to open a verified project".into());
                ui.set_error_text(
                    native_failure
                        .map(native_failure_message)
                        .unwrap_or_default()
                        .into(),
                );
            }
        }
        IdeWorkerEvent::Running(observation) => {
            ui.set_app_mode(AppMode::Running);
            ui.set_project_name(observation.project_name().into());
            ui.set_project_id(observation.project_id().into());
            ui.set_sample_rate(format!("{} Hz", observation.sample_rate_hz()).into());
            ui.set_scene_count(observation.scene_count().to_string().into());
            set_scenes(ui, observation.scenes());
            apply_mixer_observation(ui, &observation);
            ui.set_stream_state(format!("{:?}", observation.output_state()).into());
            ui.set_callback_count(observation.output().callback_count.to_string().into());
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
                format_transport_clock(position_frame, observation.sample_rate_hz()).into(),
            );
            ui.set_transport_state(
                match observation.recovery().state() {
                    PlaybackRecoveryTransportState::Stopped => "Stopped",
                    PlaybackRecoveryTransportState::Rolling => "Rolling",
                }
                .into(),
            );
            let (left_level, left_reading, right_level, right_reading) = observation
                .meter()
                .snapshot()
                .map_or((0.0, "—".to_owned(), 0.0, "—".to_owned()), |snapshot| {
                    let meter = snapshot.meter();
                    let (left_level, left_reading) = meter_presentation(meter.left().sample_peak());
                    let (right_level, right_reading) =
                        meter_presentation(meter.right().sample_peak());
                    (left_level, left_reading, right_level, right_reading)
                });
            ui.set_left_meter(left_level);
            ui.set_right_meter(right_level);
            ui.set_left_reading(left_reading.into());
            ui.set_right_reading(right_reading.into());
            ui.set_status_text("Runtime evidence updated".into());
            ui.set_error_text(SharedString::default());
        }
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
        IdeWorkerEvent::MixerQueued(_) => {
            ui.set_status_text("Mixer base command queued for the audio runtime".into());
            ui.set_error_text(SharedString::default());
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

fn reset_project(ui: &MainWindow) {
    ui.set_project_name("No project open".into());
    ui.set_project_id("—".into());
    ui.set_sample_rate("—".into());
    ui.set_scene_count("—".into());
    ui.set_scenes(ModelRc::default());
    ui.set_mixer_visible(false);
    ui.set_mixer_channels(ModelRc::default());
    ui.set_mixer_main_state_available(false);
    ui.set_mixer_main_gain_percent(0.0);
    ui.set_mixer_main_muted(false);
    ui.set_transport_state("Stopped".into());
    ui.set_stream_state("No stream".into());
    ui.set_frame_position("0".into());
    ui.set_transport_clock("00:00:00.000".into());
    ui.set_callback_count("0".into());
    ui.set_xrun_count("Unavailable".into());
    ui.set_left_meter(0.0);
    ui.set_right_meter(0.0);
    ui.set_left_reading("—".into());
    ui.set_right_reading("—".into());
}

fn mode_after_failure(current: AppMode, command: IdeWorkerCommandKind) -> AppMode {
    match command {
        IdeWorkerCommandKind::Initialize if current == AppMode::Discovering => AppMode::Unavailable,
        IdeWorkerCommandKind::RetryDiscovery if current == AppMode::Unavailable => {
            AppMode::Unavailable
        }
        IdeWorkerCommandKind::OpenProject if current == AppMode::Opening => AppMode::Closed,
        IdeWorkerCommandKind::Observe if current == AppMode::Opening => AppMode::Running,
        IdeWorkerCommandKind::CloseProject if current == AppMode::Closing => AppMode::Running,
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
                name: channel.name().into(),
                display_name: compact_name(channel.name(), MAX_VISIBLE_MIXER_NAME_CHARS).into(),
                kind: mixer_kind_name(channel.kind()).into(),
                state_available: state.is_some(),
                gain_percent: state.map_or(0.0, |state| state.gain * 100.0),
                pan_percent: state.map_or(0.0, |state| state.pan * 100.0),
                muted: state.is_some_and(|state| state.muted),
                solo: state.is_some_and(|state| state.solo),
                solo_available: channel.kind() == IdeMixerChannelKind::Audio,
            }
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
            })
        });
    if !unchanged {
        ui.set_mixer_channels(ModelRc::from(Rc::new(VecModel::from(mixer_rows(channels)))));
    }
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
            if current == AppMode::Opening && command == IdeWorkerCommandKind::OpenProject =>
        {
            AppMode::Closed
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

fn format_transport_clock(frame: usize, sample_rate_hz: u32) -> String {
    if sample_rate_hz == 0 {
        return "—".to_owned();
    }
    let total_milliseconds = (frame as u128 * 1_000) / u128::from(sample_rate_hz);
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
    match report.native_failure() {
        Some(failure) => Err(DesktopUiShutdownError::Native(failure)),
        None => Ok(()),
    }
}

#[derive(Debug)]
enum DesktopUiShutdownError {
    TimedOut,
    Join(hyper_desktop_ide::IdeWorkerJoinError),
    Native(IdeNativeCloseFailure),
}

impl fmt::Display for DesktopUiShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => {
                formatter.write_str("desktop IDE worker did not stop within 5 seconds")
            }
            Self::Join(source) => source.fmt(formatter),
            Self::Native(failure) => formatter.write_str(&native_failure_message(*failure)),
        }
    }
}

impl Error for DesktopUiShutdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Join(source) => Some(source),
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
        (Err(run), Ok(())) => Err(DesktopUiExitError::Run(run)),
        (Ok(()), Err(shutdown)) => Err(DesktopUiExitError::Shutdown(shutdown)),
        (Err(run), Err(shutdown)) => Err(DesktopUiExitError::RunAndShutdown { run, shutdown }),
    }
}

#[derive(Debug)]
enum DesktopUiExitError {
    Run(PlatformError),
    Shutdown(DesktopUiShutdownError),
    RunAndShutdown {
        run: PlatformError,
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
    fn transport_clock_uses_authoritative_frames_without_rounding_ahead() {
        assert_eq!(format_transport_clock(0, 48_000), "00:00:00.000");
        assert_eq!(format_transport_clock(71_999, 48_000), "00:00:01.499");
        assert_eq!(format_transport_clock(72_000, 48_000), "00:00:01.500");
        assert_eq!(format_transport_clock(172_800_000, 48_000), "01:00:00.000");
        assert_eq!(format_transport_clock(1, 0), "—");
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
        let channels = ModelRc::from(Rc::new(VecModel::from(vec![MixerChannelRow {
            index: 0,
            name: "Audio channel".into(),
            display_name: "Audio".into(),
            kind: "AUDIO".into(),
            state_available: true,
            gain_percent: 100.0,
            pan_percent: 0.0,
            muted: false,
            solo: false,
            solo_available: true,
        }])));
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
            mode_after_failure(AppMode::Opening, IdeWorkerCommandKind::Observe),
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
    fn compound_exit_preserves_run_and_shutdown_failures() {
        let result = finish_exit(
            Err(PlatformError::Unsupported),
            Err(DesktopUiShutdownError::TimedOut),
        );
        assert!(matches!(
            result,
            Err(DesktopUiExitError::RunAndShutdown {
                run: PlatformError::Unsupported,
                shutdown: DesktopUiShutdownError::TimedOut,
            })
        ));
    }
}
