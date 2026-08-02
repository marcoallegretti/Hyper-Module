use core::fmt;

use crc::{CRC_32_ISCSI, Crc};

use crate::message::{
    Animation, FirmwareVersion, InputState, LedBlink, Message, PanelDescriptor, PanelErrorCode,
    RingSegment, SupportedVersions,
};
use crate::{
    CHECKSUM_LEN, FRAME_DELIMITER, Frame, HEADER_LEN, MAGIC, MAX_DECODED_FRAME_LEN,
    MAX_ENCODED_FRAME_LEN, MAX_PAYLOAD_LEN, WIRE_VERSION,
};

const CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    InvalidFrame,
    InvalidMessage,
    BufferTooSmall,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidFrame => "panel frame is structurally invalid",
            Self::InvalidMessage => "panel message contains an invalid value",
            Self::BufferTooSmall => "panel frame output buffer is too small",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeError {
    FrameTooLong,
    MalformedCobs,
    FrameTooShort,
    InvalidMagic,
    UnsupportedWireVersion(u8),
    ReservedFlags(u8),
    PayloadLengthMismatch,
    ChecksumMismatch,
    InvalidFrame,
    UnknownMessage(u8),
    InvalidPayload,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLong => formatter.write_str("panel frame exceeds the fixed wire limit"),
            Self::MalformedCobs => formatter.write_str("panel frame has malformed COBS encoding"),
            Self::FrameTooShort => formatter.write_str("panel frame is shorter than its envelope"),
            Self::InvalidMagic => formatter.write_str("panel frame magic is invalid"),
            Self::UnsupportedWireVersion(version) => {
                write!(formatter, "panel wire version {version} is unsupported")
            }
            Self::ReservedFlags(flags) => {
                write!(formatter, "panel frame uses reserved flags 0x{flags:02x}")
            }
            Self::PayloadLengthMismatch => {
                formatter.write_str("panel payload length does not match its envelope")
            }
            Self::ChecksumMismatch => formatter.write_str("panel frame checksum failed"),
            Self::InvalidFrame => formatter.write_str("panel frame is structurally invalid"),
            Self::UnknownMessage(kind) => {
                write!(formatter, "panel message kind 0x{kind:02x} is unsupported")
            }
            Self::InvalidPayload => formatter.write_str("panel message payload is invalid"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecoderEvent {
    Pending,
    Frame(Frame),
    Error(DecodeError),
}

pub struct StreamDecoder {
    encoded: [u8; MAX_ENCODED_FRAME_LEN - 1],
    length: usize,
    discarding: bool,
}

impl Default for StreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamDecoder {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            encoded: [0; MAX_ENCODED_FRAME_LEN - 1],
            length: 0,
            discarding: false,
        }
    }

    pub fn reset(&mut self) {
        self.length = 0;
        self.discarding = false;
    }

    pub fn push(&mut self, byte: u8) -> DecoderEvent {
        if byte == FRAME_DELIMITER {
            if self.discarding {
                self.reset();
                return DecoderEvent::Error(DecodeError::FrameTooLong);
            }
            if self.length == 0 {
                return DecoderEvent::Pending;
            }

            let mut decoded = [0_u8; MAX_DECODED_FRAME_LEN];
            let encoded = &self.encoded[..self.length];
            self.length = 0;
            let decoded_length = match cobs::decode(encoded, &mut decoded) {
                Ok(report) => report.frame_size(),
                Err(_) => return DecoderEvent::Error(DecodeError::MalformedCobs),
            };
            return match decode_raw(&decoded[..decoded_length]) {
                Ok(frame) => DecoderEvent::Frame(frame),
                Err(error) => DecoderEvent::Error(error),
            };
        }

        if self.discarding {
            return DecoderEvent::Pending;
        }
        if self.length == self.encoded.len() {
            self.length = 0;
            self.discarding = true;
            return DecoderEvent::Pending;
        }
        self.encoded[self.length] = byte;
        self.length += 1;
        DecoderEvent::Pending
    }
}

