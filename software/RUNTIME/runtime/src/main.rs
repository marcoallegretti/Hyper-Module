//! The runtime process: hosts one verified bundle and serves one panel.

#![deny(unsafe_code)]

use std::io::BufRead;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hyper_audio_source::DecodeLimits;
use hyper_control_runtime::{
    PanelBinding, PanelConnectError, PanelConnection, PanelDispatchError, PanelDispatchOutcome,
    PanelDisplay, PanelReleaseFailure,
};
use hyper_desktop_audio::{
    DesktopControlSinkError, DesktopOutputConfig, DesktopPlaybackSession, OutputStreamCounters,
    OutputStreamSnapshot, OutputStreamState, OutputStreamStatus,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{
    ChannelSamplePeakBallistics, LocateState, OutputMeterReceiver, PlaybackRecoveryCheckpoint,
    PlaybackRecoveryTransportState, SamplePeakBallisticsConfig, StereoSamplePeakBallistics,
    StereoSamplePeakBallisticsSnapshot, TransportCommand,
};
use hyper_panel_protocol::{LinkConfig, Message, PanelDescriptor};
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_runtime_deployment::resolve_runtime_deployment;
use hyper_runtime_host::{PanelHost, RuntimeArguments, ServedPanel, serve_panels_with_idle};
use slint::{CloseRequestResponse, ComponentHandle, Timer, TimerMode};

slint::include_modules!();

/// The runtime's own hosting policy. Stated here rather than borrowed from the
/// IDE: these are the limits this process accepts, and they are software
/// limits, not evidence of embedded hardware capacity.
const MAXIMUM_BLOCK_FRAMES: usize = 1_024;
const SAMPLE_RATE_HZ: u32 = 48_000;
const MAX_AUDIO_FILE_BYTES: u64 = 512 * 1_024 * 1_024;
const MAX_AUDIO_FRAMES: u64 = 43_200_000;
const MAX_AUDIO_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_RESOURCE_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_RUNTIME_BUNDLE_BYTES: u64 = 1_024 * 1_024 * 1_024;
const OUTPUT_METER_FLOOR_DBFS: f32 = -90.0;
const OUTPUT_METER_FALLOFF_DB_PER_SECOND: f32 = 13.3;

const HEARTBEAT_INTERVAL_MS: u32 = 50;
const HEARTBEAT_TIMEOUT_MS: u32 = 500;

/// The runtime's control owner. Every method forwards; the session owns the
/// sole control runtime, so it is the only thing that can answer these.
struct SessionHost {
    session: DesktopPlaybackSession,
    panel_connected: Arc<AtomicBool>,
}

impl PanelHost for SessionHost {
    type Error = DesktopControlSinkError;

    fn mixer_channel_count(&self) -> usize {
        self.session.mixer_channel_count()
    }

    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
        let connection = self.session.attach_panel(descriptor, bindings)?;
        self.panel_connected.store(true, Ordering::Release);
        Ok(connection)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        let failures = self.session.detach_panel();
        self.panel_connected
            .store(self.session.has_panel(), Ordering::Release);
        failures
    }

    fn panel_connected(&self) -> bool {
        self.session.has_panel()
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        self.session.deliver_panel_message(message)
    }

    fn panel_display(&mut self, out: &mut Vec<PanelDisplay>) {
        self.session.panel_display(out);
    }
}

impl SessionHost {
    fn take_meter_receiver(&mut self) -> Option<OutputMeterReceiver> {
        self.session.take_meter_receiver()
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hyper-runtime: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let arguments = RuntimeArguments::parse(std::env::args().skip(1)).map_err(|error| {
        format!(
            "{error}\nusage: hyper-runtime <deployment-target-directory> [--panel-listen ADDRESS] [--operator-ui]"
        )
    })?;

    if arguments.operator_ui() {
        run_operator_ui(arguments)
    } else {
        run_headless(&arguments)
    }
}

#[derive(Clone, Debug)]
struct RuntimeFacts {
    bundle_identity: String,
    deployment_generation: u64,
    recovered_from: Option<String>,
    project_name: String,
    project_id: String,
    project_revision: u64,
}

#[derive(Clone, Debug)]
struct RuntimeObservers {
    facts: RuntimeFacts,
    output_status: OutputStreamStatus,
    output_counters: OutputStreamCounters,
    recovery: PlaybackRecoveryCheckpoint,
    panel_connected: Arc<AtomicBool>,
}

struct OperatorReady {
    observers: RuntimeObservers,
    meters: OutputMeterReceiver,
}

struct OperatorWorker {
    ready: Receiver<Result<OperatorReady, String>>,
    completion: Receiver<Result<(), String>>,
    handle: std::thread::JoinHandle<Result<(), String>>,
}

struct OperatorMeterObserver {
    receiver: OutputMeterReceiver,
    ballistics: StereoSamplePeakBallistics,
    latest: Option<StereoSamplePeakBallisticsSnapshot>,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
}

impl OperatorMeterObserver {
    fn new(receiver: OutputMeterReceiver, config: SamplePeakBallisticsConfig) -> Self {
        Self {
            receiver,
            ballistics: StereoSamplePeakBallistics::new(config),
            latest: None,
            coalesced_snapshot_count: 0,
            dropped_snapshot_count: 0,
            sender_disconnected: false,
        }
    }

