//! The runtime's panel link, driven against the real simulator over a real
//! socket, into a real control runtime. Audio needs a device and is not here;
//! everything between the wire and a mixer command is.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};

use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingAvailability, ControlRuntime, DirectControlStateUpdate,
    PanelBinding, PanelConnectError, PanelConnection, PanelDispatchError, PanelDispatchOutcome,
    PanelReleaseFailure, RuntimeControlCommand, RuntimeControlSink, RuntimeControlTarget,
    ValueControlTarget, ValueDomain,
};
use hyper_mapping_compiler::CompiledControlMappingTable;
use hyper_panel_protocol::{FirmwareVersion, LinkConfig, Message, PanelDescriptor};
use hyper_panel_simulator::{PanelSimulator, PanelSimulatorConfig};
use hyper_runtime_host::{
    DEFAULT_GAIN_STEP_MILLIONTHS, PanelClosure, PanelHost, PanelLink, PanelStep, RuntimeArguments,
    default_layout,
};

const CHANNELS: usize = 3;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct SinkError;

#[derive(Debug, Default)]
struct Sink {
    commands: Vec<RuntimeControlCommand>,
    reject_releases: bool,
}

impl RuntimeControlSink for Sink {
    type Error = SinkError;
    type DirectCommand = RuntimeControlCommand;
    type DirectError = SinkError;

    fn validate_target(&self, _target: RuntimeControlTarget) -> Result<(), Self::Error> {
        Ok(())
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
        if self.reject_releases && matches!(command, RuntimeControlCommand::ChannelMute { .. }) {
            return Err(SinkError);
        }
        self.commands.push(command);
        Ok(())
    }

    fn try_send_direct(
        &mut self,
        command: Self::DirectCommand,
    ) -> Result<DirectControlStateUpdate, Self::DirectError> {
        self.try_send(command)?;
        Ok(DirectControlStateUpdate::Unchanged)
    }
}

struct Host {
    runtime: ControlRuntime<Sink>,
    channels: usize,
    refuse_connect: bool,
}