/// Encodes one complete delimiter-terminated frame into caller-owned storage.
///
/// # Errors
///
/// Returns an error when the frame or message is invalid, or when `output`
/// cannot hold the complete bounded frame.
pub fn encode_frame(frame: Frame, output: &mut [u8]) -> Result<usize, EncodeError> {
    if !frame.is_structurally_valid() {
        return Err(EncodeError::InvalidFrame);
    }
    let mut payload = [0_u8; MAX_PAYLOAD_LEN];
    let payload_length = encode_message(frame.message, &mut payload)?;
    let decoded_length = HEADER_LEN + payload_length + CHECKSUM_LEN;
    let mut decoded = [0_u8; MAX_DECODED_FRAME_LEN];
    decoded[0..2].copy_from_slice(&MAGIC);
    decoded[2] = WIRE_VERSION;
    decoded[3] = frame.protocol_version;
    decoded[4] = frame.message.kind();
    decoded[5] = 0;
    decoded[6..8].copy_from_slice(
        &u16::try_from(payload_length)
            .map_err(|_| EncodeError::InvalidMessage)?
            .to_le_bytes(),
    );
    decoded[8..16].copy_from_slice(&frame.session_id.to_le_bytes());
    decoded[16..20].copy_from_slice(&frame.sequence.to_le_bytes());
    decoded[HEADER_LEN..HEADER_LEN + payload_length].copy_from_slice(&payload[..payload_length]);
    let checksum_start = HEADER_LEN + payload_length;
    let checksum = CRC32C.checksum(&decoded[..checksum_start]);
    decoded[checksum_start..decoded_length].copy_from_slice(&checksum.to_le_bytes());

    if output.len() < cobs::max_encoding_length(decoded_length) + 1 {
        return Err(EncodeError::BufferTooSmall);
    }
    let encoded_length = cobs::try_encode(&decoded[..decoded_length], output)
        .map_err(|_| EncodeError::BufferTooSmall)?;
    output[encoded_length] = FRAME_DELIMITER;
    Ok(encoded_length + 1)
}

fn decode_raw(decoded: &[u8]) -> Result<Frame, DecodeError> {
    if decoded.len() < HEADER_LEN + CHECKSUM_LEN {
        return Err(DecodeError::FrameTooShort);
    }
    if decoded[0..2] != MAGIC {
        return Err(DecodeError::InvalidMagic);
    }
    if decoded[2] != WIRE_VERSION {
        return Err(DecodeError::UnsupportedWireVersion(decoded[2]));
    }
    if decoded[5] != 0 {
        return Err(DecodeError::ReservedFlags(decoded[5]));
    }
    let payload_length = usize::from(u16::from_le_bytes([decoded[6], decoded[7]]));
    if payload_length > MAX_PAYLOAD_LEN
        || decoded.len() != HEADER_LEN + payload_length + CHECKSUM_LEN
    {
        return Err(DecodeError::PayloadLengthMismatch);
    }
    let checksum_offset = HEADER_LEN + payload_length;
    let expected = u32::from_le_bytes(
        decoded[checksum_offset..]
            .try_into()
            .map_err(|_| DecodeError::FrameTooShort)?,
    );
    if CRC32C.checksum(&decoded[..checksum_offset]) != expected {
        return Err(DecodeError::ChecksumMismatch);
    }

    let message = decode_message(decoded[4], &decoded[HEADER_LEN..checksum_offset])?;
    let frame = Frame {
        protocol_version: decoded[3],
        session_id: u64::from_le_bytes(
            decoded[8..16]
                .try_into()
                .map_err(|_| DecodeError::FrameTooShort)?,
        ),
        sequence: u32::from_le_bytes(
            decoded[16..20]
                .try_into()
                .map_err(|_| DecodeError::FrameTooShort)?,
        ),
        message,
    };
    if !frame.is_structurally_valid() {
        return Err(DecodeError::InvalidFrame);
    }
    Ok(frame)
}

struct Writer<'a> {
    output: &'a mut [u8],
    position: usize,
}

impl<'a> Writer<'a> {
    fn new(output: &'a mut [u8]) -> Self {
        Self {
            output,
            position: 0,
        }
    }