    fn observe(&mut self) -> OperatorMeterPresentation {
        let read = self.receiver.read_ballistics(&mut self.ballistics);
        let latest = read.latest();
        self.coalesced_snapshot_count = self
            .coalesced_snapshot_count
            .saturating_add(u64::try_from(latest.coalesced_snapshot_count()).unwrap_or(u64::MAX));
        self.dropped_snapshot_count = self
            .dropped_snapshot_count
            .saturating_add(u64::try_from(latest.dropped_snapshot_count()).unwrap_or(u64::MAX));
        self.sender_disconnected |= latest.sender_disconnected();
        if let Some(snapshot) = read.ballistics() {
            self.latest = Some(snapshot);
        }
        operator_meter_presentation(
            self.latest,
            self.coalesced_snapshot_count,
            self.dropped_snapshot_count,
            self.sender_disconnected,
        )
    }
}

struct StartedRuntime {
    host: SessionHost,
    observers: RuntimeObservers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorCommand {
    Start,
    Stop,
    ReturnToStart,
    Quit,
}

const OPERATOR_COMMAND_CAPACITY: usize = 8;

trait RuntimeOperatorControl {
    fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String>;
}

trait RuntimeOutputObservation {
    fn output_state(&self) -> OutputStreamState;
}

impl RuntimeOutputObservation for OutputStreamStatus {
    fn output_state(&self) -> OutputStreamState {
        self.state()
    }
}

impl RuntimeOperatorControl for SessionHost {
    fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String> {
        self.session
            .try_send_transport_command(command)
            .map_err(|error| error.to_string())
    }
}

fn start_runtime(arguments: &RuntimeArguments) -> Result<StartedRuntime, String> {
    let (facts, session) = prepare_output_and_host(
        || {
            let deployment = resolve_runtime_deployment(
                arguments.deployment_root(),
                env!("CARGO_PKG_VERSION"),
                MAX_RUNTIME_BUNDLE_BYTES,
                limits()?,
            )
            .map_err(|error| {
                format!(
                    "deployment target {} could not be resolved: {}",
                    arguments.deployment_root().display(),
                    format_error_chain(&error)
                )
            })?;
            let manifest = deployment.bundle().project().package().manifest();
            let facts = RuntimeFacts {
                bundle_identity: hex_digest(deployment.bundle().bundle_sha256()),
                deployment_generation: deployment.generation(),
                recovered_from: deployment.recovered_from_bundle_sha256().map(hex_digest),
                project_name: manifest.name().to_owned(),
                project_id: manifest.project_id().to_owned(),
                project_revision: manifest.revision(),
            };
            Ok((facts, deployment.into_bundle().into_project()))
        },
        || {
            DesktopOutputConfig::try_new(SAMPLE_RATE_HZ, MAXIMUM_BLOCK_FRAMES)
                .map_err(|error| format!("output configuration is invalid: {error}"))
        },
        |output, (facts, project)| {
            DesktopPlaybackSession::start_project(output, project)
                .map(|session| (facts, session))
                .map_err(|error| format!("verified bundle could not be hosted: {error}"))
        },
    )?;
    let panel_connected = Arc::new(AtomicBool::new(false));
    let observers = RuntimeObservers {
        facts,
        output_status: session.output_status(),
        output_counters: session.counters(),
        recovery: session.recovery_checkpoint(),
        panel_connected: Arc::clone(&panel_connected),
    };
    Ok(StartedRuntime {
        host: SessionHost {
            session,
            panel_connected,
        },
        observers,
    })
}

fn announce_runtime(arguments: &RuntimeArguments, runtime: &StartedRuntime) {
    println!(
        "hosting deployment generation {} bundle {} from {} with {} mixer channels",
        runtime.observers.facts.deployment_generation,
        runtime.observers.facts.bundle_identity,
        arguments.deployment_root().display(),
        runtime.host.mixer_channel_count()
    );
    if let Some(recovered) = &runtime.observers.facts.recovered_from {
        println!("recovered from rejected bundle {recovered}");
    }
}

fn run_headless(arguments: &RuntimeArguments) -> Result<(), String> {
    let mut runtime = start_runtime(arguments)?;
    announce_runtime(arguments, &runtime);
    let stop = Arc::new(AtomicBool::new(false));
    watch_for_quit(Arc::clone(&stop));
    let output_status = runtime.observers.output_status.clone();
    host_runtime(
        arguments,
        &mut runtime.host,
        &output_status,
        &stop,
        None,
        &|message| {
            println!("{message}");
        },
    )?;
    println!("stopping");
    Ok(())
}

fn host_runtime<H, O>(
    arguments: &RuntimeArguments,
    host: &mut H,
    output_status: &O,
    stop: &AtomicBool,
    commands: Option<&Receiver<OperatorCommand>>,
    diagnostic: &dyn Fn(String),
) -> Result<(), String>
where
    H: PanelHost + RuntimeOperatorControl,
    O: RuntimeOutputObservation,
{
    let mut owner_turn = |host: &mut H| {
        if let Some(commands) = commands {
            drain_operator_commands(host, commands, stop, diagnostic);
        }
        if output_status.output_state() == OutputStreamState::Failed {
            stop.store(true, Ordering::Release);
        }
    };

    let Some(address) = arguments.panel_listen() else {
        diagnostic("no panel link requested".to_owned());
        while !stop.load(Ordering::Acquire) {
            owner_turn(host);
            std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL);
        }
        return finish_runtime_output(output_status, diagnostic);
    };

    let listener = match prepare_panel_listener(address) {
        Ok(listener) => listener,
        Err(error) => {
            contain_panel_service_failure(host, stop, diagnostic, error, &mut owner_turn);
            return finish_runtime_output(output_status, diagnostic);
        }
    };
    diagnostic(format!(
        "panel link listening on {}",
        listener.local_addr().unwrap_or(address)
    ));

    let config = match LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS) {
        Ok(config) => config,
        Err(error) => {
            drop(listener);
            contain_panel_service_failure(
                host,
                stop,
                diagnostic,
                format!("panel link configuration is invalid: {error:?}"),
                &mut owner_turn,
            );
            return finish_runtime_output(output_status, diagnostic);
        }
    };
    let started = Instant::now();
    let service = serve_panels_with_idle(
        &listener,
        host,
        config,
        stop,
        || millis_since(started),
        |served| report_to(served, diagnostic),
        &mut owner_turn,
    );
    drop(listener);
    if let Err(error) = service {
        contain_panel_service_failure(
            host,
            stop,
            diagnostic,
            format!("panel service ended: {error}"),
            &mut owner_turn,
        );
    }
    finish_runtime_output(output_status, diagnostic)
}

fn prepare_panel_listener(address: std::net::SocketAddr) -> Result<TcpListener, String> {
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot poll {address}: {error}"))?;
    Ok(listener)
}

fn contain_panel_service_failure<H, I>(
    host: &mut H,
    stop: &AtomicBool,
    diagnostic: &dyn Fn(String),
    error: String,
    mut owner_turn: I,
) where
    H: PanelHost,
    I: FnMut(&mut H),
{
    diagnostic(error);
    for failure in host.disconnect_panel() {
        diagnostic(format!(
            "{:?} could not be released and stays held",
            failure.control()
        ));
    }
    diagnostic("the project stays hosted; request shutdown to stop it".to_owned());
    while !stop.load(Ordering::Acquire) {
        owner_turn(host);
        std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL);
    }
}

