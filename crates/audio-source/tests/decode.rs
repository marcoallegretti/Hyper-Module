use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_audio_source::{
    AudioContainer, DecodeError, DecodeLimits, DecodedAudio, decode_bytes, decode_file,
    decode_file_at_source_rate, encode_float_wav, resample_audio,
};

static NEXT_FILE_ID: AtomicU64 = AtomicU64::new(0);
const EPSILON: f32 = 1.0e-6;
const FLAC_FIXTURE_BASE64: &str = "ZkxhQwAAACIQABAAAAALAAANCsRA8AADXVSKK2c+p/lnjCv6lOIyKK0KhAAAKCAAAAByZWZlcmVuY2UgbGliRkxBQyAxLjQuMiAyMDIyMTAyMgAAAAD/+MkIAJUAAAAhvf/4yQgBkgAAAE3F//jJCAKbAAAA+U3/+MkIA5wAAACVNf/4yQgEiQAAABBY//jJCAWOAAAAfCD/+MkIBocAAADIqP/4yQgHgAAAAKTQ//jJCAitAAAAQnf/+MkICaoAAAAuD//4yQgKowAAAJqH//jJCAukAAAA9v//+MkIDLEAAABzkv/4yQgNtgAAAB/q//jJCA6/AAAAq2L/+MkID7gAAADHGv/4yQgQ5QAAAOYp//jJCBHiAAAAilH/+MkIEusAAAA+2f/4yQgT7AAAAFKh//jJCBT5AAAA18z/+MkIFf4AAAC7tP/4yQgW9wAAAA88//jJCBfwAAAAY0T/+MkIGN0AAACF4//4yQgZ2gAAAOmb//jJCBrTAAAAXRP/+MkIG9QAAAAxa//4yQgcwQAAALQG//jJCB3GAAAA2H7/+MkIHs8AAABs9v/4yQgfyAAAAACO//jJCCB1AAAALpD/+MkIIXIAAABC6P/4yQgiewAAAPZg//jJCCN8AAAAmhj/+MkIJGkAAAAfdf/4yQglbgAAAHMN//jJCCZnAAAAx4X/+MkIJ2AAAACr/f/4yQgoTQAAAE1a//jJCClKAAAAISL/+MkIKkMAAACVqv/4yQgrRAAAAPnS//jJCCxRAAAAfL//+MkILVYAAAAQx//4yQguXwAAAKRP//jJCC9YAAAAyDf/+MkIMAUAAADpBP/4yQgxAgAAAIV8//jJCDILAAAAMfT/+MkIMwwAAABdjP/4yQg0GQAAANjh//h5CDUNUxcAAAAcSg==";

struct TemporaryAudioFile {
    path: PathBuf,
}

impl TemporaryAudioFile {
    fn create(extension: &str, bytes: &[u8]) -> Self {
        let sequence = NEXT_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hyper-audio-source-{}-{sequence}.{extension}",
            std::process::id()
        ));
        let mut file = File::create(&path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryAudioFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn pcm16_wav(channels: u16, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
    assert_eq!(samples.len() % usize::from(channels), 0);
    let data_bytes = u32::try_from(samples.len() * 2).unwrap();
    let block_align = channels * 2;
    let mut bytes = Vec::with_capacity(44 + usize::try_from(data_bytes).unwrap());
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn float32_wav(sample: f32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(48);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&40_u32.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&3_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&48_000_u32.to_le_bytes());
    bytes.extend_from_slice(&192_000_u32.to_le_bytes());
    bytes.extend_from_slice(&4_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&4_u32.to_le_bytes());
    bytes.extend_from_slice(&sample.to_le_bytes());
    bytes
}

fn limits() -> DecodeLimits {
    DecodeLimits::new(1_000_000, 300_000, 2_400_000).unwrap()
}

fn assert_samples(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "sample {index}: actual={actual}, expected={expected}"
        );
    }
}

