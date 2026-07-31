use core::error::Error;
use core::f64::consts::PI;
use core::fmt;

pub const TRUE_PEAK_SAMPLE_RATE_HZ: u32 = 48_000;
pub const TRUE_PEAK_OVERSAMPLE_FACTOR: u32 = 4;

const HISTORY_FRAMES: usize = 48;
const MIRRORED_HISTORY_SAMPLES: usize = HISTORY_FRAMES * 2;
const INTERPOLATED_PHASES: usize = 3;
const WINDOW_RADIUS: f64 = 24.0;

/// Linear true-peak estimates emitted for one stereo input block.
///
/// The original sample peak is included without delay. Inter-sample estimates
/// use the preceding 48 input frames and therefore appear about 24 frames
/// after the samples that contribute to them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoTruePeakBlock {
    frame_count: usize,
    left: f32,
    right: f32,
}

impl StereoTruePeakBlock {
    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    #[must_use]
    pub const fn left_linear(self) -> f32 {
        self.left
    }

    #[must_use]
    pub const fn right_linear(self) -> f32 {
        self.right
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoTruePeakError {
    UnsupportedSampleRate { actual: u32, supported: u32 },
    ChannelLengthMismatch { left: usize, right: usize },
    NonFiniteSamples { samples: usize },
}

impl fmt::Display for StereoTruePeakError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSampleRate { actual, supported } => write!(
                formatter,
                "true-peak input rate is {actual} Hz; only {supported} Hz is supported"
            ),
            Self::ChannelLengthMismatch { left, right } => write!(
                formatter,
                "true-peak channel lengths differ: left={left}, right={right}"
            ),
            Self::NonFiniteSamples { samples } => {
                write!(
                    formatter,
                    "true-peak input contains {samples} non-finite samples"
                )
            }
        }
    }
}

impl Error for StereoTruePeakError {}

#[derive(Clone, Debug, PartialEq)]
struct ChannelTruePeakMeter {
    history: [f32; MIRRORED_HISTORY_SAMPLES],
    next_history: usize,
}

impl Default for ChannelTruePeakMeter {
    fn default() -> Self {
        Self {
            history: [0.0; MIRRORED_HISTORY_SAMPLES],
            next_history: 0,
        }
    }
}

impl ChannelTruePeakMeter {
    fn observe(&mut self, sample: f32, coefficients: &[[f64; HISTORY_FRAMES]]) -> f32 {
        self.history[self.next_history] = sample;
        self.history[self.next_history + HISTORY_FRAMES] = sample;
        self.next_history += 1;
        if self.next_history == HISTORY_FRAMES {
            self.next_history = 0;
        }

        let mut peak = sample.abs();
        let history = &self.history[self.next_history..self.next_history + HISTORY_FRAMES];
        for phase in coefficients {
            let mut value = 0.0_f64;
            for (sample, coefficient) in history.iter().zip(phase) {
                value += f64::from(*sample) * coefficient;
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the finite interpolated result intentionally returns to the engine's f32 meter domain"
            )]
            let value = value.abs() as f32;
            peak = peak.max(value);
        }
        peak
    }
}

/// Streaming stereo true-peak estimator for the version-1 48 kHz audio domain.
///
/// Each channel is evaluated at the original samples and at three
/// cosine-windowed sinc interpolation phases, producing a 192 kHz effective
/// meter rate. Construction and processing use fixed storage. Processing
/// performs no allocation, I/O, locking, logging, or dynamic dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct StereoTruePeakMeter {
    coefficients: [[f64; HISTORY_FRAMES]; INTERPOLATED_PHASES],
    left: ChannelTruePeakMeter,
    right: ChannelTruePeakMeter,
}

