#![doc = "Bounded offline WAV and FLAC decoding for Hyper Module."]
#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use symphonia::core::audio::{Channels, SampleBuffer};
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const MAX_DECODE_PACKET_FRAMES: u64 = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioContainer {
    Wav,
    Flac,
}

impl AudioContainer {
    const fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    file_bytes: u64,
    frames: u64,
    decoded_sample_bytes: usize,
}

impl DecodeLimits {
    /// Creates explicit encoded-size, decoded-frame, and decoded-sample limits.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidLimits`] when any limit is zero or the
    /// frame limit cannot be represented by the current platform.
    pub fn new(
        max_file_bytes: u64,
        max_frames: u64,
        max_decoded_sample_bytes: usize,
    ) -> Result<Self, DecodeError> {
        let stereo_samples_are_addressable = max_frames
            .checked_mul(2)
            .and_then(|samples| usize::try_from(samples).ok())
            .is_some();
        if max_file_bytes == 0
            || max_frames == 0
            || max_decoded_sample_bytes == 0
            || usize::try_from(max_frames).is_err()
            || !stereo_samples_are_addressable
        {
            Err(DecodeError::InvalidLimits)
        } else {
            Ok(Self {
                file_bytes: max_file_bytes,
                frames: max_frames,
                decoded_sample_bytes: max_decoded_sample_bytes,
            })
        }
    }

    #[must_use]
    pub const fn max_file_bytes(self) -> u64 {
        self.file_bytes
    }

    #[must_use]
    pub const fn max_frames(self) -> u64 {
        self.frames
    }

    #[must_use]
    pub const fn max_decoded_sample_bytes(self) -> usize {
        self.decoded_sample_bytes
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Planes {
    Mono(Vec<f32>),
    Stereo { left: Vec<f32>, right: Vec<f32> },
}

/// Fully decoded, finite, planar mono or stereo floating-point audio.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedAudio {
    sample_rate_hz: u32,
    planes: Planes,
}

impl DecodedAudio {
    /// Creates a validated mono source.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidDecodedAudio`] for a zero sample rate, no
    /// frames, or a non-finite sample.
    pub fn from_mono(sample_rate_hz: u32, samples: Vec<f32>) -> Result<Self, DecodeError> {
        validate_decoded_planes(sample_rate_hz, &samples, None)?;
        Ok(Self {
            sample_rate_hz,
            planes: Planes::Mono(samples),
        })
    }

    /// Creates a validated stereo source.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::InvalidDecodedAudio`] for a zero sample rate,
    /// empty or unequal planes, or a non-finite sample.
    pub fn from_stereo(
        sample_rate_hz: u32,
        left: Vec<f32>,
        right: Vec<f32>,
    ) -> Result<Self, DecodeError> {
        validate_decoded_planes(sample_rate_hz, &left, Some(&right))?;
        Ok(Self {
            sample_rate_hz,
            planes: Planes::Stereo { left, right },
        })
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        match self.planes {
            Planes::Mono(_) => 1,
            Planes::Stereo { .. } => 2,
        }
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.plane(0).map_or(0, <[f32]>::len)
    }

