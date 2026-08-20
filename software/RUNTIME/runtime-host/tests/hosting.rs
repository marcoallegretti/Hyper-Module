//! The shipped panel-serving code, driven against the real simulator over a
//! real socket, into a real control runtime.
//!
//! The driver below implements `Read`/`Write`, so `serve_panel` — the loop that
//! actually runs in production — is what these tests exercise, single-threaded
//! and on an injected clock. Nothing here reimplements the loop under test.

use std::cell::Cell;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingAvailability, ControlRuntime, DirectControlStateUpdate,
    PanelBinding, PanelButtonBehavior, PanelConnectError, PanelConnection, PanelDispatchError,
    PanelDispatchOutcome, PanelReleaseFailure, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget, ValueControlTarget, ValueDomain,
};
use hyper_mapping_compiler::CompiledControlMappingTable;
use hyper_panel_protocol::{FirmwareVersion, LinkConfig, Message, PanelDescriptor};
use hyper_panel_simulator::{PanelSimulator, PanelSimulatorConfig};
use hyper_runtime_host::{
    ArgumentError, DEFAULT_GAIN_STEP_MILLIONTHS, PANEL_READ_TIMEOUT, PanelClosure, PanelHost,
    PanelLink, PanelNotice, RuntimeArguments, ServeError, default_layout, prepare_panel_stream,
    refuse_waiting_panels, serve_panel, serve_panels,
};

const CHANNELS: usize = 3;
/// How far the injected clock advances per read. The link's timeout is 500 ms,
/// so a silent panel is noticed after fifty reads and no test waits on a wall
/// clock to get there.
const CLOCK_STEP_MS: u64 = 10;
/// The link's own heartbeat timeout, which bounds how long any socket-level
/// wait may be before it starts making the link's decisions for it.
const HEARTBEAT_TIMEOUT_MS: u32 = 500;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SinkError {
    Rejected,
    InvalidTarget,
}

#[derive(Debug, Default)]
struct Sink {
    commands: Vec<RuntimeControlCommand>,
    invalid_target: Option<RuntimeControlTarget>,
    reject_release: bool,
    /// Refuses this many releases and then accepts, so a retry that would have
    /// succeeded is distinguishable from one that never happens.
    refuse_releases_first: usize,
    reject_gain: bool,
}

impl RuntimeControlSink for Sink {
    type Error = SinkError;
    type DirectCommand = RuntimeControlCommand;
    type DirectError = SinkError;

    fn validate_target(&self, target: RuntimeControlTarget) -> Result<(), Self::Error> {
        if self.invalid_target == Some(target) {
            Err(SinkError::InvalidTarget)
        } else {
            Ok(())
        }
    }

    fn target_availability(&self, _target: RuntimeControlTarget) -> ControlBindingAvailability {
        ControlBindingAvailability::Live
    }

    fn boolean_state(&self, _target: BooleanControlTarget) -> Option<bool> {
        Some(false)
    }

    fn transport_rolling(&self) -> Option<bool> {
        Some(false)
    }

    fn value_state(&self, _target: ValueControlTarget) -> Option<i32> {
        Some(1_000_000)
    }

    fn value_domain(&self, target: ValueControlTarget) -> ValueDomain {
        match target {
            ValueControlTarget::ChannelPan { .. } | ValueControlTarget::SendPan { .. } => {
                ValueDomain::new(-1_000_000, 1_000_000)
            }
            _ => ValueDomain::new(0, 2_000_000),
        }
    }

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        let is_release = matches!(
            command,
            RuntimeControlCommand::ChannelMute { muted: false, .. }
        );
        if is_release && self.refuse_releases_first > 0 {
            self.refuse_releases_first -= 1;
            return Err(SinkError::Rejected);
        }
        if self.reject_gain && matches!(command, RuntimeControlCommand::ChannelGain { .. }) {
            return Err(SinkError::Rejected);
        }
        if self.reject_release && is_release {
            return Err(SinkError::Rejected);
        }
        self.commands.push(command);
        Ok(())
    }

    fn try_send_direct(
        &mut self,
        command: Self::DirectCommand,
    ) -> Result<DirectControlStateUpdate, Self::DirectError> {
        self.try_send(command)?;
        // Reports the state effect the real sink reports; a double that always
        // answers `Unchanged` cannot show a shadow following an edit made
        // anywhere but the panel.
        Ok(match command {
            RuntimeControlCommand::ChannelGain {
                channel_index,
                gain_millionths,
            } => DirectControlStateUpdate::Value {
                target: ValueControlTarget::ChannelGain { channel_index },
                value: i32::try_from(gain_millionths).unwrap(),
            },
            RuntimeControlCommand::ChannelMute {
                channel_index,
                muted,
            } => DirectControlStateUpdate::Boolean {
                target: BooleanControlTarget::ChannelMute { channel_index },
                value: muted,
            },
            _ => DirectControlStateUpdate::Unchanged,
        })
    }
}

struct Host {
    runtime: ControlRuntime<Sink>,
    channels: usize,
    /// Stands in for a configured layout, which ADR-125 anticipates and the
    /// default layout deliberately is not: a momentary button is the only way a
    /// panel ever holds a control.
    momentary_layout: bool,
    /// Models a teardown that could not release everything — the state
    /// `ServeError::ControlsStillHeld` exists for.
    controls_stuck: bool,
    disconnects: usize,
    /// Answers with a display even when no panel is connected. The link must
    /// not rely on the owner's discipline for whether it is safe to talk.
    displays_regardless: bool,
}

impl Host {
    fn new(channels: usize) -> Self {
        Self {
            runtime: ControlRuntime::try_new(CompiledControlMappingTable::empty(), Sink::default())
                .expect("an empty table needs no capacity"),
            channels,
            momentary_layout: false,
            controls_stuck: false,
            disconnects: 0,
            displays_regardless: false,
        }
    }

    fn commands(&self) -> &[RuntimeControlCommand] {
        &self.runtime.sink().commands
    }
}

impl PanelHost for Host {
    type Error = SinkError;