impl StereoTruePeakMeter {
    /// Creates a true-peak meter for the supported project sample rate.
    ///
    /// # Errors
    ///
    /// Returns [`StereoTruePeakError::UnsupportedSampleRate`] unless the input
    /// rate is exactly 48 kHz. This preserves the minimum 192 kHz effective
    /// meter rate without silently claiming other rate domains.
    pub fn try_new(sample_rate_hz: u32) -> Result<Self, StereoTruePeakError> {
        if sample_rate_hz != TRUE_PEAK_SAMPLE_RATE_HZ {
            return Err(StereoTruePeakError::UnsupportedSampleRate {
                actual: sample_rate_hz,
                supported: TRUE_PEAK_SAMPLE_RATE_HZ,
            });
        }
        Ok(Self {
            coefficients: interpolation_coefficients(),
            left: ChannelTruePeakMeter::default(),
            right: ChannelTruePeakMeter::default(),
        })
    }

    pub fn reset(&mut self) {
        self.left = ChannelTruePeakMeter::default();
        self.right = ChannelTruePeakMeter::default();
    }

    /// Measures one planar stereo block while retaining interpolation history.
    ///
    /// Shape and finite-value validation complete before either channel state
    /// changes. Empty matching channels produce zero peaks without advancing
    /// the filter.
    ///
    /// # Errors
    ///
    /// Returns [`StereoTruePeakError`] for mismatched channel lengths or any
    /// non-finite input sample.
    pub fn process(
        &mut self,
        left: &[f32],
        right: &[f32],
    ) -> Result<StereoTruePeakBlock, StereoTruePeakError> {
        if left.len() != right.len() {
            return Err(StereoTruePeakError::ChannelLengthMismatch {
                left: left.len(),
                right: right.len(),
            });
        }
        let non_finite_samples = left
            .iter()
            .chain(right)
            .filter(|sample| !sample.is_finite())
            .count();
        if non_finite_samples != 0 {
            return Err(StereoTruePeakError::NonFiniteSamples {
                samples: non_finite_samples,
            });
        }

        let mut left_peak = 0.0_f32;
        let mut right_peak = 0.0_f32;
        for (&left, &right) in left.iter().zip(right) {
            left_peak = left_peak.max(self.left.observe(left, &self.coefficients));
            right_peak = right_peak.max(self.right.observe(right, &self.coefficients));
        }
        Ok(StereoTruePeakBlock {
            frame_count: left.len(),
            left: left_peak,
            right: right_peak,
        })
    }
}