fn finish_runtime_output<O: RuntimeOutputObservation>(
    output_status: &O,
    diagnostic: &dyn Fn(String),
) -> Result<(), String> {
    if output_status.output_state() == OutputStreamState::Failed {
        let message = "native audio ownership ended unexpectedly".to_owned();
        diagnostic(message.clone());
        Err(message)
    } else {
        Ok(())
    }
}

fn drain_operator_commands<H: RuntimeOperatorControl>(
    host: &mut H,
    commands: &Receiver<OperatorCommand>,
    stop: &AtomicBool,
    diagnostic: &dyn Fn(String),
) {
    for _ in 0..OPERATOR_COMMAND_CAPACITY {
        let command = match commands.try_recv() {
            Ok(command) => command,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                stop.store(true, Ordering::Release);
                break;
            }
        };
        match command {
            OperatorCommand::Start => match host.queue_transport(TransportCommand::Start) {
                Ok(()) => diagnostic("transport start queued".to_owned()),
                Err(error) => diagnostic(format!("transport start refused: {error}")),
            },
            OperatorCommand::Stop => match host.queue_transport(TransportCommand::Stop) {
                Ok(()) => diagnostic("transport stop queued".to_owned()),
                Err(error) => diagnostic(format!("transport stop refused: {error}")),
            },
            OperatorCommand::ReturnToStart => match host.queue_transport(TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            }) {
                Ok(()) => diagnostic("transport return to start queued".to_owned()),
                Err(error) => diagnostic(format!("transport return to start refused: {error}")),
            },
            OperatorCommand::Quit => stop.store(true, Ordering::Release),
        }
    }
}

fn spawn_operator_worker(
    arguments: RuntimeArguments,
    stop: Arc<AtomicBool>,
    commands: Receiver<OperatorCommand>,
    diagnostic: Arc<Mutex<String>>,
) -> OperatorWorker {
    let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
    let (completion_tx, completion) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let result = match start_runtime(&arguments) {
            Ok(mut runtime) => {
                let ready_model = runtime
                    .host
                    .take_meter_receiver()
                    .map(|meters| OperatorReady {
                        observers: runtime.observers.clone(),
                        meters,
                    })
                    .ok_or_else(|| {
                        "operator output meter authority was already transferred".to_owned()
                    });
                match ready_model {
                    Ok(ready_model) => {
                        announce_runtime(&arguments, &runtime);
                        if ready_tx.send(Ok(ready_model)).is_err() {
                            Err("operator UI ended before accepting startup evidence".to_owned())
                        } else {
                            host_runtime(
                                &arguments,
                                &mut runtime.host,
                                &runtime.observers.output_status,
                                &stop,
                                Some(&commands),
                                &|message| set_diagnostic(&diagnostic, message),
                            )
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error.clone()));
                Err(error)
            }
        };
        let _ = completion_tx.send(result.clone());
        result
    });
    OperatorWorker {
        ready,
        completion,
        handle,
    }
}

fn run_operator_ui(arguments: RuntimeArguments) -> Result<(), String> {
    let ui =
        OperatorWindow::new().map_err(|error| format!("operator UI could not open: {error}"))?;
    let meter_config = SamplePeakBallisticsConfig::try_new(
        SAMPLE_RATE_HZ,
        OUTPUT_METER_FLOOR_DBFS,
        OUTPUT_METER_FALLOFF_DB_PER_SECOND,
        0,
    )
    .map_err(|error| format!("operator output meter policy is invalid: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands_rx) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
    let diagnostic = Arc::new(Mutex::new(
        "starting verified Runtime deployment".to_owned(),
    ));
    let completion_close_error = Arc::new(Mutex::new(None));
    let worker = spawn_operator_worker(
        arguments,
        Arc::clone(&stop),
        commands_rx,
        Arc::clone(&diagnostic),
    );
    let ready = match worker.ready.recv() {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => return finish_failed_operator_startup(error, worker.handle),
        Err(_) => {
            return finish_failed_operator_startup(
                "Runtime worker ended before publishing startup evidence".to_owned(),
                worker.handle,
            );
        }
    };
    let OperatorReady { observers, meters } = ready;
    let mut meter_observer = OperatorMeterObserver::new(meters, meter_config);

    apply_static_facts(&ui, &observers.facts);
    wire_operator_action(&ui, &commands_tx, &diagnostic, OperatorCommand::Start);
    wire_operator_stop(&ui, &commands_tx, &diagnostic);
    wire_operator_return_to_start(&ui, &commands_tx, &diagnostic);
    wire_operator_quit(&ui, &commands_tx, &stop, &diagnostic);

    apply_observations(&ui, &observers, &diagnostic);
    apply_operator_meter_observation(&ui, &mut meter_observer);

    let weak = ui.as_weak();
    let timer_observers = observers.clone();
    let mut timer_meter_observer = meter_observer;
    let timer_diagnostic = Arc::clone(&diagnostic);
    let timer_close_error = Arc::clone(&completion_close_error);
    let mut completion_observed = false;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        if let Some(ui) = weak.upgrade() {
            apply_observations(&ui, &timer_observers, &timer_diagnostic);
            apply_operator_meter_observation(&ui, &mut timer_meter_observer);
            if !completion_observed
                && let Some(close_result) = apply_operator_worker_completion(
                    &ui,
                    &worker.completion,
                    &timer_diagnostic,
                    || {
                        close_operator_surface(
                            || {
                                ui.hide().map_err(|error| {
                                    format!("operator surface could not hide: {error}")
                                })
                            },
                            || {
                                slint::quit_event_loop().map_err(|error| {
                                    format!("operator event loop could not terminate: {error}")
                                })
                            },
                        )
                    },
                )
            {
                completion_observed = true;
                if let Err(error) = close_result {
                    set_first_error(&timer_close_error, error);
                }
            }
        }
    });

    let ui_result = ui
        .run()
        .map_err(|error| format!("operator UI failed: {error}"));
    drop(timer);
    let ui_result = reconcile_operator_ui_close(ui_result, &completion_close_error);
    finish_operator_worker(ui_result, &stop, &commands_tx, worker.handle)
}

fn finish_failed_operator_startup(
    reported_error: String,
    worker: std::thread::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    match worker.join() {
        Ok(Ok(())) => Err(reported_error),
        Ok(Err(worker_error)) => Err(worker_error),
        Err(_) => Err("Runtime worker panicked during startup".to_owned()),
    }
}