    fn mixer_channel_count(&self) -> usize {
        self.channels
    }

    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
        if self.momentary_layout {
            return self.runtime.connect_panel(
                descriptor,
                &[PanelBinding::Button {
                    index: 0,
                    target: BooleanControlTarget::ChannelMute { channel_index: 0 },
                    behavior: PanelButtonBehavior::Momentary,
                }],
            );
        }
        self.runtime.connect_panel(descriptor, bindings)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        self.disconnects += 1;
        self.runtime.disconnect_panel()
    }

    fn panel_connected(&self) -> bool {
        self.controls_stuck || self.runtime.panel_connected()
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        self.runtime.dispatch_panel(message)
    }

    fn panel_display(&mut self, out: &mut Vec<hyper_control_runtime::PanelDisplay>) {
        self.runtime.panel_display(out);
        if self.displays_regardless && out.is_empty() {
            out.push(hyper_control_runtime::PanelDisplay::Led {
                index: 0,
                enabled: true,
            });
        }
    }
}

fn descriptor(encoders: u8, buttons: u8) -> PanelDescriptor {
    PanelDescriptor {
        firmware: FirmwareVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 7,
        },
        boot_id: 0x00AB_CDEF,
        encoders,
        buttons,
        leds: 0,
        rings: 0,
        ring_segments: 0,
    }
}

fn loopback() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    client.set_nonblocking(true).unwrap();
    server.set_nonblocking(true).unwrap();
    (client, server)
}

/// Something the panel does, scheduled against the serving loop's read count so
/// a single thread can drive both peers deterministically.
#[derive(Clone, Copy, Debug)]
enum PanelAction {
    Turn { control_id: u16, delta: i16 },
    Press(u16),
    GoSilent,
    ClosePeer,
    FailWrites,
    RequestStop,
}

/// A real socket with a real panel simulator on the far end, presented to
/// `serve_panel` as an ordinary stream.
struct PanelDriver<'stop> {
    /// Which kind a quiet read reports. A prepared socket reports `TimedOut`;
    /// a non-blocking one reports `WouldBlock`. Production sees the former on
    /// this platform, so it is the default here — the previous harness reported
    /// only `WouldBlock`, which left the shipped branch untested.
    idle_kind: ErrorKind,
    runtime_side: TcpStream,
    panel_side: Option<TcpStream>,
    panel: PanelSimulator,
    now: Rc<Cell<u64>>,
    scratch: [u8; 2048],
    reads: usize,
    actions: Vec<(usize, PanelAction)>,
    writes_fail: bool,
    stop: &'stop AtomicBool,
}

impl<'stop> PanelDriver<'stop> {
    fn new(
        descriptor: PanelDescriptor,
        now: Rc<Cell<u64>>,
        stop: &'stop AtomicBool,
        actions: Vec<(usize, PanelAction)>,
    ) -> Self {
        let (runtime_side, panel_side) = loopback();
        Self {
            idle_kind: ErrorKind::TimedOut,
            runtime_side,
            panel_side: Some(panel_side),
            panel: PanelSimulator::new(PanelSimulatorConfig {
                descriptor,
                bootloader_available: false,
            })
            .unwrap(),
            now,
            scratch: [0; 2048],
            reads: 0,
            actions,
            writes_fail: false,
            stop,
        }
    }

    /// Moves whatever the link queued into the panel, so its display can be
    /// read back off the simulator.
    fn deliver(&mut self, link: &mut PanelLink) {
        let outbound = link.take_transmit();
        if outbound.is_empty() {
            return;
        }
        self.runtime_side.write_all(&outbound).unwrap();
        let now = self.now.get();
        let read = match self.panel_side.as_mut() {
            Some(panel_side) => panel_side.read(&mut self.scratch).unwrap_or_default(),
            None => 0,
        };
        if read > 0 {
            self.panel.receive_runtime_bytes(&self.scratch[..read], now);
        }
    }

    fn apply_scheduled(&mut self) {
        let due: Vec<PanelAction> = self
            .actions
            .iter()
            .filter(|(at, _)| *at == self.reads)
            .map(|(_, action)| *action)
            .collect();
        let now = self.now.get();
        for action in due {
            match action {
                PanelAction::Turn { control_id, delta } => {
                    self.panel.encoder_delta(control_id, delta, now).unwrap();
                }
                PanelAction::Press(control_id) => {
                    self.panel.button_press(control_id, now).unwrap();
                }
                PanelAction::GoSilent => self.panel.disconnect(),
                PanelAction::ClosePeer => self.panel_side = None,
                PanelAction::FailWrites => self.writes_fail = true,
                PanelAction::RequestStop => self.stop.store(true, Ordering::Release),
            }
        }
    }
}

impl Read for PanelDriver<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        self.now.set(self.now.get() + CLOCK_STEP_MS);
        self.apply_scheduled();
        let now = self.now.get();
        let Some(panel_side) = self.panel_side.as_mut() else {
            return Ok(0);
        };
        match panel_side.read(&mut self.scratch) {
            Ok(0) => {}
            Ok(read) => {
                self.panel.receive_runtime_bytes(&self.scratch[..read], now);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
        self.panel.poll(now).unwrap();
        let produced = self.panel.read_panel_bytes(&mut self.scratch);
        if produced > 0 {
            panel_side.write_all(&self.scratch[..produced])?;
        }
        match self.runtime_side.read(buffer) {
            Ok(read) => Ok(read),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Err(self.idle_kind.into()),
            Err(error) => Err(error),
        }
    }
}

impl Write for PanelDriver<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if self.writes_fail {
            return Err(std::io::Error::other("the panel stopped reading"));
        }
        self.runtime_side.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.runtime_side.flush()
    }
}

/// Raises `stop` after a deadline, so a loop that would otherwise wedge the
/// whole suite fails with a test name and a line number instead of hanging.
///
/// Dropping it stops the watchdog, so a test that ends normally does not wait.
struct Watchdog {
    finished: std::sync::Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn watchdog(stop: &std::sync::Arc<AtomicBool>, after: std::time::Duration) -> Watchdog {
    let stop = std::sync::Arc::clone(stop);
    let finished = std::sync::Arc::new(AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&finished);
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + after;
        while !flag.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                stop.store(true, Ordering::Release);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    });
    Watchdog {
        finished,
        handle: Some(handle),
    }
}

