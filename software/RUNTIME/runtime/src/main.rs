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
#[cfg(test)]
use hyper_control_runtime::PanelControl;
use hyper_control_runtime::{
    PanelBinding, PanelConnectError, PanelConnection, PanelDispatchError, PanelDispatchOutcome,
    PanelDisplay, PanelReleaseFailure,
};
use hyper_desktop_audio::{
    DesktopControlSinkError, DesktopOutputConfig, DesktopPlaybackSession, OutputStreamCounters,
    OutputStreamState, OutputStreamStatus,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{
    PlaybackRecoveryCheckpoint, PlaybackRecoveryTransportState, TransportCommand,
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

struct StartedRuntime {
    host: SessionHost,
    observers: RuntimeObservers,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorCommand {
    Start,
    Stop,
    Quit,
}

const OPERATOR_COMMAND_CAPACITY: usize = 8;

trait RuntimeOperatorControl {
    fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String>;
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

fn host_runtime(
    arguments: &RuntimeArguments,
    host: &mut SessionHost,
    output_status: &OutputStreamStatus,
    stop: &AtomicBool,
    commands: Option<&Receiver<OperatorCommand>>,
    diagnostic: &dyn Fn(String),
) -> Result<(), String> {
    let mut owner_turn = |host: &mut SessionHost| {
        if let Some(commands) = commands {
            drain_operator_commands(host, commands, stop, diagnostic);
        }
        if output_status.state() == OutputStreamState::Failed {
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

    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot poll {address}: {error}"))?;
    diagnostic(format!(
        "panel link listening on {}",
        listener.local_addr().unwrap_or(address)
    ));

    let config = LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS)
        .map_err(|error| format!("panel link configuration is invalid: {error:?}"))?;
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
        diagnostic(format!("panel service ended: {error}"));
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
    finish_runtime_output(output_status, diagnostic)
}

fn finish_runtime_output(
    output_status: &OutputStreamStatus,
    diagnostic: &dyn Fn(String),
) -> Result<(), String> {
    if output_status.state() == OutputStreamState::Failed {
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
            OperatorCommand::Quit => stop.store(true, Ordering::Release),
        }
    }
}

fn run_operator_ui(arguments: RuntimeArguments) -> Result<(), String> {
    let ui =
        OperatorWindow::new().map_err(|error| format!("operator UI could not open: {error}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let (commands_tx, commands_rx) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let diagnostic = Arc::new(Mutex::new(
        "starting verified Runtime deployment".to_owned(),
    ));
    let worker_diagnostic = Arc::clone(&diagnostic);
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let mut runtime = match start_runtime(&arguments) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = ready_tx.send(Err(error.clone()));
                return Err(error);
            }
        };
        announce_runtime(&arguments, &runtime);
        let _ = ready_tx.send(Ok(runtime.observers.clone()));
        host_runtime(
            &arguments,
            &mut runtime.host,
            &runtime.observers.output_status,
            &worker_stop,
            Some(&commands_rx),
            &|message| set_diagnostic(&worker_diagnostic, message),
        )
    });
    let observers = ready_rx
        .recv()
        .map_err(|_| "Runtime worker ended before publishing startup evidence".to_owned())??;

    apply_static_facts(&ui, &observers.facts);
    wire_operator_action(&ui, &commands_tx, &diagnostic, OperatorCommand::Start);
    wire_operator_stop(&ui, &commands_tx, &diagnostic);
    wire_operator_quit(&ui, &commands_tx, &stop, &diagnostic);

    let weak = ui.as_weak();
    let timer_observers = observers.clone();
    let timer_diagnostic = Arc::clone(&diagnostic);
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        if let Some(ui) = weak.upgrade() {
            apply_observations(&ui, &timer_observers, &timer_diagnostic);
        }
    });
    apply_observations(&ui, &observers, &diagnostic);

    let ui_result = ui
        .run()
        .map_err(|error| format!("operator UI failed: {error}"));
    stop.store(true, Ordering::Release);
    let _ = commands_tx.try_send(OperatorCommand::Quit);
    drop(timer);
    let worker_result = worker
        .join()
        .map_err(|_| "Runtime worker panicked during shutdown".to_owned())?;
    ui_result?;
    worker_result
}

