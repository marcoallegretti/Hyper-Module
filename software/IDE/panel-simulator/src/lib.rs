#![doc = "Deterministic desktop implementation of the panel UART peer."]

use std::fmt;

use hyper_panel_protocol::{
    Animation, DecodeError, DecoderEvent, EncodeError, InputState, LedBlink, LinkFault, LinkPoll,
    MAX_BUTTONS, MAX_ENCODED_FRAME_LEN, MAX_ENCODERS, MAX_LEDS, MAX_RING_SEGMENTS, MAX_RINGS,
    Message, PanelDescriptor, PanelErrorCode, PanelLink, PanelReceive, RingSegment, StreamDecoder,
    encode_frame,
};

pub const TRANSMIT_FRAME_CAPACITY: usize = 16;
pub const TRANSMIT_BYTE_CAPACITY: usize = MAX_ENCODED_FRAME_LEN * TRANSMIT_FRAME_CAPACITY;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelSimulatorConfig {
    pub descriptor: PanelDescriptor,
    pub bootloader_available: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelSimulatorConfigError {
    InvalidDescriptor,
}

impl fmt::Display for PanelSimulatorConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("panel simulator descriptor is invalid")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelSimulatorError {
    Link(LinkFault),
    Encode(EncodeError),
    UnsupportedControl,
    InvalidStateTransition,
    TimestampOverflow,
    TransmitOverflow,
}

impl fmt::Display for PanelSimulatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(error) => write!(formatter, "panel simulator link failed: {error}"),
            Self::Encode(error) => write!(formatter, "panel simulator encoding failed: {error}"),
            Self::UnsupportedControl => {
                formatter.write_str("panel simulator control is unsupported")
            }
            Self::InvalidStateTransition => {
                formatter.write_str("panel simulator input transition is invalid")
            }
            Self::TimestampOverflow => {
                formatter.write_str("panel simulator timestamp exceeds microsecond range")
            }
            Self::TransmitOverflow => formatter.write_str("panel simulator transmit queue is full"),
        }
    }
}

impl From<LinkFault> for PanelSimulatorError {
    fn from(value: LinkFault) -> Self {
        Self::Link(value)
    }
}

