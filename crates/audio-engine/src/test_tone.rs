use core::error::Error;
use core::f64::consts::TAU;
use core::fmt;

const STEREO_CHANNELS: usize = 2;

/// Validated parameters for a phase-continuous stereo sine test tone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoSineTestToneConfig {
    sample_rate_hz: u32,
    frequency_hz: f64,
    level_dbfs: f32,
    amplitude: f32,
    phase_increment: f64,
}

impl StereoSineTestToneConfig {
    /// Creates an explicit diagnostic sine-tone configuration.
    ///
    /// Frequency must be finite, positive, and strictly below Nyquist. Level
    /// is peak amplitude in dBFS and must be finite, no greater than 0 dBFS,
    /// and representable as a normal positive `f32` linear amplitude.
    ///
    /// # Errors
    ///
    /// Returns [`StereoSineTestToneConfigError`] when any domain is invalid.
    pub fn try_new(
        sample_rate_hz: u32,
        frequency_hz: f64,
        level_dbfs: f32,
    ) -> Result<Self, StereoSineTestToneConfigError> {
        if sample_rate_hz == 0 {
            return Err(StereoSineTestToneConfigError::ZeroSampleRate);
        }
        if !frequency_hz.is_finite() {
            return Err(StereoSineTestToneConfigError::NonFiniteFrequency);
        }
        let nyquist_hz = f64::from(sample_rate_hz) / 2.0;
        if frequency_hz <= 0.0 || frequency_hz >= nyquist_hz {
            return Err(StereoSineTestToneConfigError::FrequencyOutsideNyquist);
        }
        if !level_dbfs.is_finite() {
            return Err(StereoSineTestToneConfigError::NonFiniteLevel);
        }
        if level_dbfs > 0.0 {
            return Err(StereoSineTestToneConfigError::LevelAboveFullScale);
        }
        let amplitude = 10.0_f64.powf(f64::from(level_dbfs) / 20.0);
        if amplitude < f64::from(f32::MIN_POSITIVE) {
            return Err(StereoSineTestToneConfigError::LevelBelowNormalAmplitude);
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the validated amplitude is intentionally stored in the engine's f32 sample domain"
        )]
        let amplitude = amplitude as f32;
        Ok(Self {
            sample_rate_hz,
            frequency_hz,
            level_dbfs,
            amplitude,
            phase_increment: frequency_hz / f64::from(sample_rate_hz),
        })
    }

    #[must_use]
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn frequency_hz(self) -> f64 {
        self.frequency_hz
    }

    #[must_use]
    pub const fn level_dbfs(self) -> f32 {
        self.level_dbfs
    }

    #[must_use]
    pub const fn linear_amplitude(self) -> f32 {
        self.amplitude
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoSineTestToneConfigError {
    ZeroSampleRate,
    NonFiniteFrequency,
    FrequencyOutsideNyquist,
    NonFiniteLevel,
    LevelAboveFullScale,
    LevelBelowNormalAmplitude,
}

impl fmt::Display for StereoSineTestToneConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroSampleRate => "test tone requires a nonzero sample rate",
            Self::NonFiniteFrequency => "test tone frequency must be finite",
            Self::FrequencyOutsideNyquist => {
                "test tone frequency must be positive and strictly below Nyquist"
            }
            Self::NonFiniteLevel => "test tone level must be finite",
            Self::LevelAboveFullScale => "test tone level must not exceed 0 dBFS",
            Self::LevelBelowNormalAmplitude => {
                "test tone level is below the normal f32 amplitude range"
            }
        })
    }
}

impl Error for StereoSineTestToneConfigError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoSineTestToneRenderError {
    SampleCountNotStereoAligned { samples: usize },
}

impl fmt::Display for StereoSineTestToneRenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleCountNotStereoAligned { samples } => write!(
                formatter,
                "test tone output has {samples} samples; expected complete stereo frames"
            ),
        }
    }
}

impl Error for StereoSineTestToneRenderError {}

/// Phase-continuous dual-mono sine generator for output diagnostics.
///
/// Rendering performs no allocation, I/O, locking, logging, or dynamic
/// dispatch. The first sample and every explicit reset begin at a zero crossing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoSineTestTone {
    config: StereoSineTestToneConfig,
    phase_cycles: f64,
}

impl StereoSineTestTone {
    #[must_use]
    pub const fn new(config: StereoSineTestToneConfig) -> Self {
        Self {
            config,
            phase_cycles: 0.0,
        }
    }

    #[must_use]
    pub const fn config(self) -> StereoSineTestToneConfig {
        self.config
    }

    /// Restarts the next rendered sample at a zero crossing.
    pub const fn reset(&mut self) {
        self.phase_cycles = 0.0;
    }