fn finish_operator_worker(
    ui_result: Result<(), String>,
    stop: &AtomicBool,
    commands: &SyncSender<OperatorCommand>,
    worker: std::thread::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    stop.store(true, Ordering::Release);
    let _ = commands.try_send(OperatorCommand::Quit);
    let worker_result = worker
        .join()
        .map_err(|_| "Runtime worker panicked during shutdown".to_owned())?;
    reconcile_operator_results(ui_result, worker_result)
}

fn reconcile_operator_results(
    ui_result: Result<(), String>,
    worker_result: Result<(), String>,
) -> Result<(), String> {
    match (worker_result, ui_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(ui_error)) => Err(ui_error),
        (Err(worker_error), Ok(())) => Err(worker_error),
        (Err(worker_error), Err(ui_error)) => Err(format!(
            "{worker_error}; operator UI shutdown also failed: {ui_error}"
        )),
    }
}

fn reconcile_operator_ui_close(
    ui_result: Result<(), String>,
    close_error: &Mutex<Option<String>>,
) -> Result<(), String> {
    let close_error = close_error.lock().ok().and_then(|mut error| error.take());
    match (ui_result, close_error) {
        (Ok(()), None) => Ok(()),
        (Err(error), None) | (Ok(()), Some(error)) => Err(error),
        (Err(ui_error), Some(close_error)) => Err(format!(
            "{ui_error}; operator surface shutdown also failed: {close_error}"
        )),
    }
}

fn set_diagnostic(diagnostic: &Mutex<String>, message: String) {
    if let Ok(mut current) = diagnostic.lock() {
        *current = message;
    }
}

fn set_first_error(error: &Mutex<Option<String>>, message: String) {
    if let Ok(mut current) = error.lock()
        && current.is_none()
    {
        *current = Some(message);
    }
}

fn close_operator_surface(
    hide: impl FnOnce() -> Result<(), String>,
    quit_event_loop: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Err(hide_error) = hide() else {
        return Ok(());
    };
    match quit_event_loop() {
        Ok(()) => Err(format!(
            "{hide_error}; explicit event-loop termination was required"
        )),
        Err(quit_error) => Err(format!("{hide_error}; {quit_error}")),
    }
}

fn apply_operator_worker_completion(
    ui: &OperatorWindow,
    completion: &Receiver<Result<(), String>>,
    diagnostic: &Mutex<String>,
    close_operator_surface: impl FnOnce() -> Result<(), String>,
) -> Option<Result<(), String>> {
    let message = match completion.try_recv() {
        Ok(Ok(())) => "Runtime worker stopped".to_owned(),
        Ok(Err(error)) => format!("Runtime worker stopped: {error}"),
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => {
            "Runtime worker ended without publishing its result".to_owned()
        }
    };
    let close_result = close_operator_surface();
    let message = close_result
        .as_ref()
        .err()
        .map_or(message.clone(), |error| format!("{message}; {error}"));
    set_diagnostic(diagnostic, message.clone());
    ui.set_diagnostic(message.into());
    Some(close_result)
}

fn wire_operator_action(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    command: OperatorCommand,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    ui.on_start_requested(move || send_operator_command(&sender, command, &diagnostic));
}

fn wire_operator_stop(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    ui.on_stop_requested(move || {
        send_operator_command(&sender, OperatorCommand::Stop, &diagnostic);
    });
}

fn wire_operator_return_to_start(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    ui.on_return_to_start_requested(move || {
        send_operator_command(&sender, OperatorCommand::ReturnToStart, &diagnostic);
    });
}

