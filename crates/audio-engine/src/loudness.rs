use core::error::Error;
use core::fmt;

pub const LOUDNESS_SAMPLE_RATE_HZ: u32 = 48_000;

const MOMENTARY_FRAMES: usize = 19_200;
const MOMENTARY_FRAMES_F64: f64 = 19_200.0;
const SHORT_TERM_FRAMES: usize = 144_000;
const SHORT_TERM_FRAMES_F64: f64 = 144_000.0;
const PROGRAMME_STEP_FRAMES: u64 = 4_800;
const LRA_STABLE_FRAMES: u64 = 2_880_000;
const LOUDNESS_OFFSET: f64 = -0.691;
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
const HISTOGRAM_BIN_WIDTH_LU: f64 = 0.1;
const HISTOGRAM_BIN_COUNT: usize = 8_700;
const DENORMAL_LIMIT: f64 = 1.0e-30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoudnessMeasurementState {
    Running,
    Standby,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoudnessMeasurementCommand {
    SetState(LoudnessMeasurementState),
    Reset,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoLoudnessError {
    UnsupportedSampleRate { actual: u32, supported: u32 },
    CapacityUnavailable,
    ChannelLengthMismatch { left: usize, right: usize },
    NonFiniteInput { samples: usize },
    InvalidTruePeak,
}

impl fmt::Display for StereoLoudnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSampleRate { actual, supported } => write!(
                formatter,
                "loudness meter sample rate is {actual} Hz; supported rate is {supported} Hz"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("loudness meter history capacity is unavailable")
            }
            Self::ChannelLengthMismatch { left, right } => write!(
                formatter,
                "loudness input channel lengths differ: left={left}, right={right}"
            ),
            Self::NonFiniteInput { samples } => {
                write!(
                    formatter,
                    "loudness input contains {samples} non-finite samples"
                )
            }
            Self::InvalidTruePeak => {
                formatter.write_str("loudness maximum true peak must be finite and non-negative")
            }
        }
    }
}

impl Error for StereoLoudnessError {}

/// One live stereo loudness reading in absolute LUFS/LKFS units.
///
/// Momentary and short-term values use ungated 400 ms and 3 s rectangular
/// windows. Integrated loudness and loudness range cover only programme frames
/// accepted while [`LoudnessMeasurementState::Running`]. `lra_stable` becomes
/// true after 60 s of accepted programme audio; it does not certify the value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoLoudnessSnapshot {
    state: LoudnessMeasurementState,
    momentary_lufs: Option<f32>,
    short_term_lufs: Option<f32>,
    integrated_lufs: Option<f32>,
    loudness_range_lu: Option<f32>,
    maximum_momentary_lufs: Option<f32>,
    maximum_short_term_lufs: Option<f32>,
    maximum_true_peak_linear: Option<f32>,
    programme_frame_count: u64,
    lra_stable: bool,
}

impl StereoLoudnessSnapshot {
    #[must_use]
    pub const fn state(self) -> LoudnessMeasurementState {
        self.state
    }

    #[must_use]
    pub const fn momentary_lufs(self) -> Option<f32> {
        self.momentary_lufs
    }

    #[must_use]
    pub const fn short_term_lufs(self) -> Option<f32> {
        self.short_term_lufs
    }

    #[must_use]
    pub const fn integrated_lufs(self) -> Option<f32> {
        self.integrated_lufs
    }

    #[must_use]
    pub const fn loudness_range_lu(self) -> Option<f32> {
        self.loudness_range_lu
    }

    #[must_use]
    pub const fn maximum_momentary_lufs(self) -> Option<f32> {
        self.maximum_momentary_lufs
    }

    #[must_use]
    pub const fn maximum_short_term_lufs(self) -> Option<f32> {
        self.maximum_short_term_lufs
    }

    #[must_use]
    pub const fn maximum_true_peak_linear(self) -> Option<f32> {
        self.maximum_true_peak_linear
    }

    #[must_use]
    pub const fn programme_frame_count(self) -> u64 {
        self.programme_frame_count
    }