    #[must_use]
    pub fn plane(&self, channel: usize) -> Option<&[f32]> {
        match (&self.planes, channel) {
            (Planes::Mono(samples), 0) => Some(samples),
            (Planes::Stereo { left, .. }, 0) => Some(left),
            (Planes::Stereo { right, .. }, 1) => Some(right),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Io(io::Error),
    Media(SymphoniaError),
    InvalidLimits,
    FileTooLarge { actual: u64, maximum: u64 },
    ContainerMismatch { expected: AudioContainer },
    AudioTrackCount { actual: usize },
    MissingStreamProperty(&'static str),
    SampleRateMismatch { expected: u32, actual: u32 },
    UnsupportedChannelLayout { channels: usize },
    FrameLimitExceeded { declared: u64, maximum: u64 },
    DecodedSampleByteLimitExceeded { declared: usize, maximum: usize },
    FrameCountMismatch { declared: u64, decoded: u64 },
    VerificationFailed,
    NonFiniteSample { frame: u64, channel: usize },
    CapacityUnavailable,
    InvalidDecodedAudio(&'static str),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "audio source I/O failed: {error}"),
            Self::Media(error) => write!(formatter, "audio media decoding failed: {error}"),
            Self::InvalidLimits => {
                formatter.write_str("decode limits must be positive and addressable")
            }
            Self::FileTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "audio file has {actual} bytes; maximum is {maximum}"
                )
            }
            Self::ContainerMismatch { expected } => {
                write!(
                    formatter,
                    "audio signature does not match declared {expected:?} container"
                )
            }
            Self::AudioTrackCount { actual } => {
                write!(
                    formatter,
                    "audio container has {actual} decodable tracks; exactly one is required"
                )
            }
            Self::MissingStreamProperty(property) => {
                write!(formatter, "audio stream does not declare {property}")
            }
            Self::SampleRateMismatch { expected, actual } => {
                write!(
                    formatter,
                    "audio sample rate is {actual} Hz; expected {expected} Hz"
                )
            }
            Self::UnsupportedChannelLayout { channels } => {
                write!(
                    formatter,
                    "audio channel layout has {channels} channels; front mono or stereo is required"
                )
            }
            Self::FrameLimitExceeded { declared, maximum } => {
                write!(
                    formatter,
                    "audio stream declares {declared} frames; maximum is {maximum}"
                )
            }
            Self::DecodedSampleByteLimitExceeded { declared, maximum } => write!(
                formatter,
                "audio stream declares {declared} decoded sample bytes; maximum is {maximum}"
            ),
            Self::FrameCountMismatch { declared, decoded } => {
                write!(
                    formatter,
                    "audio stream declares {declared} frames but decoded {decoded}"
                )
            }
            Self::VerificationFailed => {
                formatter.write_str("audio decoder integrity verification failed")
            }
            Self::NonFiniteSample { frame, channel } => {
                write!(
                    formatter,
                    "decoded sample at frame {frame}, channel {channel} is not finite"
                )
            }
            Self::CapacityUnavailable => {
                formatter.write_str("decoded audio capacity cannot be reserved")
            }
            Self::InvalidDecodedAudio(reason) => {
                write!(formatter, "invalid decoded audio: {reason}")
            }
        }
    }
}

impl Error for DecodeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Media(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for DecodeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<SymphoniaError> for DecodeError {
    fn from(error: SymphoniaError) -> Self {
        Self::Media(error)
    }
}

/// Decodes one complete WAV or native FLAC file outside the real-time path.
///
/// # Errors
///
/// Returns [`DecodeError`] for I/O failure, a mismatched container, unsupported
/// or incomplete stream metadata, malformed media, an exceeded bound,
/// integrity failure, a non-finite sample, or decoded/header frame mismatch.
pub fn decode_file(
    path: &Path,
    container: AudioContainer,
    expected_sample_rate_hz: u32,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    require_expected_sample_rate(expected_sample_rate_hz)?;
    let mut file = File::open(path)?;
    let file_length = file.metadata()?.len();
    require_file_limit(file_length, limits)?;
    verify_container_signature(&mut file, container)?;
    file.seek(SeekFrom::Start(0))?;
    decode_source(Box::new(file), container, expected_sample_rate_hz, limits)
}

/// Decodes complete verified WAV or native FLAC bytes outside the real-time
/// path.
///
/// Owning the encoded bytes keeps the decoded media tied to the exact package
/// payload that was hashed by the caller.
///
/// # Errors
///
/// Returns [`DecodeError`] under the same rules as [`decode_file`].
pub fn decode_bytes(
    bytes: Vec<u8>,
    container: AudioContainer,
    expected_sample_rate_hz: u32,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    require_expected_sample_rate(expected_sample_rate_hz)?;
    let file_length = u64::try_from(bytes.len()).map_err(|_| DecodeError::CapacityUnavailable)?;
    require_file_limit(file_length, limits)?;
    let mut cursor = Cursor::new(bytes);
    verify_container_signature(&mut cursor, container)?;
    cursor.seek(SeekFrom::Start(0))?;
    decode_source(Box::new(cursor), container, expected_sample_rate_hz, limits)
}

fn require_file_limit(file_length: u64, limits: DecodeLimits) -> Result<(), DecodeError> {
    if file_length > limits.file_bytes {
        Err(DecodeError::FileTooLarge {
            actual: file_length,
            maximum: limits.file_bytes,
        })
    } else {
        Ok(())
    }
}

fn require_expected_sample_rate(expected_sample_rate_hz: u32) -> Result<(), DecodeError> {
    if expected_sample_rate_hz == 0 {
        Err(DecodeError::InvalidDecodedAudio(
            "expected sample rate must be positive",
        ))
    } else {
        Ok(())
    }
}

// Keep the validation and decode transaction visible as one fail-closed pipeline.
#[allow(clippy::too_many_lines)]
fn decode_source(
    source: Box<dyn MediaSource>,
    container: AudioContainer,
    expected_sample_rate_hz: u32,
    limits: DecodeLimits,
) -> Result<DecodedAudio, DecodeError> {
    let media_stream = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    hint.with_extension(container.extension());
    let probed = symphonia::default::get_probe().format(
        &hint,
        media_stream,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    let audio_track_count = format
        .tracks()
        .iter()
        .filter(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .count();
    if audio_track_count != 1 {
        return Err(DecodeError::AudioTrackCount {
            actual: audio_track_count,
        });
    }

    let audio_track = format
        .tracks()
        .iter()
        .find(|track| track.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::AudioTrackCount { actual: 0 })?;
    let track_id = audio_track.id;
    let parameters = audio_track.codec_params.clone();
    let sample_rate_hz = parameters
        .sample_rate
        .ok_or(DecodeError::MissingStreamProperty("sample rate"))?;
    if sample_rate_hz != expected_sample_rate_hz {
        return Err(DecodeError::SampleRateMismatch {
            expected: expected_sample_rate_hz,
            actual: sample_rate_hz,
        });
    }
    let channels = parameters
        .channels
        .ok_or(DecodeError::MissingStreamProperty("channel layout"))?;
    let channel_count = validate_channel_layout(channels)?;
    let declared_frames = parameters
        .n_frames
        .ok_or(DecodeError::MissingStreamProperty("frame count"))?;
    if declared_frames == 0 {
        return Err(DecodeError::InvalidDecodedAudio(
            "declared frame count must be positive",
        ));
    }
    if declared_frames > limits.frames {
        return Err(DecodeError::FrameLimitExceeded {
            declared: declared_frames,
            maximum: limits.frames,
        });
    }
    let declared_sample_bytes = usize::try_from(declared_frames)
        .ok()
        .and_then(|frames| frames.checked_mul(channel_count))
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or(DecodeError::CapacityUnavailable)?;
    if declared_sample_bytes > limits.decoded_sample_bytes {
        return Err(DecodeError::DecodedSampleByteLimitExceeded {
            declared: declared_sample_bytes,
            maximum: limits.decoded_sample_bytes,
        });
    }

    let declared_capacity =
        usize::try_from(declared_frames).map_err(|_| DecodeError::CapacityUnavailable)?;
    let mut left = Vec::new();
    left.try_reserve_exact(declared_capacity)
        .map_err(|_| DecodeError::CapacityUnavailable)?;
    let mut right = if channel_count == 2 {
        let mut plane = Vec::new();
        plane
            .try_reserve_exact(declared_capacity)
            .map_err(|_| DecodeError::CapacityUnavailable)?;
        Some(plane)
    } else {
        None
    };

    let mut decoder =
        symphonia::default::get_codecs().make(&parameters, &DecoderOptions { verify: true })?;
    let mut conversion_buffer: Option<SampleBuffer<f32>> = None;
    let mut decoded_frames = 0_u64;

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            Err(SymphoniaError::IoError(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let audio_buffer = decoder.decode(&packet)?;
        let spec = *audio_buffer.spec();
        if spec.rate != sample_rate_hz || spec.channels != channels {
            return Err(DecodeError::Media(SymphoniaError::ResetRequired));
        }

        let packet_capacity_frames =
            u64::try_from(audio_buffer.capacity()).map_err(|_| DecodeError::CapacityUnavailable)?;
        if packet_capacity_frames > MAX_DECODE_PACKET_FRAMES {
            return Err(DecodeError::FrameLimitExceeded {
                declared: packet_capacity_frames,
                maximum: MAX_DECODE_PACKET_FRAMES,
            });
        }
        let required_samples = audio_buffer
            .capacity()
            .checked_mul(channel_count)
            .ok_or(DecodeError::CapacityUnavailable)?;
        if conversion_buffer
            .as_ref()
            .is_none_or(|buffer| buffer.capacity() < required_samples)
        {
            conversion_buffer = Some(SampleBuffer::<f32>::new(packet_capacity_frames, spec));
        }
        let conversion = conversion_buffer
            .as_mut()
            .ok_or(DecodeError::CapacityUnavailable)?;
        conversion.copy_interleaved_ref(audio_buffer);
        let samples = conversion.samples();
        if samples.len() % channel_count != 0 {
            return Err(DecodeError::Media(SymphoniaError::DecodeError(
                "decoded sample count is not channel aligned",
            )));
        }
        let packet_frames = u64::try_from(samples.len() / channel_count)
            .map_err(|_| DecodeError::CapacityUnavailable)?;
        let next_frame_count = decoded_frames
            .checked_add(packet_frames)
            .ok_or(DecodeError::CapacityUnavailable)?;
        if next_frame_count > limits.frames || next_frame_count > declared_frames {
            return Err(DecodeError::FrameLimitExceeded {
                declared: next_frame_count,
                maximum: limits.frames.min(declared_frames),
            });
        }

        for (packet_frame, frame) in samples.chunks_exact(channel_count).enumerate() {
            for (channel, sample) in frame.iter().copied().enumerate() {
                if !sample.is_finite() {
                    return Err(DecodeError::NonFiniteSample {
                        frame: decoded_frames
                            + u64::try_from(packet_frame)
                                .map_err(|_| DecodeError::CapacityUnavailable)?,
                        channel,
                    });
                }
            }
            left.push(frame[0]);
            if let Some(right) = &mut right {
                right.push(frame[1]);
            }
        }
        decoded_frames = next_frame_count;
    }

    if decoder.finalize().verify_ok == Some(false) {
        return Err(DecodeError::VerificationFailed);
    }
    if decoded_frames != declared_frames {
        return Err(DecodeError::FrameCountMismatch {
            declared: declared_frames,
            decoded: decoded_frames,
        });
    }

    match right {
        Some(right) => DecodedAudio::from_stereo(sample_rate_hz, left, right),
        None => DecodedAudio::from_mono(sample_rate_hz, left),
    }
}

fn validate_decoded_planes(
    sample_rate_hz: u32,
    left: &[f32],
    right: Option<&[f32]>,
) -> Result<(), DecodeError> {
    if sample_rate_hz == 0 {
        return Err(DecodeError::InvalidDecodedAudio(
            "sample rate must be positive",
        ));
    }
    if left.is_empty() {
        return Err(DecodeError::InvalidDecodedAudio(
            "at least one frame is required",
        ));
    }
    if right.is_some_and(|right| right.len() != left.len()) {
        return Err(DecodeError::InvalidDecodedAudio(
            "stereo plane lengths must match",
        ));
    }
    for (channel, plane) in [Some(left), right].into_iter().flatten().enumerate() {
        if let Some(frame) = plane.iter().position(|sample| !sample.is_finite()) {
            return Err(DecodeError::NonFiniteSample {
                frame: u64::try_from(frame).map_err(|_| DecodeError::CapacityUnavailable)?,
                channel,
            });
        }
    }
    Ok(())
}

fn validate_channel_layout(channels: Channels) -> Result<usize, DecodeError> {
    let mono = Channels::FRONT_LEFT;
    let stereo = Channels::FRONT_LEFT | Channels::FRONT_RIGHT;
    if channels == mono {
        Ok(1)
    } else if channels == stereo {
        Ok(2)
    } else {
        Err(DecodeError::UnsupportedChannelLayout {
            channels: channels.count(),
        })
    }
}

fn verify_container_signature<R: Read>(
    file: &mut R,
    container: AudioContainer,
) -> Result<(), DecodeError> {
    let mut header = [0_u8; 12];
    let read = match file.read_exact(&mut header) {
        Ok(()) => header.len(),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => 0,
        Err(error) => return Err(error.into()),
    };
    let valid = match container {
        AudioContainer::Wav => read >= 12 && &header[0..4] == b"RIFF" && &header[8..12] == b"WAVE",
        AudioContainer::Flac => read >= 4 && &header[0..4] == b"fLaC",
    };
    if valid {
        Ok(())
    } else {
        Err(DecodeError::ContainerMismatch {
            expected: container,
        })
    }
}