impl Host {
    fn new(channels: usize) -> Self {
        Self {
            runtime: ControlRuntime::try_new(CompiledControlMappingTable::empty(), Sink::default())
                .expect("an empty table needs no capacity"),
            channels,
            refuse_connect: false,
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
        if self.refuse_connect {
            return Err(PanelConnectError::AlreadyConnected);
        }
        self.runtime.connect_panel(descriptor, bindings)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        self.runtime.disconnect_panel()
    }

    fn panel_connected(&self) -> bool {
        self.runtime.panel_connected()
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        self.runtime.dispatch_panel(message)
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

/// Two ends of one real loopback socket, both non-blocking so a single thread
/// can drive both peers and the clock.
fn loopback() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    client.set_nonblocking(true).unwrap();
    server.set_nonblocking(true).unwrap();
    (client, server)
}

fn read_available(stream: &mut TcpStream, buffer: &mut [u8]) -> usize {
    match stream.read(buffer) {
        Ok(read) => read,
        Err(error) if error.kind() == ErrorKind::WouldBlock => 0,
        Err(error) => panic!("loopback read failed: {error}"),
    }
}

struct Peers {
    runtime_side: TcpStream,
    panel_side: TcpStream,
    panel: PanelSimulator,
    buffer: [u8; 1024],
}

impl Peers {
    fn new(descriptor: PanelDescriptor) -> Self {
        let (runtime_side, panel_side) = loopback();
        Self {
            runtime_side,
            panel_side,
            panel: PanelSimulator::new(PanelSimulatorConfig {
                descriptor,
                bootloader_available: false,
            })
            .unwrap(),
            buffer: [0; 1024],
        }
    }

    /// Moves whatever the runtime queued across the socket into the panel, then
    /// whatever the panel produced back across it into the link.
    fn pump(&mut self, link: &mut PanelLink, host: &mut Host, now_ms: u64) -> PanelStep {
        let outbound = link.take_transmit();
        if !outbound.is_empty() {
            self.runtime_side.write_all(&outbound).unwrap();
        }
        let read = read_available(&mut self.panel_side, &mut self.buffer);
        if read > 0 {
            self.panel
                .receive_runtime_bytes(&self.buffer[..read], now_ms);
        }
        self.panel.poll(now_ms).unwrap();
        let produced = self.panel.read_panel_bytes(&mut self.buffer);
        if produced > 0 {
            self.panel_side.write_all(&self.buffer[..produced]).unwrap();
        }
        let mut inbound = [0_u8; 1024];
        let received = read_available(&mut self.runtime_side, &mut inbound);
        link.step(&inbound[..received], now_ms, host)
    }
}

fn config() -> LinkConfig {
    LinkConfig::new(50, 500).unwrap()
}

fn connected(descriptor: PanelDescriptor, channels: usize) -> (Peers, PanelLink, Host) {
    let mut peers = Peers::new(descriptor);
    let mut link = PanelLink::new(config());
    let mut host = Host::new(channels);
    link.start(0x5EED, 0).unwrap();
    for now_ms in 1..4 {
        assert_eq!(peers.pump(&mut link, &mut host, now_ms), PanelStep::Live);
    }
    assert!(link.is_connected(), "the handshake completes over a socket");
    assert!(host.panel_connected());
    (peers, link, host)
}

#[test]
fn the_default_layout_names_only_controls_the_panel_and_the_project_both_have() {
    let bindings = default_layout(descriptor(2, 8), CHANNELS);

    assert_eq!(
        bindings,
        vec![
            PanelBinding::Encoder {
                index: 0,
                target: ValueControlTarget::ChannelGain { channel_index: 0 },
                step_millionths: DEFAULT_GAIN_STEP_MILLIONTHS.try_into().unwrap(),
            },
            PanelBinding::Encoder {
                index: 1,
                target: ValueControlTarget::ChannelGain { channel_index: 1 },
                step_millionths: DEFAULT_GAIN_STEP_MILLIONTHS.try_into().unwrap(),
            },
            PanelBinding::Button {
                index: 0,
                target: BooleanControlTarget::ChannelMute { channel_index: 0 },
                behavior: hyper_control_runtime::PanelButtonBehavior::Toggle,
            },
            PanelBinding::Button {
                index: 1,
                target: BooleanControlTarget::ChannelMute { channel_index: 1 },
                behavior: hyper_control_runtime::PanelButtonBehavior::Toggle,
            },
            PanelBinding::Button {
                index: 2,
                target: BooleanControlTarget::ChannelMute { channel_index: 2 },
                behavior: hyper_control_runtime::PanelButtonBehavior::Toggle,
            },
            PanelBinding::ActionButton {
                index: 3,
                action: hyper_control_runtime::PanelActionTarget::TransportToggle,
            },
        ],
        "two encoders bind two channels, three buttons bind three, and the fourth button takes the transport"
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

    assert!(
        default_layout(descriptor(0, 0), CHANNELS).is_empty(),
        "a panel with no controls gets no bindings"
    );
    assert!(
        default_layout(descriptor(4, 4), 0).len() == 1,
        "a project with no channels leaves only the transport button"
    );
}

#[test]
fn a_default_layout_installs_completely_against_the_panel_that_produced_it() {
    let (_peers, _link, host) = connected(descriptor(2, 4), CHANNELS);
    assert!(host.panel_connected());
}

#[test]
fn a_panel_turn_over_a_real_socket_reaches_the_bound_mixer_channel() {
    let (mut peers, mut link, mut host) = connected(descriptor(2, 4), CHANNELS);

    peers.panel.encoder_delta(1, 4, 5).unwrap();
    assert_eq!(peers.pump(&mut link, &mut host, 5), PanelStep::Live);

    assert_eq!(
        host.commands(),
        [RuntimeControlCommand::ChannelGain {
            channel_index: 1,
            gain_millionths: 1_000_000 + 4 * DEFAULT_GAIN_STEP_MILLIONTHS,
        }],
        "the second encoder moves the second channel by four detents"
    );
}

#[test]
fn a_panel_button_over_a_real_socket_toggles_the_bound_channel() {
    let (mut peers, mut link, mut host) = connected(descriptor(2, 4), CHANNELS);

    peers.panel.button_press(2, 5).unwrap();
    assert_eq!(peers.pump(&mut link, &mut host, 5), PanelStep::Live);
    peers.panel.button_release(2, 6).unwrap();
    assert_eq!(peers.pump(&mut link, &mut host, 6), PanelStep::Live);

    assert_eq!(
        host.commands(),
        [RuntimeControlCommand::ChannelMute {
            channel_index: 2,
            muted: true,
        }],
        "a toggle acts on press and stays put on release"
    );
}

#[test]
fn a_panel_that_stops_answering_takes_its_bindings_down_with_it() {
    let (mut peers, mut link, mut host) = connected(descriptor(2, 4), CHANNELS);
    peers.panel.disconnect();

    let mut now_ms = 10_u64;
    let mut closed = None;
    while now_ms < 4_000 {
        if let PanelStep::Closed(closure) = peers.pump(&mut link, &mut host, now_ms) {
            closed = Some(closure);
            break;
        }
        now_ms += 50;
    }

    assert_eq!(closed, Some(PanelClosure::LinkLost));
    assert!(!link.is_connected());
    assert!(
        !host.panel_connected(),
        "a panel that went away leaves no bindings behind"
    );
}

#[test]
fn a_link_lost_while_a_control_is_held_still_ends_the_connection() {
    let mut peers = Peers::new(descriptor(2, 4));
    let mut link = PanelLink::new(config());
    let mut host = Host::new(CHANNELS);
    host.runtime.sink_mut().reject_releases = true;
    link.start(0x5EED, 0).unwrap();
    for now_ms in 1..4 {
        assert_eq!(peers.pump(&mut link, &mut host, now_ms), PanelStep::Live);
    }

    peers.panel.disconnect();
    let mut now_ms = 10_u64;
    while now_ms < 4_000 && link.is_connected() {
        if let PanelStep::Closed(_) = peers.pump(&mut link, &mut host, now_ms) {
            break;
        }
        now_ms += 50;
    }

    assert!(
        !link.is_connected(),
        "a sink that refuses a release does not wedge the link open forever"
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
    use hyper_runtime_host::ArgumentError;

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
fn a_panel_the_owner_will_not_accept_is_dropped_rather_than_driven() {
    let mut peers = Peers::new(descriptor(2, 4));
    let mut link = PanelLink::new(config());
    let mut host = Host::new(CHANNELS);
    host.refuse_connect = true;
    link.start(0x5EED, 0).unwrap();

    let mut closed = None;
    for now_ms in 1..8 {
        if let PanelStep::Closed(closure) = peers.pump(&mut link, &mut host, now_ms) {
            closed = Some(closure);
            break;
        }
    }

    assert_eq!(
        closed,
        Some(PanelClosure::LinkLost),
        "an owner that will not take the bindings ends the connection"
    );
    assert!(!link.is_connected());
    assert!(!host.panel_connected());

    peers.panel.encoder_delta(0, 4, 20).unwrap();
    let _ = peers.pump(&mut link, &mut host, 20);
    assert!(
        host.commands().is_empty(),
        "a panel that was never accepted commands nothing"
    );
}