    fn bytes(&mut self, bytes: &[u8]) -> Result<(), EncodeError> {
        let end = self
            .position
            .checked_add(bytes.len())
            .ok_or(EncodeError::InvalidMessage)?;
        let destination = self
            .output
            .get_mut(self.position..end)
            .ok_or(EncodeError::InvalidMessage)?;
        destination.copy_from_slice(bytes);
        self.position = end;
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<(), EncodeError> {
        self.bytes(&[value])
    }

    fn bool(&mut self, value: bool) -> Result<(), EncodeError> {
        self.u8(u8::from(value))
    }

    fn u16(&mut self, value: u16) -> Result<(), EncodeError> {
        self.bytes(&value.to_le_bytes())
    }

    fn i16(&mut self, value: i16) -> Result<(), EncodeError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u32(&mut self, value: u32) -> Result<(), EncodeError> {
        self.bytes(&value.to_le_bytes())
    }

    fn u64(&mut self, value: u64) -> Result<(), EncodeError> {
        self.bytes(&value.to_le_bytes())
    }
}

struct Reader<'a> {
    input: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, position: 0 }
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self
            .position
            .checked_add(N)
            .ok_or(DecodeError::InvalidPayload)?;
        let bytes = self
            .input
            .get(self.position..end)
            .ok_or(DecodeError::InvalidPayload)?;
        self.position = end;
        bytes.try_into().map_err(|_| DecodeError::InvalidPayload)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes::<1>()?[0])
    }

    fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::InvalidPayload),
        }
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_le_bytes(self.bytes()?))
    }

    fn i16(&mut self) -> Result<i16, DecodeError> {
        Ok(i16::from_le_bytes(self.bytes()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.bytes()?))
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.bytes()?))
    }

    fn finish(self) -> Result<(), DecodeError> {
        if self.position == self.input.len() {
            Ok(())
        } else {
            Err(DecodeError::InvalidPayload)
        }
    }
}

fn encode_versions(
    writer: &mut Writer<'_>,
    versions: SupportedVersions,
) -> Result<(), EncodeError> {
    writer.u8(versions.minimum())?;
    writer.u8(versions.maximum())
}

fn decode_versions(reader: &mut Reader<'_>) -> Result<SupportedVersions, DecodeError> {
    SupportedVersions::new(reader.u8()?, reader.u8()?).ok_or(DecodeError::InvalidPayload)
}

fn encode_descriptor(writer: &mut Writer<'_>, value: PanelDescriptor) -> Result<(), EncodeError> {
    if !value.is_valid() {
        return Err(EncodeError::InvalidMessage);
    }
    writer.u16(value.firmware.major)?;
    writer.u16(value.firmware.minor)?;
    writer.u16(value.firmware.patch)?;
    writer.u32(value.firmware.build)?;
    writer.u64(value.boot_id)?;
    writer.u8(value.encoders)?;
    writer.u8(value.buttons)?;
    writer.u8(value.leds)?;
    writer.u8(value.rings)?;
    writer.u8(value.ring_segments)
}

fn decode_descriptor(reader: &mut Reader<'_>) -> Result<PanelDescriptor, DecodeError> {
    let descriptor = PanelDescriptor {
        firmware: FirmwareVersion {
            major: reader.u16()?,
            minor: reader.u16()?,
            patch: reader.u16()?,
            build: reader.u32()?,
        },
        boot_id: reader.u64()?,
        encoders: reader.u8()?,
        buttons: reader.u8()?,
        leds: reader.u8()?,
        rings: reader.u8()?,
        ring_segments: reader.u8()?,
    };
    descriptor
        .is_valid()
        .then_some(descriptor)
        .ok_or(DecodeError::InvalidPayload)
}

fn encode_input_context(
    writer: &mut Writer<'_>,
    control_id: u16,
    timestamp_us: u64,
    modifiers: u16,
) -> Result<(), EncodeError> {
    writer.u16(control_id)?;
    writer.u64(timestamp_us)?;
    writer.u16(modifiers)
}

fn decode_input_context(reader: &mut Reader<'_>) -> Result<(u16, u64, u16), DecodeError> {
    Ok((reader.u16()?, reader.u64()?, reader.u16()?))
}