#[test]
fn decodes_pcm16_mono_and_stereo_to_planar_f32() {
    let mono_bytes = pcm16_wav(1, 48_000, &[-32_768, -16_384, 0, 16_384, 32_767]);
    let mono_file = TemporaryAudioFile::create("wav", &mono_bytes);
    let mono = decode_file(mono_file.path(), AudioContainer::Wav, 48_000, limits()).unwrap();
    let memory_mono = decode_bytes(mono_bytes, AudioContainer::Wav, 48_000, limits()).unwrap();
    assert_eq!(memory_mono, mono);
    assert_eq!(mono.sample_rate_hz(), 48_000);
    assert_eq!(mono.channels(), 1);
    assert_eq!(mono.frame_count(), 5);
    assert_samples(
        mono.plane(0).unwrap(),
        &[-1.0, -0.5, 0.0, 0.5, 32_767.0 / 32_768.0],
    );

    let stereo_file =
        TemporaryAudioFile::create("wav", &pcm16_wav(2, 48_000, &[-32_768, 0, 16_384, 32_767]));
    let stereo = decode_file(stereo_file.path(), AudioContainer::Wav, 48_000, limits()).unwrap();
    assert_eq!(stereo.channels(), 2);
    assert_eq!(stereo.frame_count(), 2);
    assert_samples(stereo.plane(0).unwrap(), &[-1.0, 0.5]);
    assert_samples(stereo.plane(1).unwrap(), &[0.0, 32_767.0 / 32_768.0]);
}

#[test]
fn decodes_and_verifies_pinned_flac_fixture() {
    let fixture = TemporaryAudioFile::create("flac", &decode_base64(FLAC_FIXTURE_BASE64));
    let audio = decode_file(fixture.path(), AudioContainer::Flac, 44_100, limits()).unwrap();
    assert_eq!(audio.sample_rate_hz(), 44_100);
    assert_eq!(audio.channels(), 1);
    assert_eq!(audio.frame_count(), 220_500);
    assert!(
        audio
            .plane(0)
            .unwrap()
            .iter()
            .all(|sample| sample.is_finite())
    );
}

#[test]
fn rejects_wrong_container_rate_layout_and_limits() {
    let wav = pcm16_wav(1, 48_000, &[0, 1, 2, 3]);
    let file = TemporaryAudioFile::create("wav", &wav);
    assert!(matches!(
        decode_file(file.path(), AudioContainer::Flac, 48_000, limits()),
        Err(DecodeError::ContainerMismatch {
            expected: AudioContainer::Flac
        })
    ));
    assert!(matches!(
        decode_file(file.path(), AudioContainer::Wav, 44_100, limits()),
        Err(DecodeError::SampleRateMismatch {
            expected: 44_100,
            actual: 48_000
        })
    ));
    assert!(matches!(
        decode_file(
            file.path(),
            AudioContainer::Wav,
            48_000,
            DecodeLimits::new(43, 10, 80).unwrap()
        ),
        Err(DecodeError::FileTooLarge { .. })
    ));
    assert!(matches!(
        decode_file(
            file.path(),
            AudioContainer::Wav,
            48_000,
            DecodeLimits::new(1_000, 3, 24).unwrap()
        ),
        Err(DecodeError::FrameLimitExceeded {
            declared: 4,
            maximum: 3
        })
    ));
    assert!(matches!(
        decode_file(
            file.path(),
            AudioContainer::Wav,
            48_000,
            DecodeLimits::new(1_000, 10, 15).unwrap()
        ),
        Err(DecodeError::DecodedSampleByteLimitExceeded {
            declared: 16,
            maximum: 15
        })
    ));

    let surround = TemporaryAudioFile::create("wav", &pcm16_wav(3, 48_000, &[0, 0, 0]));
    assert!(matches!(
        decode_file(surround.path(), AudioContainer::Wav, 48_000, limits()),
        Err(DecodeError::UnsupportedChannelLayout { channels: 3 })
    ));
}