/// How long an accept-loop test may run before the watchdog ends it. Generous
/// against the observed ~250 ms these tests need.
const TEST_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

fn config() -> LinkConfig {
    LinkConfig::new(50, HEARTBEAT_TIMEOUT_MS).unwrap()
}

/// Runs the shipped `serve_panel` against a real panel until it returns.
fn serve(
    host: &mut Host,
    actions: Vec<(usize, PanelAction)>,
    stop: &AtomicBool,
) -> (std::io::Result<PanelClosure>, Vec<PanelNotice>) {
    serve_with(host, actions, stop, ErrorKind::TimedOut)
}

fn serve_with(
    host: &mut Host,
    actions: Vec<(usize, PanelAction)>,
    stop: &AtomicBool,
    idle_kind: ErrorKind,
) -> (std::io::Result<PanelClosure>, Vec<PanelNotice>) {
    let now = Rc::new(Cell::new(0_u64));
    let mut driver = PanelDriver::new(descriptor(2, 4), Rc::clone(&now), stop, actions);
    driver.idle_kind = idle_kind;
    let clock = Rc::clone(&now);
    serve_panel(
        &mut driver,
        host,
        config(),
        0x5EED,
        stop,
        move || clock.get(),
        || {},
    )
}

#[test]
fn the_shipped_loop_carries_a_panel_turn_to_a_mixer_command() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let (outcome, notices) = serve(
        &mut host,
        vec![
            (
                6,
                PanelAction::Turn {
                    control_id: 1,
                    delta: 4,
                },
            ),
            (10, PanelAction::ClosePeer),
        ],
        &stop,
    );

    assert_eq!(outcome.unwrap(), PanelClosure::PeerClosed);
    assert_eq!(notices, Vec::new());
    assert_eq!(
        host.commands(),
        [RuntimeControlCommand::ChannelGain {
            channel_index: 1,
            gain_millionths: 1_000_000 + 4 * DEFAULT_GAIN_STEP_MILLIONTHS,
        }],
        "the second encoder moves the second channel by four detents"
    );
    assert!(!host.panel_connected(), "a closed panel leaves no bindings");
}

#[test]
fn a_failed_write_tears_the_panel_down_instead_of_leaking_it() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let (outcome, _) = serve(&mut host, vec![(4, PanelAction::FailWrites)], &stop);
    let error = outcome.expect_err("a stream that will not take bytes is an error");

    assert_eq!(error.kind(), ErrorKind::Other);
    assert!(
        !host.panel_connected(),
        "a write failure must release the panel; leaving it installed refuses every later panel for the life of the process"
    );
}

#[test]
fn a_panel_that_stops_answering_is_reported_as_a_lost_link() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let (outcome, _) = serve(&mut host, vec![(5, PanelAction::GoSilent)], &stop);
    let closure = outcome.unwrap();

    assert_eq!(closure, PanelClosure::LinkLost);
    assert!(!host.panel_connected());
}

#[test]
fn the_stop_flag_ends_a_connection_that_is_still_live() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let (outcome, _) = serve(&mut host, vec![(6, PanelAction::RequestStop)], &stop);
    let closure = outcome.unwrap();

    assert_eq!(
        closure,
        PanelClosure::HostStopping,
        "quit must take effect while a panel is attached, not only between connections"
    );
    assert!(!host.panel_connected());
}

#[test]
fn a_release_the_sink_keeps_refusing_is_bounded_and_reported() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.momentary_layout = true;
    host.runtime.sink_mut().reject_release = true;

    let started = Instant::now();
    let (outcome, notices) = serve(
        &mut host,
        vec![(6, PanelAction::Press(0)), (10, PanelAction::ClosePeer)],
        &stop,
    );
    let closure = outcome.unwrap();

    assert_eq!(closure, PanelClosure::PeerClosed);
    assert!(
        started.elapsed().as_secs() < 20,
        "a refused release must not spin the teardown forever"
    );
    assert!(
        notices
            .iter()
            .any(|notice| matches!(notice, PanelNotice::ReleaseFailed { .. })),
        "a control left held is reported rather than swallowed: {notices:?}"
    );
    assert_eq!(
        host.commands(),
        [RuntimeControlCommand::ChannelMute {
            channel_index: 0,
            muted: true,
        }],
        "the press landed and no release was accepted"
    );
}

#[test]
fn a_configured_momentary_control_is_released_when_the_link_drops_mid_press() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.momentary_layout = true;

    let (outcome, notices) = serve(
        &mut host,
        vec![(6, PanelAction::Press(0)), (10, PanelAction::GoSilent)],
        &stop,
    );
    let closure = outcome.unwrap();

    assert_eq!(closure, PanelClosure::LinkLost);
    assert!(notices.is_empty(), "nothing to report: {notices:?}");
    assert_eq!(
        host.commands(),
        [
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: false,
            },
        ],
        "the held control is released rather than left latched"
    );
    assert!(!host.panel_connected());
}

#[test]
fn a_binding_the_owner_refuses_is_reported_rather_than_dropped() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.runtime.sink_mut().invalid_target = Some(RuntimeControlTarget::Channel {
        channel_index: 1,
        parameter: hyper_control_mappings::ChannelParameter::Gain,
    });

    let (_, notices) = serve(&mut host, vec![(8, PanelAction::ClosePeer)], &stop);

    assert!(
        notices
            .iter()
            .any(|notice| matches!(notice, PanelNotice::BindingsRefused { count: 1 })),
        "the refusal the previous decisions built must reach the operator: {notices:?}"
    );
}