#[allow(clippy::too_many_lines)]
fn encode_message(message: Message, output: &mut [u8]) -> Result<usize, EncodeError> {
    let mut writer = Writer::new(output);
    match message {
        Message::RuntimeHello {
            versions,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
        } => {
            if heartbeat_interval_ms == 0 || heartbeat_timeout_ms <= heartbeat_interval_ms {
                return Err(EncodeError::InvalidMessage);
            }
            encode_versions(&mut writer, versions)?;
            writer.u32(heartbeat_interval_ms)?;
            writer.u32(heartbeat_timeout_ms)?;
        }
        Message::PanelHello {
            selected_version,
            heartbeat_interval_ms,
            heartbeat_timeout_ms,
            descriptor,
        } => {
            if selected_version == 0
                || heartbeat_interval_ms == 0
                || heartbeat_timeout_ms <= heartbeat_interval_ms
            {
                return Err(EncodeError::InvalidMessage);
            }
            writer.u8(selected_version)?;
            writer.u32(heartbeat_interval_ms)?;
            writer.u32(heartbeat_timeout_ms)?;
            encode_descriptor(&mut writer, descriptor)?;
        }
        Message::VersionRejected { supported } => encode_versions(&mut writer, supported)?,
        Message::Heartbeat {
            uptime_ms,
            last_received_sequence,
        } => {
            writer.u64(uptime_ms)?;
            writer.u32(last_received_sequence)?;
        }
        Message::InputSnapshot(value) => {
            writer.u64(value.timestamp_us)?;
            writer.u16(value.encoder_pressed)?;
            writer.u32(value.button_pressed)?;
            writer.u16(value.modifiers)?;
        }
        Message::EncoderDelta {
            control_id,
            delta,
            timestamp_us,
            modifiers,
        } => {
            if delta == 0 {
                return Err(EncodeError::InvalidMessage);
            }
            encode_input_context(&mut writer, control_id, timestamp_us, modifiers)?;
            writer.i16(delta)?;
        }
        Message::EncoderPress {
            control_id,
            timestamp_us,
            modifiers,
        }
        | Message::EncoderRelease {
            control_id,
            timestamp_us,
            modifiers,
        }
        | Message::ButtonPress {
            control_id,
            timestamp_us,
            modifiers,
        }
        | Message::ButtonRelease {
            control_id,
            timestamp_us,
            modifiers,
        }
        | Message::ButtonDoublePress {
            control_id,
            timestamp_us,
            modifiers,
        } => encode_input_context(&mut writer, control_id, timestamp_us, modifiers)?,
        Message::ButtonLongPress {
            control_id,
            held_ms,
            timestamp_us,
            modifiers,
        } => {
            if held_ms == 0 {
                return Err(EncodeError::InvalidMessage);
            }
            encode_input_context(&mut writer, control_id, timestamp_us, modifiers)?;
            writer.u32(held_ms)?;
        }
        Message::PanelError { code, detail } => {
            writer.u16(code as u16)?;
            writer.u32(detail)?;
        }
        Message::SetLed {
            control_id,
            enabled,
        } => {
            writer.u16(control_id)?;
            writer.bool(enabled)?;
        }
        Message::SetLedBrightness {
            control_id,
            brightness,
        } => {
            writer.u16(control_id)?;
            writer.u8(brightness)?;
        }
        Message::SetLedBlink(value) => {
            if (value.on_ms == 0) != (value.off_ms == 0) {
                return Err(EncodeError::InvalidMessage);
            }
            writer.u16(value.control_id)?;
            writer.u16(value.on_ms)?;
            writer.u16(value.off_ms)?;
        }
        Message::SetRingSegment(value) => {
            writer.u8(value.ring_id)?;
            writer.u8(value.segment)?;
            writer.u8(value.brightness)?;
        }
        Message::SetRingValue { ring_id, value } => {
            writer.u8(ring_id)?;
            writer.u16(value)?;
        }
        Message::SetRingAnimation {
            ring_id,
            animation,
            period_ms,
        } => {
            if (animation == Animation::Off) != (period_ms == 0) {
                return Err(EncodeError::InvalidMessage);
            }
            writer.u8(ring_id)?;
            writer.u8(animation as u8)?;
            writer.u16(period_ms)?;
        }
        Message::SetGlobalBrightness { brightness } => writer.u8(brightness)?,
        Message::EnterBootloader { confirmation } => {
            if confirmation != Message::BOOTLOADER_CONFIRMATION {
                return Err(EncodeError::InvalidMessage);
            }
            writer.u32(confirmation)?;
        }
    }
    Ok(writer.position)
}

