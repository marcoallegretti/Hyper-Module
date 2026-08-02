use core::fmt;

use crate::message::{Direction, Message, PanelDescriptor, SupportedVersions};
use crate::{Frame, next_sequence};

const MIN_HEARTBEAT_INTERVAL_MS: u32 = 10;
const MAX_HEARTBEAT_INTERVAL_MS: u32 = 60_000;
const MAX_HEARTBEAT_TIMEOUT_MS: u32 = 300_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkConfig {
    heartbeat_interval_ms: u32,
    heartbeat_timeout_ms: u32,
}

impl LinkConfig {
    /// Builds bounded heartbeat timing for both peers.
    ///
    /// # Errors
    ///
    /// Returns an error when the interval is outside the supported range or
    /// the timeout cannot tolerate at least one missed heartbeat.
    pub const fn new(
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u32,
    ) -> Result<Self, LinkConfigError> {
        if heartbeat_interval_ms < MIN_HEARTBEAT_INTERVAL_MS
            || heartbeat_interval_ms > MAX_HEARTBEAT_INTERVAL_MS
        {
            return Err(LinkConfigError::HeartbeatInterval);
        }
        let Some(minimum_timeout) = heartbeat_interval_ms.checked_mul(2) else {
            return Err(LinkConfigError::HeartbeatTimeout);
        };
        if heartbeat_timeout_ms < minimum_timeout || heartbeat_timeout_ms > MAX_HEARTBEAT_TIMEOUT_MS
        {
            return Err(LinkConfigError::HeartbeatTimeout);
        }
        Ok(Self {
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
        })
    }

    #[must_use]
    pub const fn heartbeat_interval_ms(self) -> u32 {
        self.heartbeat_interval_ms
    }