#[test]
fn preparing_an_accepted_stream_makes_it_wait_rather_than_spin() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    listener.set_nonblocking(true).unwrap();
    let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    // The accept loop needs a non-blocking listener to observe the stop flag;
    // on Windows the accepted socket inherits that mode.
    let accepted = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept failed: {error}"),
        }
    };

    prepare_panel_stream(&accepted).unwrap();

    assert_eq!(
        accepted.read_timeout().unwrap(),
        Some(PANEL_READ_TIMEOUT),
        "a quiet panel must not stall the loop"
    );
    let write_timeout = accepted
        .write_timeout()
        .unwrap()
        .expect("a panel that stops reading must not stall the loop either");
    // Compared against the link's own budget rather than against the constant
    // the code just read, which would compare a value with itself.
    assert!(
        write_timeout > PANEL_READ_TIMEOUT,
        "a write that is merely slow must not be mistaken for a panel that has gone: {write_timeout:?}"
    );
    assert!(
        write_timeout <= std::time::Duration::from_millis(HEARTBEAT_TIMEOUT_MS.into()),
        "whether a panel has gone is the link's call, not the socket's: {write_timeout:?}"
    );

    let mut buffer = [0_u8; 16];
    let started = Instant::now();
    let outcome = (&accepted).read(&mut buffer);
    let waited = started.elapsed();

    assert!(
        outcome.is_err(),
        "a quiet peer produces no bytes, so the read reports a timeout"
    );
    assert!(
        waited >= PANEL_READ_TIMEOUT / 2,
        "the read honoured its timeout instead of returning at once: waited {waited:?}"
    );
}

#[test]
fn a_second_panel_is_turned_away_while_one_is_being_served() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut first = TcpStream::connect(address).unwrap();
    let mut second = TcpStream::connect(address).unwrap();
    let served = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept failed: {error}"),
        }
    };

    refuse_waiting_panels(&listener);

    second
        .set_read_timeout(Some(PANEL_READ_TIMEOUT * 20))
        .unwrap();
    let mut buffer = [0_u8; 8];
    assert!(
        matches!(second.read(&mut buffer), Ok(0)),
        "the second panel learns it was refused rather than waiting in the backlog"
    );
    assert!(
        first.write_all(b"still here").is_ok(),
        "the panel being served is untouched"
    );
    drop(served);
}

#[test]
fn the_accept_loop_serves_one_panel_reports_it_and_then_turns_away_a_second() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    // The first panel is gone before it is served, which is the cheapest way to
    // reach the end of one connection without a second thread.
    drop(TcpStream::connect(address).unwrap());
    let mut second = TcpStream::connect(address).unwrap();

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);
    let mut host = Host::new(CHANNELS);
    let mut reported: Vec<(u64, String)> = Vec::new();

    serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || 0,
        |served| {
            reported.push((served.session_id, format!("{:?}", served.outcome)));
            stop.store(true, Ordering::Release);
        },
    )
    .unwrap();

    assert_eq!(
        reported.len(),
        1,
        "the loop accepted, prepared, served, and reported one panel: {reported:?}"
    );
    assert_eq!(reported[0].0, 1, "the first connection is session one");

    second
        .set_read_timeout(Some(PANEL_READ_TIMEOUT * 20))
        .unwrap();
    let mut buffer = [0_u8; 8];
    assert!(
        matches!(second.read(&mut buffer), Ok(0)),
        "the panel that waited while another was served is turned away"
    );
}

#[test]
fn the_accept_loop_stops_serving_when_a_teardown_leaves_controls_held() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    drop(TcpStream::connect(address).unwrap());

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);
    let mut host = Host::new(CHANNELS);
    host.controls_stuck = true;
    let mut reported = 0_usize;

    let error = serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || 0,
        |_| reported += 1,
    )
    .expect_err("an owner still holding controls cannot take another panel");

    assert!(matches!(error, ServeError::ControlsStillHeld));
    assert_eq!(reported, 1, "the panel that ended is still reported first");
    assert!(
        error.to_string().contains("controls held"),
        "the operator is told why: {error}"
    );
}

#[test]
fn a_listener_failure_is_the_only_stream_problem_that_ends_the_service() {
    assert!(
        ServeError::Listener(std::io::Error::other("bind lost"))
            .to_string()
            .contains("bind lost"),
        "a listener failure names its cause"
    );
}

#[test]
fn a_quiet_read_is_recognised_whichever_kind_the_platform_reports() {
    assert!(hyper_runtime_host::is_quiet(ErrorKind::TimedOut));
    assert!(hyper_runtime_host::is_quiet(ErrorKind::WouldBlock));
    assert!(!hyper_runtime_host::is_quiet(ErrorKind::ConnectionReset));
    assert!(!hyper_runtime_host::is_quiet(ErrorKind::BrokenPipe));

    // Both kinds must carry a live connection to the same place.
    for kind in [ErrorKind::TimedOut, ErrorKind::WouldBlock] {
        let stop = AtomicBool::new(false);
        let mut host = Host::new(CHANNELS);
        let (outcome, _) = serve_with(
            &mut host,
            vec![
                (
                    6,
                    PanelAction::Turn {
                        control_id: 0,
                        delta: 1,
                    },
                ),
                (10, PanelAction::ClosePeer),
            ],
            &stop,
            kind,
        );
        assert_eq!(outcome.unwrap(), PanelClosure::PeerClosed, "kind {kind:?}");
        assert_eq!(host.commands().len(), 1, "kind {kind:?}");
    }
}

#[test]
fn the_handshake_completes_before_any_scheduled_action_runs() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    // Nothing is scheduled before read 6; if the handshake needed more reads
    // than that, every scheduled action in this suite would be testing nothing.
    let (outcome, _) = serve(&mut host, vec![(2, PanelAction::ClosePeer)], &stop);

    assert_eq!(outcome.unwrap(), PanelClosure::PeerClosed);
    assert_eq!(
        host.disconnects, 1,
        "the panel was bound within two reads, so the scheduled-action indices are safe"
    );
}

#[test]
fn a_teardown_runs_once_however_many_times_it_is_asked() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let (outcome, _) = serve(&mut host, vec![(5, PanelAction::GoSilent)], &stop);

    assert_eq!(outcome.unwrap(), PanelClosure::LinkLost);
    assert_eq!(
        host.disconnects, 1,
        "the fault closes the link and the outer teardown must not release again"
    );
}