#[allow(clippy::too_many_lines)]
fn decode_message(kind: u8, payload: &[u8]) -> Result<Message, DecodeError> {
    let mut reader = Reader::new(payload);
    let message = match kind {
        0x01 => {
            let versions = decode_versions(&mut reader)?;
            let heartbeat_interval_ms = reader.u32()?;
            let heartbeat_timeout_ms = reader.u32()?;
            if heartbeat_interval_ms == 0 || heartbeat_timeout_ms <= heartbeat_interval_ms {
                return Err(DecodeError::InvalidPayload);
            }
            Message::RuntimeHello {
                versions,
                heartbeat_interval_ms,
                heartbeat_timeout_ms,
            }
        }
        0x02 => {
            let selected_version = reader.u8()?;
            let heartbeat_interval_ms = reader.u32()?;
            let heartbeat_timeout_ms = reader.u32()?;
            let descriptor = decode_descriptor(&mut reader)?;
            if selected_version == 0
                || heartbeat_interval_ms == 0
                || heartbeat_timeout_ms <= heartbeat_interval_ms
            {
                return Err(DecodeError::InvalidPayload);
            }
            Message::PanelHello {
                selected_version,
                heartbeat_interval_ms,
                heartbeat_timeout_ms,
                descriptor,
            }
        }
        0x03 => Message::VersionRejected {
            supported: decode_versions(&mut reader)?,
        },
        0x04 => Message::Heartbeat {
            uptime_ms: reader.u64()?,
            last_received_sequence: reader.u32()?,
        },
        0x10 => Message::InputSnapshot(InputState {
            timestamp_us: reader.u64()?,
            encoder_pressed: reader.u16()?,
            button_pressed: reader.u32()?,
            modifiers: reader.u16()?,
        }),
        0x11 => {
            let (control_id, timestamp_us, modifiers) = decode_input_context(&mut reader)?;
            let delta = reader.i16()?;
            if delta == 0 {
                return Err(DecodeError::InvalidPayload);
            }
            Message::EncoderDelta {
                control_id,
                delta,
                timestamp_us,
                modifiers,
            }
        }
        0x12..=0x15 | 0x17 => {
            let (control_id, timestamp_us, modifiers) = decode_input_context(&mut reader)?;
            match kind {
                0x12 => Message::EncoderPress {
                    control_id,
                    timestamp_us,
                    modifiers,
                },
                0x13 => Message::EncoderRelease {
                    control_id,
                    timestamp_us,
                    modifiers,
                },
                0x14 => Message::ButtonPress {
                    control_id,
                    timestamp_us,
                    modifiers,
                },
                0x15 => Message::ButtonRelease {
                    control_id,
                    timestamp_us,
                    modifiers,
                },
                _ => Message::ButtonDoublePress {
                    control_id,
                    timestamp_us,
                    modifiers,
                },
            }
        }
        0x16 => {
            let (control_id, timestamp_us, modifiers) = decode_input_context(&mut reader)?;
            let held_ms = reader.u32()?;
            if held_ms == 0 {
                return Err(DecodeError::InvalidPayload);
            }
            Message::ButtonLongPress {
                control_id,
                held_ms,
                timestamp_us,
                modifiers,
            }
        }
        0x18 => Message::PanelError {
            code: PanelErrorCode::from_wire(reader.u16()?).ok_or(DecodeError::InvalidPayload)?,
            detail: reader.u32()?,
        },
        0x20 => Message::SetLed {
            control_id: reader.u16()?,
            enabled: reader.bool()?,
        },
        0x21 => Message::SetLedBrightness {
            control_id: reader.u16()?,
            brightness: reader.u8()?,
        },
        0x22 => {
            let value = LedBlink {
                control_id: reader.u16()?,
                on_ms: reader.u16()?,
                off_ms: reader.u16()?,
            };
            if (value.on_ms == 0) != (value.off_ms == 0) {
                return Err(DecodeError::InvalidPayload);
            }
            Message::SetLedBlink(value)
        }
        0x23 => Message::SetRingSegment(RingSegment {
            ring_id: reader.u8()?,
            segment: reader.u8()?,
            brightness: reader.u8()?,
        }),
        0x24 => Message::SetRingValue {
            ring_id: reader.u8()?,
            value: reader.u16()?,
        },
        0x25 => {
            let ring_id = reader.u8()?;
            let animation =
                Animation::from_wire(reader.u8()?).ok_or(DecodeError::InvalidPayload)?;
            let period_ms = reader.u16()?;
            if (animation == Animation::Off) != (period_ms == 0) {
                return Err(DecodeError::InvalidPayload);
            }
            Message::SetRingAnimation {
                ring_id,
                animation,
                period_ms,
            }
        }
        0x26 => Message::SetGlobalBrightness {
            brightness: reader.u8()?,
        },
        0x27 => {
            let confirmation = reader.u32()?;
            if confirmation != Message::BOOTLOADER_CONFIRMATION {
                return Err(DecodeError::InvalidPayload);
            }
            Message::EnterBootloader { confirmation }
        }
        _ => return Err(DecodeError::UnknownMessage(kind)),
    };
    reader.finish()?;
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Direction;

    fn descriptor() -> PanelDescriptor {
        PanelDescriptor {
            firmware: FirmwareVersion {
                major: 1,
                minor: 2,
                patch: 3,
                build: 0xAABB_CCDD,
            },
            boot_id: 0x0102_0304_0506_0708,
            encoders: 4,
            buttons: 8,
            leds: 12,
            rings: 1,
            ring_segments: 32,
        }
    }

    fn round_trip(message: Message, protocol_version: u8, sequence: u32) {
        let frame = if message.is_handshake() {
            Frame::handshake(42, message).unwrap()
        } else {
            Frame::application(protocol_version, 42, sequence, message).unwrap()
        };
        let mut bytes = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut bytes).unwrap();
        assert_eq!(bytes[length - 1], FRAME_DELIMITER);
        assert!(!bytes[..length - 1].contains(&FRAME_DELIMITER));

        let mut decoder = StreamDecoder::new();
        for byte in &bytes[..length - 1] {
            assert_eq!(decoder.push(*byte), DecoderEvent::Pending);
        }
        assert_eq!(decoder.push(FRAME_DELIMITER), DecoderEvent::Frame(frame));
    }

    fn assert_message_layout(message: Message, expected_kind: u8, expected_payload: &[u8]) {
        let mut payload = [0_u8; MAX_PAYLOAD_LEN];
        let length = encode_message(message, &mut payload).unwrap();
        assert_eq!(message.kind(), expected_kind);
        assert_eq!(&payload[..length], expected_payload);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_message_round_trips_through_the_wire_codec() {
        let messages = [
            Message::RuntimeHello {
                versions: SupportedVersions::CURRENT,
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 350,
            },
            Message::PanelHello {
                selected_version: 1,
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 350,
                descriptor: descriptor(),
            },
            Message::VersionRejected {
                supported: SupportedVersions::CURRENT,
            },
            Message::Heartbeat {
                uptime_ms: u64::MAX,
                last_received_sequence: u32::MAX,
            },
            Message::InputSnapshot(InputState {
                timestamp_us: 9,
                encoder_pressed: 3,
                button_pressed: 5,
                modifiers: 7,
            }),
            Message::EncoderDelta {
                control_id: 2,
                delta: -7,
                timestamp_us: 11,
                modifiers: 3,
            },
            Message::EncoderPress {
                control_id: 2,
                timestamp_us: 12,
                modifiers: 3,
            },
            Message::EncoderRelease {
                control_id: 2,
                timestamp_us: 13,
                modifiers: 3,
            },
            Message::ButtonPress {
                control_id: 4,
                timestamp_us: 14,
                modifiers: 1,
            },
            Message::ButtonRelease {
                control_id: 4,
                timestamp_us: 15,
                modifiers: 1,
            },
            Message::ButtonLongPress {
                control_id: 4,
                held_ms: 750,
                timestamp_us: 16,
                modifiers: 1,
            },
            Message::ButtonDoublePress {
                control_id: 4,
                timestamp_us: 17,
                modifiers: 1,
            },
            Message::PanelError {
                code: PanelErrorCode::UnsupportedControl,
                detail: 6,
            },
            Message::SetLed {
                control_id: 1,
                enabled: true,
            },
            Message::SetLedBrightness {
                control_id: 1,
                brightness: 127,
            },
            Message::SetLedBlink(LedBlink {
                control_id: 1,
                on_ms: 100,
                off_ms: 200,
            }),
            Message::SetRingSegment(RingSegment {
                ring_id: 0,
                segment: 31,
                brightness: 200,
            }),
            Message::SetRingValue {
                ring_id: 0,
                value: 32_768,
            },
            Message::SetRingAnimation {
                ring_id: 0,
                animation: Animation::Pulse,
                period_ms: 500,
            },
            Message::SetGlobalBrightness { brightness: 64 },
            Message::EnterBootloader {
                confirmation: Message::BOOTLOADER_CONFIRMATION,
            },
        ];
        for (index, message) in messages.into_iter().enumerate() {
            round_trip(message, 1, u32::try_from(index + 1).unwrap());
        }
        assert_eq!(
            Message::Heartbeat {
                uptime_ms: 0,
                last_received_sequence: 0
            }
            .direction(),
            Direction::Bidirectional
        );
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn every_message_has_a_fixed_payload_layout() {
        assert_message_layout(
            Message::RuntimeHello {
                versions: SupportedVersions::CURRENT,
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 350,
            },
            0x01,
            &[0x01, 0x01, 0x64, 0x00, 0x00, 0x00, 0x5E, 0x01, 0x00, 0x00],
        );
        assert_message_layout(
            Message::PanelHello {
                selected_version: 1,
                heartbeat_interval_ms: 100,
                heartbeat_timeout_ms: 350,
                descriptor: descriptor(),
            },
            0x02,
            &[
                0x01, 0x64, 0x00, 0x00, 0x00, 0x5E, 0x01, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03,
                0x00, 0xDD, 0xCC, 0xBB, 0xAA, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0x04,
                0x08, 0x0C, 0x01, 0x20,
            ],
        );
        assert_message_layout(
            Message::VersionRejected {
                supported: SupportedVersions::CURRENT,
            },
            0x03,
            &[0x01, 0x01],
        );
        assert_message_layout(
            Message::Heartbeat {
                uptime_ms: u64::MAX,
                last_received_sequence: u32::MAX,
            },
            0x04,
            &[
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            ],
        );
        assert_message_layout(
            Message::InputSnapshot(InputState {
                timestamp_us: 9,
                encoder_pressed: 3,
                button_pressed: 5,
                modifiers: 7,
            }),
            0x10,
            &[
                0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x05, 0x00, 0x00, 0x00,
                0x07, 0x00,
            ],
        );
        assert_message_layout(
            Message::EncoderDelta {
                control_id: 2,
                delta: -7,
                timestamp_us: 11,
                modifiers: 3,
            },
            0x11,
            &[
                0x02, 0x00, 0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0xF9, 0xFF,
            ],
        );
        assert_message_layout(
            Message::EncoderPress {
                control_id: 2,
                timestamp_us: 12,
                modifiers: 3,
            },
            0x12,
            &[
                0x02, 0x00, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00,
            ],
        );
        assert_message_layout(
            Message::EncoderRelease {
                control_id: 2,
                timestamp_us: 13,
                modifiers: 3,
            },
            0x13,
            &[
                0x02, 0x00, 0x0D, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00,
            ],
        );
        assert_message_layout(
            Message::ButtonPress {
                control_id: 4,
                timestamp_us: 14,
                modifiers: 1,
            },
            0x14,
            &[
                0x04, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            ],
        );
        assert_message_layout(
            Message::ButtonRelease {
                control_id: 4,
                timestamp_us: 15,
                modifiers: 1,
            },
            0x15,
            &[
                0x04, 0x00, 0x0F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            ],
        );
        assert_message_layout(
            Message::ButtonLongPress {
                control_id: 4,
                held_ms: 750,
                timestamp_us: 16,
                modifiers: 1,
            },
            0x16,
            &[
                0x04, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0xEE, 0x02,
                0x00, 0x00,
            ],
        );
        assert_message_layout(
            Message::ButtonDoublePress {
                control_id: 4,
                timestamp_us: 17,
                modifiers: 1,
            },
            0x17,
            &[
                0x04, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            ],
        );
        assert_message_layout(
            Message::PanelError {
                code: PanelErrorCode::UnsupportedControl,
                detail: 6,
            },
            0x18,
            &[0x01, 0x00, 0x06, 0x00, 0x00, 0x00],
        );
        assert_message_layout(
            Message::SetLed {
                control_id: 1,
                enabled: true,
            },
            0x20,
            &[0x01, 0x00, 0x01],
        );
        assert_message_layout(
            Message::SetLedBrightness {
                control_id: 1,
                brightness: 127,
            },
            0x21,
            &[0x01, 0x00, 0x7F],
        );
        assert_message_layout(
            Message::SetLedBlink(LedBlink {
                control_id: 1,
                on_ms: 100,
                off_ms: 200,
            }),
            0x22,
            &[0x01, 0x00, 0x64, 0x00, 0xC8, 0x00],
        );
        assert_message_layout(
            Message::SetRingSegment(RingSegment {
                ring_id: 0,
                segment: 31,
                brightness: 200,
            }),
            0x23,
            &[0x00, 0x1F, 0xC8],
        );
        assert_message_layout(
            Message::SetRingValue {
                ring_id: 0,
                value: 32_768,
            },
            0x24,
            &[0x00, 0x00, 0x80],
        );
        assert_message_layout(
            Message::SetRingAnimation {
                ring_id: 0,
                animation: Animation::Pulse,
                period_ms: 500,
            },
            0x25,
            &[0x00, 0x03, 0xF4, 0x01],
        );
        assert_message_layout(
            Message::SetGlobalBrightness { brightness: 64 },
            0x26,
            &[0x40],
        );
        assert_message_layout(
            Message::EnterBootloader {
                confirmation: Message::BOOTLOADER_CONFIRMATION,
            },
            0x27,
            &[0xAD, 0x10, 0x07, 0xB0],
        );
    }

    #[test]
    fn corruption_is_detected_and_the_stream_resynchronizes() {
        let frame =
            Frame::application(1, 7, 1, Message::SetGlobalBrightness { brightness: 99 }).unwrap();
        let mut bytes = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut bytes).unwrap();
        let mut raw_frame = [0_u8; MAX_DECODED_FRAME_LEN];
        let report = cobs::decode(&bytes[..length - 1], &mut raw_frame).unwrap();
        raw_frame[8] ^= 0x40;
        let encoded_length =
            cobs::try_encode(&raw_frame[..report.frame_size()], &mut bytes).unwrap();
        bytes[encoded_length] = FRAME_DELIMITER;

        let mut decoder = StreamDecoder::new();
        let mut error = None;
        for byte in &bytes[..=encoded_length] {
            if let DecoderEvent::Error(value) = decoder.push(*byte) {
                error = Some(value);
            }
        }
        assert_eq!(error, Some(DecodeError::ChecksumMismatch));

        let length = encode_frame(frame, &mut bytes).unwrap();
        let mut event = DecoderEvent::Pending;
        for byte in &bytes[..length] {
            event = decoder.push(*byte);
        }
        assert_eq!(event, DecoderEvent::Frame(frame));
    }

    #[test]
    fn oversized_input_is_discarded_until_the_next_delimiter() {
        let mut decoder = StreamDecoder::new();
        for _ in 0..MAX_ENCODED_FRAME_LEN + 10 {
            assert_eq!(decoder.push(1), DecoderEvent::Pending);
        }
        assert_eq!(
            decoder.push(FRAME_DELIMITER),
            DecoderEvent::Error(DecodeError::FrameTooLong)
        );
        assert_eq!(decoder.push(FRAME_DELIMITER), DecoderEvent::Pending);
    }

    #[test]
    fn output_capacity_is_checked_before_encoding() {
        let frame =
            Frame::application(1, 1, 1, Message::SetGlobalBrightness { brightness: 1 }).unwrap();
        assert_eq!(
            encode_frame(frame, &mut [0_u8; 2]),
            Err(EncodeError::BufferTooSmall)
        );
    }

    #[test]
    fn version_one_has_a_fixed_cross_language_wire_vector() {
        let frame = Frame::application(
            1,
            0x0102_0304_0506_0708,
            0x0A0B_0C0D,
            Message::SetGlobalBrightness { brightness: 0x40 },
        )
        .unwrap();
        let mut output = [0_u8; MAX_ENCODED_FRAME_LEN];
        let length = encode_frame(frame, &mut output).unwrap();
        assert_eq!(
            &output[..length],
            &[
                0x06, 0x48, 0x50, 0x01, 0x01, 0x26, 0x02, 0x01, 0x12, 0x08, 0x07, 0x06, 0x05, 0x04,
                0x03, 0x02, 0x01, 0x0D, 0x0C, 0x0B, 0x0A, 0x40, 0xF8, 0x02, 0x72, 0xB7, 0x00,
            ]
        );
    }
}
