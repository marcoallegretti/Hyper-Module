use core::fmt;

use crate::PROTOCOL_VERSION;

pub const MAX_ENCODERS: u8 = 16;
pub const MAX_BUTTONS: u8 = 32;
pub const MAX_LEDS: u8 = 32;
pub const MAX_RINGS: u8 = 8;
pub const MAX_RING_SEGMENTS: u8 = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedVersions {
    minimum: u8,
    maximum: u8,
}

impl SupportedVersions {
    pub const CURRENT: Self = Self {
        minimum: PROTOCOL_VERSION,
        maximum: PROTOCOL_VERSION,
    };

    #[must_use]
    pub const fn new(minimum: u8, maximum: u8) -> Option<Self> {
        if minimum == 0 || minimum > maximum {
            None
        } else {
            Some(Self { minimum, maximum })
        }
    }

    #[must_use]
    pub const fn minimum(self) -> u8 {
        self.minimum
    }

    #[must_use]
    pub const fn maximum(self) -> u8 {
        self.maximum
    }

    #[must_use]
    pub const fn negotiate(self, peer: Self) -> Option<u8> {
        let minimum = if self.minimum > peer.minimum {
            self.minimum
        } else {
            peer.minimum
        };
        let maximum = if self.maximum < peer.maximum {
            self.maximum
        } else {
            peer.maximum
        };
        if minimum <= maximum {
            Some(maximum)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareVersion {
    pub major: u16,
    pub minor: u16,
    pub patch: u16,
    pub build: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelDescriptor {
    pub firmware: FirmwareVersion,
    pub boot_id: u64,
    pub encoders: u8,
    pub buttons: u8,
    pub leds: u8,
    pub rings: u8,
    pub ring_segments: u8,
}

impl PanelDescriptor {
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.boot_id != 0
            && self.encoders <= MAX_ENCODERS
            && self.buttons <= MAX_BUTTONS
            && self.leds <= MAX_LEDS
            && self.rings <= MAX_RINGS
            && ((self.rings == 0 && self.ring_segments == 0)
                || (self.rings > 0
                    && self.ring_segments > 0
                    && self.ring_segments <= MAX_RING_SEGMENTS))
    }

    pub(crate) const fn accepts_panel_message(self, message: Message) -> bool {
        match message {
            Message::Heartbeat { .. } | Message::PanelError { .. } => true,
            Message::InputSnapshot(state) => {
                state.encoder_pressed & !low_u16_mask(self.encoders) == 0
                    && state.button_pressed & !low_u32_mask(self.buttons) == 0
            }
            Message::EncoderDelta { control_id, .. }
            | Message::EncoderPress { control_id, .. }
            | Message::EncoderRelease { control_id, .. } => control_id < self.encoders as u16,
            Message::ButtonPress { control_id, .. }
            | Message::ButtonRelease { control_id, .. }
            | Message::ButtonLongPress { control_id, .. }
            | Message::ButtonDoublePress { control_id, .. } => control_id < self.buttons as u16,
            _ => false,
        }
    }

    pub(crate) const fn accepts_runtime_message(self, message: Message) -> bool {
        match message {
            Message::Heartbeat { .. }
            | Message::SetGlobalBrightness { .. }
            | Message::EnterBootloader { .. } => true,
            Message::SetLed { control_id, .. }
            | Message::SetLedBrightness { control_id, .. }
            | Message::SetLedBlink(LedBlink { control_id, .. }) => control_id < self.leds as u16,
            Message::SetRingSegment(RingSegment {
                ring_id, segment, ..
            }) => ring_id < self.rings && segment < self.ring_segments,
            Message::SetRingValue { ring_id, .. } | Message::SetRingAnimation { ring_id, .. } => {
                ring_id < self.rings
            }
            _ => false,
        }
    }
}

const fn low_u16_mask(bits: u8) -> u16 {
    if bits == 16 {
        u16::MAX
    } else {
        (1_u16 << bits) - 1
    }
}

const fn low_u32_mask(bits: u8) -> u32 {
    if bits == 32 {
        u32::MAX
    } else {
        (1_u32 << bits) - 1
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputState {
    pub timestamp_us: u64,
    pub encoder_pressed: u16,
    pub button_pressed: u32,
    pub modifiers: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedBlink {
    pub control_id: u16,
    pub on_ms: u16,
    pub off_ms: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingSegment {
    pub ring_id: u8,
    pub segment: u8,
    pub brightness: u8,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Animation {
    Off = 0,
    SpinClockwise = 1,
    SpinCounterClockwise = 2,
    Pulse = 3,
}

impl Animation {
    pub(crate) const fn from_wire(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Off),
            1 => Some(Self::SpinClockwise),
            2 => Some(Self::SpinCounterClockwise),
            3 => Some(Self::Pulse),
            _ => None,
        }
    }
}

#[repr(u16)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelErrorCode {
    UnsupportedControl = 1,
    InvalidStateTransition = 2,
    TransmitOverflow = 3,
    BootloaderUnavailable = 4,
}

impl PanelErrorCode {
    pub(crate) const fn from_wire(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::UnsupportedControl),
            2 => Some(Self::InvalidStateTransition),
            3 => Some(Self::TransmitOverflow),
            4 => Some(Self::BootloaderUnavailable),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Message {
    RuntimeHello {
        versions: SupportedVersions,
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u32,
    },
    PanelHello {
        selected_version: u8,
        heartbeat_interval_ms: u32,
        heartbeat_timeout_ms: u32,
        descriptor: PanelDescriptor,
    },
    VersionRejected {
        supported: SupportedVersions,
    },
    Heartbeat {
        uptime_ms: u64,
        last_received_sequence: u32,
    },
    InputSnapshot(InputState),
    EncoderDelta {
        control_id: u16,
        delta: i16,
        timestamp_us: u64,
        modifiers: u16,
    },
    EncoderPress {
        control_id: u16,
        timestamp_us: u64,
        modifiers: u16,
    },
    EncoderRelease {
        control_id: u16,
        timestamp_us: u64,
        modifiers: u16,
    },
    ButtonPress {
        control_id: u16,
        timestamp_us: u64,
        modifiers: u16,
    },
    ButtonRelease {
        control_id: u16,
        timestamp_us: u64,
        modifiers: u16,
    },
    ButtonLongPress {
        control_id: u16,
        held_ms: u32,
        timestamp_us: u64,
        modifiers: u16,
    },
    ButtonDoublePress {
        control_id: u16,
        timestamp_us: u64,
        modifiers: u16,
    },
    PanelError {
        code: PanelErrorCode,
        detail: u32,
    },
    SetLed {
        control_id: u16,
        enabled: bool,
    },
    SetLedBrightness {
        control_id: u16,
        brightness: u8,
    },
    SetLedBlink(LedBlink),
    SetRingSegment(RingSegment),
    SetRingValue {
        ring_id: u8,
        value: u16,
    },
    SetRingAnimation {
        ring_id: u8,
        animation: Animation,
        period_ms: u16,
    },
    SetGlobalBrightness {
        brightness: u8,
    },
    EnterBootloader {
        confirmation: u32,
    },
}

impl Message {
    pub const BOOTLOADER_CONFIRMATION: u32 = 0xB0_07_10_AD;

    #[must_use]
    pub const fn direction(self) -> Direction {
        match self {
            Self::RuntimeHello { .. }
            | Self::SetLed { .. }
            | Self::SetLedBrightness { .. }
            | Self::SetLedBlink(_)
            | Self::SetRingSegment(_)
            | Self::SetRingValue { .. }
            | Self::SetRingAnimation { .. }
            | Self::SetGlobalBrightness { .. }
            | Self::EnterBootloader { .. } => Direction::RuntimeToPanel,
            Self::PanelHello { .. }
            | Self::VersionRejected { .. }
            | Self::InputSnapshot(_)
            | Self::EncoderDelta { .. }
            | Self::EncoderPress { .. }
            | Self::EncoderRelease { .. }
            | Self::ButtonPress { .. }
            | Self::ButtonRelease { .. }
            | Self::ButtonLongPress { .. }
            | Self::ButtonDoublePress { .. }
            | Self::PanelError { .. } => Direction::PanelToRuntime,
            Self::Heartbeat { .. } => Direction::Bidirectional,
        }
    }

    #[must_use]
    pub const fn is_handshake(self) -> bool {
        matches!(
            self,
            Self::RuntimeHello { .. } | Self::PanelHello { .. } | Self::VersionRejected { .. }
        )
    }

    pub(crate) const fn kind(self) -> u8 {
        match self {
            Self::RuntimeHello { .. } => 0x01,
            Self::PanelHello { .. } => 0x02,
            Self::VersionRejected { .. } => 0x03,
            Self::Heartbeat { .. } => 0x04,
            Self::InputSnapshot(_) => 0x10,
            Self::EncoderDelta { .. } => 0x11,
            Self::EncoderPress { .. } => 0x12,
            Self::EncoderRelease { .. } => 0x13,
            Self::ButtonPress { .. } => 0x14,
            Self::ButtonRelease { .. } => 0x15,
            Self::ButtonLongPress { .. } => 0x16,
            Self::ButtonDoublePress { .. } => 0x17,
            Self::PanelError { .. } => 0x18,
            Self::SetLed { .. } => 0x20,
            Self::SetLedBrightness { .. } => 0x21,
            Self::SetLedBlink(_) => 0x22,
            Self::SetRingSegment(_) => 0x23,
            Self::SetRingValue { .. } => 0x24,
            Self::SetRingAnimation { .. } => 0x25,
            Self::SetGlobalBrightness { .. } => 0x26,
            Self::EnterBootloader { .. } => 0x27,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    RuntimeToPanel,
    PanelToRuntime,
    Bidirectional,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Frame {
    pub protocol_version: u8,
    pub session_id: u64,
    pub sequence: u32,
    pub message: Message,
}

impl Frame {
    #[must_use]
    pub const fn handshake(session_id: u64, message: Message) -> Option<Self> {
        if session_id == 0 || !message.is_handshake() {
            None
        } else {
            Some(Self {
                protocol_version: 0,
                session_id,
                sequence: 0,
                message,
            })
        }
    }

    #[must_use]
    pub const fn application(
        protocol_version: u8,
        session_id: u64,
        sequence: u32,
        message: Message,
    ) -> Option<Self> {
        if protocol_version == 0 || session_id == 0 || sequence == 0 || message.is_handshake() {
            None
        } else {
            Some(Self {
                protocol_version,
                session_id,
                sequence,
                message,
            })
        }
    }

    #[must_use]
    pub const fn is_structurally_valid(self) -> bool {
        self.session_id != 0
            && if self.message.is_handshake() {
                self.protocol_version == 0 && self.sequence == 0
            } else {
                self.protocol_version != 0 && self.sequence != 0
            }
    }
}

impl fmt::Display for PanelErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::UnsupportedControl => "unsupported control",
            Self::InvalidStateTransition => "invalid state transition",
            Self::TransmitOverflow => "transmit overflow",
            Self::BootloaderUnavailable => "bootloader unavailable",
        };
        formatter.write_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_selects_the_highest_shared_version() {
        let local = SupportedVersions::new(1, 3).unwrap();
        let peer = SupportedVersions::new(2, 4).unwrap();
        assert_eq!(local.negotiate(peer), Some(3));
        assert_eq!(local.negotiate(SupportedVersions::new(4, 5).unwrap()), None);
    }

    #[test]
    fn descriptors_reject_impossible_counts() {
        let valid = PanelDescriptor {
            firmware: FirmwareVersion {
                major: 1,
                minor: 2,
                patch: 3,
                build: 4,
            },
            boot_id: 1,
            encoders: 4,
            buttons: 8,
            leds: 8,
            rings: 1,
            ring_segments: 32,
        };
        assert!(valid.is_valid());
        assert!(
            !PanelDescriptor {
                boot_id: 0,
                ..valid
            }
            .is_valid()
        );
        assert!(
            !PanelDescriptor {
                encoders: 17,
                ..valid
            }
            .is_valid()
        );
        assert!(
            !PanelDescriptor {
                rings: 0,
                ring_segments: 32,
                ..valid
            }
            .is_valid()
        );
    }

    #[test]
    fn descriptors_bound_messages_and_snapshot_bits() {
        let descriptor = PanelDescriptor {
            firmware: FirmwareVersion {
                major: 1,
                minor: 0,
                patch: 0,
                build: 0,
            },
            boot_id: 1,
            encoders: 4,
            buttons: 8,
            leds: 12,
            rings: 1,
            ring_segments: 32,
        };
        assert!(
            descriptor.accepts_panel_message(Message::InputSnapshot(InputState {
                timestamp_us: 0,
                encoder_pressed: 0b1111,
                button_pressed: 0xff,
                modifiers: 0,
            }))
        );
        assert!(
            !descriptor.accepts_panel_message(Message::InputSnapshot(InputState {
                timestamp_us: 0,
                encoder_pressed: 1 << 4,
                button_pressed: 0,
                modifiers: 0,
            }))
        );
        assert!(
            !descriptor.accepts_runtime_message(Message::SetRingSegment(RingSegment {
                ring_id: 0,
                segment: 32,
                brightness: 1,
            }))
        );
    }
}