#[test]
fn the_default_layout_names_only_controls_the_panel_and_the_project_both_have() {
    let bindings = default_layout(descriptor(2, 8), CHANNELS);
    let encoders = bindings
        .iter()
        .filter(|binding| matches!(binding, PanelBinding::Encoder { .. }))
        .count();
    let buttons = bindings
        .iter()
        .filter(|binding| matches!(binding, PanelBinding::Button { .. }))
        .count();
    let actions = bindings
        .iter()
        .filter(|binding| matches!(binding, PanelBinding::ActionButton { .. }))
        .count();

    assert_eq!((encoders, buttons, actions), (2, 3, 1));
    assert_eq!(
        bindings[0],
        PanelBinding::Encoder {
            index: 0,
            target: ValueControlTarget::ChannelGain { channel_index: 0 },
            step_millionths: DEFAULT_GAIN_STEP_MILLIONTHS.try_into().unwrap(),
        }
    );
    assert!(
        !bindings.iter().any(|binding| matches!(
            binding,
            PanelBinding::Button {
                behavior: PanelButtonBehavior::Momentary,
                ..
            }
        )),
        "the default layout holds nothing, so a dropped link strands nothing"
    );
}

#[test]
fn the_default_layout_stops_at_whichever_of_the_panel_and_the_project_runs_out() {
    let no_spare_button = default_layout(descriptor(8, 2), 2);
    assert_eq!(no_spare_button.len(), 4);
    assert!(
        !no_spare_button
            .iter()
            .any(|binding| matches!(binding, PanelBinding::ActionButton { .. })),
        "a panel with no button past the last channel gets no transport button"
    );

    assert!(default_layout(descriptor(0, 0), CHANNELS).is_empty());
    assert_eq!(default_layout(descriptor(4, 4), 0).len(), 1);
}

#[test]
fn a_project_with_more_channels_than_a_panel_can_address_binds_every_control() {
    let bindings = default_layout(descriptor(16, 32), 300);

    assert_eq!(
        bindings.len(),
        48,
        "a channel count past the panel's addressable range binds all sixteen encoders and all thirty-two buttons"
    );
    assert!(
        !bindings
            .iter()
            .any(|binding| matches!(binding, PanelBinding::ActionButton { .. })),
        "no button is spare, so nothing takes the transport"
    );
}

#[test]
fn arguments_name_a_project_and_optionally_a_panel_address() {
    let parsed = RuntimeArguments::parse(["/projects/demo".to_owned()]).unwrap();
    assert_eq!(
        parsed.project_root(),
        std::path::Path::new("/projects/demo")
    );
    assert_eq!(parsed.panel_listen(), None);

    let parsed = RuntimeArguments::parse([
        "--panel-listen".to_owned(),
        "127.0.0.1:9000".to_owned(),
        "/projects/demo".to_owned(),
    ])
    .unwrap();
    assert_eq!(
        parsed.project_root(),
        std::path::Path::new("/projects/demo")
    );
    assert_eq!(
        parsed.panel_listen(),
        Some("127.0.0.1:9000".parse::<SocketAddr>().unwrap())
    );
}

#[test]
fn a_malformed_argument_list_is_reported_rather_than_assumed() {
    assert_eq!(
        RuntimeArguments::parse(Vec::new()),
        Err(ArgumentError::MissingProjectRoot)
    );
    assert_eq!(
        RuntimeArguments::parse(["--panel-listen".to_owned()]),
        Err(ArgumentError::MissingValue("--panel-listen"))
    );
    assert_eq!(
        RuntimeArguments::parse(["--panel-listen".to_owned(), "not-an-address".to_owned()]),
        Err(ArgumentError::UnreadableAddress(
            "not-an-address".to_owned()
        ))
    );
    assert_eq!(
        RuntimeArguments::parse(["--sample-rate".to_owned()]),
        Err(ArgumentError::UnknownOption("--sample-rate".to_owned()))
    );
    assert_eq!(
        RuntimeArguments::parse(["one".to_owned(), "two".to_owned()]),
        Err(ArgumentError::Repeated("the project directory"))
    );
    assert_eq!(
        RuntimeArguments::parse([
            "--panel-listen".to_owned(),
            "127.0.0.1:1".to_owned(),
            "--panel-listen".to_owned(),
            "127.0.0.1:2".to_owned(),
        ]),
        Err(ArgumentError::Repeated("--panel-listen"))
    );
}

#[test]
fn every_argument_failure_says_what_was_wrong() {
    let rendered: Vec<String> = [
        ArgumentError::MissingProjectRoot,
        ArgumentError::UnknownOption("--nope".to_owned()),
        ArgumentError::MissingValue("--panel-listen"),
        ArgumentError::UnreadableAddress("1.2.3".to_owned()),
        ArgumentError::Repeated("--panel-listen"),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();

    assert!(rendered[0].contains("project directory"));
    assert!(rendered[1].contains("--nope"));
    assert!(rendered[2].contains("--panel-listen"));
    assert!(rendered[3].contains("1.2.3"));
    assert!(rendered[4].contains("more than once"));
    assert!(
        rendered.iter().all(|message| !message.is_empty()),
        "every failure renders something the operator can act on"
    );
}

#[test]
fn a_link_that_never_bound_releases_nothing() {
    let mut host = Host::new(CHANNELS);
    let mut link = PanelLink::new(config());

    assert!(!link.holds_bindings());
    link.release(&mut host);
    assert!(host.commands().is_empty());
}

#[test]
fn each_accepted_panel_gets_its_own_session_identifier() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);
    let mut host = Host::new(CHANNELS);
    let mut sessions = Vec::new();
    let mut connected = 0_usize;

    // A fresh peer is offered for each pass; the callback stops after two.
    drop(TcpStream::connect(address).unwrap());
    serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || 0,
        |served| {
            sessions.push(served.session_id);
            connected += 1;
            if connected < 2 {
                drop(TcpStream::connect(address).unwrap());
            } else {
                stop.store(true, Ordering::Release);
            }
        },
    )
    .unwrap();

    assert_eq!(
        sessions,
        vec![1, 2],
        "a run of failures must be distinguishable from one failure reported twice"
    );
}