fn set_diagnostic(diagnostic: &Mutex<String>, message: String) {
    if let Ok(mut current) = diagnostic.lock() {
        *current = message;
    }
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
        stop_for_button.store(true, Ordering::Release);
        send_operator_command(
            &sender_for_button,
            OperatorCommand::Quit,
            &diagnostic_for_button,
        );
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let sender = sender.clone();
    let stop = Arc::clone(stop);
    let diagnostic = Arc::clone(diagnostic);
    ui.window().on_close_requested(move || {
        stop.store(true, Ordering::Release);
        send_operator_command(&sender, OperatorCommand::Quit, &diagnostic);
        CloseRequestResponse::HideWindow
    });
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

fn apply_observations(
    ui: &OperatorWindow,
    observers: &RuntimeObservers,
    diagnostic: &Mutex<String>,
) {
    let recovery = observers.recovery.snapshot();
    ui.set_transport_state(
        match recovery.state() {
            PlaybackRecoveryTransportState::Stopped => "Stopped",
            PlaybackRecoveryTransportState::Rolling => "Rolling",
        }
        .into(),
    );
    ui.set_transport_frame(recovery.position_frame().to_string().into());
    ui.set_stream_state(format!("{:?}", observers.output_status.state()).into());
    let counters = observers.output_counters.snapshot();
    ui.set_callback_count(counters.callback_count.to_string().into());
    ui.set_stream_failures(counters.stream_error_count.to_string().into());
    ui.set_native_xruns(
        counters
            .native_xruns
            .and_then(|snapshot| snapshot.recovery_count)
            .map_or_else(|| "unavailable".to_owned(), |count| count.to_string())
            .into(),
    );
    ui.set_panel_state(
        if observers.panel_connected.load(Ordering::Acquire) {
            "Connected"
        } else {
            "Disconnected"
        }
        .into(),
    );
    if observers.output_status.state() == OutputStreamState::Failed {
        set_diagnostic(
            diagnostic,
            "native audio ownership ended unexpectedly".to_owned(),
        );
    }
    if let Ok(current) = diagnostic.lock() {
        ui.set_diagnostic(current.clone().into());
    }
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

/// Receives the process-level events that follow the panel service ending.
#[cfg(test)]
trait PanelServiceReporter<E> {
    fn service_ended(&mut self, error: &E);
    fn release_failed(&mut self, control: PanelControl);
    fn project_still_hosted(&mut self);
}

/// Releases the listener and applies the process policy after panel service.
///
/// The listener is owned so it is closed before any failure is reported: a
/// panel must never connect to a port the process no longer services. A service
/// failure gets one process-owner release attempt, then project playback stays
/// alive until the ordinary quit flag is observed.
#[cfg(test)]
fn finish_panel_service<L, E, D, R, W>(
    listener: L,
    service: Result<(), E>,
    stop: &AtomicBool,
    mut disconnect: D,
    reporter: &mut R,
    mut wait: W,
) where
    D: FnMut() -> Vec<PanelControl>,
    R: PanelServiceReporter<E>,
    W: FnMut(),
{
    drop(listener);
    if let Err(error) = service {
        reporter.service_ended(&error);
        for control in disconnect() {
            reporter.release_failed(control);
        }
        reporter.project_still_hosted();
        while !stop.load(Ordering::Acquire) {
            wait();
        }
    }
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

    struct DropProbe<'a>(&'a Cell<bool>);

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Reported {
        ServiceEnded,
        ReleaseFailed(PanelControl),
        ProjectStillHosted,
    }

    #[derive(Default)]
    struct Reporter {
        events: Vec<Reported>,
    }

    impl PanelServiceReporter<()> for Reporter {
        fn service_ended(&mut self, _error: &()) {
            self.events.push(Reported::ServiceEnded);
        }

        fn release_failed(&mut self, control: PanelControl) {
            self.events.push(Reported::ReleaseFailed(control));
        }

        fn project_still_hosted(&mut self) {
            self.events.push(Reported::ProjectStillHosted);
        }
    }

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
    fn a_failed_panel_service_releases_the_port_retries_controls_and_keeps_hosting() {
        let dropped = Cell::new(false);
        let disconnects = Cell::new(0);
        let waits = Cell::new(0);
        let stop = AtomicBool::new(false);
        let mut reporter = Reporter::default();

        finish_panel_service(
            DropProbe(&dropped),
            Err(()),
            &stop,
            || {
                assert!(dropped.get(), "the unattended port is closed first");
                disconnects.set(disconnects.get() + 1);
                vec![PanelControl::Button { index: 2 }]
            },
            &mut reporter,
            || {
                waits.set(waits.get() + 1);
                stop.store(true, Ordering::Release);
            },
        );

        assert!(dropped.get());
        assert_eq!(disconnects.get(), 1, "the process owner retries once");
        assert_eq!(waits.get(), 1, "the project remains hosted until quit");
        assert_eq!(
            reporter.events,
            [
                Reported::ServiceEnded,
                Reported::ReleaseFailed(PanelControl::Button { index: 2 }),
                Reported::ProjectStillHosted,
            ]
        );
    }

    #[test]
    fn a_clean_panel_service_end_only_releases_the_listener() {
        let dropped = Cell::new(false);
        let stop = AtomicBool::new(false);
        let mut reporter = Reporter::default();

        finish_panel_service(
            DropProbe(&dropped),
            Ok(()),
            &stop,
            || panic!("a clean service end has no stranded controls"),
            &mut reporter,
            || panic!("a clean service end does not enter failure waiting"),
        );

        assert!(dropped.get());
        assert!(reporter.events.is_empty());
    }

    #[derive(Default)]
    struct OperatorControl {
        commands: Vec<TransportCommand>,
        reject: bool,
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

    #[test]
    fn operator_commands_reach_the_sole_transport_owner_in_order() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Start).unwrap();
        sender.send(OperatorCommand::Stop).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert_eq!(
            owner.commands,
            [TransportCommand::Start, TransportCommand::Stop]
        );
        assert_eq!(
            diagnostics.into_inner(),
            ["transport start queued", "transport stop queued"]
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

    #[test]
    fn operator_buttons_publish_only_the_commands_the_worker_accepts() {
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_action(&ui, &sender, &diagnostic, OperatorCommand::Start);
        wire_operator_stop(&ui, &sender, &diagnostic);

        ui.invoke_start_requested();
        ui.invoke_stop_requested();

        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Stop));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    }
}