    #[must_use]
    pub const fn lra_stable(self) -> bool {
        self.lra_stable
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct BiquadState {
    z1: f64,
    z2: f64,
}

impl BiquadState {
    fn process(&mut self, sample: f64, coefficients: BiquadCoefficients) -> f64 {
        let output = coefficients.b0.mul_add(sample, self.z1);
        self.z1 = coefficients
            .b1
            .mul_add(sample, (-coefficients.a1).mul_add(output, self.z2));
        self.z2 = coefficients.b2.mul_add(sample, -coefficients.a2 * output);
        if self.z1.abs() < DENORMAL_LIMIT {
            self.z1 = 0.0;
        }
        if self.z2.abs() < DENORMAL_LIMIT {
            self.z2 = 0.0;
        }
        output
    }
}

#[derive(Clone, Copy, Debug)]
struct BiquadCoefficients {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

const PRE_FILTER: BiquadCoefficients = BiquadCoefficients {
    b0: 1.535_124_859_586_97,
    b1: -2.691_696_189_406_38,
    b2: 1.198_392_810_852_85,
    a1: -1.690_659_293_182_41,
    a2: 0.732_480_774_215_85,
};

const RLB_FILTER: BiquadCoefficients = BiquadCoefficients {
    b0: 1.0,
    b1: -2.0,
    b2: 1.0,
    a1: -1.990_047_454_833_98,
    a2: 0.990_072_250_366_21,
};

#[derive(Clone, Copy, Debug, Default)]
struct KWeightingFilter {
    pre: BiquadState,
    rlb: BiquadState,
}

impl KWeightingFilter {
    fn process(&mut self, sample: f32) -> f64 {
        let prefiltered = self.pre.process(f64::from(sample), PRE_FILTER);
        self.rlb.process(prefiltered, RLB_FILTER)
    }
}

#[derive(Debug)]
struct EnergyWindow {
    samples: Vec<f64>,
    write_index: usize,
    length: usize,
    momentary_sum: f64,
    short_term_sum: f64,
}

impl EnergyWindow {
    fn try_new() -> Result<Self, StereoLoudnessError> {
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(SHORT_TERM_FRAMES)
            .map_err(|_| StereoLoudnessError::CapacityUnavailable)?;
        samples.resize(SHORT_TERM_FRAMES, 0.0);
        Ok(Self {
            samples,
            write_index: 0,
            length: 0,
            momentary_sum: 0.0,
            short_term_sum: 0.0,
        })
    }

    fn push(&mut self, energy: f64) -> WindowEnergies {
        if self.length >= MOMENTARY_FRAMES {
            let outgoing =
                (self.write_index + SHORT_TERM_FRAMES - MOMENTARY_FRAMES) % SHORT_TERM_FRAMES;
            self.momentary_sum -= self.samples[outgoing];
            self.momentary_sum = self.momentary_sum.max(0.0);
        }
        if self.length == SHORT_TERM_FRAMES {
            self.short_term_sum -= self.samples[self.write_index];
            self.short_term_sum = self.short_term_sum.max(0.0);
        }
        self.samples[self.write_index] = energy;
        self.write_index = (self.write_index + 1) % SHORT_TERM_FRAMES;
        self.length = self.length.saturating_add(1).min(SHORT_TERM_FRAMES);
        self.momentary_sum += energy;
        self.short_term_sum += energy;
        WindowEnergies {
            momentary: (self.length >= MOMENTARY_FRAMES)
                .then_some(self.momentary_sum / MOMENTARY_FRAMES_F64),
            short_term: (self.length == SHORT_TERM_FRAMES)
                .then_some(self.short_term_sum / SHORT_TERM_FRAMES_F64),
        }
    }

    fn current(&self) -> WindowEnergies {
        WindowEnergies {
            momentary: (self.length >= MOMENTARY_FRAMES)
                .then_some(self.momentary_sum / MOMENTARY_FRAMES_F64),
            short_term: (self.length == SHORT_TERM_FRAMES)
                .then_some(self.short_term_sum / SHORT_TERM_FRAMES_F64),
        }
    }

    fn clear(&mut self) {
        self.write_index = 0;
        self.length = 0;
        self.momentary_sum = 0.0;
        self.short_term_sum = 0.0;
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WindowEnergies {
    momentary: Option<f64>,
    short_term: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct HistogramBin {
    count: u64,
    energy_sum: f64,
}

#[derive(Debug)]
struct EnergyHistogram {
    bins: Vec<HistogramBin>,
}

impl EnergyHistogram {
    fn try_new() -> Result<Self, StereoLoudnessError> {
        let mut bins = Vec::new();
        bins.try_reserve_exact(HISTOGRAM_BIN_COUNT)
            .map_err(|_| StereoLoudnessError::CapacityUnavailable)?;
        bins.resize(HISTOGRAM_BIN_COUNT, HistogramBin::default());
        Ok(Self { bins })
    }

    fn observe(&mut self, energy: f64) {
        let loudness = energy_to_loudness(energy);
        if loudness < ABSOLUTE_GATE_LUFS {
            return;
        }
        let scaled = ((loudness - ABSOLUTE_GATE_LUFS) / HISTOGRAM_BIN_WIDTH_LU).floor();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "finite loudness is non-negative after subtracting the absolute gate and is clamped to the fixed histogram"
        )]
        let index = (scaled as usize).min(HISTOGRAM_BIN_COUNT - 1);
        let bin = &mut self.bins[index];
        if bin.count != u64::MAX {
            bin.count += 1;
            bin.energy_sum += energy;
        }
    }

    fn clear(&mut self) {
        self.bins.fill(HistogramBin::default());
    }

    fn gated_loudness(&self, relative_gate_lu: f64) -> Option<f64> {
        let (absolute_count, absolute_energy) = self.sum_from(0);
        if absolute_count == 0 {
            return None;
        }
        let relative_threshold =
            absolute_energy / count_to_f64(absolute_count) * 10.0_f64.powf(relative_gate_lu / 10.0);
        let start = histogram_index_for_energy(relative_threshold);
        let (gated_count, gated_energy) = self.sum_from(start);
        (gated_count != 0).then(|| energy_to_loudness(gated_energy / count_to_f64(gated_count)))
    }

    fn loudness_range(&self) -> Option<f64> {
        let (absolute_count, absolute_energy) = self.sum_from(0);
        if absolute_count == 0 {
            return None;
        }
        let relative_threshold = absolute_energy / count_to_f64(absolute_count) * 0.01;
        let start = histogram_index_for_energy(relative_threshold);
        let eligible_count = self.count_from(start);
        if eligible_count == 0 {
            return None;
        }
        let low_rank = percentile_rank(eligible_count, 10);
        let high_rank = percentile_rank(eligible_count, 95);
        let low = self.loudness_at_rank(start, low_rank)?;
        let high = self.loudness_at_rank(start, high_rank)?;
        Some(high - low)
    }

    fn sum_from(&self, start: usize) -> (u64, f64) {
        self.bins[start..]
            .iter()
            .fold((0_u64, 0.0_f64), |(count, energy), bin| {
                (count.saturating_add(bin.count), energy + bin.energy_sum)
            })
    }

    fn count_from(&self, start: usize) -> u64 {
        self.bins[start..]
            .iter()
            .fold(0_u64, |count, bin| count.saturating_add(bin.count))
    }

    fn loudness_at_rank(&self, start: usize, rank: u64) -> Option<f64> {
        let mut cumulative = 0_u64;
        for (index, bin) in self.bins.iter().enumerate().skip(start) {
            cumulative = cumulative.saturating_add(bin.count);
            if cumulative > rank {
                return Some(
                    ABSOLUTE_GATE_LUFS + (index_to_f64(index) + 0.5) * HISTOGRAM_BIN_WIDTH_LU,
                );
            }
        }
        None
    }
}

fn histogram_index_for_energy(energy: f64) -> usize {
    let loudness = energy_to_loudness(energy);
    if loudness <= ABSOLUTE_GATE_LUFS {
        return 0;
    }
    let scaled = ((loudness - ABSOLUTE_GATE_LUFS) / HISTOGRAM_BIN_WIDTH_LU).floor();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the positive finite histogram position is clamped to fixed storage"
    )]
    let mut index = (scaled as usize).min(HISTOGRAM_BIN_COUNT - 1);
    let midpoint = ABSOLUTE_GATE_LUFS + (index_to_f64(index) + 0.5) * HISTOGRAM_BIN_WIDTH_LU;
    if loudness > midpoint && index + 1 < HISTOGRAM_BIN_COUNT {
        index += 1;
    }
    index
}

fn percentile_rank(count: u64, percentile: u64) -> u64 {
    let last = count.saturating_sub(1);
    (last.saturating_mul(percentile).saturating_add(50)) / 100
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a 10 Hz histogram cannot reach the f64 integer precision limit during a realizable live session"
)]
fn count_to_f64(count: u64) -> f64 {
    count as f64
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the fixed histogram index is below 8,700 and exactly representable in f64"
)]
fn index_to_f64(index: usize) -> f64 {
    index as f64
}

