#![deny(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use hyper_desktop_ide::{
    DesktopAudioPreviewState, DesktopIdeWorker, IdeApplicationConfig, IdeAudioInputInventory,
    IdeAudioOutputInventory, IdeAudioPreviewControlCommand, IdeInputSelection, IdeMixerChannel,
    IdeMixerChannelKind, IdeMixerCommand, IdeMixerSendTap, IdeMonitoringCommand,
    IdeNativeCloseFailure, IdeOutputSelection, IdePlaybackExtent, IdeRecordingAbortOutcome,
    IdeRecordingCaptureKind, IdeRecordingImportOutcome, IdeRecordingTake, IdeRunningObservation,
    IdeScene, IdeSceneAvailability, IdeWorkerCommand, IdeWorkerCommandKind, IdeWorkerEvent,
    IdeWorkerPendingShutdownWait, IdeWorkerSendError, IdeWorkerShutdownOutcome,
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
type InputRoutes = Rc<RefCell<Vec<InputRoute>>>;
type RecordingRoutes = Rc<RefCell<Vec<usize>>>;
type TakeRoutes = Rc<RefCell<Vec<String>>>;
type PlaybackExtentCell = Rc<RefCell<Option<usize>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
enum InputRoute {
    Disabled,
    SystemDefault,
    Routed(String),
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
    let playback_extent = Rc::new(RefCell::new(None));

