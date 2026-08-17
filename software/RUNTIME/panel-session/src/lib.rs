#![doc = "Runtime-side panel session over a byte stream."]
#![forbid(unsafe_code)]

use hyper_panel_protocol::{
    DecodeError, DecoderEvent, Frame, LinkConfig, LinkFault, LinkPoll, MAX_ENCODED_FRAME_LEN,
    Message, PanelDescriptor, RuntimeLink, RuntimeReceive, StreamDecoder, encode_frame,
};

/// How many encoded frames the session will buffer for the caller to drain
/// before it refuses to produce more.
pub const TRANSMIT_FRAME_CAPACITY: usize = 16;

const TRANSMIT_BYTE_CAPACITY: usize = MAX_ENCODED_FRAME_LEN * TRANSMIT_FRAME_CAPACITY;

/// One thing the panel told the runtime, already separated from link upkeep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelEvent {
    /// The handshake completed and the panel described itself.
    Connected(PanelDescriptor),
    /// The panel rejected every protocol version this runtime offers.
    VersionRejected,
    /// Panel input the runtime should act on.
    Input(Message),
    /// The link failed. The session stops driving the panel; the caller keeps
    /// playing audio.
    Fault(LinkFault),
    /// The panel stopped answering within the agreed heartbeat timeout.
    TimedOut,
    /// A byte stream that did not decode. Reported rather than hidden, because
    /// a panel that garbles frames is a fault the operator should see.
    Undecodable(DecodeError),
}

/// The runtime half of the panel link, driven entirely by bytes and injected
/// time so a session is deterministic and needs no hardware to exercise.
pub struct PanelSession {
    link: RuntimeLink,
    decoder: StreamDecoder,
    transmit: Vec<u8>,
    connected: bool,
    faulted: bool,
}

impl PanelSession {
    #[must_use]
    pub fn new(config: LinkConfig) -> Self {
        Self {
            link: RuntimeLink::new(config),
            decoder: StreamDecoder::new(),
            transmit: Vec::new(),
            connected: false,
            faulted: false,
        }
    }

    /// Whether the handshake has completed and the panel is answering.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Whether the link has failed. A faulted session produces no further
    /// bytes; the runtime keeps rendering audio regardless.
    #[must_use]
    pub const fn is_faulted(&self) -> bool {
        self.faulted
    }

    /// Opens the session by offering the runtime's supported versions.
    ///
    /// # Errors
    ///
    /// Returns [`LinkFault`] when the link refuses to start.
    pub fn start(&mut self, session_id: u64, now_ms: u64) -> Result<(), LinkFault> {
        let frame = self.link.start(session_id, now_ms)?;
        self.queue(frame);
        Ok(())
    }

    /// Feeds bytes received from the panel and returns what they meant.
    pub fn receive(&mut self, bytes: &[u8], now_ms: u64) -> Vec<PanelEvent> {
        let mut events = Vec::new();
        if self.faulted {
            return events;
        }
        for &byte in bytes {
            match self.decoder.push(byte) {
                DecoderEvent::Pending => {}
                DecoderEvent::Error(error) => events.push(PanelEvent::Undecodable(error)),
                DecoderEvent::Frame(frame) => {
                    if let Some(event) = self.apply(frame, now_ms) {
                        let faulted = matches!(event, PanelEvent::Fault(_));
                        events.push(event);
                        if faulted {
                            return events;
                        }
                    }
                }
            }
        }
        events
    }

    /// Advances link upkeep — heartbeats and timeout detection — without any
    /// bytes arriving.
    pub fn poll(&mut self, now_ms: u64) -> Option<PanelEvent> {
        if self.faulted {
            return None;
        }
        match self.link.poll(now_ms) {
            Ok(LinkPoll::Idle) => None,
            Ok(LinkPoll::Send(frame)) => {
                self.queue(frame);
                None
            }
            Ok(LinkPoll::TimedOut) => {
                self.faulted = true;
                self.connected = false;
                self.transmit.clear();
                Some(PanelEvent::TimedOut)
            }
            Err(fault) => Some(self.fault(fault)),
        }
    }

    /// Sends one runtime message to the panel.
    ///
    /// # Errors
    ///
    /// Returns [`LinkFault`] when the link refuses the message.
    pub fn send(&mut self, message: Message, now_ms: u64) -> Result<(), LinkFault> {
        let frame = self.link.send(message, now_ms)?;
        self.queue(frame);
        Ok(())
    }

    /// Takes the bytes the caller should write to the panel.
    pub fn take_transmit(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.transmit)
    }

    fn apply(&mut self, frame: Frame, now_ms: u64) -> Option<PanelEvent> {
        match self.link.receive(frame, now_ms) {
            Ok(RuntimeReceive::Connected(descriptor)) => {
                self.connected = true;
                Some(PanelEvent::Connected(descriptor))
            }
            Ok(RuntimeReceive::VersionRejected(_)) => {
                self.connected = false;
                Some(PanelEvent::VersionRejected)
            }
            Ok(RuntimeReceive::Message(Message::Heartbeat { .. })) => None,
            Ok(RuntimeReceive::Message(message)) => Some(PanelEvent::Input(message)),
            Err(fault) => Some(self.fault(fault)),
        }
    }

    fn fault(&mut self, fault: LinkFault) -> PanelEvent {
        self.faulted = true;
        self.connected = false;
        self.transmit.clear();
        PanelEvent::Fault(fault)
    }

    fn queue(&mut self, frame: Frame) {
        if self.transmit.len() >= TRANSMIT_BYTE_CAPACITY {
            return;
        }
        let mut encoded = [0_u8; MAX_ENCODED_FRAME_LEN];
        if let Ok(length) = encode_frame(frame, &mut encoded) {
            self.transmit.extend_from_slice(&encoded[..length]);
        }
    }
}