#[test]
fn rejects_truncation_malformed_media_and_non_finite_samples() {
    let mut truncated = pcm16_wav(1, 48_000, &[0, 1, 2, 3]);
    truncated.truncate(truncated.len() - 2);
    let truncated_file = TemporaryAudioFile::create("wav", &truncated);
    assert!(decode_file(truncated_file.path(), AudioContainer::Wav, 48_000, limits()).is_err());

    let malformed_file = TemporaryAudioFile::create("wav", b"RIFF\x04\0\0\0WAVE");
    assert!(decode_file(malformed_file.path(), AudioContainer::Wav, 48_000, limits()).is_err());

    let non_finite_file = TemporaryAudioFile::create("wav", &float32_wav(f32::NAN));
    assert!(matches!(
        decode_file(
            non_finite_file.path(),
            AudioContainer::Wav,
            48_000,
            limits()
        ),
        Err(DecodeError::NonFiniteSample {
            frame: 0,
            channel: 0
        })
    ));
}

#[test]
fn validates_limits_and_in_memory_sources() {
    assert!(matches!(
        DecodeLimits::new(0, 1, 8),
        Err(DecodeError::InvalidLimits)
    ));
    assert!(matches!(
        DecodeLimits::new(1, 0, 8),
        Err(DecodeError::InvalidLimits)
    ));
    assert!(matches!(
        DecodeLimits::new(1, 1, 0),
        Err(DecodeError::InvalidLimits)
    ));
    assert!(matches!(
        DecodedAudio::from_mono(48_000, vec![]),
        Err(DecodeError::InvalidDecodedAudio(_))
    ));
    assert!(matches!(
        DecodedAudio::from_stereo(48_000, vec![0.0], vec![]),
        Err(DecodeError::InvalidDecodedAudio(_))
    ));
    assert!(matches!(
        DecodedAudio::from_mono(48_000, vec![f32::INFINITY]),
        Err(DecodeError::NonFiniteSample {
            frame: 0,
            channel: 0
        })
    ));
    assert!(matches!(
        decode_bytes(Vec::new(), AudioContainer::Wav, 0, limits()),
        Err(DecodeError::InvalidDecodedAudio(_))
    ));
}

#[test]
fn source_rate_decode_resample_and_float_wav_round_trip() {
    let source_samples: Vec<i16> = (0..441)
        .map(|frame| if frame == 120 { 16_384 } else { 0 })
        .collect();
    let source = TemporaryAudioFile::create("wav", &pcm16_wav(1, 44_100, &source_samples));
    let decoded = decode_file_at_source_rate(source.path(), AudioContainer::Wav, limits()).unwrap();
    assert_eq!(decoded.sample_rate_hz(), 44_100);
    assert_eq!(decoded.frame_count(), 441);

    let converted = resample_audio(decoded, 48_000, 500).unwrap();
    assert_eq!(converted.sample_rate_hz(), 48_000);
    assert_eq!(converted.channels(), 1);
    assert_eq!(converted.frame_count(), 480);
    assert!(
        converted
            .plane(0)
            .unwrap()
            .iter()
            .all(|sample| sample.is_finite())
    );
    assert!(
        converted
            .plane(0)
            .unwrap()
            .iter()
            .copied()
            .fold(0.0_f32, f32::max)
            > 0.4
    );

    let wav = encode_float_wav(&converted).unwrap();
    let round_trip = decode_bytes(wav, AudioContainer::Wav, 48_000, limits()).unwrap();
    assert_eq!(round_trip, converted);
}

#[test]
fn same_rate_conversion_is_exact_and_output_bound_is_enforced() {
    let audio = DecodedAudio::from_stereo(48_000, vec![0.25, -0.5], vec![-0.25, 0.5]).unwrap();
    assert_eq!(resample_audio(audio.clone(), 48_000, 2).unwrap(), audio);
    assert!(matches!(
        resample_audio(audio.clone(), 48_000, 1),
        Err(DecodeError::OutputFrameLimitExceeded {
            actual: 2,
            maximum: 1
        })
    ));

    let upsampled = resample_audio(audio, 96_000, 3);
    assert!(matches!(
        upsampled,
        Err(DecodeError::OutputFrameLimitExceeded {
            actual: 4,
            maximum: 3
        })
    ));
}

fn decode_base64(encoded: &str) -> Vec<u8> {
    let mut output = Vec::with_capacity(encoded.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in encoded.bytes().filter(|byte| *byte != b'=') {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("invalid base64 fixture"),
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(u8::try_from((accumulator >> bits) & 0xff).unwrap());
            accumulator &= (1_u32 << bits) - 1;
        }
    }
    output
}