    bind_callbacks(
        &ui,
        &worker,
        &output_routes,
        &input_routes,
        &recording_routes,
        &take_routes,
        &playback_extent,
    );
    let timer = start_event_pump(
        &ui,
        &worker,
        &output_routes,
        &input_routes,
        &recording_routes,
        &take_routes,
        &playback_extent,
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
    playback_extent: &PlaybackExtentCell,
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

    bind_input_selection_callbacks(ui, worker, input_routes);

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

    bind_playback_callbacks(ui, worker, playback_extent);
    bind_monitoring_callbacks(ui, worker);
    bind_recording_callbacks(ui, worker, recording_routes);
    bind_audio_preview_callbacks(ui, worker, take_routes);
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
        let asset_id = usize::try_from(index)
            .ok()
            .and_then(|index| routes_for_toggle.borrow().get(index).cloned());
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
}

fn bind_playback_callbacks(
    ui: &MainWindow,
    worker: &WorkerCell,
    playback_extent: &PlaybackExtentCell,
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
    let extent_for_seek = Rc::clone(playback_extent);
    actions.on_transport_seek(move |percent| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if ui.get_app_mode() != AppMode::Running {
            return;
        }
        let Some(end_frame) = *extent_for_seek.borrow() else {
            ui.set_error_text("Finite playback extent is no longer available".into());
            return;
        };
        match seek_percent_to_frame(percent, end_frame) {
            Ok(frame) => submit_to_ui(&ui, &worker_for_seek, IdeWorkerCommand::Seek { frame }),
            Err(message) => ui.set_error_text(message.into()),
        }
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

fn start_event_pump(
    ui: &MainWindow,
    worker: &WorkerCell,
    output_routes: &OutputRoutes,
    input_routes: &InputRoutes,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    playback_extent: &PlaybackExtentCell,
) -> Timer {
    let timer = Timer::default();
    let weak = ui.as_weak();
    let worker = Rc::clone(worker);
    let output_routes = Rc::clone(output_routes);
    let input_routes = Rc::clone(input_routes);
    let recording_routes = Rc::clone(recording_routes);
    let take_routes = Rc::clone(take_routes);
    let playback_extent = Rc::clone(playback_extent);
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
                        &playback_extent,
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
            AppMode::Running | AppMode::Previewing | AppMode::Recording
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
            match command_kind {
                IdeWorkerCommandKind::SaveProject => ui.set_app_mode(AppMode::Saving),
                IdeWorkerCommandKind::StartRecording => {
                    ui.set_app_mode(AppMode::PreparingRecording);
                }
                IdeWorkerCommandKind::StopRecording | IdeWorkerCommandKind::AbortRecording => {
                    ui.set_app_mode(AppMode::FinalizingRecording);
                }
                IdeWorkerCommandKind::StartAudioPreview => {
                    ui.set_app_mode(AppMode::PreparingPreview);
                }
                _ => {}
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
        IdeWorkerCommandKind::OpenProject => "Recovering and verifying project…",
        IdeWorkerCommandKind::Transport => "Transport command queued…",
        IdeWorkerCommandKind::Seek => "Bounded transport locate queued…",
        IdeWorkerCommandKind::Mixer => "Mixer command queued…",
        IdeWorkerCommandKind::Monitoring => "Applying input cue control…",
        IdeWorkerCommandKind::SaveProject => "Saving mixer state and reopening project…",
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

fn apply_event(
    ui: &MainWindow,
    output_routes: &OutputRoutes,
    input_routes: &InputRoutes,
    recording_routes: &RecordingRoutes,
    take_routes: &TakeRoutes,
    playback_extent: &PlaybackExtentCell,
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
            reset_project(ui, recording_routes, take_routes, playback_extent);
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
            playback_extent,
            &observation,
        ),
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
        IdeWorkerEvent::MonitoringApplied(_) => {
            ui.set_status_text("Input cue control applied".into());
            ui.set_error_text(SharedString::default());
        }
        IdeWorkerEvent::ProjectSaved { revision, changed } => {
            ui.set_status_text(
                if changed {
                    format!("Project revision {revision} saved and reopened")
                } else {
                    format!("Project revision {revision} already matched the live mixer")
                }
                .into(),
            );
            ui.set_error_text(SharedString::default());
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
    playback_extent: &PlaybackExtentCell,
    observation: &IdeRunningObservation,
) {
    ui.set_project_name(observation.project_name().into());
    ui.set_project_id(observation.project_id().into());
    ui.set_sample_rate(format!("{} Hz", observation.sample_rate_hz()).into());
    ui.set_scene_count(observation.scene_count().to_string().into());
    set_scenes(ui, observation.scenes());
    apply_mixer_observation(ui, observation);
    set_recording_targets(ui, recording_routes, observation.mixer_channels());
    set_recording_takes(ui, take_routes, observation);
    set_playback_extent(ui, playback_extent, observation);
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
        routes.push(take.asset_id().to_owned());
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
    playback_extent: &PlaybackExtentCell,
    observation: &IdeRunningObservation,
) {
    match observation.playback_extent() {
        IdePlaybackExtent::Bounded { end_frame } => {
            *playback_extent.borrow_mut() = Some(end_frame);
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
            *playback_extent.borrow_mut() = None;
            ui.set_transport_extent_bounded(false);
            ui.set_transport_seek_available(false);
            ui.set_transport_end_clock("—".into());
            ui.set_transport_progress_percent(0.0);
        }
    }
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
    playback_extent: &PlaybackExtentCell,
) {
    ui.set_project_name("No project open".into());
    ui.set_project_id("—".into());
    ui.set_sample_rate("—".into());
    ui.set_scene_count("—".into());
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
    ui.set_takes_visible(false);
    ui.set_preview_active(false);
    ui.set_preview_gain_percent(25.0);
    ui.set_preview_muted(false);
    ui.set_preview_state("Idle".into());
    ui.set_preview_progress("—".into());
    *playback_extent.borrow_mut() = None;
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
        IdeWorkerCommandKind::OpenProject if current == AppMode::Opening => AppMode::Closed,
        IdeWorkerCommandKind::Observe if current == AppMode::Opening => AppMode::Running,
        IdeWorkerCommandKind::CloseProject if current == AppMode::Closing => AppMode::Running,
        IdeWorkerCommandKind::SaveProject if current == AppMode::Saving => AppMode::Running,
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