fn wire_operator_quit(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    stop: &Arc<AtomicBool>,
    diagnostic: &Arc<Mutex<String>>,
) {
    let sender_for_button = sender.clone();
    let stop_for_button = Arc::clone(stop);
    let diagnostic_for_button = Arc::clone(diagnostic);
    let weak = ui.as_weak();
    ui.on_quit_requested(move || {
        request_operator_shutdown(&stop_for_button, &sender_for_button, &diagnostic_for_button);
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let sender = sender.clone();
    let stop = Arc::clone(stop);
    let diagnostic = Arc::clone(diagnostic);
    ui.window().on_close_requested(move || {
        request_operator_shutdown(&stop, &sender, &diagnostic);
        CloseRequestResponse::HideWindow
    });
}

fn request_operator_shutdown(
    stop: &AtomicBool,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Mutex<String>,
) {
    stop.store(true, Ordering::Release);
    send_operator_command(sender, OperatorCommand::Quit, diagnostic);
}

fn send_operator_command(
    sender: &SyncSender<OperatorCommand>,
    command: OperatorCommand,
    diagnostic: &Mutex<String>,
) {
    if let Err(error) = sender.try_send(command) {
        set_diagnostic(diagnostic, format!("operator command refused: {error}"));
    }
}

fn apply_static_facts(ui: &OperatorWindow, facts: &RuntimeFacts) {
    ui.set_project_name(facts.project_name.clone().into());
    ui.set_project_identity(facts.project_id.clone().into());
    ui.set_project_revision(facts.project_revision.to_string().into());
    ui.set_deployment_generation(facts.deployment_generation.to_string().into());
    ui.set_bundle_identity(facts.bundle_identity.clone().into());
    ui.set_recovery_state(
        facts
            .recovered_from
            .as_ref()
            .map_or_else(
                || "activated bundle accepted without fallback".to_owned(),
                |identity| format!("recovered from rejected bundle {identity}"),
            )
            .into(),
    );
}

#[derive(Debug, Eq, PartialEq)]
struct OperatorObservationPresentation {
    transport_state: &'static str,
    transport_time: String,
    transport_frame: String,
    stream_state: String,
    callback_count: String,
    stream_failures: String,
    native_xruns: String,
    panel_state: &'static str,
    diagnostic: String,
}

#[derive(Debug, PartialEq)]
struct OperatorMeterPresentation {
    left_level: f32,
    left_reading: String,
    right_level: f32,
    right_reading: String,
    state: &'static str,
    telemetry: String,
}

fn operator_meter_presentation(
    snapshot: Option<StereoSamplePeakBallisticsSnapshot>,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
) -> OperatorMeterPresentation {
    let (left_level, left_reading, right_level, right_reading, frame) = snapshot.map_or_else(
        || (0.0, "— dBFS".to_owned(), 0.0, "— dBFS".to_owned(), None),
        |snapshot| {
            let (left_level, left_reading) = operator_meter_channel_presentation(snapshot.left());
            let (right_level, right_reading) =
                operator_meter_channel_presentation(snapshot.right());
            (
                left_level,
                left_reading,
                right_level,
                right_reading,
                Some(snapshot.processed_frame_end()),
            )
        },
    );
    let state = match (snapshot.is_some(), sender_disconnected) {
        (false, false) => "Waiting for final-output evidence",
        (false, true) => "Meter producer ended before a snapshot",
        (true, false) => "Live final-output sample peak",
        (true, true) => "Final meter snapshot · producer ended",
    };
    let frame = frame.map_or_else(|| "unavailable".to_owned(), |frame| frame.to_string());
    OperatorMeterPresentation {
        left_level,
        left_reading,
        right_level,
        right_reading,
        state,
        telemetry: format!(
            "meter frame {frame} · {coalesced_snapshot_count} coalesced · {dropped_snapshot_count} dropped"
        ),
    }
}

fn operator_meter_channel_presentation(channel: ChannelSamplePeakBallistics) -> (f32, String) {
    operator_meter_level_presentation(channel.measurement_available(), channel.level_dbfs())
}

fn operator_meter_level_presentation(measurement_available: bool, dbfs: f32) -> (f32, String) {
    if !measurement_available || !dbfs.is_finite() {
        return (0.0, "— dBFS".to_owned());
    }
    let level = ((dbfs - OUTPUT_METER_FLOOR_DBFS) / -OUTPUT_METER_FLOOR_DBFS).clamp(0.0, 1.0);
    let reading = if dbfs <= OUTPUT_METER_FLOOR_DBFS {
        format!("≤ {OUTPUT_METER_FLOOR_DBFS:.1} dBFS")
    } else {
        format!("{dbfs:.1} dBFS")
    };
    (level, reading)
}

fn operator_observation_presentation(
    transport_state: PlaybackRecoveryTransportState,
    transport_frame: usize,
    stream_state: OutputStreamState,
    counters: OutputStreamSnapshot,
    panel_connected: bool,
    diagnostic: String,
) -> OperatorObservationPresentation {
    OperatorObservationPresentation {
        transport_state: match transport_state {
            PlaybackRecoveryTransportState::Stopped => "Stopped",
            PlaybackRecoveryTransportState::Rolling => "Rolling",
        },
        transport_time: format_transport_time(transport_frame as u64),
        transport_frame: transport_frame.to_string(),
        stream_state: format!("{stream_state:?}"),
        callback_count: counters.callback_count.to_string(),
        stream_failures: counters.stream_error_count.to_string(),
        native_xruns: counters
            .native_xruns
            .and_then(|snapshot| snapshot.recovery_count)
            .map_or_else(|| "unavailable".to_owned(), |count| count.to_string()),
        panel_state: if panel_connected {
            "Connected"
        } else {
            "Disconnected"
        },
        diagnostic: if stream_state == OutputStreamState::Failed {
            "native audio ownership ended unexpectedly".to_owned()
        } else {
            diagnostic
        },
    }
}

fn format_transport_time(frame: u64) -> String {
    let sample_rate_hz = u64::from(SAMPLE_RATE_HZ);
    let whole_seconds = frame / sample_rate_hz;
    let milliseconds = (frame % sample_rate_hz) * 1_000 / sample_rate_hz;
    let hours = whole_seconds / 3_600;
    let minutes = whole_seconds / 60 % 60;
    let seconds = whole_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

fn apply_observations(
    ui: &OperatorWindow,
    observers: &RuntimeObservers,
    diagnostic: &Mutex<String>,
) {
    let recovery = observers.recovery.snapshot();
    let stream_state = observers.output_status.state();
    let counters = observers.output_counters.snapshot();
    if stream_state == OutputStreamState::Failed {
        set_diagnostic(
            diagnostic,
            "native audio ownership ended unexpectedly".to_owned(),
        );
    }
    let current_diagnostic = diagnostic.lock().map_or_else(
        |_| "diagnostic state unavailable".to_owned(),
        |value| value.clone(),
    );
    let presentation = operator_observation_presentation(
        recovery.state(),
        recovery.position_frame(),
        stream_state,
        counters,
        observers.panel_connected.load(Ordering::Acquire),
        current_diagnostic,
    );
    ui.set_transport_state(presentation.transport_state.into());
    ui.set_transport_time(presentation.transport_time.into());
    ui.set_transport_frame(presentation.transport_frame.into());
    ui.set_stream_state(presentation.stream_state.into());
    ui.set_callback_count(presentation.callback_count.into());
    ui.set_stream_failures(presentation.stream_failures.into());
    ui.set_native_xruns(presentation.native_xruns.into());
    ui.set_panel_state(presentation.panel_state.into());
    ui.set_diagnostic(presentation.diagnostic.into());
}

fn apply_operator_meter_observation(ui: &OperatorWindow, observer: &mut OperatorMeterObserver) {
    let presentation = observer.observe();
    ui.set_left_output_level(presentation.left_level);
    ui.set_left_output_reading(presentation.left_reading.into());
    ui.set_right_output_level(presentation.right_level);
    ui.set_right_output_reading(presentation.right_reading.into());
    ui.set_output_meter_state(presentation.state.into());
    ui.set_output_meter_telemetry(presentation.telemetry.into());
}

/// Enforces the process startup boundary: artifact acceptance and semantic
/// preparation finish before the native output owner can be constructed.
fn prepare_output_and_host<P, O, S>(
    prepare: impl FnOnce() -> Result<P, String>,
    open_output: impl FnOnce() -> Result<O, String>,
    host: impl FnOnce(O, P) -> Result<S, String>,
) -> Result<S, String> {
    let prepared = prepare()?;
    let output = open_output()?;
    host(output, prepared)
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use core::fmt::Write;
        write!(&mut rendered, ": {cause}").expect("writing to a string cannot fail");
        source = cause.source();
    }
    rendered
}

fn report_to(served: &ServedPanel, diagnostic: &dyn Fn(String)) {
    diagnostic(match &served.outcome {
        Ok(closure) => format!(
            "panel {} (#{}) ended: {closure:?}",
            served.peer, served.session_id
        ),
        Err(error) => format!(
            "panel {} (#{}) failed: {error}",
            served.peer, served.session_id
        ),
    });
    for notice in &served.notices {
        diagnostic(format!("panel notice: {notice:?}"));
    }
}

fn limits() -> Result<ProjectPlaybackLimits, String> {
    let audio = DecodeLimits::new(
        MAX_AUDIO_FILE_BYTES,
        MAX_AUDIO_FRAMES,
        MAX_AUDIO_DECODED_BYTES,
    )
    .map_err(|error| format!("decode policy is invalid: {error}"))?;
    let max_frames_per_asset = usize::try_from(MAX_AUDIO_FRAMES)
        .map_err(|_| "decode policy exceeds this platform's addressable frames".to_owned())?;
    Ok(ProjectPlaybackLimits {
        max_total_resource_bytes: MAX_PROJECT_RESOURCE_BYTES,
        audio,
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: LoaderLimits::VERSION_1.max_clip_documents,
                max_frames_per_asset,
                max_decoded_sample_bytes: MAX_PROJECT_DECODED_BYTES,
            },
        },
    })
}