#[test]
fn the_accept_loop_really_serves_the_panel_it_accepted() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    // A real panel on the far end of a real connection, driven from its own
    // thread because `serve_panels` owns the accept loop on this one.
    let panel = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(PANEL_READ_TIMEOUT)).unwrap();
        let mut simulator = PanelSimulator::new(PanelSimulatorConfig {
            descriptor: descriptor(2, 4),
            bootloader_available: false,
        })
        .unwrap();
        let mut scratch = [0_u8; 2048];
        let mut turned = false;
        for step in 0..200_u64 {
            let now = step * 10;
            match stream.read(&mut scratch) {
                Ok(0) => break,
                Ok(read) => {
                    simulator.receive_runtime_bytes(&scratch[..read], now);
                }
                Err(error) if hyper_runtime_host::is_quiet(error.kind()) => {}
                Err(_) => break,
            }
            if !turned && simulator.is_connected() {
                simulator.encoder_delta(1, 4, now).unwrap();
                turned = true;
            }
            simulator.poll(now).unwrap();
            let produced = simulator.read_panel_bytes(&mut scratch);
            if produced > 0 && stream.write_all(&scratch[..produced]).is_err() {
                break;
            }
            if turned && step > 20 {
                break;
            }
        }
        drop(stream);
    });

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);
    let mut host = Host::new(CHANNELS);
    // One binding the owner will not take, so the refusal has to reach the
    // callback as well as being counted inside the link.
    host.runtime.sink_mut().invalid_target = Some(RuntimeControlTarget::Channel {
        channel_index: 2,
        parameter: hyper_control_mappings::ChannelParameter::Mute,
    });
    let mut seen: Vec<(u64, bool, usize)> = Vec::new();
    serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || {
            static TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            TICK.fetch_add(10, Ordering::Relaxed)
        },
        |served| {
            seen.push((
                served.session_id,
                served.outcome.is_ok(),
                served.notices.len(),
            ));
            assert_eq!(
                served.peer.ip(),
                std::net::IpAddr::from([127, 0, 0, 1]),
                "the reported peer is the panel that connected, not a placeholder"
            );
            stop.store(true, Ordering::Release);
        },
    )
    .unwrap();
    panel.join().unwrap();

    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, 1, "the first connection is session one");
    assert!(
        seen[0].2 >= 1,
        "the refused binding reaches the operator through the callback: {seen:?}"
    );
    let reported = seen.len();
    assert_eq!(reported, 1);
    assert_eq!(
        host.commands(),
        [RuntimeControlCommand::ChannelGain {
            channel_index: 1,
            gain_millionths: 1_000_000 + 4 * DEFAULT_GAIN_STEP_MILLIONTHS,
        }],
        "the loop did not merely accept the panel, it served it"
    );
}

#[test]
fn a_panel_that_dies_mid_connection_costs_one_panel_and_not_the_service() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    // Reading one byte proves the offer arrived; dropping with the rest still
    // unread is what a panel losing power looks like on the wire — a reset
    // rather than an orderly close.
    let dying = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut one = [0_u8; 1];
        let _ = stream.read(&mut one);
        drop(stream);
    });

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);
    let mut host = Host::new(CHANNELS);
    let mut reported: Vec<bool> = Vec::new();
    let mut second_offered = false;

    serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || 0,
        |served| {
            reported.push(served.outcome.is_err());
            if second_offered {
                stop.store(true, Ordering::Release);
            } else {
                second_offered = true;
                drop(TcpStream::connect(address).unwrap());
            }
        },
    )
    .unwrap();
    dying.join().unwrap();

    assert_eq!(
        reported,
        vec![true, true],
        "a panel that dies mid-connection is reported as a failure, not as a clean close: {reported:?}"
    );
}

#[test]
fn a_panel_arriving_during_a_session_is_turned_away_rather_than_left_silent() {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let _watchdog = watchdog(&stop, TEST_DEADLINE);

    // The first panel holds the session open until the late one has had its
    // answer, so this tests the live case rather than the pre-queued one.
    let held = std::sync::Arc::new(AtomicBool::new(false));
    let holding = std::sync::Arc::clone(&held);
    let first = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(PANEL_READ_TIMEOUT)).unwrap();
        let mut sink = [0_u8; 512];
        while !holding.load(Ordering::Acquire) {
            let _ = stream.read(&mut sink);
        }
        drop(stream);
    });

    let late = std::thread::spawn(move || {
        // Connect once the session is genuinely under way.
        std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL * 4);
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(PANEL_READ_TIMEOUT * 40))
            .unwrap();
        let mut buffer = [0_u8; 8];
        let answer = stream.read(&mut buffer);
        held.store(true, Ordering::Release);
        matches!(answer, Ok(0))
    });

    let mut host = Host::new(CHANNELS);
    let mut reported = 0_usize;
    serve_panels(
        &listener,
        &mut host,
        config(),
        &stop,
        || 0,
        |_| {
            reported += 1;
            stop.store(true, Ordering::Release);
        },
    )
    .unwrap();

    let refused = late.join().unwrap();
    first.join().unwrap();

    assert!(
        refused,
        "a panel arriving mid-session must be told, not left holding an open connection in silence"
    );
    assert_eq!(reported, 1, "only the first panel was served");
}

#[test]
fn a_write_failure_still_reports_the_control_it_could_not_release() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.momentary_layout = true;
    host.runtime.sink_mut().reject_release = true;

    // The error path is exactly when the operator most needs to know a control
    // was left held, so it must carry notices too.
    let (outcome, notices) = serve(
        &mut host,
        vec![(6, PanelAction::Press(0)), (9, PanelAction::FailWrites)],
        &stop,
    );

    assert!(outcome.is_err(), "the write failure is reported");
    assert!(
        notices
            .iter()
            .any(|notice| matches!(notice, PanelNotice::ReleaseFailed { .. })),
        "a control left held must survive the error path: {notices:?}"
    );
}