fn interpolation_coefficients() -> [[f64; HISTORY_FRAMES]; INTERPOLATED_PHASES] {
    let mut coefficients = [[0.0_f64; HISTORY_FRAMES]; INTERPOLATED_PHASES];
    for (phase_index, phase) in coefficients.iter_mut().enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "the phase index is bounded to three exact small integers"
        )]
        let fraction = (phase_index + 1) as f64 / f64::from(TRUE_PEAK_OVERSAMPLE_FACTOR);
        for (tap, coefficient) in phase.iter_mut().enumerate() {
            #[allow(
                clippy::cast_precision_loss,
                reason = "the tap index is bounded to 47 and exactly representable in f64"
            )]
            let distance = tap as f64 - (23.0 + fraction);
            let sinc = if distance == 0.0 {
                1.0
            } else {
                (PI * distance).sin() / (PI * distance)
            };
            let cosine_window = 0.5 * (1.0 + (PI * distance / WINDOW_RADIUS).cos());
            *coefficient = sinc * cosine_window;
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::*;

    const EBU_MINIMUM_DBTP: f64 = -6.4;
    const EBU_MAXIMUM_DBTP: f64 = -5.8;

    fn linear_to_dbtp(value: f32) -> f64 {
        20.0 * f64::from(value).log10()
    }

    fn ebu_tapered_sine(divisor: u32, amplitude: f32, phase_degrees: f64) -> Vec<f32> {
        const FRAMES: u32 = TRUE_PEAK_SAMPLE_RATE_HZ;
        const FADE_FRAMES: u32 = TRUE_PEAK_SAMPLE_RATE_HZ / 100;
        let mut signal = Vec::with_capacity(usize::try_from(FRAMES).unwrap());
        for frame in 0..FRAMES {
            let fade_in = f64::from(frame) / f64::from(FADE_FRAMES);
            let fade_out = f64::from(FRAMES - 1 - frame) / f64::from(FADE_FRAMES);
            let fade = fade_in.min(fade_out).min(1.0);
            let phase =
                2.0 * PI * f64::from(frame) / f64::from(divisor) + phase_degrees.to_radians();
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the conformance tone intentionally enters the engine's f32 sample domain"
            )]
            signal.push((f64::from(amplitude) * fade * phase.sin()) as f32);
        }
        signal
    }

    fn ebu_transient_offset(offset: usize) -> Vec<f32> {
        const HIGH_RATE_HZ: u32 = TRUE_PEAK_SAMPLE_RATE_HZ * TRUE_PEAK_OVERSAMPLE_FACTOR;
        const HIGH_FRAMES: usize = HIGH_RATE_HZ as usize / 10;
        const FADE_FRAMES: u32 = HIGH_RATE_HZ / 100;
        const INSERTION_START: usize = HIGH_FRAMES / 2;
        const INSERTION_FRAMES: usize = 16;
        const FILTER_TAPS: usize = 257;
        const FILTER_MIDDLE: i32 = 128;
        const CUTOFF: f64 = 0.115;

        let mut high_rate = vec![0.0_f64; HIGH_FRAMES];
        for (frame, sample) in high_rate.iter_mut().enumerate() {
            let (phase_frame, divisor, amplitude) = if frame < INSERTION_START {
                (frame, 24_u32, 0.5)
            } else if frame < INSERTION_START + INSERTION_FRAMES {
                (frame - INSERTION_START, 16, 1.0)
            } else {
                (frame - INSERTION_START - INSERTION_FRAMES, 24, 0.5)
            };
            let frame_u32 = u32::try_from(frame).unwrap();
            let phase_frame_u32 = u32::try_from(phase_frame).unwrap();
            let fade_in = f64::from(frame_u32) / f64::from(FADE_FRAMES);
            let fade_out = f64::from(HIGH_RATE_HZ / 10 - 1 - frame_u32) / f64::from(FADE_FRAMES);
            let fade = fade_in.min(fade_out).min(1.0);
            *sample = amplitude
                * fade
                * (2.0 * PI * f64::from(phase_frame_u32) / f64::from(divisor)).sin();
        }

        let mut filter = [0.0_f64; FILTER_TAPS];
        let mut filter_sum = 0.0_f64;
        for (tap, coefficient) in filter.iter_mut().enumerate() {
            let tap_u32 = u32::try_from(tap).unwrap();
            let distance = f64::from(i32::try_from(tap).unwrap() - FILTER_MIDDLE);
            let sinc_argument = 2.0 * CUTOFF * distance;
            let sinc = if sinc_argument == 0.0 {
                1.0
            } else {
                (PI * sinc_argument).sin() / (PI * sinc_argument)
            };
            let denominator = f64::from(u32::try_from(FILTER_TAPS - 1).unwrap());
            let window = 0.42 - 0.5 * (2.0 * PI * f64::from(tap_u32) / denominator).cos()
                + 0.08 * (4.0 * PI * f64::from(tap_u32) / denominator).cos();
            *coefficient = 2.0 * CUTOFF * sinc * window;
            filter_sum += *coefficient;
        }
        for coefficient in &mut filter {
            *coefficient /= filter_sum;
        }

        let mut downsampled = Vec::with_capacity(HIGH_FRAMES / 4);
        for source_frame in (offset..HIGH_FRAMES).step_by(4) {
            let source_frame = i32::try_from(source_frame).unwrap();
            let mut sample = 0.0_f64;
            for (tap, coefficient) in filter.iter().enumerate() {
                let input_frame = source_frame + i32::try_from(tap).unwrap() - FILTER_MIDDLE;
                if let Ok(input_frame) = usize::try_from(input_frame)
                    && let Some(input) = high_rate.get(input_frame)
                {
                    sample += input * coefficient;
                }
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "the independently synthesized conformance signal enters the f32 sample domain"
            )]
            downsampled.push(sample as f32);
        }
        downsampled
    }

    #[test]
    fn rejects_unsupported_rate_shape_and_non_finite_values_atomically() {
        assert_eq!(
            StereoTruePeakMeter::try_new(44_100),
            Err(StereoTruePeakError::UnsupportedSampleRate {
                actual: 44_100,
                supported: TRUE_PEAK_SAMPLE_RATE_HZ,
            })
        );

        let mut meter = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
        let initial = meter.clone();
        assert_eq!(
            meter.process(&[0.0], &[]),
            Err(StereoTruePeakError::ChannelLengthMismatch { left: 1, right: 0 })
        );
        assert_eq!(meter, initial);
        assert_eq!(
            meter.process(&[f32::NAN], &[f32::INFINITY]),
            Err(StereoTruePeakError::NonFiniteSamples { samples: 2 })
        );
        assert_eq!(meter, initial);
    }

    #[test]
    fn processing_is_callback_partition_independent_and_resettable() {
        let signal = ebu_tapered_sine(6, 0.5, 60.0);
        let mut continuous = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
        let mut partitioned = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
        let expected = continuous.process(&signal, &signal).unwrap();
        let mut actual_peak = 0.0_f32;
        for block in signal.chunks(113) {
            let measured = partitioned.process(block, block).unwrap();
            actual_peak = actual_peak.max(measured.left_linear());
        }
        assert_eq!(actual_peak.to_bits(), expected.left_linear().to_bits());

        partitioned.reset();
        let reset = partitioned.process(&signal, &signal).unwrap();
        assert_eq!(reset, expected);
    }

    #[test]
    fn reports_an_inter_sample_peak_above_the_sample_peak() {
        let signal = ebu_tapered_sine(4, 0.5, 45.0);
        let sample_peak = signal.iter().copied().map(f32::abs).fold(0.0, f32::max);
        let mut meter = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
        let measured = meter.process(&signal, &signal).unwrap().left_linear();

        assert!(sample_peak < 0.36);
        assert!(measured > 0.49);
    }

    #[test]
    fn meets_ebu_true_peak_minimum_requirement_tones_15_through_19() {
        let cases = [
            (4, 0.5, 0.0, EBU_MINIMUM_DBTP, EBU_MAXIMUM_DBTP),
            (4, 0.5, 45.0, EBU_MINIMUM_DBTP, EBU_MAXIMUM_DBTP),
            (6, 0.5, 60.0, EBU_MINIMUM_DBTP, EBU_MAXIMUM_DBTP),
            (8, 0.5, 67.5, EBU_MINIMUM_DBTP, EBU_MAXIMUM_DBTP),
            (4, 1.41, 45.0, 2.6, 3.2),
        ];
        for (divisor, amplitude, phase, minimum, maximum) in cases {
            let signal = ebu_tapered_sine(divisor, amplitude, phase);
            let mut meter = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
            let measured = meter.process(&signal, &signal).unwrap();
            let dbtp = linear_to_dbtp(measured.left_linear());
            assert!(
                (minimum..=maximum).contains(&dbtp),
                "fs/{divisor}, phase {phase}: expected {minimum}..={maximum} dBTP, got {dbtp}"
            );
        }
    }

    #[test]
    fn meets_ebu_true_peak_minimum_requirement_transient_offsets_20_through_23() {
        for offset in 0..4 {
            let signal = ebu_transient_offset(offset);
            let mut meter = StereoTruePeakMeter::try_new(TRUE_PEAK_SAMPLE_RATE_HZ).unwrap();
            let measured = meter.process(&signal, &signal).unwrap();
            let dbtp = linear_to_dbtp(measured.left_linear());
            assert!(
                (-0.4..=0.2).contains(&dbtp),
                "4x downsample offset {offset}: expected -0.4..=0.2 dBTP, got {dbtp}"
            );
        }
    }
}