    #[must_use]
    pub const fn heartbeat_timeout_ms(self) -> u32 {
        self.heartbeat_timeout_ms
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkConfigError {
    HeartbeatInterval,
    HeartbeatTimeout,
}

impl fmt::Display for LinkConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::HeartbeatInterval => "heartbeat interval is outside the supported range",
            Self::HeartbeatTimeout => {
                "heartbeat timeout must tolerate a missed heartbeat and remain bounded"
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkFault {
    InvalidSession,
    ClockRegression,
    ClockExhausted,
    UnexpectedState,
    UnexpectedMessage,
    WrongDirection,
    SessionMismatch { expected: u64, received: u64 },
    ProtocolMismatch { expected: u8, received: u8 },
    SequenceMismatch { expected: u32, received: u32 },
    InvalidNegotiation,
    UnsupportedControl,
    HeartbeatTimedOut,
}

impl fmt::Display for LinkFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSession => formatter.write_str("panel session identifier must be nonzero"),
            Self::ClockRegression => formatter.write_str("panel link clock moved backwards"),
            Self::ClockExhausted => {
                formatter.write_str("panel link clock cannot represent its next deadline")
            }
            Self::UnexpectedState => {
                formatter.write_str("panel link operation is invalid in its current state")
            }
            Self::UnexpectedMessage => {
                formatter.write_str("panel link received an unexpected message")
            }
            Self::WrongDirection => {
                formatter.write_str("panel message was sent in the wrong direction")
            }
            Self::SessionMismatch { expected, received } => write!(
                formatter,
                "panel session mismatch: expected {expected}, received {received}"
            ),
            Self::ProtocolMismatch { expected, received } => write!(
                formatter,
                "panel protocol mismatch: expected {expected}, received {received}"
            ),
            Self::SequenceMismatch { expected, received } => write!(
                formatter,
                "panel sequence mismatch: expected {expected}, received {received}"
            ),
            Self::InvalidNegotiation => {
                formatter.write_str("panel version or heartbeat negotiation is invalid")
            }
            Self::UnsupportedControl => {
                formatter.write_str("panel message exceeds negotiated control capabilities")
            }
            Self::HeartbeatTimedOut => formatter.write_str("panel heartbeat deadline expired"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkPoll {
    Idle,
    Send(Frame),
    TimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeReceive {
    Connected(PanelDescriptor),
    VersionRejected(SupportedVersions),
    Message(Message),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelReceive {
    SessionEstablished(Frame),
    VersionRejected(Frame),
    Command(Message),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeState {
    Disconnected,
    Negotiating,
    Connected,
    Incompatible,
}

pub struct RuntimeLink {
    config: LinkConfig,
    supported: SupportedVersions,
    state: RuntimeState,
    session_id: u64,
    protocol_version: u8,
    next_tx_sequence: u32,
    expected_rx_sequence: u32,
    started_ms: u64,
    last_rx_ms: u64,
    next_heartbeat_ms: u64,
    last_observed_ms: Option<u64>,
    descriptor: Option<PanelDescriptor>,
    last_fault: Option<LinkFault>,
}

impl RuntimeLink {
    #[must_use]
    pub const fn new(config: LinkConfig) -> Self {
        Self {
            config,
            supported: SupportedVersions::CURRENT,
            state: RuntimeState::Disconnected,
            session_id: 0,
            protocol_version: 0,
            next_tx_sequence: 1,
            expected_rx_sequence: 1,
            started_ms: 0,
            last_rx_ms: 0,
            next_heartbeat_ms: 0,
            last_observed_ms: None,
            descriptor: None,
            last_fault: None,
        }
    }

    /// Starts or replaces a session and returns its negotiation frame.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero session identifier, a regressing clock, or
    /// an unrepresentable heartbeat deadline.
    pub fn start(&mut self, session_id: u64, now_ms: u64) -> Result<Frame, LinkFault> {
        self.observe(now_ms)?;
        if session_id == 0 {
            return Err(self.fail(LinkFault::InvalidSession));
        }
        self.state = RuntimeState::Negotiating;
        self.session_id = session_id;
        self.protocol_version = 0;
        self.next_tx_sequence = 1;
        self.expected_rx_sequence = 1;
        self.started_ms = now_ms;
        self.last_rx_ms = now_ms;
        self.next_heartbeat_ms = deadline(now_ms, self.config.heartbeat_interval_ms)
            .map_err(|fault| self.fail(fault))?;
        self.descriptor = None;
        self.last_fault = None;
        Frame::handshake(
            session_id,
            Message::RuntimeHello {
                versions: self.supported,
                heartbeat_interval_ms: self.config.heartbeat_interval_ms,
                heartbeat_timeout_ms: self.config.heartbeat_timeout_ms,
            },
        )
        .ok_or_else(|| self.fail(LinkFault::InvalidNegotiation))
    }

    /// Accepts one decoded panel frame.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects on invalid state, direction, session,
    /// protocol, sequence, negotiation, or monotonic time.
    pub fn receive(&mut self, frame: Frame, now_ms: u64) -> Result<RuntimeReceive, LinkFault> {
        self.observe(now_ms)?;
        match self.state {
            RuntimeState::Negotiating => self.receive_negotiation(frame, now_ms),
            RuntimeState::Connected => {
                self.validate_application(frame, Direction::PanelToRuntime)?;
                let descriptor = self
                    .descriptor
                    .ok_or_else(|| self.fail(LinkFault::UnexpectedState))?;
                if !descriptor.accepts_panel_message(frame.message) {
                    return Err(self.fail(LinkFault::UnsupportedControl));
                }
                self.expected_rx_sequence = next_sequence(self.expected_rx_sequence);
                self.last_rx_ms = now_ms;
                Ok(RuntimeReceive::Message(frame.message))
            }
            RuntimeState::Disconnected | RuntimeState::Incompatible => {
                Err(self.fail(LinkFault::UnexpectedState))
            }
        }
    }

    /// Creates the next sequenced runtime-to-panel application frame.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects when the link is unavailable, expired,
    /// time regresses, or the message direction is invalid.
    pub fn send(&mut self, message: Message, now_ms: u64) -> Result<Frame, LinkFault> {
        self.observe(now_ms)?;
        self.ensure_connected_and_alive(now_ms)?;
        if message.is_handshake() || message.direction() == Direction::PanelToRuntime {
            return Err(self.fail(LinkFault::WrongDirection));
        }
        let descriptor = self
            .descriptor
            .ok_or_else(|| self.fail(LinkFault::UnexpectedState))?;
        if !descriptor.accepts_runtime_message(message) {
            return Err(self.fail(LinkFault::UnsupportedControl));
        }
        let frame = Frame::application(
            self.protocol_version,
            self.session_id,
            self.next_tx_sequence,
            message,
        )
        .ok_or_else(|| self.fail(LinkFault::UnexpectedMessage))?;
        self.next_tx_sequence = next_sequence(self.next_tx_sequence);
        Ok(frame)
    }

    /// Evaluates timeout first and then heartbeat transmission for `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects if time regresses or the next deadline
    /// cannot be represented.
    pub fn poll(&mut self, now_ms: u64) -> Result<LinkPoll, LinkFault> {
        self.observe(now_ms)?;
        if !matches!(
            self.state,
            RuntimeState::Negotiating | RuntimeState::Connected
        ) {
            return Ok(LinkPoll::Idle);
        }
        if expired(self.last_rx_ms, self.config.heartbeat_timeout_ms, now_ms) {
            self.fail(LinkFault::HeartbeatTimedOut);
            return Ok(LinkPoll::TimedOut);
        }
        if self.state == RuntimeState::Connected && now_ms >= self.next_heartbeat_ms {
            let frame = self.heartbeat(now_ms)?;
            self.next_heartbeat_ms = deadline(now_ms, self.config.heartbeat_interval_ms)
                .map_err(|fault| self.fail(fault))?;
            return Ok(LinkPoll::Send(frame));
        }
        Ok(LinkPoll::Idle)
    }

    pub fn disconnect(&mut self) {
        self.state = RuntimeState::Disconnected;
        self.descriptor = None;
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self.state, RuntimeState::Connected)
    }

    #[must_use]
    pub const fn descriptor(&self) -> Option<PanelDescriptor> {
        self.descriptor
    }

    #[must_use]
    pub const fn last_fault(&self) -> Option<LinkFault> {
        self.last_fault
    }

    fn receive_negotiation(
        &mut self,
        frame: Frame,
        now_ms: u64,
    ) -> Result<RuntimeReceive, LinkFault> {
        if frame.session_id != self.session_id {
            return Err(self.fail(LinkFault::SessionMismatch {
                expected: self.session_id,
                received: frame.session_id,
            }));
        }
        if frame.protocol_version != 0 || frame.sequence != 0 {
            return Err(self.fail(LinkFault::InvalidNegotiation));
        }
        match frame.message {
            Message::PanelHello {
                selected_version,
                heartbeat_interval_ms,
                heartbeat_timeout_ms,
                descriptor,
            } => {
                let Some(selected) = SupportedVersions::new(selected_version, selected_version)
                else {
                    return Err(self.fail(LinkFault::InvalidNegotiation));
                };
                if self.supported.negotiate(selected) != Some(selected_version)
                    || heartbeat_interval_ms != self.config.heartbeat_interval_ms
                    || heartbeat_timeout_ms != self.config.heartbeat_timeout_ms
                    || !descriptor.is_valid()
                {
                    return Err(self.fail(LinkFault::InvalidNegotiation));
                }
                self.state = RuntimeState::Connected;
                self.protocol_version = selected_version;
                self.started_ms = now_ms;
                self.last_rx_ms = now_ms;
                self.next_heartbeat_ms = deadline(now_ms, self.config.heartbeat_interval_ms)
                    .map_err(|fault| self.fail(fault))?;
                self.descriptor = Some(descriptor);
                Ok(RuntimeReceive::Connected(descriptor))
            }
            Message::VersionRejected { supported } => {
                self.state = RuntimeState::Incompatible;
                self.descriptor = None;
                Ok(RuntimeReceive::VersionRejected(supported))
            }
            _ => Err(self.fail(LinkFault::UnexpectedMessage)),
        }
    }

    fn validate_application(
        &mut self,
        frame: Frame,
        direction: Direction,
    ) -> Result<(), LinkFault> {
        if frame.message.is_handshake() || frame.message.direction() == direction.opposite() {
            return Err(self.fail(LinkFault::WrongDirection));
        }
        if frame.session_id != self.session_id {
            return Err(self.fail(LinkFault::SessionMismatch {
                expected: self.session_id,
                received: frame.session_id,
            }));
        }
        if frame.protocol_version != self.protocol_version {
            return Err(self.fail(LinkFault::ProtocolMismatch {
                expected: self.protocol_version,
                received: frame.protocol_version,
            }));
        }
        if frame.sequence != self.expected_rx_sequence {
            return Err(self.fail(LinkFault::SequenceMismatch {
                expected: self.expected_rx_sequence,
                received: frame.sequence,
            }));
        }
        Ok(())
    }

    fn ensure_connected_and_alive(&mut self, now_ms: u64) -> Result<(), LinkFault> {
        if self.state != RuntimeState::Connected {
            return Err(self.fail(LinkFault::UnexpectedState));
        }
        if expired(self.last_rx_ms, self.config.heartbeat_timeout_ms, now_ms) {
            return Err(self.fail(LinkFault::HeartbeatTimedOut));
        }
        Ok(())
    }

    fn heartbeat(&mut self, now_ms: u64) -> Result<Frame, LinkFault> {
        let message = Message::Heartbeat {
            uptime_ms: now_ms - self.started_ms,
            last_received_sequence: previous_sequence(self.expected_rx_sequence),
        };
        let frame = Frame::application(
            self.protocol_version,
            self.session_id,
            self.next_tx_sequence,
            message,
        )
        .ok_or_else(|| self.fail(LinkFault::UnexpectedMessage))?;
        self.next_tx_sequence = next_sequence(self.next_tx_sequence);
        Ok(frame)
    }

    fn observe(&mut self, now_ms: u64) -> Result<(), LinkFault> {
        if self.last_observed_ms.is_some_and(|last| now_ms < last) {
            return Err(self.fail(LinkFault::ClockRegression));
        }
        self.last_observed_ms = Some(now_ms);
        Ok(())
    }

    fn fail(&mut self, fault: LinkFault) -> LinkFault {
        self.state = RuntimeState::Disconnected;
        self.descriptor = None;
        self.last_fault = Some(fault);
        fault
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanelState {
    Disconnected,
    Connected,
}

pub struct PanelLink {
    descriptor: PanelDescriptor,
    supported: SupportedVersions,
    state: PanelState,
    config: Option<LinkConfig>,
    session_id: u64,
    protocol_version: u8,
    next_tx_sequence: u32,
    expected_rx_sequence: u32,
    started_ms: u64,
    last_rx_ms: u64,
    next_heartbeat_ms: u64,
    last_observed_ms: Option<u64>,
    last_fault: Option<LinkFault>,
}

impl PanelLink {
    #[must_use]
    pub const fn new(descriptor: PanelDescriptor) -> Option<Self> {
        if !descriptor.is_valid() {
            return None;
        }
        Some(Self {
            descriptor,
            supported: SupportedVersions::CURRENT,
            state: PanelState::Disconnected,
            config: None,
            session_id: 0,
            protocol_version: 0,
            next_tx_sequence: 1,
            expected_rx_sequence: 1,
            started_ms: 0,
            last_rx_ms: 0,
            next_heartbeat_ms: 0,
            last_observed_ms: None,
            last_fault: None,
        })
    }

    /// Accepts one decoded runtime frame, including replacement handshakes.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects on invalid state, direction, session,
    /// protocol, sequence, negotiation, or monotonic time.
    pub fn receive(&mut self, frame: Frame, now_ms: u64) -> Result<PanelReceive, LinkFault> {
        self.observe(now_ms)?;
        if let Message::RuntimeHello {
            versions,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
        } = frame.message
        {
            return self.receive_hello(
                frame,
                versions,
                heartbeat_interval_ms,
                heartbeat_timeout_ms,
                now_ms,
            );
        }
        if self.state != PanelState::Connected {
            return Err(self.fail(LinkFault::UnexpectedState));
        }
        self.validate_application(frame, Direction::RuntimeToPanel)?;
        self.expected_rx_sequence = next_sequence(self.expected_rx_sequence);
        self.last_rx_ms = now_ms;
        Ok(PanelReceive::Command(frame.message))
    }

    /// Creates the next sequenced panel-to-runtime application frame.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects when the link is unavailable, expired,
    /// time regresses, or the message direction is invalid.
    pub fn send(&mut self, message: Message, now_ms: u64) -> Result<Frame, LinkFault> {
        self.observe(now_ms)?;
        self.ensure_connected_and_alive(now_ms)?;
        if message.is_handshake() || message.direction() == Direction::RuntimeToPanel {
            return Err(self.fail(LinkFault::WrongDirection));
        }
        if !self.descriptor.accepts_panel_message(message) {
            return Err(self.fail(LinkFault::UnsupportedControl));
        }
        let frame = Frame::application(
            self.protocol_version,
            self.session_id,
            self.next_tx_sequence,
            message,
        )
        .ok_or_else(|| self.fail(LinkFault::UnexpectedMessage))?;
        self.next_tx_sequence = next_sequence(self.next_tx_sequence);
        Ok(frame)
    }

    /// Evaluates timeout first and then heartbeat transmission for `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error and disconnects if time regresses or the next deadline
    /// cannot be represented.
    pub fn poll(&mut self, now_ms: u64) -> Result<LinkPoll, LinkFault> {
        self.observe(now_ms)?;
        if self.state != PanelState::Connected {
            return Ok(LinkPoll::Idle);
        }
        let config = self
            .config
            .ok_or_else(|| self.fail(LinkFault::UnexpectedState))?;
        if expired(self.last_rx_ms, config.heartbeat_timeout_ms, now_ms) {
            self.fail(LinkFault::HeartbeatTimedOut);
            return Ok(LinkPoll::TimedOut);
        }
        if now_ms >= self.next_heartbeat_ms {
            let frame = self.heartbeat(now_ms)?;
            self.next_heartbeat_ms =
                deadline(now_ms, config.heartbeat_interval_ms).map_err(|fault| self.fail(fault))?;
            return Ok(LinkPoll::Send(frame));
        }
        Ok(LinkPoll::Idle)
    }

    pub fn disconnect(&mut self) {
        self.state = PanelState::Disconnected;
        self.config = None;
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        matches!(self.state, PanelState::Connected)
    }

    #[must_use]
    pub const fn session_id(&self) -> Option<u64> {
        if self.is_connected() {
            Some(self.session_id)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn last_fault(&self) -> Option<LinkFault> {
        self.last_fault
    }

    fn receive_hello(
        &mut self,
        frame: Frame,
        versions: SupportedVersions,
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u32,
        now_ms: u64,
    ) -> Result<PanelReceive, LinkFault> {
        if frame.protocol_version != 0 || frame.sequence != 0 || frame.session_id == 0 {
            return Err(self.fail(LinkFault::InvalidNegotiation));
        }
        let Some(selected_version) = self.supported.negotiate(versions) else {
            self.disconnect();
            let reply = Frame::handshake(
                frame.session_id,
                Message::VersionRejected {
                    supported: self.supported,
                },
            )
            .ok_or_else(|| self.fail(LinkFault::InvalidNegotiation))?;
            return Ok(PanelReceive::VersionRejected(reply));
        };
        let config = LinkConfig::new(heartbeat_interval_ms, heartbeat_timeout_ms)
            .map_err(|_| self.fail(LinkFault::InvalidNegotiation))?;
        self.state = PanelState::Connected;
        self.config = Some(config);
        self.session_id = frame.session_id;
        self.protocol_version = selected_version;
        self.next_tx_sequence = 1;
        self.expected_rx_sequence = 1;
        self.started_ms = now_ms;
        self.last_rx_ms = now_ms;
        self.next_heartbeat_ms =
            deadline(now_ms, config.heartbeat_interval_ms).map_err(|fault| self.fail(fault))?;
        self.last_fault = None;
        let reply = Frame::handshake(
            self.session_id,
            Message::PanelHello {
                selected_version,
                heartbeat_interval_ms,
                heartbeat_timeout_ms,
                descriptor: self.descriptor,
            },
        )
        .ok_or_else(|| self.fail(LinkFault::InvalidNegotiation))?;
        Ok(PanelReceive::SessionEstablished(reply))
    }

    fn validate_application(
        &mut self,
        frame: Frame,
        direction: Direction,
    ) -> Result<(), LinkFault> {
        if frame.message.is_handshake() || frame.message.direction() == direction.opposite() {
            return Err(self.fail(LinkFault::WrongDirection));
        }
        if frame.session_id != self.session_id {
            return Err(self.fail(LinkFault::SessionMismatch {
                expected: self.session_id,
                received: frame.session_id,
            }));
        }
        if frame.protocol_version != self.protocol_version {
            return Err(self.fail(LinkFault::ProtocolMismatch {
                expected: self.protocol_version,
                received: frame.protocol_version,
            }));
        }
        if frame.sequence != self.expected_rx_sequence {
            return Err(self.fail(LinkFault::SequenceMismatch {
                expected: self.expected_rx_sequence,
                received: frame.sequence,
            }));
        }
        Ok(())
    }

    fn ensure_connected_and_alive(&mut self, now_ms: u64) -> Result<(), LinkFault> {
        if self.state != PanelState::Connected {
            return Err(self.fail(LinkFault::UnexpectedState));
        }
        let config = self
            .config
            .ok_or_else(|| self.fail(LinkFault::UnexpectedState))?;
        if expired(self.last_rx_ms, config.heartbeat_timeout_ms, now_ms) {
            return Err(self.fail(LinkFault::HeartbeatTimedOut));
        }
        Ok(())
    }

    fn heartbeat(&mut self, now_ms: u64) -> Result<Frame, LinkFault> {
        let message = Message::Heartbeat {
            uptime_ms: now_ms - self.started_ms,
            last_received_sequence: previous_sequence(self.expected_rx_sequence),
        };
        let frame = Frame::application(
            self.protocol_version,
            self.session_id,
            self.next_tx_sequence,
            message,
        )
        .ok_or_else(|| self.fail(LinkFault::UnexpectedMessage))?;
        self.next_tx_sequence = next_sequence(self.next_tx_sequence);
        Ok(frame)
    }

    fn observe(&mut self, now_ms: u64) -> Result<(), LinkFault> {
        if self.last_observed_ms.is_some_and(|last| now_ms < last) {
            return Err(self.fail(LinkFault::ClockRegression));
        }
        self.last_observed_ms = Some(now_ms);
        Ok(())
    }

    fn fail(&mut self, fault: LinkFault) -> LinkFault {
        self.state = PanelState::Disconnected;
        self.config = None;
        self.last_fault = Some(fault);
        fault
    }
}

impl Direction {
    const fn opposite(self) -> Self {
        match self {
            Self::RuntimeToPanel => Self::PanelToRuntime,
            Self::PanelToRuntime => Self::RuntimeToPanel,
            Self::Bidirectional => Self::Bidirectional,
        }
    }
}

const fn previous_sequence(next: u32) -> u32 {
    if next == 1 { 0 } else { next - 1 }
}

fn deadline(now_ms: u64, delta_ms: u32) -> Result<u64, LinkFault> {
    now_ms
        .checked_add(u64::from(delta_ms))
        .ok_or(LinkFault::ClockExhausted)
}

const fn expired(last_rx_ms: u64, timeout_ms: u32, now_ms: u64) -> bool {
    now_ms - last_rx_ms >= timeout_ms as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FirmwareVersion, InputState, PROTOCOL_VERSION};

    fn descriptor() -> PanelDescriptor {
        PanelDescriptor {
            firmware: FirmwareVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 7,
            },
            boot_id: 99,
            encoders: 4,
            buttons: 8,
            leds: 8,
            rings: 1,
            ring_segments: 32,
        }
    }

    fn config() -> LinkConfig {
        LinkConfig::new(100, 300).unwrap()
    }

    fn connect(runtime: &mut RuntimeLink, panel: &mut PanelLink, session: u64, now: u64) {
        let hello = runtime.start(session, now).unwrap();
        let PanelReceive::SessionEstablished(reply) = panel.receive(hello, now).unwrap() else {
            panic!("panel did not establish the session");
        };
        assert_eq!(
            runtime.receive(reply, now).unwrap(),
            RuntimeReceive::Connected(descriptor())
        );
    }

    #[test]
    fn negotiation_establishes_bidirectional_sequencing() {
        let mut runtime = RuntimeLink::new(config());
        let mut panel = PanelLink::new(descriptor()).unwrap();
        connect(&mut runtime, &mut panel, 42, 1_000);

        let snapshot = panel
            .send(
                Message::InputSnapshot(InputState {
                    timestamp_us: 1_000_000,
                    encoder_pressed: 0,
                    button_pressed: 0,
                    modifiers: 0,
                }),
                1_000,
            )
            .unwrap();
        assert_eq!(snapshot.sequence, 1);
        assert!(matches!(
            runtime.receive(snapshot, 1_000).unwrap(),
            RuntimeReceive::Message(Message::InputSnapshot(_))
        ));

        let command = runtime
            .send(Message::SetGlobalBrightness { brightness: 128 }, 1_001)
            .unwrap();
        assert_eq!(command.sequence, 1);
        assert_eq!(
            panel.receive(command, 1_001).unwrap(),
            PanelReceive::Command(Message::SetGlobalBrightness { brightness: 128 })
        );
    }

    #[test]
    fn sequence_gap_disconnects_before_delivering_the_message() {
        let mut runtime = RuntimeLink::new(config());
        let mut panel = PanelLink::new(descriptor()).unwrap();
        connect(&mut runtime, &mut panel, 42, 0);
        let skipped = Frame::application(
            PROTOCOL_VERSION,
            42,
            2,
            Message::ButtonPress {
                control_id: 0,
                timestamp_us: 1,
                modifiers: 0,
            },
        )
        .unwrap();
        assert_eq!(
            runtime.receive(skipped, 1),
            Err(LinkFault::SequenceMismatch {
                expected: 1,
                received: 2,
            })
        );
        assert!(!runtime.is_connected());
    }

    #[test]
    fn wrong_direction_and_protocol_are_rejected_before_delivery() {
        let mut runtime = RuntimeLink::new(config());
        let mut panel = PanelLink::new(descriptor()).unwrap();
        connect(&mut runtime, &mut panel, 42, 0);
        let wrong_direction = Frame::application(
            PROTOCOL_VERSION,
            42,
            1,
            Message::SetGlobalBrightness { brightness: 1 },
        )
        .unwrap();
        assert_eq!(
            runtime.receive(wrong_direction, 1),
            Err(LinkFault::WrongDirection)
        );

        connect(&mut runtime, &mut panel, 43, 2);
        let wrong_protocol = Frame::application(
            PROTOCOL_VERSION + 1,
            43,
            1,
            Message::ButtonPress {
                control_id: 0,
                timestamp_us: 2_000,
                modifiers: 0,
            },
        )
        .unwrap();
        assert_eq!(
            runtime.receive(wrong_protocol, 2),
            Err(LinkFault::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                received: PROTOCOL_VERSION + 1,
            })
        );
    }

    #[test]
    fn reconnect_replaces_the_session_and_resets_sequences() {
        let mut runtime = RuntimeLink::new(config());
        let mut panel = PanelLink::new(descriptor()).unwrap();
        connect(&mut runtime, &mut panel, 42, 0);
        let first = runtime
            .send(Message::SetGlobalBrightness { brightness: 1 }, 1)
            .unwrap();
        assert_eq!(first.sequence, 1);
        panel.receive(first, 1).unwrap();

        connect(&mut runtime, &mut panel, 43, 2);
        let replacement = runtime
            .send(Message::SetGlobalBrightness { brightness: 2 }, 3)
            .unwrap();
        assert_eq!(replacement.session_id, 43);
        assert_eq!(replacement.sequence, 1);
        panel.receive(replacement, 3).unwrap();

        let stale = Frame::application(
            PROTOCOL_VERSION,
            42,
            2,
            Message::SetGlobalBrightness { brightness: 3 },
        )
        .unwrap();
        assert!(matches!(
            panel.receive(stale, 4),
            Err(LinkFault::SessionMismatch { .. })
        ));
    }

    #[test]
    fn incompatible_versions_return_an_explicit_rejection() {
        let mut panel = PanelLink::new(descriptor()).unwrap();
        let hello = Frame::handshake(
            7,
            Message::RuntimeHello {
                versions: SupportedVersions::new(2, 2).unwrap(),
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 300,
            },
        )
        .unwrap();
        let PanelReceive::VersionRejected(frame) = panel.receive(hello, 0).unwrap() else {
            panic!("panel did not reject the incompatible version");
        };
        assert_eq!(
            frame.message,
            Message::VersionRejected {
                supported: SupportedVersions::CURRENT,
            }
        );
        assert!(!panel.is_connected());
    }

    #[test]
    fn heartbeat_deadline_is_fail_safe_and_precedes_transmission() {
        let mut runtime = RuntimeLink::new(config());
        let mut panel = PanelLink::new(descriptor()).unwrap();
        connect(&mut runtime, &mut panel, 42, 0);
        assert!(matches!(runtime.poll(100).unwrap(), LinkPoll::Send(_)));
        assert_eq!(runtime.poll(300), Ok(LinkPoll::TimedOut));
        assert!(!runtime.is_connected());
        assert_eq!(runtime.last_fault(), Some(LinkFault::HeartbeatTimedOut));
    }

    #[test]
    fn monotonic_clock_and_deadline_overflow_are_rejected() {
        let mut runtime = RuntimeLink::new(config());
        runtime.start(1, 100).unwrap();
        assert_eq!(runtime.poll(99), Err(LinkFault::ClockRegression));

        let mut runtime = RuntimeLink::new(config());
        assert_eq!(
            runtime.start(1, u64::MAX - 10),
            Err(LinkFault::ClockExhausted)
        );
    }

    #[test]
    fn sequence_wrap_skips_the_reserved_zero_value() {
        assert_eq!(next_sequence(u32::MAX), 1);
        assert_eq!(previous_sequence(1), 0);
    }
}
