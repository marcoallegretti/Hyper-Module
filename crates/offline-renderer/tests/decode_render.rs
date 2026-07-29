use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
use hyper_audio_source::{AudioContainer, DecodeLimits, decode_file};
use hyper_offline_renderer::{AlignedInput, RenderSpec, render_aligned};

const EPSILON: f32 = 1.0e-6;

struct TemporaryWave {
    path: PathBuf,
}

impl TemporaryWave {
    fn mono(samples: &[i16]) -> Self {
        let data_bytes = u32::try_from(samples.len() * 2).unwrap();
        let mut bytes = Vec::with_capacity(44 + usize::try_from(data_bytes).unwrap());
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&96_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }

        let path =
            std::env::temp_dir().join(format!("hyper-decode-render-{}.wav", std::process::id()));
        let mut file = File::create(&path).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        Self { path }
    }
}

impl Drop for TemporaryWave {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[test]
fn decoded_wave_reaches_the_shared_offline_mixer() {
    let wave = TemporaryWave::mono(&[-32_768, -16_384, 0, 16_384]);
    let source = decode_file(
        &wave.path,
        AudioContainer::Wav,
        48_000,
        DecodeLimits::new(1_000, 4, 32).unwrap(),
    )
    .unwrap();
    let output = render_aligned(
        &[AlignedInput {
            source: &source,
            parameters: ChannelParameters::new(LinearGain::UNITY, Pan::HARD_RIGHT, true),
        }],
        RenderSpec {
            sample_rate_hz: 48_000,
            frame_count: 4,
            block_frames: 2,
        },
        LinearGain::UNITY,
    )
    .unwrap();

    assert_samples(output.plane(0).unwrap(), &[0.0; 4]);
    assert_samples(output.plane(1).unwrap(), &[-1.0, -0.5, 0.0, 0.5]);
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