#[test]
fn a_release_that_succeeds_on_a_later_attempt_is_not_reported_as_stranded() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.momentary_layout = true;
    // Refused twice, accepted on the third attempt — within the retry budget.
    host.runtime.sink_mut().refuse_releases_first = 2;

    let (outcome, notices) = serve(
        &mut host,
        vec![(6, PanelAction::Press(0)), (10, PanelAction::ClosePeer)],
        &stop,
    );

    assert_eq!(outcome.unwrap(), PanelClosure::PeerClosed);
    assert!(
        !notices
            .iter()
            .any(|notice| matches!(notice, PanelNotice::ReleaseFailed { .. })),
        "a release the sink eventually took is not a control left held: {notices:?}"
    );
    assert_eq!(
        host.commands(),
        [
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: false,
            },
        ],
        "the retry is what got the control released"
    );
}

#[test]
fn a_command_the_owner_rejects_is_reported_rather_than_swallowed() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    host.runtime.sink_mut().reject_gain = true;

    let (_, notices) = serve(
        &mut host,
        vec![
            (
                6,
                PanelAction::Turn {
                    control_id: 1,
                    delta: 4,
                },
            ),
            (10, PanelAction::ClosePeer),
        ],
        &stop,
    );

    assert!(
        notices
            .iter()
            .any(|notice| matches!(notice, PanelNotice::Rejected { .. })),
        "a refused command reaches the operator instead of vanishing: {notices:?}"
    );
    assert!(host.commands().is_empty());
}

/// A panel with rings and LEDs, which the default fixture deliberately lacks.
fn display_descriptor(encoders: u8, buttons: u8) -> PanelDescriptor {
    PanelDescriptor {
        firmware: FirmwareVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 7,
        },
        boot_id: 0x00AB_CDEF,
        encoders,
        buttons,
        leds: 2,
        rings: 2,
        ring_segments: 16,
    }
}

fn serve_display(
    host: &mut Host,
    actions: Vec<(usize, PanelAction)>,
    stop: &AtomicBool,
) -> (std::io::Result<PanelClosure>, PanelSimulatorSnapshot) {
    let now = Rc::new(Cell::new(0_u64));
    let mut driver = PanelDriver::new(display_descriptor(2, 4), Rc::clone(&now), stop, actions);
    let clock = Rc::clone(&now);
    let (outcome, _) = serve_panel(
        &mut driver,
        host,
        config(),
        0x5EED,
        stop,
        move || clock.get(),
        || {},
    );
    let snapshot = PanelSimulatorSnapshot {
        rings: (0..2)
            .map(|index| {
                driver
                    .panel
                    .output_state()
                    .ring(index)
                    .map(|ring| ring.value)
            })
            .collect(),
        leds: (0..2)
            .map(|index| {
                driver
                    .panel
                    .output_state()
                    .led(index)
                    .map(|led| led.enabled)
            })
            .collect(),
    };
    (outcome, snapshot)
}

struct PanelSimulatorSnapshot {
    rings: Vec<Option<u16>>,
    leds: Vec<Option<bool>>,
}

#[test]
fn a_bound_channel_gain_reaches_the_matching_ring_on_a_real_panel() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);

    let (outcome, panel) = serve_display(
        &mut host,
        vec![
            (
                6,
                PanelAction::Turn {
                    control_id: 1,
                    delta: 20,
                },
            ),
            (30, PanelAction::ClosePeer),
        ],
        &stop,
    );

    assert_eq!(outcome.unwrap(), PanelClosure::PeerClosed);
    // Unity is 1_000_000 in a 0..=2_000_000 domain, so the ring sits at half
    // scale before the turn and above it after twenty detents.
    let half = u16::MAX / 2;
    let shown = panel.rings[1].expect("the second encoder drives the second ring");
    assert!(
        shown > half,
        "the ring followed the gain the encoder moved: {shown} should exceed {half}"
    );
    assert_eq!(
        panel.rings[0],
        Some(half),
        "an untouched channel still shows its own position"
    );
    assert_eq!(
        host.runtime
            .sink()
            .value_state(ValueControlTarget::ChannelGain { channel_index: 1 }),
        Some(1_000_000),
        "the sink never moved, so a display built from it would still read half scale —          this ring followed the shadow"
    );
}

#[test]
fn a_bound_mute_reaches_the_matching_led_on_a_real_panel() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);

    let (_, panel) = serve_display(
        &mut host,
        vec![(6, PanelAction::Press(1)), (30, PanelAction::ClosePeer)],
        &stop,
    );

    assert_eq!(
        panel.leds[1],
        Some(true),
        "the second button drives the second light"
    );
    assert_eq!(
        panel.leds[0],
        Some(false),
        "an untouched channel is shown unmuted rather than left dark by accident"
    );
}

#[test]
fn a_display_that_has_not_changed_sends_nothing_more() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let now = Rc::new(Cell::new(0_u64));
    let mut driver = PanelDriver::new(display_descriptor(2, 4), Rc::clone(&now), &stop, Vec::new());
    let clock = Rc::clone(&now);
    let mut link = PanelLink::new(config());
    link.start(0x5EED, 0).unwrap();
    // Reach a connected link by driving the handshake through the same loop
    // the production code uses, then stop it.
    stop.store(true, Ordering::Release);
    let _ = serve_panel(
        &mut driver,
        &mut host,
        config(),
        0x5EED,
        &stop,
        move || clock.get(),
        || {},
    );
    drop(link);

    // A fresh link, connected, then refreshed twice with nothing moving.
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let now = Rc::new(Cell::new(0_u64));
    let mut driver = PanelDriver::new(display_descriptor(2, 4), Rc::clone(&now), &stop, Vec::new());
    let clock = Rc::clone(&now);
    let mut link = PanelLink::new(config());
    link.start(0x5EED, 0).unwrap();
    let mut buffer = [0_u8; 1024];
    for _ in 0..6 {
        let outbound = link.take_transmit();
        if !outbound.is_empty() {
            driver.write_all(&outbound).unwrap();
        }
        let read = driver.read(&mut buffer).unwrap_or_default();
        link.step(&buffer[..read], clock.get(), &mut host);
    }
    assert!(link.holds_bindings(), "the handshake completed");

    let first = link.refresh_display(&mut host, clock.get());
    let second = link.refresh_display(&mut host, clock.get());

    assert!(first > 0, "the first refresh has everything to say");
    assert_eq!(
        second, 0,
        "nothing moved, so the second refresh says nothing at all"
    );
}