/// Ends the process when standard input closes or asks it to.
///
/// `std` has no signal handling, so the process is stopped the way a headless
/// process is driven: through the stream it was started with. The serving loop
/// observes this flag between reads, so it takes effect while a panel is
/// connected rather than only between connections.
fn watch_for_quit(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut input = std::io::stdin().lock();
        loop {
            line.clear();
            let Ok(read) = input.read_line(&mut line) else {
                break;
            };
            if read == 0 || line.trim() == "quit" {
                break;
            }
        }
        stop.store(true, Ordering::Release);
    });
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn hex_digest(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[test]
    fn rejected_bundle_preparation_cannot_open_output() {
        let events = RefCell::new(Vec::new());

        let result = prepare_output_and_host(
            || {
                events.borrow_mut().push("prepare");
                Err::<(), _>("bundle rejected".to_owned())
            },
            || {
                events.borrow_mut().push("output");
                Ok(())
            },
            |(), ()| {
                events.borrow_mut().push("host");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err(), "bundle rejected");
        assert_eq!(*events.borrow(), ["prepare"]);
    }

    #[test]
    fn successful_startup_prepares_then_opens_output_then_hosts() {
        let events = RefCell::new(Vec::new());

        prepare_output_and_host(
            || {
                events.borrow_mut().push("prepare");
                Ok(())
            },
            || {
                events.borrow_mut().push("output");
                Ok(())
            },
            |(), ()| {
                events.borrow_mut().push("host");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["prepare", "output", "host"]);
    }

    #[test]
    fn startup_diagnostics_retain_the_bundle_rejection_cause() {
        let output_opened = Cell::new(false);
        let missing = std::env::temp_dir().join(format!(
            "hyper-runtime-missing-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let diagnostic = prepare_output_and_host(
            || {
                resolve_runtime_deployment(
                    missing,
                    env!("CARGO_PKG_VERSION"),
                    MAX_RUNTIME_BUNDLE_BYTES,
                    limits().unwrap(),
                )
                .map_err(|error| format_error_chain(&error))
            },
            || {
                output_opened.set(true);
                Ok(())
            },
            |(), _| Ok(()),
        )
        .unwrap_err();

        assert!(diagnostic.starts_with("inspect deployment target: "));
        assert!(
            diagnostic.len() > "inspect deployment target: ".len(),
            "the operating-system cause remains visible"
        );
        assert!(!output_opened.get());
    }

    #[test]
    fn an_unavailable_panel_listener_keeps_the_runtime_owner_until_quit() {
        let occupied = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = occupied.local_addr().unwrap();
        let arguments = RuntimeArguments::parse([
            "--panel-listen".to_owned(),
            address.to_string(),
            "deployment".to_owned(),
        ])
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Quit).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap();

        assert!(
            diagnostics
                .borrow()
                .iter()
                .any(|message| message.contains("cannot listen")),
            "the occupied endpoint remains visible: {:?}",
            diagnostics.borrow()
        );
        assert!(
            diagnostics
                .borrow()
                .iter()
                .any(|message| message.contains("project stays hosted"))
        );
        assert_eq!(owner.disconnects, 1, "the sole owner gets one release turn");
        assert!(stop.load(Ordering::Acquire));
        drop(occupied);
    }

    #[test]
    fn native_output_failure_ends_the_same_production_host_loop() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Failed),
            &stop,
            None,
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(error, "native audio ownership ended unexpectedly");
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(
            owner.disconnects, 0,
            "there was no panel service to release"
        );
    }

    #[derive(Default)]
    struct OperatorControl {
        commands: Vec<TransportCommand>,
        reject: bool,
        disconnects: usize,
    }

    impl RuntimeOperatorControl for OperatorControl {
        fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.commands.push(command);
                Ok(())
            }
        }
    }

    impl PanelHost for OperatorControl {
        type Error = core::convert::Infallible;

        fn mixer_channel_count(&self) -> usize {
            0
        }

        fn connect_panel(
            &mut self,
            _descriptor: PanelDescriptor,
            _bindings: &[PanelBinding],
        ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
            panic!("the occupied-port test cannot accept a panel")
        }

        fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
            self.disconnects += 1;
            Vec::new()
        }

        fn panel_connected(&self) -> bool {
            false
        }

        fn dispatch_panel(
            &mut self,
            _message: Message,
        ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
            panic!("the occupied-port test cannot dispatch a panel")
        }

        fn panel_display(&mut self, _out: &mut Vec<PanelDisplay>) {}
    }

    struct OutputObservation(OutputStreamState);

    impl RuntimeOutputObservation for OutputObservation {
        fn output_state(&self) -> OutputStreamState {
            self.0
        }
    }

    #[test]
    fn operator_commands_reach_the_sole_transport_owner_in_order() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Start).unwrap();
        sender.send(OperatorCommand::Stop).unwrap();
        sender.send(OperatorCommand::ReturnToStart).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert_eq!(
            owner.commands,
            [
                TransportCommand::Start,
                TransportCommand::Stop,
                TransportCommand::Locate {
                    frame: 0,
                    state: LocateState::Preserve,
                },
            ]
        );
        assert_eq!(
            diagnostics.into_inner(),
            [
                "transport start queued",
                "transport stop queued",
                "transport return to start queued",
            ]
        );
        assert!(!stop.load(Ordering::Acquire));
    }

    #[test]
    fn a_refused_operator_command_is_visible_and_a_lost_ui_stops_the_worker() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Start).unwrap();
        drop(sender);
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            reject: true,
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert!(owner.commands.is_empty());
        assert_eq!(
            diagnostics.into_inner(),
            ["transport start refused: owner busy"]
        );
        assert!(
            stop.load(Ordering::Acquire),
            "losing the UI command owner cannot leave a hidden Runtime behind"
        );
    }

    fn assert_generated_operator_meter_contract(ui: &OperatorWindow) {
        let (meter_sender, meter_receiver) = hyper_mixer_runtime::output_meter_channel();
        let meter_config = SamplePeakBallisticsConfig::try_new(
            SAMPLE_RATE_HZ,
            OUTPUT_METER_FLOOR_DBFS,
            OUTPUT_METER_FALLOFF_DB_PER_SECOND,
            0,
        )
        .unwrap();
        let mut meter_observer = OperatorMeterObserver::new(meter_receiver, meter_config);
        apply_operator_meter_observation(ui, &mut meter_observer);

        assert!(ui.get_left_output_level().abs() <= f32::EPSILON);
        assert_eq!(ui.get_left_output_reading().as_str(), "— dBFS");
        assert!(ui.get_right_output_level().abs() <= f32::EPSILON);
        assert_eq!(ui.get_right_output_reading().as_str(), "— dBFS");
        assert_eq!(
            ui.get_output_meter_state().as_str(),
            "Waiting for final-output evidence"
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame unavailable · 0 coalesced · 0 dropped"
        );

        drop(meter_sender);
        apply_operator_meter_observation(ui, &mut meter_observer);
        assert_eq!(
            ui.get_output_meter_state().as_str(),
            "Meter producer ended before a snapshot"
        );
    }

    #[test]
    fn operator_ui_wires_commands_and_authoritative_properties() {
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new("authoritative diagnostic".to_owned()));
        let stop = Arc::new(AtomicBool::new(false));
        wire_operator_action(&ui, &sender, &diagnostic, OperatorCommand::Start);
        wire_operator_stop(&ui, &sender, &diagnostic);
        wire_operator_return_to_start(&ui, &sender, &diagnostic);
        wire_operator_quit(&ui, &sender, &stop, &diagnostic);

        let facts = RuntimeFacts {
            bundle_identity: "f".repeat(64),
            deployment_generation: u64::MAX,
            recovered_from: Some("e".repeat(64)),
            project_name: "Night Drive".to_owned(),
            project_id: "project-night-drive".to_owned(),
            project_revision: u64::MAX - 1,
        };
        let observers = RuntimeObservers {
            facts: facts.clone(),
            output_status: OutputStreamStatus::default(),
            output_counters: OutputStreamCounters::default(),
            recovery: PlaybackRecoveryCheckpoint::default(),
            panel_connected: Arc::new(AtomicBool::new(false)),
        };
        apply_static_facts(&ui, &facts);
        apply_observations(&ui, &observers, &diagnostic);
        assert_generated_operator_meter_contract(&ui);

        ui.invoke_start_requested();
        ui.invoke_stop_requested();
        ui.invoke_return_to_start_requested();
        ui.invoke_quit_requested();

        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Stop));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::ReturnToStart));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Quit));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(ui.get_project_name().as_str(), "Night Drive");
        assert_eq!(ui.get_project_identity().as_str(), "project-night-drive");
        assert_eq!(
            ui.get_project_revision().as_str(),
            (u64::MAX - 1).to_string()
        );
        assert_eq!(
            ui.get_deployment_generation().as_str(),
            u64::MAX.to_string()
        );
        assert_eq!(ui.get_bundle_identity().as_str(), "f".repeat(64));
        assert!(ui.get_recovery_state().contains(&"e".repeat(64)));
        assert_eq!(ui.get_transport_state().as_str(), "Stopped");
        assert_eq!(ui.get_transport_time().as_str(), "00:00:00.000");
        assert_eq!(ui.get_transport_frame().as_str(), "0");
        assert_eq!(ui.get_stream_state().as_str(), "Starting");
        assert_eq!(ui.get_callback_count().as_str(), "0");
        assert_eq!(ui.get_stream_failures().as_str(), "0");
        assert_eq!(ui.get_native_xruns().as_str(), "unavailable");
        assert_eq!(ui.get_panel_state().as_str(), "Disconnected");
        assert_eq!(ui.get_diagnostic().as_str(), "authoritative diagnostic");

        let (completion_sender, completion_receiver) = std::sync::mpsc::sync_channel(1);
        let surface_closed = Cell::new(false);
        assert_eq!(
            apply_operator_worker_completion(&ui, &completion_receiver, &diagnostic, || {
                surface_closed.set(true);
                Ok(())
            },),
            None
        );
        assert!(!surface_closed.get());
        completion_sender
            .send(Err("native audio ownership ended unexpectedly".to_owned()))
            .unwrap();
        assert_eq!(
            apply_operator_worker_completion(&ui, &completion_receiver, &diagnostic, || {
                surface_closed.set(true);
                Ok(())
            },),
            Some(Ok(()))
        );
        assert!(surface_closed.get());
        assert_eq!(
            ui.get_diagnostic().as_str(),
            "Runtime worker stopped: native audio ownership ended unexpectedly"
        );

        let (lost_sender, lost_receiver) = std::sync::mpsc::sync_channel(1);
        drop(lost_sender);
        surface_closed.set(false);
        assert_eq!(
            apply_operator_worker_completion(&ui, &lost_receiver, &diagnostic, || {
                surface_closed.set(true);
                Err("adapter refused to hide".to_owned())
            }),
            Some(Err("adapter refused to hide".to_owned()))
        );
        assert!(surface_closed.get());
        assert_eq!(
            ui.get_diagnostic().as_str(),
            "Runtime worker ended without publishing its result; adapter refused to hide"
        );
    }

    #[test]
    fn operator_shutdown_waits_for_the_worker_it_signals() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = Arc::clone(&joined);
        let worker = std::thread::spawn(move || {
            assert_eq!(receiver.recv(), Ok(OperatorCommand::Quit));
            worker_joined.store(true, Ordering::Release);
            Ok(())
        });

        finish_operator_worker(Ok(()), &stop, &sender, worker).unwrap();

        assert!(stop.load(Ordering::Acquire));
        assert!(
            joined.load(Ordering::Acquire),
            "the join observed worker exit"
        );
    }

    #[test]
    fn operator_terminal_results_keep_worker_failure_primary() {
        assert_eq!(reconcile_operator_results(Ok(()), Ok(())), Ok(()));
        assert_eq!(
            reconcile_operator_results(Err("UI failed".to_owned()), Ok(())),
            Err("UI failed".to_owned())
        );
        assert_eq!(
            reconcile_operator_results(Ok(()), Err("audio owner failed".to_owned())),
            Err("audio owner failed".to_owned())
        );
        assert_eq!(
            reconcile_operator_results(
                Err("UI failed".to_owned()),
                Err("audio owner failed".to_owned())
            ),
            Err("audio owner failed; operator UI shutdown also failed: UI failed".to_owned())
        );

        let close_error = Mutex::new(Some("surface adapter refused hide".to_owned()));
        assert_eq!(
            reconcile_operator_ui_close(Ok(()), &close_error),
            Err("surface adapter refused hide".to_owned())
        );
        let close_error = Mutex::new(Some("surface adapter refused hide".to_owned()));
        assert_eq!(
            reconcile_operator_ui_close(Err("event loop failed".to_owned()), &close_error),
            Err(
                "event loop failed; operator surface shutdown also failed: surface adapter refused hide"
                    .to_owned()
            )
        );

        let stop = AtomicBool::new(false);
        let (sender, _receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let worker =
            std::thread::spawn(|| -> Result<(), String> { panic!("worker panic evidence") });
        assert_eq!(
            finish_operator_worker(Err("UI failed".to_owned()), &stop, &sender, worker),
            Err("Runtime worker panicked during shutdown".to_owned())
        );
    }

    #[test]
    fn operator_surface_close_uses_checked_event_loop_fallback() {
        let quit_called = Cell::new(false);
        assert_eq!(
            close_operator_surface(
                || Err("surface adapter refused hide".to_owned()),
                || {
                    quit_called.set(true);
                    Ok(())
                }
            ),
            Err(
                "surface adapter refused hide; explicit event-loop termination was required"
                    .to_owned()
            )
        );
        assert!(quit_called.get());

        assert_eq!(
            close_operator_surface(
                || Err("surface adapter refused hide".to_owned()),
                || Err("event loop refused quit".to_owned())
            ),
            Err("surface adapter refused hide; event loop refused quit".to_owned())
        );

        quit_called.set(false);
        assert_eq!(
            close_operator_surface(
                || Ok(()),
                || {
                    quit_called.set(true);
                    Ok(())
                }
            ),
            Ok(())
        );
        assert!(!quit_called.get());
    }

    #[test]
    fn failed_operator_startup_joins_the_worker_and_returns_its_error() {
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = Arc::clone(&joined);
        let worker = std::thread::spawn(move || {
            worker_joined.store(true, Ordering::Release);
            Err("verified deployment was rejected".to_owned())
        });

        let error =
            finish_failed_operator_startup("startup evidence channel closed".to_owned(), worker)
                .unwrap_err();

        assert_eq!(error, "verified deployment was rejected");
        assert!(joined.load(Ordering::Acquire));

        let panicked =
            std::thread::spawn(|| -> Result<(), String> { panic!("startup panic evidence") });
        assert_eq!(
            finish_failed_operator_startup("readiness disconnected".to_owned(), panicked),
            Err("Runtime worker panicked during startup".to_owned())
        );
    }

    #[test]
    fn a_full_operator_queue_refuses_without_replacing_the_queued_command() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send(OperatorCommand::Start).unwrap();
        let diagnostic = Mutex::new("healthy".to_owned());

        send_operator_command(&sender, OperatorCommand::ReturnToStart, &diagnostic);

        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            diagnostic
                .lock()
                .unwrap()
                .starts_with("operator command refused:")
        );
    }

    #[test]
    fn operator_observation_formatting_preserves_full_width_evidence() {
        let presentation = operator_observation_presentation(
            PlaybackRecoveryTransportState::Rolling,
            usize::MAX,
            OutputStreamState::Failed,
            OutputStreamSnapshot {
                callback_count: u64::MAX,
                stream_error_count: u64::MAX - 1,
                native_xruns: Some(hyper_desktop_audio::NativeXrunSnapshot {
                    recovery_count: Some(u64::MAX - 2),
                    ..hyper_desktop_audio::NativeXrunSnapshot::default()
                }),
                ..OutputStreamSnapshot::default()
            },
            true,
            "stale diagnostic".to_owned(),
        );

        assert_eq!(presentation.transport_state, "Rolling");
        assert_eq!(
            presentation.transport_time,
            format_transport_time(usize::MAX as u64)
        );
        assert_eq!(presentation.transport_frame, usize::MAX.to_string());
        assert_eq!(presentation.stream_state, "Failed");
        assert_eq!(presentation.callback_count, u64::MAX.to_string());
        assert_eq!(presentation.stream_failures, (u64::MAX - 1).to_string());
        assert_eq!(presentation.native_xruns, (u64::MAX - 2).to_string());
        assert_eq!(presentation.panel_state, "Connected");
        assert_eq!(
            presentation.diagnostic,
            "native audio ownership ended unexpectedly"
        );
    }

    #[test]
    fn runtime_transport_clock_truncates_frames_without_wrapping_hours() {
        assert_eq!(format_transport_time(0), "00:00:00.000");
        assert_eq!(format_transport_time(47_999), "00:00:00.999");
        assert_eq!(format_transport_time(72_000), "00:00:01.500");
        assert_eq!(
            format_transport_time(48_000 * (3 * 3_600 + 2 * 60 + 1) + 24_000),
            "03:02:01.500"
        );
        assert_eq!(format_transport_time(u64::MAX), "106751991167:18:02.325");
    }

    #[test]
    fn runtime_output_meter_presentation_preserves_floor_and_overload_evidence() {
        assert_eq!(
            operator_meter_level_presentation(false, -20.0),
            (0.0, "— dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, f32::NAN),
            (0.0, "— dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, OUTPUT_METER_FLOOR_DBFS),
            (0.0, "≤ -90.0 dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, -45.0),
            (0.5, "-45.0 dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, 3.0),
            (1.0, "3.0 dBFS".to_owned())
        );

        let unavailable = operator_meter_presentation(None, u64::MAX, u64::MAX - 1, true);
        assert_eq!(unavailable.state, "Meter producer ended before a snapshot");
        assert_eq!(
            unavailable.telemetry,
            format!(
                "meter frame unavailable · {} coalesced · {} dropped",
                u64::MAX,
                u64::MAX - 1
            )
        );
    }
}
