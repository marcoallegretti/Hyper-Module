#![doc = "Runtime-side panel session over a byte stream."]
#![forbid(unsafe_code)]

use core::fmt;

use hyper_panel_protocol::{
    DecodeError, DecoderEvent, EncodeError, Frame, LinkConfig, LinkFault, LinkPoll,
    MAX_ENCODED_FRAME_LEN, Message, PanelDescriptor, RuntimeLink, RuntimeReceive, StreamDecoder,
    encode_frame,
};

/// How many encoded frames the session will buffer for the caller to drain
/// before it refuses to produce more.
pub const TRANSMIT_FRAME_CAPACITY: usize = 16;

const TRANSMIT_BYTE_CAPACITY: usize = MAX_ENCODED_FRAME_LEN * TRANSMIT_FRAME_CAPACITY;

/// Why the session could not queue a runtime-to-panel frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelSessionError {
    /// The protocol state refused to produce the frame.
    Link(LinkFault),
    /// A protocol frame could not be represented on the byte stream.
    Encode(EncodeError),
    /// The caller has not drained the configured number of pending frames.
    TransmitOverflow,
}

impl fmt::Display for PanelSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(error) => write!(formatter, "the panel link refused the frame: {error}"),
            Self::Encode(error) => {
                write!(formatter, "the panel frame could not be encoded: {error}")
            }
            Self::TransmitOverflow => formatter.write_str("the panel transmit queue is full"),
        }
    }
}

impl std::error::Error for PanelSessionError {}

impl From<LinkFault> for PanelSessionError {
    fn from(error: LinkFault) -> Self {
        Self::Link(error)
    }
}

impl From<EncodeError> for PanelSessionError {
    fn from(error: EncodeError) -> Self {
        Self::Encode(error)
    }
}

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
    /// Link upkeep produced a frame the bounded transmit queue could not hold.
    /// The session is contained because the link has already advanced its
    /// sequence at this point.
    TransmitFailed(PanelSessionError),
}

/// The runtime half of the panel link, driven entirely by bytes and injected
/// time so a session is deterministic and needs no hardware to exercise.
pub struct PanelSession {
    link: RuntimeLink,
    decoder: StreamDecoder,
    transmit: Vec<u8>,
    transmit_frames: usize,
    connected: bool,
    faulted: bool,
}

impl PanelSession {
    #[must_use]
    pub fn new(config: LinkConfig) -> Self {
        Self {
            link: RuntimeLink::new(config),
            decoder: StreamDecoder::new(),
            transmit: Vec::with_capacity(TRANSMIT_BYTE_CAPACITY),
            transmit_frames: 0,
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
    /// Returns [`PanelSessionError`] when the link refuses to start or its
    /// negotiation frame cannot be queued.
    pub fn start(&mut self, session_id: u64, now_ms: u64) -> Result<(), PanelSessionError> {
        self.ensure_transmit_capacity()?;
        let frame = self.link.start(session_id, now_ms)?;
        self.queue(frame)
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
            Ok(LinkPoll::Send(frame)) => self
                .queue(frame)
                .err()
                .map(|error| self.transmit_fault(error)),
            Ok(LinkPoll::TimedOut) => {
                self.faulted = true;
                self.connected = false;
                self.transmit.clear();
                self.transmit_frames = 0;
                Some(PanelEvent::TimedOut)
            }
            Err(fault) => Some(self.fault(fault)),
        }
    }

    /// Sends one runtime message to the panel.
    ///
    /// # Errors
    ///
    /// Returns [`PanelSessionError`] when the link refuses the message, the
    /// frame cannot be encoded, or the bounded transmit queue is full. A full
    /// queue is rejected before the link allocates a sequence number, so the
    /// caller can drain and retry without creating a gap.
    pub fn send(&mut self, message: Message, now_ms: u64) -> Result<(), PanelSessionError> {
        self.ensure_transmit_capacity()?;
        let frame = self.link.send(message, now_ms)?;
        self.queue(frame)
    }

    /// Takes the bytes the caller should write to the panel.
    pub fn take_transmit(&mut self) -> Vec<u8> {
        self.transmit_frames = 0;
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
        self.transmit_frames = 0;
        PanelEvent::Fault(fault)
    }

    fn transmit_fault(&mut self, error: PanelSessionError) -> PanelEvent {
        self.faulted = true;
        self.connected = false;
        self.transmit.clear();
        self.transmit_frames = 0;
        PanelEvent::TransmitFailed(error)
    }

    fn ensure_transmit_capacity(&self) -> Result<(), PanelSessionError> {
        if self.transmit_frames >= TRANSMIT_FRAME_CAPACITY {
            return Err(PanelSessionError::TransmitOverflow);
        }
        Ok(())
    }

    fn queue(&mut self, frame: Frame) -> Result<(), PanelSessionError> {
        self.ensure_transmit_capacity()?;
        let mut encoded = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut encoded)?;
        self.transmit.extend_from_slice(&encoded[..length]);
        self.transmit_frames += 1;
        Ok(())
    }
}