fn energy_to_loudness(energy: f64) -> f64 {
    LOUDNESS_OFFSET + 10.0 * energy.log10()
}

fn loudness_value(energy: Option<f64>) -> Option<f32> {
    energy.filter(|value| *value > 0.0).map(|value| {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "finite loudness is intentionally exposed as an f32 meter value"
        )]
        let loudness = energy_to_loudness(value) as f32;
        loudness
    })
}

/// Allocation-free streaming stereo loudness meter for the version-1 48 kHz
/// project domain.
///
/// Construction allocates two fixed 3 s energy histories and two fixed
/// histograms. Processing performs no allocation, locking, I/O, or logging.
#[derive(Debug)]
pub struct StereoLoudnessMeter {
    filters: [KWeightingFilter; 2],
    live_window: EnergyWindow,
    programme_window: EnergyWindow,
    integrated_histogram: EnergyHistogram,
    lra_histogram: EnergyHistogram,
    state: LoudnessMeasurementState,
    programme_frame_count: u64,
    maximum_momentary_energy: Option<f64>,
    maximum_short_term_energy: Option<f64>,
    maximum_true_peak_linear: Option<f32>,
    integrated_lufs: Option<f32>,
    loudness_range_lu: Option<f32>,
}

impl StereoLoudnessMeter {
    /// Allocates all fixed measurement storage before callback processing.
    ///
    /// # Errors
    ///
    /// Returns [`StereoLoudnessError::UnsupportedSampleRate`] outside 48 kHz,
    /// or [`StereoLoudnessError::CapacityUnavailable`] if fixed storage cannot
    /// be reserved.
    pub fn try_new(sample_rate_hz: u32) -> Result<Self, StereoLoudnessError> {
        if sample_rate_hz != LOUDNESS_SAMPLE_RATE_HZ {
            return Err(StereoLoudnessError::UnsupportedSampleRate {
                actual: sample_rate_hz,
                supported: LOUDNESS_SAMPLE_RATE_HZ,
            });
        }
        Ok(Self {
            filters: [KWeightingFilter::default(); 2],
            live_window: EnergyWindow::try_new()?,
            programme_window: EnergyWindow::try_new()?,
            integrated_histogram: EnergyHistogram::try_new()?,
            lra_histogram: EnergyHistogram::try_new()?,
            state: LoudnessMeasurementState::Running,
            programme_frame_count: 0,
            maximum_momentary_energy: None,
            maximum_short_term_energy: None,
            maximum_true_peak_linear: None,
            integrated_lufs: None,
            loudness_range_lu: None,
        })
    }

