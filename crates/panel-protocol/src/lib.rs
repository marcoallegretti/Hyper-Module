#![no_std]
#![doc = "Bounded binary protocol shared by the runtime and panel firmware."]

mod codec;
mod link;
mod message;

pub use codec::{DecodeError, DecoderEvent, EncodeError, StreamDecoder, encode_frame};
pub use link::{
    LinkConfig, LinkConfigError, LinkFault, LinkPoll, PanelLink, PanelReceive, RuntimeLink,
    RuntimeReceive,
};
pub use message::{
    Animation, Direction, FirmwareVersion, Frame, InputState, LedBlink, MAX_BUTTONS, MAX_ENCODERS,
    MAX_LEDS, MAX_RING_SEGMENTS, MAX_RINGS, Message, PanelDescriptor, PanelErrorCode, RingSegment,
    SupportedVersions,
};

pub const WIRE_VERSION: u8 = 1;
pub const PROTOCOL_VERSION: u8 = 1;
pub const FRAME_DELIMITER: u8 = 0;
pub const MAX_PAYLOAD_LEN: usize = 64;
pub const HEADER_LEN: usize = 20;
pub const CHECKSUM_LEN: usize = 4;
pub const MAX_DECODED_FRAME_LEN: usize = HEADER_LEN + MAX_PAYLOAD_LEN + CHECKSUM_LEN;
pub const MAX_ENCODED_FRAME_LEN: usize = cobs::max_encoding_length(MAX_DECODED_FRAME_LEN) + 1;

pub(crate) const MAGIC: [u8; 2] = *b"HP";

pub(crate) const fn next_sequence(sequence: u32) -> u32 {
    if sequence == u32::MAX {
        1
    } else {
        sequence + 1
    }
}
