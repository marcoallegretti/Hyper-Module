//! The runtime's panel session driven against the real panel simulator, so both
//! halves of the protocol are exercised rather than a stub of one of them.

use hyper_panel_protocol::{FirmwareVersion, LinkConfig, Message, PanelDescriptor};
use hyper_panel_simulator::{PanelSimulator, PanelSimulatorConfig};
use hyper_runtime_panel_session::{PanelEvent, PanelSession};

const HEARTBEAT_INTERVAL_MS: u32 = 50;
const HEARTBEAT_TIMEOUT_MS: u32 = 200;

fn descriptor() -> PanelDescriptor {
    PanelDescriptor {
        firmware: FirmwareVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 7,
        },
        boot_id: 0x00AB_CDEF,
        encoders: 4,
        buttons: 8,
        leds: 8,
        rings: 2,
        ring_segments: 16,
    }
}

struct Peers {
    session: PanelSession,
    panel: PanelSimulator,
    buffer: [u8; 1024],
}

impl Peers {
    fn new() -> Self {
        let config = LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS).unwrap();
        Self {
            session: PanelSession::new(config),
            panel: PanelSimulator::new(PanelSimulatorConfig {
                descriptor: descriptor(),
                bootloader_available: false,
            })
            .unwrap(),
            buffer: [0; 1024],
        }
    }

    /// Moves whatever the runtime queued into the panel, then whatever the
    /// panel produced back into the runtime, and reports what the runtime saw.
    fn exchange(&mut self, now_ms: u64) -> Vec<PanelEvent> {
        let outbound = self.session.take_transmit();
        if !outbound.is_empty() {
            self.panel.receive_runtime_bytes(&outbound, now_ms);
        }
        self.panel.poll(now_ms).unwrap();
        let read = self.panel.read_panel_bytes(&mut self.buffer);
        if read == 0 {
            return Vec::new();
        }
        let inbound = self.buffer[..read].to_vec();
        self.session.receive(&inbound, now_ms)
    }

    fn connect(&mut self) {
        self.session.start(0x5EED, 0).unwrap();
        let events = self.exchange(1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, PanelEvent::Connected(_))),
            "the handshake completes against the real simulator: {events:?}"
        );
        assert!(self.session.is_connected());
    }
}

#[test]
fn the_handshake_completes_and_reports_the_panel_it_found() {
    let mut peers = Peers::new();
    peers.session.start(0x5EED, 0).unwrap();
    assert!(
        !peers.session.take_transmit().is_empty(),
        "starting offers the runtime's versions to the panel"
    );

    let mut peers = Peers::new();
    peers.session.start(0x5EED, 0).unwrap();
    let events = peers.exchange(1);
    let Some(PanelEvent::Connected(found)) = events
        .iter()
        .find(|event| matches!(event, PanelEvent::Connected(_)))
        .copied()
    else {
        panic!("expected a connection, saw {events:?}");
    };
    assert_eq!(
        found, descriptor(),
        "the runtime learns the panel's own description, not a default"
    );
    assert!(peers.session.is_connected());
    assert!(!peers.session.is_faulted());
}

#[test]
fn panel_input_reaches_the_runtime_and_heartbeats_do_not() {
    let mut peers = Peers::new();
    peers.connect();

    peers.panel.encoder_delta(0, 3, 2).unwrap();
    let events = peers.exchange(2);
    assert!(
        events.iter().any(|event| matches!(
            event,
            PanelEvent::Input(Message::EncoderDelta {
                control_id: 0,
                delta: 3,
                ..
            })
        )),
        "an encoder turn arrives as input: {events:?}"
    );

    peers.panel.button_press(1, 3).unwrap();
    let events = peers.exchange(3);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, PanelEvent::Input(_))),
        "and so does a button press: {events:?}"
    );

    // Advance past a heartbeat interval with no input at all.
    for now_ms in [60_u64, 120, 180] {
        let events = peers.exchange(now_ms);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, PanelEvent::Input(_))),
            "link upkeep is not input: {events:?}"
        );
        assert!(!peers.session.is_faulted(), "and it sustains the session");
    }
    assert!(peers.session.is_connected());
}

#[test]
fn a_panel_that_stops_answering_times_out_without_stopping_the_runtime() {
    let mut peers = Peers::new();
    peers.connect();
    peers.panel.disconnect();

    let mut timed_out = false;
    let mut now_ms = 10_u64;
    while now_ms < 2_000 {
        let _ = peers.session.take_transmit();
        if matches!(peers.session.poll(now_ms), Some(PanelEvent::TimedOut)) {
            timed_out = true;
            break;
        }
        now_ms += u64::from(HEARTBEAT_INTERVAL_MS);
    }

    assert!(
        timed_out,
        "a silent panel is reported as timed out rather than assumed present"
    );
    assert!(!peers.session.is_connected());
    assert!(peers.session.is_faulted());
    assert!(
        peers.session.take_transmit().is_empty(),
        "a faulted session stops driving the panel"
    );
    assert!(
        peers.session.poll(now_ms + 1_000).is_none(),
        "and stays quiet rather than reporting the same fault forever"
    );
}

#[test]
fn a_corrupted_frame_is_reported_and_the_link_survives_it() {
    let mut peers = Peers::new();
    peers.connect();

    // Take a real frame the panel produced, then flip a payload byte so the
    // stream is well delimited but does not decode.
    peers.panel.encoder_delta(0, 1, 5).unwrap();
    peers.panel.poll(5).unwrap();
    let read = peers.panel.read_panel_bytes(&mut peers.buffer);
    assert!(read > 2, "the panel produced a frame to corrupt");
    let mut corrupted = peers.buffer[..read].to_vec();
    corrupted[1] ^= 0xFF;

    let events = peers.session.receive(&corrupted, 6);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, PanelEvent::Undecodable(_))),
        "a frame that does not decode is reported: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, PanelEvent::Input(_))),
        "and never becomes input"
    );
    assert!(
        !peers.session.is_faulted(),
        "one corrupted frame does not tear down a working link"
    );

    // Loose bytes with no delimiter are discarded by the decoder, which is a
    // different thing from a frame that failed to decode.
    assert!(
        peers.session.receive(&[0xFF; 8], 7).is_empty(),
        "undelimited noise produces nothing at all"
    );
}