/// Connects a link through the shipped loop and hands it back, so a test can
/// call `refresh_display` directly against a real panel.
fn connected_link<'a>(
    host: &mut Host,
    descriptor: PanelDescriptor,
    stop: &'a AtomicBool,
    now: &Rc<Cell<u64>>,
) -> (PanelLink, PanelDriver<'a>) {
    let mut driver = PanelDriver::new(descriptor, Rc::clone(now), stop, Vec::new());
    let mut link = PanelLink::new(config());
    link.start(0x5EED, 0).unwrap();
    let mut buffer = [0_u8; 1024];
    for _ in 0..6 {
        let outbound = link.take_transmit();
        if !outbound.is_empty() {
            driver.write_all(&outbound).unwrap();
        }
        let read = driver.read(&mut buffer).unwrap_or_default();
        link.step(&buffer[..read], now.get(), host);
    }
    assert!(link.holds_bindings(), "the handshake completed");
    (link, driver)
}

#[test]
fn a_refresh_sends_no_more_than_its_budget_however_much_has_changed() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(8);
    let now = Rc::new(Cell::new(0_u64));
    let wide = PanelDescriptor {
        encoders: 8,
        buttons: 8,
        leds: 8,
        rings: 8,
        ..display_descriptor(8, 8)
    };
    let (mut link, _driver) = connected_link(&mut host, wide, &stop, &now);

    // Sixteen controls have something to say and the budget is four.
    let sent = link.refresh_display(&mut host, now.get());

    // Compared against how much was available, not against the constant the
    // code just read — which would compare a value with itself.
    let available = 16;
    assert!(
        sent > 0 && sent < available,
        "a burst of change must not starve input or the stop flag: sent {sent} of {available}"
    );
    let rest = link.refresh_display(&mut host, now.get());
    assert!(
        rest > 0,
        "what did not fit this tick goes out on the next: {rest}"
    );
    let mut total = sent + rest;
    loop {
        let more = link.refresh_display(&mut host, now.get());
        if more == 0 {
            break;
        }
        total += more;
    }
    assert!(
        total >= available,
        "every control that had something to say eventually said it: {total} of {available}"
    );
}

#[test]
fn a_panel_is_never_told_about_a_light_or_ring_it_did_not_declare() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(4);
    let now = Rc::new(Cell::new(0_u64));
    // Four controls bound, but the panel reports one ring and one light.
    let narrow = PanelDescriptor {
        encoders: 4,
        buttons: 4,
        leds: 1,
        rings: 1,
        ring_segments: 16,
        ..display_descriptor(4, 4)
    };
    let (mut link, driver) = connected_link(&mut host, narrow, &stop, &now);

    for _ in 0..8 {
        link.refresh_display(&mut host, now.get());
    }

    let outputs = driver.panel.output_state();
    assert!(
        outputs.ring(0).is_some() && outputs.led(0).is_some(),
        "what the panel declared is addressed"
    );
    assert!(
        outputs.ring(1).is_none() && outputs.led(1).is_none(),
        "and what it did not declare is never written, exactly as a binding to an absent control is refused"
    );
}

#[test]
fn a_link_with_no_bindings_shows_nothing() {
    let mut host = Host::new(CHANNELS);
    let mut link = PanelLink::new(config());

    assert!(!link.holds_bindings());
    assert_eq!(
        link.refresh_display(&mut host, 0),
        0,
        "a link that never bound has nothing to say and no panel to say it to"
    );
}

#[test]
fn a_link_that_has_released_its_bindings_stops_showing_anything() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let now = Rc::new(Cell::new(0_u64));
    let (mut link, _driver) = connected_link(&mut host, display_descriptor(2, 4), &stop, &now);
    assert!(
        link.refresh_display(&mut host, now.get()) > 0,
        "a bound link has something to show"
    );

    // Even an owner that keeps answering must not get the panel written to.
    host.displays_regardless = true;
    link.release(&mut host);

    assert_eq!(
        link.refresh_display(&mut host, now.get()),
        0,
        "the panel it was talking to is gone; the remembered descriptor is not permission to keep talking"
    );
}

#[test]
fn a_value_that_returns_to_where_it_was_is_shown_again() {
    let stop = AtomicBool::new(false);
    let mut host = Host::new(CHANNELS);
    let now = Rc::new(Cell::new(0_u64));
    let (mut link, mut driver) = connected_link(&mut host, display_descriptor(2, 4), &stop, &now);

    // Settle the display, then move a channel away and back.
    while link.refresh_display(&mut host, now.get()) > 0 {}
    driver.deliver(&mut link);
    let original = driver.panel.output_state().ring(1).unwrap().value;

    host.runtime
        .try_send_direct(RuntimeControlCommand::ChannelGain {
            channel_index: 1,
            gain_millionths: 1_500_000,
        })
        .unwrap();
    while link.refresh_display(&mut host, now.get()) > 0 {}
    driver.deliver(&mut link);
    let moved = driver.panel.output_state().ring(1).unwrap().value;
    assert_ne!(moved, original, "the display followed the change");

    host.runtime
        .try_send_direct(RuntimeControlCommand::ChannelGain {
            channel_index: 1,
            gain_millionths: 1_000_000,
        })
        .unwrap();
    while link.refresh_display(&mut host, now.get()) > 0 {}
    driver.deliver(&mut link);

    assert_eq!(
        driver.panel.output_state().ring(1).unwrap().value,
        original,
        "a value that came back must be sent again; remembering every value ever          shown rather than the latest would leave the panel displaying the old one"
    );
}