    #[must_use]
    pub const fn state(&self) -> LoudnessMeasurementState {
        self.state
    }

    /// Applies one programme-measurement control at a callback boundary.
    ///
    /// Run/standby affects Integrated Loudness and LRA only. Momentary and
    /// short-term windows keep following the live signal. Reset clears I/LRA,
    /// maximum M/S, and maximum true peak together while preserving live
    /// filter/window history and the current run/standby state.
    pub fn apply_command(&mut self, command: LoudnessMeasurementCommand) {
        match command {
            LoudnessMeasurementCommand::SetState(state) => self.state = state,
            LoudnessMeasurementCommand::Reset => self.reset_programme_measurement(),
        }
    }

    /// Processes one finite planar stereo block and returns the latest reading.
    ///
    /// `block_true_peak_linear` is the largest linear true-peak value reported
    /// for the same final-output block.
    ///
    /// # Errors
    ///
    /// Shape, sample, and true-peak validation completes before any meter state
    /// changes.
    pub fn process(
        &mut self,
        left: &[f32],
        right: &[f32],
        block_true_peak_linear: f32,
    ) -> Result<StereoLoudnessSnapshot, StereoLoudnessError> {
        if left.len() != right.len() {
            return Err(StereoLoudnessError::ChannelLengthMismatch {
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
            return Err(StereoLoudnessError::NonFiniteInput {
                samples: non_finite_samples,
            });
        }
        if !block_true_peak_linear.is_finite() || block_true_peak_linear < 0.0 {
            return Err(StereoLoudnessError::InvalidTruePeak);
        }

        if !left.is_empty() {
            self.maximum_true_peak_linear = Some(
                self.maximum_true_peak_linear
                    .unwrap_or(0.0)
                    .max(block_true_peak_linear),
            );
        }
        for (&left, &right) in left.iter().zip(right) {
            let weighted_left = self.filters[0].process(left);
            let weighted_right = self.filters[1].process(right);
            let energy = weighted_left.mul_add(weighted_left, weighted_right * weighted_right);
            let live = self.live_window.push(energy);
            if let Some(momentary) = live.momentary {
                self.maximum_momentary_energy =
                    Some(self.maximum_momentary_energy.unwrap_or(0.0).max(momentary));
            }
            if let Some(short_term) = live.short_term {
                self.maximum_short_term_energy = Some(
                    self.maximum_short_term_energy
                        .unwrap_or(0.0)
                        .max(short_term),
                );
            }
            if self.state == LoudnessMeasurementState::Running {
                let programme = self.programme_window.push(energy);
                self.programme_frame_count = self.programme_frame_count.saturating_add(1);
                if self.programme_frame_count >= MOMENTARY_FRAMES as u64
                    && (self.programme_frame_count - MOMENTARY_FRAMES as u64)
                        .is_multiple_of(PROGRAMME_STEP_FRAMES)
                    && let Some(momentary) = programme.momentary
                {
                    self.integrated_histogram.observe(momentary);
                    self.integrated_lufs = self
                        .integrated_histogram
                        .gated_loudness(-10.0)
                        .map(loudness_to_f32);
                }
                if self.programme_frame_count >= SHORT_TERM_FRAMES as u64
                    && (self.programme_frame_count - SHORT_TERM_FRAMES as u64)
                        .is_multiple_of(PROGRAMME_STEP_FRAMES)
                    && let Some(short_term) = programme.short_term
                {
                    self.lra_histogram.observe(short_term);
                    self.loudness_range_lu =
                        self.lra_histogram.loudness_range().map(loudness_to_f32);
                }
            }
        }
        Ok(self.snapshot())
    }

    #[must_use]
    pub fn snapshot(&self) -> StereoLoudnessSnapshot {
        let current = self.live_window.current();
        StereoLoudnessSnapshot {
            state: self.state,
            momentary_lufs: loudness_value(current.momentary),
            short_term_lufs: loudness_value(current.short_term),
            integrated_lufs: self.integrated_lufs,
            loudness_range_lu: self.loudness_range_lu,
            maximum_momentary_lufs: loudness_value(self.maximum_momentary_energy),
            maximum_short_term_lufs: loudness_value(self.maximum_short_term_energy),
            maximum_true_peak_linear: self.maximum_true_peak_linear,
            programme_frame_count: self.programme_frame_count,
            lra_stable: self.programme_frame_count >= LRA_STABLE_FRAMES,
        }
    }

    fn reset_programme_measurement(&mut self) {
        let current = self.live_window.current();
        self.programme_window.clear();
        self.integrated_histogram.clear();
        self.lra_histogram.clear();
        self.programme_frame_count = 0;
        self.maximum_momentary_energy = current.momentary;
        self.maximum_short_term_energy = current.short_term;
        self.maximum_true_peak_linear = None;
        self.integrated_lufs = None;
        self.loudness_range_lu = None;
    }
}

fn loudness_to_f32(value: f64) -> f32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "finite loudness is intentionally exposed as an f32 meter value"
    )]
    let value = value as f32;
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK_FRAMES: usize = 480;

    fn assert_close(actual: Option<f32>, expected: f32, tolerance: f32) {
        let actual = actual.expect("reading must be available");
        assert!(
            (actual - expected).abs() <= tolerance,
            "expected {expected} +/- {tolerance}, got {actual}"
        );
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test durations and amplitudes are small finite published fixture values"
    )]
    fn feed_tone(
        meter: &mut StereoLoudnessMeter,
        phase: &mut f64,
        seconds: f64,
        peak_dbfs: f64,
    ) -> StereoLoudnessSnapshot {
        let frames = (seconds * f64::from(LOUDNESS_SAMPLE_RATE_HZ)).round() as usize;
        let amplitude = 10.0_f64.powf(peak_dbfs / 20.0);
        let increment = 2.0 * core::f64::consts::PI * 1_000.0 / f64::from(LOUDNESS_SAMPLE_RATE_HZ);
        let mut left = [0.0_f32; CHUNK_FRAMES];
        let mut right = [0.0_f32; CHUNK_FRAMES];
        let mut remaining = frames;
        let mut snapshot = meter.snapshot();
        while remaining != 0 {
            let count = remaining.min(CHUNK_FRAMES);
            for sample in &mut left[..count] {
                *sample = (*phase).sin().mul_add(amplitude, 0.0) as f32;
                *phase += increment;
                if *phase >= 2.0 * core::f64::consts::PI {
                    *phase -= 2.0 * core::f64::consts::PI;
                }
            }
            right[..count].copy_from_slice(&left[..count]);
            snapshot = meter
                .process(&left[..count], &right[..count], amplitude as f32)
                .unwrap();
            remaining -= count;
        }
        snapshot
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "test durations are small finite published fixture values"
    )]
    fn feed_silence(meter: &mut StereoLoudnessMeter, seconds: f64) -> StereoLoudnessSnapshot {
        let frames = (seconds * f64::from(LOUDNESS_SAMPLE_RATE_HZ)).round() as usize;
        let silence = [0.0_f32; CHUNK_FRAMES];
        let mut remaining = frames;
        let mut snapshot = meter.snapshot();
        while remaining != 0 {
            let count = remaining.min(CHUNK_FRAMES);
            snapshot = meter
                .process(&silence[..count], &silence[..count], 0.0)
                .unwrap();
            remaining -= count;
        }
        snapshot
    }

    #[test]
    fn rejects_unsupported_rate_and_invalid_inputs_without_mutation() {
        assert_eq!(
            StereoLoudnessMeter::try_new(44_100).unwrap_err(),
            StereoLoudnessError::UnsupportedSampleRate {
                actual: 44_100,
                supported: 48_000
            }
        );
        let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        let before = meter.snapshot();
        assert_eq!(
            meter.process(&[0.0], &[], 0.0).unwrap_err(),
            StereoLoudnessError::ChannelLengthMismatch { left: 1, right: 0 }
        );
        assert_eq!(meter.snapshot(), before);
        assert_eq!(
            meter.process(&[f32::NAN], &[0.0], 0.0).unwrap_err(),
            StereoLoudnessError::NonFiniteInput { samples: 1 }
        );
        assert_eq!(meter.snapshot(), before);
        assert_eq!(
            meter.process(&[0.0], &[0.0], f32::INFINITY).unwrap_err(),
            StereoLoudnessError::InvalidTruePeak
        );
        assert_eq!(meter.snapshot(), before);
    }

    #[test]
    fn processing_is_callback_partition_independent() {
        let frame_count = SHORT_TERM_FRAMES + MOMENTARY_FRAMES + 731;
        let increment = 2.0 * core::f64::consts::PI * 997.0 / f64::from(LOUDNESS_SAMPLE_RATE_HZ);
        let mut phase = 0.0_f64;
        let left: Vec<f32> = (0..frame_count)
            .map(|frame| {
                let amplitude = if (frame / 7_001).is_multiple_of(2) {
                    0.2
                } else {
                    0.05
                };
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "bounded test sine values intentionally exercise the f32 input contract"
                )]
                let sample = (phase.sin() * amplitude) as f32;
                phase += increment;
                if phase >= 2.0 * core::f64::consts::PI {
                    phase -= 2.0 * core::f64::consts::PI;
                }
                sample
            })
            .collect();
        let right: Vec<f32> = left.iter().map(|sample| -*sample * 0.75).collect();

        let mut contiguous = StereoLoudnessMeter::try_new(48_000).unwrap();
        let expected = contiguous.process(&left, &right, 0.2).unwrap();
        let mut partitioned = StereoLoudnessMeter::try_new(48_000).unwrap();
        let mut offset = 0;
        for block_frames in [1, 127, 480, 2_047, 63, 8_192].into_iter().cycle() {
            if offset == frame_count {
                break;
            }
            let end = (offset + block_frames).min(frame_count);
            partitioned
                .process(&left[offset..end], &right[offset..end], 0.2)
                .unwrap();
            offset = end;
        }
        assert_eq!(partitioned.snapshot(), expected);
    }

    #[test]
    fn ebu_alignment_tones_match_momentary_short_term_and_integrated() {
        for level in [-23.0, -33.0] {
            let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
            let mut phase = 0.0;
            let snapshot = feed_tone(&mut meter, &mut phase, 20.0, level);
            let expected = loudness_to_f32(level);
            assert_close(snapshot.momentary_lufs(), expected, 0.1);
            assert_close(snapshot.short_term_lufs(), expected, 0.1);
            assert_close(snapshot.integrated_lufs(), expected, 0.1);
            assert_close(snapshot.maximum_momentary_lufs(), expected, 0.1);
            assert_close(snapshot.maximum_short_term_lufs(), expected, 0.1);
        }
    }

    #[test]
    fn ebu_integrated_gating_scenarios_match_published_values() {
        let scenarios: &[&[(f64, f64)]] = &[
            &[(10.0, -36.0), (60.0, -23.0), (10.0, -36.0)],
            &[
                (10.0, -72.0),
                (10.0, -36.0),
                (60.0, -23.0),
                (10.0, -36.0),
                (10.0, -72.0),
            ],
            &[(20.0, -26.0), (20.1, -20.0), (20.0, -26.0)],
        ];
        for segments in scenarios {
            let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
            let mut phase = 0.0;
            let mut snapshot = meter.snapshot();
            for &(seconds, level) in *segments {
                snapshot = feed_tone(&mut meter, &mut phase, seconds, level);
            }
            assert_close(snapshot.integrated_lufs(), -23.0, 0.1);
        }
    }

    #[test]
    fn ebu_alternating_tones_match_short_term_and_momentary_windows() {
        let mut short_term_meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        let mut phase = 0.0;
        let mut short_term = short_term_meter.snapshot();
        for _ in 0..5 {
            feed_tone(&mut short_term_meter, &mut phase, 1.34, -20.0);
            short_term = feed_tone(&mut short_term_meter, &mut phase, 1.66, -30.0);
        }
        assert_close(short_term.short_term_lufs(), -23.0, 0.1);

        let mut momentary_meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        phase = 0.0;
        let mut momentary = momentary_meter.snapshot();
        for _ in 0..25 {
            feed_tone(&mut momentary_meter, &mut phase, 0.18, -20.0);
            momentary = feed_tone(&mut momentary_meter, &mut phase, 0.22, -30.0);
        }
        assert_close(momentary.momentary_lufs(), -23.0, 0.1);
    }

    #[test]
    fn ebu_offset_tones_reach_published_maximum_short_term_values() {
        for index in 0..20 {
            let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
            let mut phase = 0.0;
            feed_silence(&mut meter, f64::from(index) * 0.15);
            feed_tone(&mut meter, &mut phase, 3.0, -23.0);
            let snapshot = feed_silence(&mut meter, 1.0);
            assert_close(snapshot.maximum_short_term_lufs(), -23.0, 0.1);
        }

        let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        let mut phase = 0.0;
        for index in 0..20 {
            meter.apply_command(LoudnessMeasurementCommand::Reset);
            feed_silence(&mut meter, f64::from(index) * 0.15);
            let level = -38.0 + f64::from(index);
            feed_tone(&mut meter, &mut phase, 3.0, level);
            let snapshot = feed_silence(&mut meter, 3.0 - f64::from(index) * 0.15);
            assert_close(
                snapshot.maximum_short_term_lufs(),
                loudness_to_f32(level),
                0.1,
            );
        }
    }

    #[test]
    fn ebu_offset_tones_reach_published_maximum_momentary_values() {
        for index in 0..20 {
            let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
            let mut phase = 0.0;
            feed_silence(&mut meter, f64::from(index) * 0.02);
            feed_tone(&mut meter, &mut phase, 0.4, -23.0);
            let snapshot = feed_silence(&mut meter, 1.0);
            assert_close(snapshot.maximum_momentary_lufs(), -23.0, 0.1);
        }

        let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        let mut phase = 0.0;
        for index in 0..20 {
            meter.apply_command(LoudnessMeasurementCommand::Reset);
            feed_silence(&mut meter, f64::from(index) * 0.02);
            let level = -38.0 + f64::from(index);
            feed_tone(&mut meter, &mut phase, 0.4, level);
            let snapshot = feed_silence(&mut meter, 0.4 - f64::from(index) * 0.02);
            assert_close(
                snapshot.maximum_momentary_lufs(),
                loudness_to_f32(level),
                0.1,
            );
        }
    }

    #[test]
    fn run_standby_and_reset_control_only_programme_measurement() {
        let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
        let mut phase = 0.0;
        let running = feed_tone(&mut meter, &mut phase, 3.0, -23.0);
        assert_eq!(running.programme_frame_count(), 144_000);
        meter.apply_command(LoudnessMeasurementCommand::SetState(
            LoudnessMeasurementState::Standby,
        ));
        let standby = feed_tone(&mut meter, &mut phase, 3.0, -33.0);
        assert_eq!(standby.programme_frame_count(), 144_000);
        assert_close(standby.short_term_lufs(), -33.0, 0.1);
        assert_close(standby.integrated_lufs(), -23.0, 0.1);
        meter.apply_command(LoudnessMeasurementCommand::Reset);
        let reset = meter.snapshot();
        assert_eq!(reset.state(), LoudnessMeasurementState::Standby);
        assert_eq!(reset.programme_frame_count(), 0);
        assert_eq!(reset.integrated_lufs(), None);
        assert_eq!(reset.loudness_range_lu(), None);
        assert_eq!(reset.maximum_true_peak_linear(), None);
        assert_close(reset.maximum_momentary_lufs(), -33.0, 0.1);
        assert_close(reset.maximum_short_term_lufs(), -33.0, 0.1);
    }

    #[test]
    fn ebu_loudness_range_tone_scenarios_match_published_values() {
        let scenarios: &[(&[(f64, f64)], f32)] = &[
            (&[(20.0, -20.0), (20.0, -30.0)], 10.0),
            (&[(20.0, -20.0), (20.0, -15.0)], 5.0),
            (&[(20.0, -40.0), (20.0, -20.0)], 20.0),
            (
                &[
                    (20.0, -50.0),
                    (20.0, -35.0),
                    (20.0, -20.0),
                    (20.0, -35.0),
                    (20.0, -50.0),
                ],
                15.0,
            ),
        ];
        for &(segments, expected) in scenarios {
            let mut meter = StereoLoudnessMeter::try_new(48_000).unwrap();
            let mut phase = 0.0;
            let mut snapshot = meter.snapshot();
            for &(seconds, level) in segments {
                snapshot = feed_tone(&mut meter, &mut phase, seconds, level);
            }
            assert_close(snapshot.loudness_range_lu(), expected, 1.0);
            if segments.len() == 5 {
                assert!(snapshot.lra_stable());
            }
        }
    }
}