    /// Fills complete interleaved stereo frames with one dual-mono sine wave.
    ///
    /// Invalid shape is rejected before output or phase state changes.
    ///
    /// # Errors
    ///
    /// Returns [`StereoSineTestToneRenderError::SampleCountNotStereoAligned`]
    /// when `output` does not contain complete stereo frames.
    pub fn render_interleaved_stereo(
        &mut self,
        output: &mut [f32],
    ) -> Result<(), StereoSineTestToneRenderError> {
        if !output.len().is_multiple_of(STEREO_CHANNELS) {
            return Err(StereoSineTestToneRenderError::SampleCountNotStereoAligned {
                samples: output.len(),
            });
        }
        for frame in output.chunks_exact_mut(STEREO_CHANNELS) {
            let sample = self.next_sample();
            frame.fill(sample);
        }
        Ok(())
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "the oscillator intentionally produces the engine's f32 sample domain"
    )]
    fn next_sample(&mut self) -> f32 {
        let sample = (TAU * self.phase_cycles).sin() * f64::from(self.config.amplitude);
        self.phase_cycles += self.config.phase_increment;
        if self.phase_cycles >= 1.0 {
            self.phase_cycles -= 1.0;
        }
        sample as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1.0e-6;

    fn assert_samples(actual: &[f32], expected: &[f32]) {
        assert_eq!(actual.len(), expected.len());
        for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
            assert!(
                (actual - expected).abs() <= EPSILON,
                "sample {index}: expected {expected}, got {actual}"
            );
        }
    }

    #[test]
    fn configuration_rejects_invalid_frequency_and_level_domains() {
        assert_eq!(
            StereoSineTestToneConfig::try_new(0, 440.0, -18.0),
            Err(StereoSineTestToneConfigError::ZeroSampleRate)
        );
        for frequency in [f64::NAN, f64::INFINITY] {
            assert_eq!(
                StereoSineTestToneConfig::try_new(48_000, frequency, -18.0),
                Err(StereoSineTestToneConfigError::NonFiniteFrequency)
            );
        }
        for frequency in [-1.0, 0.0, 24_000.0, 48_000.0] {
            assert_eq!(
                StereoSineTestToneConfig::try_new(48_000, frequency, -18.0),
                Err(StereoSineTestToneConfigError::FrequencyOutsideNyquist)
            );
        }
        for level in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                StereoSineTestToneConfig::try_new(48_000, 440.0, level),
                Err(StereoSineTestToneConfigError::NonFiniteLevel)
            );
        }
        assert_eq!(
            StereoSineTestToneConfig::try_new(48_000, 440.0, 0.01),
            Err(StereoSineTestToneConfigError::LevelAboveFullScale)
        );
        assert_eq!(
            StereoSineTestToneConfig::try_new(48_000, 440.0, -800.0),
            Err(StereoSineTestToneConfigError::LevelBelowNormalAmplitude)
        );
    }

    #[test]
    fn renders_known_dual_mono_samples_and_resets_at_zero() {
        let config = StereoSineTestToneConfig::try_new(4, 1.0, 0.0).unwrap();
        let mut tone = StereoSineTestTone::new(config);
        let mut output = [9.0_f32; 8];

        tone.render_interleaved_stereo(&mut output).unwrap();
        assert_samples(&output, &[0.0, 0.0, 1.0, 1.0, 0.0, 0.0, -1.0, -1.0]);

        tone.reset();
        let mut reset = [9.0_f32; 2];
        tone.render_interleaved_stereo(&mut reset).unwrap();
        assert_samples(&reset, &[0.0, 0.0]);
    }

    #[test]
    fn rendering_is_callback_partition_independent() {
        let config = StereoSineTestToneConfig::try_new(48_000, 997.0, -18.0).unwrap();
        let mut continuous = StereoSineTestTone::new(config);
        let mut partitioned = StereoSineTestTone::new(config);
        let mut expected = [0.0_f32; 2_000];
        let mut actual = [0.0_f32; 2_000];

        continuous.render_interleaved_stereo(&mut expected).unwrap();
        for chunk in actual.chunks_mut(74) {
            partitioned.render_interleaved_stereo(chunk).unwrap();
        }

        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| actual.to_bits() == expected.to_bits())
        );
    }

    #[test]
    fn malformed_output_is_failure_atomic() {
        let config = StereoSineTestToneConfig::try_new(48_000, 1_000.0, -18.0).unwrap();
        let mut tone = StereoSineTestTone::new(config);
        let initial = tone;
        let mut output = [9.0_f32; 3];

        assert_eq!(
            tone.render_interleaved_stereo(&mut output),
            Err(StereoSineTestToneRenderError::SampleCountNotStereoAligned { samples: 3 })
        );
        assert_eq!(tone, initial);
        assert_samples(&output, &[9.0; 3]);
    }
}