impl From<EncodeError> for PanelSimulatorError {
    fn from(value: EncodeError) -> Self {
        Self::Encode(value)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PanelSimulatorTelemetry {
    pub decoded_frames: u64,
    pub decode_errors: u64,
    pub checksum_errors: u64,
    pub protocol_faults: u64,
    pub sessions_established: u64,
    pub version_rejections: u64,
    pub commands_applied: u64,
    pub received_heartbeats: u64,
    pub unsupported_commands: u64,
    pub transmit_overflows: u64,
    pub heartbeat_timeouts: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceiveReport {
    pub decoded_frames: u64,
    pub decode_errors: u64,
    pub protocol_faults: u64,
    pub connected: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LedState {
    pub enabled: bool,
    pub brightness: u8,
    pub blink: Option<LedBlinkTiming>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedBlinkTiming {
    pub on_ms: u16,
    pub off_ms: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingState {
    pub segments: [u8; MAX_RING_SEGMENTS as usize],
    pub value: u16,
    pub animation: Animation,
    pub animation_period_ms: u16,
}

impl Default for RingState {
    fn default() -> Self {
        Self {
            segments: [0; MAX_RING_SEGMENTS as usize],
            value: 0,
            animation: Animation::Off,
            animation_period_ms: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelOutputState {
    leds: [LedState; MAX_LEDS as usize],
    rings: [RingState; MAX_RINGS as usize],
    global_brightness: u8,
    led_count: u8,
    ring_count: u8,
    ring_segments: u8,
}

impl Default for PanelOutputState {
    fn default() -> Self {
        Self {
            leds: [LedState::default(); MAX_LEDS as usize],
            rings: [RingState::default(); MAX_RINGS as usize],
            global_brightness: 0,
            led_count: 0,
            ring_count: 0,
            ring_segments: 0,
        }
    }
}

impl PanelOutputState {
    fn for_descriptor(descriptor: PanelDescriptor) -> Self {
        Self {
            led_count: descriptor.leds,
            ring_count: descriptor.rings,
            ring_segments: descriptor.ring_segments,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn led(&self, index: usize) -> Option<LedState> {
        if index >= usize::from(self.led_count) {
            return None;
        }
        self.leds.get(index).copied()
    }

    #[must_use]
    pub fn ring(&self, index: usize) -> Option<RingState> {
        if index >= usize::from(self.ring_count) {
            return None;
        }
        self.rings.get(index).copied()
    }

    #[must_use]
    pub const fn ring_segments(&self) -> u8 {
        self.ring_segments
    }

    #[must_use]
    pub const fn global_brightness(&self) -> u8 {
        self.global_brightness
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SimulatedInputState {
    encoder_pressed: u16,
    button_pressed: u32,
    modifiers: u16,
}

struct ByteQueue<const N: usize> {
    bytes: [u8; N],
    head: usize,
    length: usize,
}

impl<const N: usize> ByteQueue<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            head: 0,
            length: 0,
        }
    }

    fn clear(&mut self) {
        self.head = 0;
        self.length = 0;
    }

    const fn len(&self) -> usize {
        self.length
    }

    fn push_all(&mut self, input: &[u8]) -> bool {
        if input.len() > N - self.length {
            return false;
        }
        for byte in input {
            let tail = (self.head + self.length) % N;
            self.bytes[tail] = *byte;
            self.length += 1;
        }
        true
    }

    fn drain(&mut self, output: &mut [u8]) -> usize {
        let count = output.len().min(self.length);
        for destination in &mut output[..count] {
            *destination = self.bytes[self.head];
            self.head = (self.head + 1) % N;
            self.length -= 1;
        }
        count
    }
}

pub struct PanelSimulator {
    descriptor: PanelDescriptor,
    bootloader_available: bool,
    bootloader_requested: bool,
    link: PanelLink,
    decoder: StreamDecoder,
    transmit: ByteQueue<TRANSMIT_BYTE_CAPACITY>,
    inputs: SimulatedInputState,
    outputs: PanelOutputState,
    telemetry: PanelSimulatorTelemetry,
}

impl PanelSimulator {
    /// Creates a deterministic panel peer with fixed resource bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when the descriptor cannot be represented by protocol
    /// version 1.
    pub fn new(config: PanelSimulatorConfig) -> Result<Self, PanelSimulatorConfigError> {
        let link = PanelLink::new(config.descriptor)
            .ok_or(PanelSimulatorConfigError::InvalidDescriptor)?;
        Ok(Self {
            descriptor: config.descriptor,
            bootloader_available: config.bootloader_available,
            bootloader_requested: false,
            link,
            decoder: StreamDecoder::new(),
            transmit: ByteQueue::new(),
            inputs: SimulatedInputState::default(),
            outputs: PanelOutputState::for_descriptor(config.descriptor),
            telemetry: PanelSimulatorTelemetry::default(),
        })
    }

    pub fn receive_runtime_bytes(&mut self, input: &[u8], now_ms: u64) -> ReceiveReport {
        let before = self.telemetry;
        for byte in input {
            match self.decoder.push(*byte) {
                DecoderEvent::Pending => {}
                DecoderEvent::Error(error) => self.record_decode_error(error),
                DecoderEvent::Frame(frame) => {
                    self.telemetry.decoded_frames += 1;
                    self.receive_frame(frame, now_ms);
                }
            }
        }
        ReceiveReport {
            decoded_frames: self.telemetry.decoded_frames - before.decoded_frames,
            decode_errors: self.telemetry.decode_errors - before.decode_errors,
            protocol_faults: self.telemetry.protocol_faults - before.protocol_faults,
            connected: self.link.is_connected(),
        }
    }

    /// Advances heartbeat and watchdog behavior to `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error if monotonic time is invalid, a heartbeat cannot be
    /// encoded, or the fixed transmit queue is exhausted.
    pub fn poll(&mut self, now_ms: u64) -> Result<(), PanelSimulatorError> {
        match self.link.poll(now_ms) {
            Ok(LinkPoll::Idle) => Ok(()),
            Ok(LinkPoll::TimedOut) => {
                self.telemetry.heartbeat_timeouts += 1;
                self.outputs = PanelOutputState::for_descriptor(self.descriptor);
                Ok(())
            }
            Ok(LinkPoll::Send(frame)) => match self.enqueue_frame(frame) {
                Ok(()) => Ok(()),
                Err(error) => {
                    self.contain_transmit_error(error);
                    Err(error)
                }
            },
            Err(error) => {
                self.record_protocol_fault(error);
                Err(error.into())
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.link.disconnect();
        self.decoder.reset();
        self.transmit.clear();
        self.outputs = PanelOutputState::for_descriptor(self.descriptor);
    }

    pub fn read_panel_bytes(&mut self, output: &mut [u8]) -> usize {
        self.transmit.drain(output)
    }

    #[must_use]
    pub const fn pending_panel_bytes(&self) -> usize {
        self.transmit.len()
    }

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.link.is_connected()
    }

    #[must_use]
    pub const fn output_state(&self) -> &PanelOutputState {
        &self.outputs
    }

    #[must_use]
    pub const fn telemetry(&self) -> PanelSimulatorTelemetry {
        self.telemetry
    }

    #[must_use]
    pub const fn bootloader_requested(&self) -> bool {
        self.bootloader_requested
    }

    pub fn set_modifiers(&mut self, modifiers: u16) {
        self.inputs.modifiers = modifiers;
    }

    /// Emits one nonzero encoder delta through the live protocol session.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported encoder, zero delta, invalid time,
    /// disconnected link, encoding failure, or transmit overflow.
    pub fn encoder_delta(
        &mut self,
        control_id: u16,
        delta: i16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_encoder(control_id)?;
        if delta == 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.transmit_message(
            Message::EncoderDelta {
                control_id,
                delta,
                timestamp_us: timestamp_us(now_ms)?,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Presses an encoder switch and emits its transition.
    ///
    /// # Errors
    ///
    /// The physical pressed state is retained when delivery fails so a later
    /// reconnect snapshot remains truthful. Returns an error when the control
    /// is unsupported, already pressed, the clock or link is invalid, encoding
    /// fails, or transmit capacity is full.
    pub fn encoder_press(
        &mut self,
        control_id: u16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_encoder(control_id)?;
        let timestamp_us = timestamp_us(now_ms)?;
        let bit = 1_u16 << control_id;
        if self.inputs.encoder_pressed & bit != 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.inputs.encoder_pressed |= bit;
        self.transmit_message(
            Message::EncoderPress {
                control_id,
                timestamp_us,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Releases an encoder switch and emits its transition.
    ///
    /// # Errors
    ///
    /// The physical released state is retained when delivery fails so a later
    /// reconnect snapshot remains truthful. Returns an error when the control
    /// is unsupported, not pressed, the clock or link is invalid, encoding
    /// fails, or transmit capacity is full.
    pub fn encoder_release(
        &mut self,
        control_id: u16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_encoder(control_id)?;
        let timestamp_us = timestamp_us(now_ms)?;
        let bit = 1_u16 << control_id;
        if self.inputs.encoder_pressed & bit == 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.inputs.encoder_pressed &= !bit;
        self.transmit_message(
            Message::EncoderRelease {
                control_id,
                timestamp_us,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Presses a button and emits its transition.
    ///
    /// # Errors
    ///
    /// The physical pressed state is retained when delivery fails so a later
    /// reconnect snapshot remains truthful. Returns an error when the control
    /// is unsupported, already pressed, the clock or link is invalid, encoding
    /// fails, or transmit capacity is full.
    pub fn button_press(
        &mut self,
        control_id: u16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_button(control_id)?;
        let timestamp_us = timestamp_us(now_ms)?;
        let bit = 1_u32 << control_id;
        if self.inputs.button_pressed & bit != 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.inputs.button_pressed |= bit;
        self.transmit_message(
            Message::ButtonPress {
                control_id,
                timestamp_us,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Releases a button and emits its transition.
    ///
    /// # Errors
    ///
    /// The physical released state is retained when delivery fails so a later
    /// reconnect snapshot remains truthful. Returns an error when the control
    /// is unsupported, not pressed, the clock or link is invalid, encoding
    /// fails, or transmit capacity is full.
    pub fn button_release(
        &mut self,
        control_id: u16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_button(control_id)?;
        let timestamp_us = timestamp_us(now_ms)?;
        let bit = 1_u32 << control_id;
        if self.inputs.button_pressed & bit == 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.inputs.button_pressed &= !bit;
        self.transmit_message(
            Message::ButtonRelease {
                control_id,
                timestamp_us,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Emits a long-press event for a currently held button.
    ///
    /// # Errors
    ///
    /// Returns an error when the control is unsupported or not pressed, the
    /// duration is zero, time or link state is invalid, encoding fails, or the
    /// transmit queue is full.
    pub fn button_long_press(
        &mut self,
        control_id: u16,
        held_ms: u32,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_button(control_id)?;
        if held_ms == 0 || self.inputs.button_pressed & (1_u32 << control_id) == 0 {
            return Err(PanelSimulatorError::InvalidStateTransition);
        }
        self.transmit_message(
            Message::ButtonLongPress {
                control_id,
                held_ms,
                timestamp_us: timestamp_us(now_ms)?,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    /// Emits a double-press event for a supported button.
    ///
    /// # Errors
    ///
    /// Returns an error when the control is unsupported, time or link state is
    /// invalid, encoding fails, or the transmit queue is full.
    pub fn button_double_press(
        &mut self,
        control_id: u16,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        self.validate_button(control_id)?;
        self.transmit_message(
            Message::ButtonDoublePress {
                control_id,
                timestamp_us: timestamp_us(now_ms)?,
                modifiers: self.inputs.modifiers,
            },
            now_ms,
        )
    }

    fn receive_frame(&mut self, frame: hyper_panel_protocol::Frame, now_ms: u64) {
        match self.link.receive(frame, now_ms) {
            Ok(PanelReceive::SessionEstablished(reply)) => {
                self.telemetry.sessions_established += 1;
                self.bootloader_requested = false;
                self.outputs = PanelOutputState::for_descriptor(self.descriptor);
                self.transmit.clear();
                if let Err(error) = self.enqueue_frame(reply) {
                    self.contain_transmit_error(error);
                    return;
                }
                let _ = self.enqueue_snapshot(now_ms);
            }
            Ok(PanelReceive::VersionRejected(reply)) => {
                self.telemetry.version_rejections += 1;
                self.outputs = PanelOutputState::for_descriptor(self.descriptor);
                self.transmit.clear();
                if let Err(error) = self.enqueue_frame(reply) {
                    self.contain_transmit_error(error);
                }
            }
            Ok(PanelReceive::Command(Message::Heartbeat { .. })) => {
                self.telemetry.received_heartbeats += 1;
            }
            Ok(PanelReceive::Command(message)) => self.apply_command(message, now_ms),
            Err(error) => self.record_protocol_fault(error),
        }
    }

    fn apply_command(&mut self, message: Message, now_ms: u64) {
        let result = match message {
            Message::SetLed {
                control_id,
                enabled,
            } => self.led_mut(control_id).map(|led| led.enabled = enabled),
            Message::SetLedBrightness {
                control_id,
                brightness,
            } => self
                .led_mut(control_id)
                .map(|led| led.brightness = brightness),
            Message::SetLedBlink(LedBlink {
                control_id,
                on_ms,
                off_ms,
            }) => self.led_mut(control_id).map(|led| {
                led.blink = (on_ms != 0).then_some(LedBlinkTiming { on_ms, off_ms });
            }),
            Message::SetRingSegment(RingSegment {
                ring_id,
                segment,
                brightness,
            }) => self
                .ring_segment_mut(ring_id, segment)
                .map(|value| *value = brightness),
            Message::SetRingValue { ring_id, value } => {
                self.ring_mut(ring_id).map(|ring| ring.value = value)
            }
            Message::SetRingAnimation {
                ring_id,
                animation,
                period_ms,
            } => self.ring_mut(ring_id).map(|ring| {
                ring.animation = animation;
                ring.animation_period_ms = period_ms;
            }),
            Message::SetGlobalBrightness { brightness } => {
                self.outputs.global_brightness = brightness;
                Ok(())
            }
            Message::EnterBootloader { .. } if self.bootloader_available => {
                self.bootloader_requested = true;
                self.outputs = PanelOutputState::for_descriptor(self.descriptor);
                self.link.disconnect();
                Ok(())
            }
            Message::EnterBootloader { .. } => {
                self.telemetry.unsupported_commands += 1;
                self.send_command_error(PanelErrorCode::BootloaderUnavailable, 0, now_ms);
                return;
            }
            _ => {
                self.record_protocol_fault(LinkFault::WrongDirection);
                return;
            }
        };
        if result.is_ok() {
            self.telemetry.commands_applied += 1;
        } else {
            self.telemetry.unsupported_commands += 1;
            self.send_command_error(
                PanelErrorCode::UnsupportedControl,
                command_detail(message),
                now_ms,
            );
        }
    }

    fn send_command_error(&mut self, code: PanelErrorCode, detail: u32, now_ms: u64) {
        let _ = self.transmit_message(Message::PanelError { code, detail }, now_ms);
    }

    fn enqueue_snapshot(&mut self, now_ms: u64) -> Result<(), PanelSimulatorError> {
        let message = Message::InputSnapshot(InputState {
            timestamp_us: timestamp_us(now_ms)?,
            encoder_pressed: self.inputs.encoder_pressed,
            button_pressed: self.inputs.button_pressed,
            modifiers: self.inputs.modifiers,
        });
        self.transmit_message(message, now_ms)
    }

    fn transmit_message(
        &mut self,
        message: Message,
        now_ms: u64,
    ) -> Result<(), PanelSimulatorError> {
        let result = self
            .link
            .send(message, now_ms)
            .map_err(PanelSimulatorError::from)
            .and_then(|frame| self.enqueue_frame(frame));
        if let Err(error) = result {
            self.contain_transmit_error(error);
        }
        result
    }

    fn enqueue_frame(
        &mut self,
        frame: hyper_panel_protocol::Frame,
    ) -> Result<(), PanelSimulatorError> {
        let mut encoded = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut encoded)?;
        if !self.transmit.push_all(&encoded[..length]) {
            return Err(PanelSimulatorError::TransmitOverflow);
        }
        Ok(())
    }

    fn record_decode_error(&mut self, error: DecodeError) {
        self.telemetry.decode_errors += 1;
        if error == DecodeError::ChecksumMismatch {
            self.telemetry.checksum_errors += 1;
        }
    }

    fn record_protocol_fault(&mut self, _error: LinkFault) {
        self.telemetry.protocol_faults += 1;
        self.link.disconnect();
        self.transmit.clear();
        self.outputs = PanelOutputState::for_descriptor(self.descriptor);
    }

    fn record_transmit_overflow(&mut self) {
        self.telemetry.transmit_overflows += 1;
        self.link.disconnect();
        self.transmit.clear();
        self.outputs = PanelOutputState::for_descriptor(self.descriptor);
    }

    fn contain_transmit_error(&mut self, error: PanelSimulatorError) {
        if error == PanelSimulatorError::TransmitOverflow {
            self.record_transmit_overflow();
        } else {
            self.telemetry.protocol_faults += 1;
            self.link.disconnect();
            self.transmit.clear();
            self.outputs = PanelOutputState::for_descriptor(self.descriptor);
        }
    }

    fn validate_encoder(&self, control_id: u16) -> Result<(), PanelSimulatorError> {
        if control_id < u16::from(self.descriptor.encoders) && control_id < u16::from(MAX_ENCODERS)
        {
            Ok(())
        } else {
            Err(PanelSimulatorError::UnsupportedControl)
        }
    }

    fn validate_button(&self, control_id: u16) -> Result<(), PanelSimulatorError> {
        if control_id < u16::from(self.descriptor.buttons) && control_id < u16::from(MAX_BUTTONS) {
            Ok(())
        } else {
            Err(PanelSimulatorError::UnsupportedControl)
        }
    }

    fn led_mut(&mut self, control_id: u16) -> Result<&mut LedState, ()> {
        if control_id >= u16::from(self.descriptor.leds) {
            return Err(());
        }
        self.outputs.leds.get_mut(usize::from(control_id)).ok_or(())
    }

    fn ring_mut(&mut self, ring_id: u8) -> Result<&mut RingState, ()> {
        if ring_id >= self.descriptor.rings {
            return Err(());
        }
        self.outputs.rings.get_mut(usize::from(ring_id)).ok_or(())
    }

    fn ring_segment_mut(&mut self, ring_id: u8, segment: u8) -> Result<&mut u8, ()> {
        if segment >= self.descriptor.ring_segments {
            return Err(());
        }
        self.ring_mut(ring_id)?
            .segments
            .get_mut(usize::from(segment))
            .ok_or(())
    }
}

fn timestamp_us(now_ms: u64) -> Result<u64, PanelSimulatorError> {
    now_ms
        .checked_mul(1_000)
        .ok_or(PanelSimulatorError::TimestampOverflow)
}

const fn command_detail(message: Message) -> u32 {
    match message {
        Message::SetLed { control_id, .. }
        | Message::SetLedBrightness { control_id, .. }
        | Message::SetLedBlink(LedBlink { control_id, .. }) => control_id as u32,
        Message::SetRingSegment(RingSegment {
            ring_id, segment, ..
        }) => ((ring_id as u32) << 8) | segment as u32,
        Message::SetRingValue { ring_id, .. } | Message::SetRingAnimation { ring_id, .. } => {
            ring_id as u32
        }
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_panel_protocol::{
        DecoderEvent, FirmwareVersion, Frame, LinkConfig, PROTOCOL_VERSION, RuntimeLink,
        RuntimeReceive, SupportedVersions,
    };

    fn simulator(bootloader_available: bool) -> PanelSimulator {
        PanelSimulator::new(PanelSimulatorConfig {
            descriptor: PanelDescriptor {
                firmware: FirmwareVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    build: 7,
                },
                boot_id: 1,
                encoders: 4,
                buttons: 8,
                leds: 12,
                rings: 1,
                ring_segments: 32,
            },
            bootloader_available,
        })
        .unwrap()
    }

    fn encode(frame: Frame) -> ([u8; MAX_ENCODED_FRAME_LEN], usize) {
        let mut bytes = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut bytes).unwrap();
        (bytes, length)
    }

    fn drain_frames(simulator: &mut PanelSimulator) -> Vec<Frame> {
        let mut bytes = vec![0_u8; simulator.pending_panel_bytes()];
        let length = simulator.read_panel_bytes(&mut bytes);
        let mut decoder = StreamDecoder::new();
        let mut frames = Vec::new();
        for byte in &bytes[..length] {
            if let DecoderEvent::Frame(frame) = decoder.push(*byte) {
                frames.push(frame);
            }
        }
        frames
    }

    fn connect(
        simulator: &mut PanelSimulator,
        runtime: &mut RuntimeLink,
        session_id: u64,
        now_ms: u64,
    ) -> InputState {
        let hello = runtime.start(session_id, now_ms).unwrap();
        let (bytes, length) = encode(hello);
        let report = simulator.receive_runtime_bytes(&bytes[..length], now_ms);
        assert_eq!(report.decoded_frames, 1);
        assert!(report.connected);

        let frames = drain_frames(simulator);
        assert_eq!(frames.len(), 2);
        assert!(matches!(
            runtime.receive(frames[0], now_ms).unwrap(),
            RuntimeReceive::Connected(_)
        ));
        let RuntimeReceive::Message(Message::InputSnapshot(snapshot)) =
            runtime.receive(frames[1], now_ms).unwrap()
        else {
            panic!("reconnect snapshot was not emitted");
        };
        snapshot
    }

    #[test]
    fn byte_stream_handshake_precedes_snapshot_and_commands_update_outputs() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        assert_eq!(
            connect(&mut simulator, &mut runtime, 10, 0),
            InputState {
                timestamp_us: 0,
                encoder_pressed: 0,
                button_pressed: 0,
                modifiers: 0,
            }
        );

        let command = runtime
            .send(
                Message::SetLedBrightness {
                    control_id: 2,
                    brightness: 192,
                },
                1,
            )
            .unwrap();
        let (bytes, length) = encode(command);
        simulator.receive_runtime_bytes(&bytes[..length], 1);
        assert_eq!(simulator.output_state().led(2).unwrap().brightness, 192);
        assert_eq!(simulator.telemetry().commands_applied, 1);
    }

    #[test]
    fn input_transitions_are_validated_and_delivered_in_sequence() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        simulator.set_modifiers(0b101);
        simulator.encoder_press(1, 2).unwrap();
        assert_eq!(
            simulator.encoder_press(1, 2),
            Err(PanelSimulatorError::InvalidStateTransition)
        );
        simulator.encoder_delta(1, -3, 3).unwrap();
        simulator.encoder_release(1, 4).unwrap();

        let frames = drain_frames(&mut simulator);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].sequence, 2);
        assert_eq!(frames[2].sequence, 4);
        for frame in frames {
            assert!(matches!(
                runtime.receive(frame, 4).unwrap(),
                RuntimeReceive::Message(_)
            ));
        }
    }

    #[test]
    fn reconnect_clears_outputs_and_reports_current_held_inputs() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        simulator.button_press(3, 1).unwrap();
        drain_frames(&mut simulator);
        let command = runtime
            .send(Message::SetGlobalBrightness { brightness: 200 }, 2)
            .unwrap();
        let (bytes, length) = encode(command);
        simulator.receive_runtime_bytes(&bytes[..length], 2);
        assert_eq!(simulator.output_state().global_brightness(), 200);

        let snapshot = connect(&mut simulator, &mut runtime, 11, 3);
        assert_eq!(snapshot.button_pressed, 1 << 3);
        assert_eq!(simulator.output_state().global_brightness(), 0);
        assert_eq!(simulator.output_state().led(0), Some(LedState::default()));
    }

    #[test]
    fn controls_changed_while_disconnected_are_reported_on_connect() {
        let mut simulator = simulator(false);
        assert!(matches!(
            simulator.button_press(3, 1),
            Err(PanelSimulatorError::Link(LinkFault::UnexpectedState))
        ));
        assert!(matches!(
            simulator.encoder_press(1, 1),
            Err(PanelSimulatorError::Link(LinkFault::UnexpectedState))
        ));

        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        let snapshot = connect(&mut simulator, &mut runtime, 10, 2);
        assert_eq!(snapshot.button_pressed, 1 << 3);
        assert_eq!(snapshot.encoder_pressed, 1 << 1);
    }

    #[test]
    fn checksum_error_then_sequence_gap_drives_outputs_safe() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        let first = runtime
            .send(Message::SetGlobalBrightness { brightness: 200 }, 1)
            .unwrap();
        let (mut bytes, length) = encode(first);
        bytes[6] ^= 0x20;
        let report = simulator.receive_runtime_bytes(&bytes[..length], 1);
        assert_eq!(report.decode_errors, 1);

        let second = runtime
            .send(Message::SetGlobalBrightness { brightness: 201 }, 2)
            .unwrap();
        let (bytes, length) = encode(second);
        let report = simulator.receive_runtime_bytes(&bytes[..length], 2);
        assert_eq!(report.protocol_faults, 1);
        assert!(!simulator.is_connected());
        assert_eq!(simulator.output_state().global_brightness(), 0);
    }

    #[test]
    fn heartbeat_timeout_is_fail_safe_at_the_exact_boundary() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        let command = runtime
            .send(Message::SetGlobalBrightness { brightness: 200 }, 1)
            .unwrap();
        let (bytes, length) = encode(command);
        simulator.receive_runtime_bytes(&bytes[..length], 1);
        simulator.poll(100).unwrap();
        assert!(simulator.is_connected());
        simulator.poll(301).unwrap();
        assert!(!simulator.is_connected());
        assert_eq!(simulator.output_state().global_brightness(), 0);
        assert_eq!(simulator.output_state().led(0), Some(LedState::default()));
        assert_eq!(simulator.telemetry().heartbeat_timeouts, 1);
    }

    #[test]
    fn unsupported_commands_return_a_panel_error_without_mutating_state() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        let command = Frame::application(
            PROTOCOL_VERSION,
            10,
            1,
            Message::SetLed {
                control_id: 31,
                enabled: true,
            },
        )
        .unwrap();
        let (bytes, length) = encode(command);
        simulator.receive_runtime_bytes(&bytes[..length], 1);
        let frames = drain_frames(&mut simulator);
        assert_eq!(frames.len(), 1);
        assert!(matches!(
            runtime.receive(frames[0], 1).unwrap(),
            RuntimeReceive::Message(Message::PanelError {
                code: PanelErrorCode::UnsupportedControl,
                detail: 31,
            })
        ));
        assert_eq!(simulator.telemetry().unsupported_commands, 1);
    }

    #[test]
    fn transmit_overflow_disconnects_and_discards_partial_output() {
        let mut simulator = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut simulator, &mut runtime, 10, 0);
        let mut result = Ok(());
        for now_ms in 1..=u64::try_from(TRANSMIT_FRAME_CAPACITY * 3).unwrap() {
            result = simulator.encoder_delta(0, 1, now_ms);
            if result.is_err() {
                break;
            }
        }
        assert_eq!(result, Err(PanelSimulatorError::TransmitOverflow));
        assert!(!simulator.is_connected());
        assert_eq!(simulator.pending_panel_bytes(), 0);
        assert_eq!(simulator.telemetry().transmit_overflows, 1);
    }

    #[test]
    fn bootloader_policy_is_explicit() {
        let mut unavailable = simulator(false);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut unavailable, &mut runtime, 10, 0);
        let command = runtime
            .send(
                Message::EnterBootloader {
                    confirmation: Message::BOOTLOADER_CONFIRMATION,
                },
                1,
            )
            .unwrap();
        let (bytes, length) = encode(command);
        unavailable.receive_runtime_bytes(&bytes[..length], 1);
        assert!(!unavailable.bootloader_requested());
        assert!(unavailable.is_connected());

        let mut available = simulator(true);
        let mut runtime = RuntimeLink::new(LinkConfig::new(100, 300).unwrap());
        connect(&mut available, &mut runtime, 20, 0);
        let command = runtime
            .send(
                Message::EnterBootloader {
                    confirmation: Message::BOOTLOADER_CONFIRMATION,
                },
                1,
            )
            .unwrap();
        let (bytes, length) = encode(command);
        available.receive_runtime_bytes(&bytes[..length], 1);
        assert!(available.bootloader_requested());
        assert!(!available.is_connected());
    }

    #[test]
    fn unsupported_version_is_returned_on_the_byte_stream() {
        let mut simulator = simulator(false);
        let hello = Frame::handshake(
            90,
            Message::RuntimeHello {
                versions: SupportedVersions::new(2, 2).unwrap(),
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 300,
            },
        )
        .unwrap();
        let (bytes, length) = encode(hello);
        simulator.receive_runtime_bytes(&bytes[..length], 0);
        let frames = drain_frames(&mut simulator);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].protocol_version, 0);
        assert_eq!(frames[0].sequence, 0);
        assert!(matches!(frames[0].message, Message::VersionRejected { .. }));
        assert_eq!(simulator.telemetry().version_rejections, 1);
        assert_ne!(PROTOCOL_VERSION, 2);
    }
}
