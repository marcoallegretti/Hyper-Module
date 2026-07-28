#![doc = "Real-time-safe audio processing primitives for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

const MAX_LINEAR_GAIN: f32 = 2.0;
const PAN_SCALE: f32 = 1_000_000.0;
const GAIN_SCALE: f32 = 1_000_000.0;
const MONO_PAN_SCALE: f32 = -0.831_783_1;
pub const MAX_PROCESS_FRAMES: usize = 65_536;

/// A finite linear amplitude gain in the supported mixer range `0.0..=2.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinearGain(f32);

impl LinearGain {
    pub const SILENCE: Self = Self(0.0);
    pub const UNITY: Self = Self(1.0);

    /// Creates a linear gain from an amplitude multiplier.
    ///
    /// # Errors
    ///
    /// Returns [`ParameterError::LinearGainOutOfRange`] for a non-finite value
    /// or a value outside `0.0..=2.0`.
    pub fn new(value: f32) -> Result<Self, ParameterError> {
        if value.is_finite() && (0.0..=MAX_LINEAR_GAIN).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ParameterError::LinearGainOutOfRange)
        }
    }

    /// Converts the persisted integer-millionths representation.
    ///
    /// # Errors
    ///
    /// Returns [`ParameterError::LinearGainOutOfRange`] above `2_000_000`.
    #[allow(clippy::cast_precision_loss)]
    pub fn from_millionths(value: u32) -> Result<Self, ParameterError> {
        if value <= 2_000_000 {
            Ok(Self(value as f32 / GAIN_SCALE))
        } else {
            Err(ParameterError::LinearGainOutOfRange)
        }
    }

    #[must_use]
    pub const fn value(self) -> f32 {
        self.0
    }
}

/// A normalized pan or balance position in the range `-1.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pan(f32);

impl Pan {
    pub const HARD_LEFT: Self = Self(-1.0);
    pub const CENTER: Self = Self(0.0);
    pub const HARD_RIGHT: Self = Self(1.0);

    /// Creates a normalized pan or balance position.
    ///
    /// # Errors
    ///
    /// Returns [`ParameterError::PanOutOfRange`] for a non-finite value or a
    /// value outside `-1.0..=1.0`.
    pub fn new(value: f32) -> Result<Self, ParameterError> {
        if value.is_finite() && (-1.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(ParameterError::PanOutOfRange)
        }
    }

    /// Converts the persisted integer-millionths representation.
    ///
    /// # Errors
    ///
    /// Returns [`ParameterError::PanOutOfRange`] outside
    /// `-1_000_000..=1_000_000`.
    #[allow(clippy::cast_precision_loss)]
    pub fn from_millionths(value: i32) -> Result<Self, ParameterError> {
        if (-1_000_000..=1_000_000).contains(&value) {
            Ok(Self(value as f32 / PAN_SCALE))
        } else {
            Err(ParameterError::PanOutOfRange)
        }
    }

    #[must_use]
    pub const fn value(self) -> f32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterError {
    LinearGainOutOfRange,
    PanOutOfRange,
}

impl fmt::Display for ParameterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LinearGainOutOfRange => {
                formatter.write_str("linear gain must be finite and in 0.0..=2.0")
            }
            Self::PanOutOfRange => formatter.write_str("pan must be finite and in -1.0..=1.0"),
        }
    }
}

impl Error for ParameterError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerCreationError {
    CapacityUnavailable,
}

impl fmt::Display for MixerCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("mixer channel-state capacity cannot be reserved")
            }
        }
    }
}

impl Error for MixerCreationError {}

/// Parameters for one fixed mixer input slot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelParameters {
    pub gain: LinearGain,
    pub pan: Pan,
    pub audible: bool,
}

impl ChannelParameters {
    #[must_use]
    pub const fn new(gain: LinearGain, pan: Pan, audible: bool) -> Self {
        Self { gain, pan, audible }
    }

    const fn effective_gain(self) -> f32 {
        if self.audible { self.gain.value() } else { 0.0 }
    }
}

/// Borrowed planar input audio for one processing block.
#[derive(Clone, Copy, Debug)]
pub enum InputBlock<'a> {
    Mono(&'a [f32]),
    Stereo { left: &'a [f32], right: &'a [f32] },
}

/// One input plus exact per-sample mixer controls for a processing block.
#[derive(Clone, Copy, Debug)]
pub struct ControlledInputBlock<'a> {
    pub input: InputBlock<'a>,
    pub parameters: &'a [ChannelParameters],
}

impl InputBlock<'_> {
    fn frame_count(self) -> Result<usize, ProcessError> {
        match self {
            Self::Mono(samples) => Ok(samples.len()),
            Self::Stereo { left, right } if left.len() == right.len() => Ok(left.len()),
            Self::Stereo { left, right } => Err(ProcessError::StereoInputLengthMismatch {
                left: left.len(),
                right: right.len(),
            }),
        }
    }
}

/// Borrowed planar stereo output audio for one processing block.
#[derive(Debug)]
pub struct StereoOutput<'a> {
    pub left: &'a mut [f32],
    pub right: &'a mut [f32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessError {
    OutputLengthMismatch {
        left: usize,
        right: usize,
    },
    InputCountMismatch {
        configured: usize,
        inputs: usize,
        parameters: usize,
    },
    StereoInputLengthMismatch {
        left: usize,
        right: usize,
    },
    InputFrameCountMismatch {
        input: usize,
        expected: usize,
        actual: usize,
    },
    ControlFrameCountMismatch {
        input: usize,
        expected: usize,
        actual: usize,
    },
    MasterControlFrameCountMismatch {
        expected: usize,
        actual: usize,
    },
    BlockTooLarge {
        actual: usize,
        maximum: usize,
    },
}

impl fmt::Display for ProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLengthMismatch { left, right } => write!(
                formatter,
                "output channel lengths differ: left={left}, right={right}"
            ),
            Self::InputCountMismatch {
                configured,
                inputs,
                parameters,
            } => write!(
                formatter,
                "mixer slot count differs: configured={configured}, inputs={inputs}, parameters={parameters}"
            ),
            Self::StereoInputLengthMismatch { left, right } => write!(
                formatter,
                "stereo input channel lengths differ: left={left}, right={right}"
            ),
            Self::InputFrameCountMismatch {
                input,
                expected,
                actual,
            } => write!(
                formatter,
                "input {input} has {actual} frames; expected {expected}"
            ),
            Self::ControlFrameCountMismatch {
                input,
                expected,
                actual,
            } => write!(
                formatter,
                "input {input} has {actual} control frames; expected {expected}"
            ),
            Self::MasterControlFrameCountMismatch { expected, actual } => write!(
                formatter,
                "master has {actual} control frames; expected {expected}"
            ),
            Self::BlockTooLarge { actual, maximum } => {
                write!(formatter, "block has {actual} frames; maximum is {maximum}")
            }
        }
    }
}

impl Error for ProcessError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallbackAdapterCreationError {
    InvalidMaximumFrames { actual: usize, maximum: usize },
    CapacityUnavailable,
}

impl fmt::Display for CallbackAdapterCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaximumFrames { actual, maximum } => write!(
                formatter,
                "callback maximum has {actual} frames; valid range is 1..={maximum}"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("callback planar buffer capacity is unavailable")
            }
        }
    }
}

impl Error for CallbackAdapterCreationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoCallbackError<E> {
    SampleCountNotStereoAligned { samples: usize },
    BlockTooLarge { actual: usize, maximum: usize },
    Processor(E),
}

impl<E: fmt::Display> fmt::Display for StereoCallbackError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleCountNotStereoAligned { samples } => write!(
                formatter,
                "interleaved callback has {samples} samples; stereo requires an even sample count"
            ),
            Self::BlockTooLarge { actual, maximum } => write!(
                formatter,
                "callback block has {actual} frames; configured maximum is {maximum}"
            ),
            Self::Processor(source) => write!(formatter, "stereo processing failed: {source}"),
        }
    }
}

impl<E: Error + 'static> Error for StereoCallbackError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Processor(source) => Some(source),
            Self::SampleCountNotStereoAligned { .. } | Self::BlockTooLarge { .. } => None,
        }
    }
}

/// Per-channel measurements for one successful device callback.
///
/// `sample_peak` is the largest absolute sample value; it is not an
/// oversampled true-peak measurement. `block_rms` covers only this callback
/// block and has no loudness weighting or meter ballistics. Both readings are
/// unavailable when the channel contains any non-finite sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelBlockMeter {
    sample_peak: Option<f32>,
    block_rms: Option<f32>,
    full_scale_sample_count: usize,
    non_finite_sample_count: usize,
}

impl ChannelBlockMeter {
    #[must_use]
    pub const fn sample_peak(self) -> Option<f32> {
        self.sample_peak
    }

    #[must_use]
    pub const fn block_rms(self) -> Option<f32> {
        self.block_rms
    }

    /// Counts finite samples whose absolute value is at least `1.0`.
    ///
    /// This reports sample-domain full-scale excursions, not inter-sample
    /// peaks or proof that a preceding process clipped the signal.
    #[must_use]
    pub const fn full_scale_sample_count(self) -> usize {
        self.full_scale_sample_count
    }

    #[must_use]
    pub const fn non_finite_sample_count(self) -> usize {
        self.non_finite_sample_count
    }
}

/// Measurements derived from the final planar stereo output of one callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoBlockMeter {
    frame_count: usize,
    left: ChannelBlockMeter,
    right: ChannelBlockMeter,
}

impl StereoBlockMeter {
    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    #[must_use]
    pub const fn left(self) -> ChannelBlockMeter {
        self.left
    }

    #[must_use]
    pub const fn right(self) -> ChannelBlockMeter {
        self.right
    }
}

/// Processor result and final-output measurements from one callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcessedStereoCallback<R> {
    processor_result: R,
    meter: StereoBlockMeter,
}

impl<R> ProcessedStereoCallback<R> {
    #[must_use]
    pub const fn processor_result(&self) -> &R {
        &self.processor_result
    }

    #[must_use]
    pub const fn meter(&self) -> StereoBlockMeter {
        self.meter
    }

    #[must_use]
    pub fn into_parts(self) -> (R, StereoBlockMeter) {
        (self.processor_result, self.meter)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ChannelMeterAccumulator {
    sample_peak: f32,
    sum_squares: f64,
    full_scale_sample_count: usize,
    non_finite_sample_count: usize,
}

impl ChannelMeterAccumulator {
    fn observe(&mut self, sample: f32) {
        if !sample.is_finite() {
            self.non_finite_sample_count += 1;
            return;
        }
        let absolute = sample.abs();
        self.sample_peak = self.sample_peak.max(absolute);
        self.sum_squares += f64::from(sample) * f64::from(sample);
        if absolute >= 1.0 {
            self.full_scale_sample_count += 1;
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "the validated frame count is at most 65,536 and f64 RMS intentionally returns f32"
    )]
    fn finish(self, frame_count: usize) -> ChannelBlockMeter {
        let readings = if self.non_finite_sample_count == 0 {
            let rms = if frame_count == 0 {
                0.0
            } else {
                (self.sum_squares / frame_count as f64).sqrt() as f32
            };
            Some((self.sample_peak, rms))
        } else {
            None
        };
        ChannelBlockMeter {
            sample_peak: readings.map(|(peak, _)| peak),
            block_rms: readings.map(|(_, rms)| rms),
            full_scale_sample_count: self.full_scale_sample_count,
            non_finite_sample_count: self.non_finite_sample_count,
        }
    }
}

/// Preallocated adapter from interleaved stereo device buffers to planar DSP.
///
/// Device setup owns sample-format and channel negotiation. Callback processing
/// performs no allocation, waiting, I/O, logging, or dynamic dispatch. Output
/// is interleaved only after the processor succeeds; every rejected or failed
/// callback is replaced with silence.
#[derive(Debug)]
pub struct InterleavedStereoCallback {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl InterleavedStereoCallback {
    /// Allocates both planar callback buffers before device processing starts.
    ///
    /// # Errors
    ///
    /// Returns [`CallbackAdapterCreationError`] for a zero or unsupported frame
    /// bound, or if either planar buffer cannot be reserved.
    pub fn try_new(maximum_frames: usize) -> Result<Self, CallbackAdapterCreationError> {
        if maximum_frames == 0 || maximum_frames > MAX_PROCESS_FRAMES {
            return Err(CallbackAdapterCreationError::InvalidMaximumFrames {
                actual: maximum_frames,
                maximum: MAX_PROCESS_FRAMES,
            });
        }
        let mut left = Vec::new();
        left.try_reserve_exact(maximum_frames)
            .map_err(|_| CallbackAdapterCreationError::CapacityUnavailable)?;
        left.resize(maximum_frames, 0.0);
        let mut right = Vec::new();
        right
            .try_reserve_exact(maximum_frames)
            .map_err(|_| CallbackAdapterCreationError::CapacityUnavailable)?;
        right.resize(maximum_frames, 0.0);
        Ok(Self { left, right })
    }

    #[must_use]
    pub fn maximum_frames(&self) -> usize {
        self.left.len()
    }

    /// Processes one interleaved stereo `f32` device callback.
    ///
    /// The supplied processor receives zeroed planar slices of the exact
    /// callback length. The device buffer is not exposed to partial processor
    /// writes, and frames left unwritten by a successful processor stay silent.
    ///
    /// # Errors
    ///
    /// Returns [`StereoCallbackError`] when the sample count is not stereo
    /// aligned, the callback exceeds the configured frame bound, or the
    /// processor fails. Every error clears the complete device buffer.
    pub fn process<R, E, F>(
        &mut self,
        interleaved_output: &mut [f32],
        processor: F,
    ) -> Result<ProcessedStereoCallback<R>, StereoCallbackError<E>>
    where
        F: FnOnce(StereoOutput<'_>) -> Result<R, E>,
    {
        if !interleaved_output.len().is_multiple_of(2) {
            let samples = interleaved_output.len();
            interleaved_output.fill(0.0);
            return Err(StereoCallbackError::SampleCountNotStereoAligned { samples });
        }
        let frame_count = interleaved_output.len() / 2;
        if frame_count > self.maximum_frames() {
            interleaved_output.fill(0.0);
            return Err(StereoCallbackError::BlockTooLarge {
                actual: frame_count,
                maximum: self.maximum_frames(),
            });
        }
        self.left[..frame_count].fill(0.0);
        self.right[..frame_count].fill(0.0);
        let result = processor(StereoOutput {
            left: &mut self.left[..frame_count],
            right: &mut self.right[..frame_count],
        });
        let value = match result {
            Ok(value) => value,
            Err(source) => {
                interleaved_output.fill(0.0);
                return Err(StereoCallbackError::Processor(source));
            }
        };
        let mut left_meter = ChannelMeterAccumulator::default();
        let mut right_meter = ChannelMeterAccumulator::default();
        for (frame, samples) in interleaved_output.chunks_exact_mut(2).enumerate() {
            let left = self.left[frame];
            let right = self.right[frame];
            left_meter.observe(left);
            right_meter.observe(right);
            samples[0] = left;
            samples[1] = right;
        }
        Ok(ProcessedStereoCallback {
            processor_result: value,
            meter: StereoBlockMeter {
                frame_count,
                left: left_meter.finish(frame_count),
                right: right_meter.finish(frame_count),
            },
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct ChannelState {
    gain: f32,
    pan: f32,
}

impl From<ChannelParameters> for ChannelState {
    fn from(parameters: ChannelParameters) -> Self {
        Self {
            gain: parameters.effective_gain(),
            pan: parameters.pan.value(),
        }
    }
}

/// A fixed-slot, stereo summing mixer.
///
/// Construction allocates its slot state. [`StereoMixer::process`] performs no
/// heap allocation, I/O, logging, locking, or dynamic dispatch. Parameter
/// changes are linearly ramped from the preceding block state to the requested
/// state across the current block. The output remains unclipped floating-point
/// audio; device-format conversion owns final limiting or clipping policy.
#[derive(Debug)]
pub struct StereoMixer {
    channels: Vec<ChannelState>,
    master_gain: f32,
}

impl StereoMixer {
    /// Creates a fixed-slot mixer and allocates all channel state up front.
    ///
    /// # Errors
    ///
    /// Returns [`MixerCreationError::CapacityUnavailable`] when channel state
    /// cannot be reserved.
    pub fn try_new(
        initial_channels: &[ChannelParameters],
        master_gain: LinearGain,
    ) -> Result<Self, MixerCreationError> {
        let mut channels = Vec::new();
        channels
            .try_reserve_exact(initial_channels.len())
            .map_err(|_| MixerCreationError::CapacityUnavailable)?;
        channels.extend(initial_channels.iter().copied().map(ChannelState::from));
        Ok(Self {
            channels,
            master_gain: master_gain.value(),
        })
    }

    #[must_use]
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Processes one planar audio block into a stereo summing bus.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError`] before touching output or state when channel
    /// counts or frame counts disagree, stereo planes differ in length, or the
    /// block exceeds the supported real-time bound.
    #[allow(clippy::needless_pass_by_value)]
    pub fn process(
        &mut self,
        inputs: &[InputBlock<'_>],
        target_channels: &[ChannelParameters],
        target_master_gain: LinearGain,
        output: StereoOutput<'_>,
    ) -> Result<(), ProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(ProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        if frame_count > MAX_PROCESS_FRAMES {
            return Err(ProcessError::BlockTooLarge {
                actual: frame_count,
                maximum: MAX_PROCESS_FRAMES,
            });
        }
        if inputs.len() != self.channels.len() || target_channels.len() != self.channels.len() {
            return Err(ProcessError::InputCountMismatch {
                configured: self.channels.len(),
                inputs: inputs.len(),
                parameters: target_channels.len(),
            });
        }
        for (index, input) in inputs.iter().copied().enumerate() {
            let actual = input.frame_count()?;
            if actual != frame_count {
                return Err(ProcessError::InputFrameCountMismatch {
                    input: index,
                    expected: frame_count,
                    actual,
                });
            }
        }

        if frame_count == 0 {
            return Ok(());
        }

        output.left.fill(0.0);
        output.right.fill(0.0);
        #[allow(clippy::cast_precision_loss)]
        let reciprocal_frames = 1.0 / frame_count as f32;

        for (slot, input) in inputs.iter().copied().enumerate() {
            let start = self.channels[slot];
            let target = ChannelState::from(target_channels[slot]);
            match input {
                InputBlock::Mono(samples) => mix_mono(
                    samples,
                    output.left,
                    output.right,
                    start,
                    target,
                    reciprocal_frames,
                ),
                InputBlock::Stereo { left, right } => mix_stereo(
                    left,
                    right,
                    output.left,
                    output.right,
                    start,
                    target,
                    reciprocal_frames,
                ),
            }
            self.channels[slot] = target;
        }

        apply_master_gain(
            output.left,
            output.right,
            self.master_gain,
            target_master_gain.value(),
            reciprocal_frames,
        );
        self.master_gain = target_master_gain.value();
        Ok(())
    }

    /// Processes exact per-sample controls without adding a block ramp.
    ///
    /// This path is intended for already compiled automation vectors. The
    /// control value at index `n` applies to sample frame `n`; the last values
    /// become the scalar-ramp starting state for any later [`Self::process`]
    /// call.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessError`] before touching output or state when channel
    /// counts, audio frame counts, control frame counts, or output lengths
    /// disagree, or the block exceeds the supported real-time bound.
    #[allow(clippy::needless_pass_by_value)]
    pub fn process_controlled(
        &mut self,
        inputs: &[ControlledInputBlock<'_>],
        master_gain: &[LinearGain],
        output: StereoOutput<'_>,
    ) -> Result<(), ProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(ProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        if frame_count > MAX_PROCESS_FRAMES {
            return Err(ProcessError::BlockTooLarge {
                actual: frame_count,
                maximum: MAX_PROCESS_FRAMES,
            });
        }
        if inputs.len() != self.channels.len() {
            return Err(ProcessError::InputCountMismatch {
                configured: self.channels.len(),
                inputs: inputs.len(),
                parameters: inputs.len(),
            });
        }
        if master_gain.len() != frame_count {
            return Err(ProcessError::MasterControlFrameCountMismatch {
                expected: frame_count,
                actual: master_gain.len(),
            });
        }
        for (index, controlled) in inputs.iter().enumerate() {
            let actual = controlled.input.frame_count()?;
            if actual != frame_count {
                return Err(ProcessError::InputFrameCountMismatch {
                    input: index,
                    expected: frame_count,
                    actual,
                });
            }
            if controlled.parameters.len() != frame_count {
                return Err(ProcessError::ControlFrameCountMismatch {
                    input: index,
                    expected: frame_count,
                    actual: controlled.parameters.len(),
                });
            }
        }
        if frame_count == 0 {
            return Ok(());
        }

        output.left.fill(0.0);
        output.right.fill(0.0);
        for (slot, controlled) in inputs.iter().enumerate() {
            match controlled.input {
                InputBlock::Mono(samples) => {
                    mix_mono_controlled(samples, controlled.parameters, output.left, output.right);
                }
                InputBlock::Stereo { left, right } => {
                    mix_stereo_controlled(
                        left,
                        right,
                        controlled.parameters,
                        output.left,
                        output.right,
                    );
                }
            }
            self.channels[slot] =
                ChannelState::from(controlled.parameters[controlled.parameters.len() - 1]);
        }
        apply_controlled_master_gain(output.left, output.right, master_gain);
        self.master_gain = master_gain[master_gain.len() - 1].value();
        Ok(())
    }
}

fn ramp(start: f32, end: f32, frame: usize, reciprocal_frames: f32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    (end - start).mul_add((frame + 1) as f32 * reciprocal_frames, start)
}

fn mono_pan_coefficients(pan: f32) -> (f32, f32) {
    if pan <= -1.0 {
        return (1.0, 0.0);
    }
    if pan >= 1.0 {
        return (0.0, 1.0);
    }
    let right_position = (pan + 1.0) * 0.5;
    let left_position = 1.0 - right_position;
    (
        left_position * MONO_PAN_SCALE.mul_add(left_position, 1.0 - MONO_PAN_SCALE),
        right_position * MONO_PAN_SCALE.mul_add(right_position, 1.0 - MONO_PAN_SCALE),
    )
}

fn stereo_balance_coefficients(pan: f32) -> (f32, f32) {
    if pan > 0.0 {
        (1.0 - pan, 1.0)
    } else {
        (1.0, 1.0 + pan)
    }
}

fn mix_mono(
    input: &[f32],
    output_left: &mut [f32],
    output_right: &mut [f32],
    start: ChannelState,
    target: ChannelState,
    reciprocal_frames: f32,
) {
    for (frame, sample) in input.iter().copied().enumerate() {
        let gain = ramp(start.gain, target.gain, frame, reciprocal_frames);
        let pan = ramp(start.pan, target.pan, frame, reciprocal_frames);
        let (pan_left, pan_right) = mono_pan_coefficients(pan);
        output_left[frame] += sample * gain * pan_left;
        output_right[frame] += sample * gain * pan_right;
    }
}

fn mix_stereo(
    input_left: &[f32],
    input_right: &[f32],
    output_left: &mut [f32],
    output_right: &mut [f32],
    start: ChannelState,
    target: ChannelState,
    reciprocal_frames: f32,
) {
    for frame in 0..input_left.len() {
        let gain = ramp(start.gain, target.gain, frame, reciprocal_frames);
        let pan = ramp(start.pan, target.pan, frame, reciprocal_frames);
        let (balance_left, balance_right) = stereo_balance_coefficients(pan);
        output_left[frame] += input_left[frame] * gain * balance_left;
        output_right[frame] += input_right[frame] * gain * balance_right;
    }
}

fn mix_mono_controlled(
    input: &[f32],
    parameters: &[ChannelParameters],
    output_left: &mut [f32],
    output_right: &mut [f32],
) {
    for (frame, sample) in input.iter().copied().enumerate() {
        let state = ChannelState::from(parameters[frame]);
        let (pan_left, pan_right) = mono_pan_coefficients(state.pan);
        output_left[frame] += sample * state.gain * pan_left;
        output_right[frame] += sample * state.gain * pan_right;
    }
}

fn mix_stereo_controlled(
    input_left: &[f32],
    input_right: &[f32],
    parameters: &[ChannelParameters],
    output_left: &mut [f32],
    output_right: &mut [f32],
) {
    for frame in 0..input_left.len() {
        let state = ChannelState::from(parameters[frame]);
        let (balance_left, balance_right) = stereo_balance_coefficients(state.pan);
        output_left[frame] += input_left[frame] * state.gain * balance_left;
        output_right[frame] += input_right[frame] * state.gain * balance_right;
    }
}

fn apply_master_gain(
    output_left: &mut [f32],
    output_right: &mut [f32],
    start: f32,
    target: f32,
    reciprocal_frames: f32,
) {
    for frame in 0..output_left.len() {
        let gain = ramp(start, target, frame, reciprocal_frames);
        output_left[frame] *= gain;
        output_right[frame] *= gain;
    }
}

fn apply_controlled_master_gain(
    output_left: &mut [f32],
    output_right: &mut [f32],
    master_gain: &[LinearGain],
) {
    for frame in 0..output_left.len() {
        output_left[frame] *= master_gain[frame].value();
        output_right[frame] *= master_gain[frame].value();
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
                "sample {index}: actual={actual}, expected={expected}"
            );
        }
    }

    fn parameters(gain: LinearGain, pan: Pan, audible: bool) -> ChannelParameters {
        ChannelParameters::new(gain, pan, audible)
    }

    #[test]
    fn callback_adapter_interleaves_exact_variable_sized_blocks() {
        let mut adapter = InterleavedStereoCallback::try_new(4).unwrap();
        assert_eq!(adapter.maximum_frames(), 4);
        let mut output = [9.0_f32; 6];

        let processed = adapter
            .process(&mut output, |planar| {
                assert_eq!(planar.left.len(), 3);
                assert_eq!(planar.right.len(), 3);
                planar.left.copy_from_slice(&[1.0, 2.0, 3.0]);
                planar.right.copy_from_slice(&[-1.0, -2.0, -3.0]);
                Ok::<_, ()>(17)
            })
            .unwrap();

        assert_eq!(*processed.processor_result(), 17);
        assert_samples(&output, &[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);

        let mut shorter = [9.0_f32; 4];
        adapter
            .process(&mut shorter, |planar| {
                planar.left[0] = 4.0;
                planar.right[0] = 5.0;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_samples(&shorter, &[4.0, 5.0, 0.0, 0.0]);

        let mut empty = [];
        adapter
            .process(&mut empty, |planar| {
                assert!(planar.left.is_empty());
                assert!(planar.right.is_empty());
                Ok::<_, ()>(())
            })
            .unwrap();
    }

    #[test]
    fn callback_adapter_measures_final_sample_peak_and_block_rms() {
        let mut adapter = InterleavedStereoCallback::try_new(4).unwrap();
        let mut output = [9.0_f32; 8];

        let processed = adapter
            .process(&mut output, |planar| {
                planar.left.copy_from_slice(&[0.0, -0.5, 1.0, -1.5]);
                planar.right.copy_from_slice(&[0.25, -0.25, 0.25, -0.25]);
                Ok::<_, ()>("rendered")
            })
            .unwrap();
        let (processor_result, meter) = processed.into_parts();

        assert_eq!(processor_result, "rendered");
        assert_eq!(meter.frame_count(), 4);
        assert_eq!(meter.left().sample_peak(), Some(1.5));
        assert_eq!(meter.left().full_scale_sample_count(), 2);
        assert_eq!(meter.left().non_finite_sample_count(), 0);
        assert!((meter.left().block_rms().unwrap() - (3.5_f32 / 4.0).sqrt()).abs() <= EPSILON);
        assert_eq!(meter.right().sample_peak(), Some(0.25));
        assert_eq!(meter.right().block_rms(), Some(0.25));
        assert_eq!(meter.right().full_scale_sample_count(), 0);
        assert_eq!(meter.right().non_finite_sample_count(), 0);
    }

    #[test]
    fn callback_meter_invalidates_readings_containing_non_finite_samples() {
        let mut adapter = InterleavedStereoCallback::try_new(3).unwrap();
        let mut output = [9.0_f32; 6];

        let meter = adapter
            .process(&mut output, |planar| {
                planar.left.copy_from_slice(&[0.5, f32::NAN, f32::INFINITY]);
                planar.right.copy_from_slice(&[0.0, -1.0, 0.5]);
                Ok::<_, ()>(())
            })
            .unwrap()
            .meter();

        assert_eq!(meter.left().sample_peak(), None);
        assert_eq!(meter.left().block_rms(), None);
        assert_eq!(meter.left().full_scale_sample_count(), 0);
        assert_eq!(meter.left().non_finite_sample_count(), 2);
        assert_eq!(meter.right().sample_peak(), Some(1.0));
        assert_eq!(meter.right().block_rms(), Some((1.25_f32 / 3.0).sqrt()));
        assert_eq!(meter.right().full_scale_sample_count(), 1);
        assert_eq!(meter.right().non_finite_sample_count(), 0);
        assert!(output[2].is_nan());
        assert!(output[4].is_infinite());
    }

    #[test]
    fn callback_meter_reports_zero_for_an_empty_block() {
        let mut adapter = InterleavedStereoCallback::try_new(1).unwrap();
        let mut output = [];

        let meter = adapter
            .process(&mut output, |_| Ok::<_, ()>(()))
            .unwrap()
            .meter();

        assert_eq!(meter.frame_count(), 0);
        assert_eq!(meter.left().sample_peak(), Some(0.0));
        assert_eq!(meter.left().block_rms(), Some(0.0));
        assert_eq!(meter.right().sample_peak(), Some(0.0));
        assert_eq!(meter.right().block_rms(), Some(0.0));
    }

    #[test]
    fn callback_adapter_silences_every_rejected_or_failed_block() {
        assert_eq!(
            InterleavedStereoCallback::try_new(0).unwrap_err(),
            CallbackAdapterCreationError::InvalidMaximumFrames {
                actual: 0,
                maximum: MAX_PROCESS_FRAMES,
            }
        );
        assert_eq!(
            InterleavedStereoCallback::try_new(MAX_PROCESS_FRAMES + 1).unwrap_err(),
            CallbackAdapterCreationError::InvalidMaximumFrames {
                actual: MAX_PROCESS_FRAMES + 1,
                maximum: MAX_PROCESS_FRAMES,
            }
        );

        let mut adapter = InterleavedStereoCallback::try_new(4).unwrap();
        let mut odd = [9.0_f32; 3];
        assert_eq!(
            adapter.process(&mut odd, |_| -> Result<(), &'static str> {
                panic!("processor must not run for an invalid sample count")
            }),
            Err(StereoCallbackError::SampleCountNotStereoAligned { samples: 3 })
        );
        assert_samples(&odd, &[0.0; 3]);

        let mut oversized = [9.0_f32; 10];
        assert_eq!(
            adapter.process(&mut oversized, |_| -> Result<(), &'static str> {
                panic!("processor must not run for an oversized block")
            }),
            Err(StereoCallbackError::BlockTooLarge {
                actual: 5,
                maximum: 4,
            })
        );
        assert_samples(&oversized, &[0.0; 10]);

        let mut failed = [9.0_f32; 4];
        assert_eq!(
            adapter.process(&mut failed, |planar| {
                planar.left.fill(1.0);
                planar.right.fill(2.0);
                Err::<(), _>("render failure")
            }),
            Err(StereoCallbackError::Processor("render failure"))
        );
        assert_samples(&failed, &[0.0; 4]);
    }

    #[test]
    fn rejects_invalid_parameter_domains() {
        assert_eq!(
            LinearGain::new(f32::NAN),
            Err(ParameterError::LinearGainOutOfRange)
        );
        assert_eq!(
            LinearGain::new(-0.01),
            Err(ParameterError::LinearGainOutOfRange)
        );
        assert_eq!(
            LinearGain::from_millionths(2_000_001),
            Err(ParameterError::LinearGainOutOfRange)
        );
        assert_eq!(Pan::new(f32::INFINITY), Err(ParameterError::PanOutOfRange));
        assert_eq!(Pan::new(1.01), Err(ParameterError::PanOutOfRange));
        assert_eq!(
            Pan::from_millionths(-1_000_001),
            Err(ParameterError::PanOutOfRange)
        );
    }

    #[test]
    fn clears_output_and_sums_stereo_inputs() {
        let initial = [
            parameters(LinearGain::UNITY, Pan::CENTER, true),
            parameters(LinearGain::UNITY, Pan::CENTER, true),
        ];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let first_left = [0.25, -0.5];
        let first_right = [0.5, 0.25];
        let second_left = [0.75, 0.25];
        let second_right = [-0.25, 0.75];
        let inputs = [
            InputBlock::Stereo {
                left: &first_left,
                right: &first_right,
            },
            InputBlock::Stereo {
                left: &second_left,
                right: &second_right,
            },
        ];
        let mut left = [99.0; 2];
        let mut right = [99.0; 2];

        mixer
            .process(
                &inputs,
                &initial,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();

        assert_samples(&left, &[1.0, -0.25]);
        assert_samples(&right, &[0.25, 1.0]);
    }

    #[test]
    fn uses_minus_three_decibel_mono_pan_law_with_unity_endpoints() {
        let center = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let mut center_mixer = StereoMixer::try_new(&center, LinearGain::UNITY).unwrap();
        let input = [1.0];
        let mut left = [0.0];
        let mut right = [0.0];
        center_mixer
            .process(
                &[InputBlock::Mono(&input)],
                &center,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();
        assert_samples(&left, &[0.707_945_76]);
        assert_samples(&right, &[0.707_945_76]);

        for (pan, expected) in [
            (Pan::HARD_LEFT, ([1.0], [0.0])),
            (Pan::HARD_RIGHT, ([0.0], [1.0])),
        ] {
            let hard_pan = [parameters(LinearGain::UNITY, pan, true)];
            let mut mixer = StereoMixer::try_new(&hard_pan, LinearGain::UNITY).unwrap();
            let mut left = [0.0];
            let mut right = [0.0];
            mixer
                .process(
                    &[InputBlock::Mono(&input)],
                    &hard_pan,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut left,
                        right: &mut right,
                    },
                )
                .unwrap();
            assert_samples(&left, &expected.0);
            assert_samples(&right, &expected.1);
        }
    }

    #[test]
    fn stereo_pan_is_a_balance_control() {
        let initial = [parameters(LinearGain::UNITY, Pan::HARD_RIGHT, true)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let source_left = [1.0, -1.0];
        let source_right = [0.5, -0.5];
        let mut left = [0.0; 2];
        let mut right = [0.0; 2];

        mixer
            .process(
                &[InputBlock::Stereo {
                    left: &source_left,
                    right: &source_right,
                }],
                &initial,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();

        assert_samples(&left, &[0.0, 0.0]);
        assert_samples(&right, &source_right);
    }

    #[test]
    fn ramps_gain_mute_and_master_changes_across_the_block() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let target = [parameters(LinearGain::UNITY, Pan::CENTER, false)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let source_left = [1.0; 4];
        let source_right = [1.0; 4];
        let mut left = [0.0; 4];
        let mut right = [0.0; 4];

        mixer
            .process(
                &[InputBlock::Stereo {
                    left: &source_left,
                    right: &source_right,
                }],
                &target,
                LinearGain::new(0.5).unwrap(),
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();

        assert_samples(&left, &[0.65625, 0.375, 0.15625, 0.0]);
        assert_samples(&right, &[0.65625, 0.375, 0.15625, 0.0]);
    }

    #[test]
    fn ramps_mono_pan_to_the_target_without_cross_block_discontinuity() {
        let initial = [parameters(LinearGain::UNITY, Pan::HARD_LEFT, true)];
        let target = [parameters(LinearGain::UNITY, Pan::HARD_RIGHT, true)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let source = [1.0; 4];
        let mut left = [0.0; 4];
        let mut right = [0.0; 4];

        mixer
            .process(
                &[InputBlock::Mono(&source)],
                &target,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();

        assert_samples(&left, &[0.905_959_3, 0.707_945_76, 0.405_959_34, 0.0]);
        assert_samples(&right, &[0.405_959_34, 0.707_945_76, 0.905_959_3, 1.0]);

        let mut next_left = [0.0];
        let mut next_right = [0.0];
        mixer
            .process(
                &[InputBlock::Mono(&source[..1])],
                &target,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut next_left,
                    right: &mut next_right,
                },
            )
            .unwrap();
        assert_samples(&next_left, &[0.0]);
        assert_samples(&next_right, &[1.0]);
    }

    #[test]
    fn rejects_every_invalid_block_shape_before_processing() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let source = [1.0; 2];

        let mut left = [3.0; 2];
        let mut right = [4.0; 2];
        assert_eq!(
            mixer
                .process(
                    &[],
                    &initial,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut left,
                        right: &mut right,
                    },
                )
                .unwrap_err(),
            ProcessError::InputCountMismatch {
                configured: 1,
                inputs: 0,
                parameters: 1,
            }
        );
        assert_samples(&left, &[3.0; 2]);
        assert_samples(&right, &[4.0; 2]);

        assert_eq!(
            mixer
                .process(
                    &[InputBlock::Stereo {
                        left: &source,
                        right: &source[..1],
                    }],
                    &initial,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut left,
                        right: &mut right,
                    },
                )
                .unwrap_err(),
            ProcessError::StereoInputLengthMismatch { left: 2, right: 1 }
        );

        assert_eq!(
            mixer
                .process(
                    &[InputBlock::Mono(&source[..1])],
                    &initial,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut left,
                        right: &mut right,
                    },
                )
                .unwrap_err(),
            ProcessError::InputFrameCountMismatch {
                input: 0,
                expected: 2,
                actual: 1,
            }
        );

        let oversized_source = vec![0.0; MAX_PROCESS_FRAMES + 1];
        let mut oversized_left = vec![3.0; MAX_PROCESS_FRAMES + 1];
        let mut oversized_right = vec![4.0; MAX_PROCESS_FRAMES + 1];
        assert_eq!(
            mixer
                .process(
                    &[InputBlock::Mono(&oversized_source)],
                    &initial,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut oversized_left,
                        right: &mut oversized_right,
                    },
                )
                .unwrap_err(),
            ProcessError::BlockTooLarge {
                actual: MAX_PROCESS_FRAMES + 1,
                maximum: MAX_PROCESS_FRAMES,
            }
        );
        assert_samples(&oversized_left, &vec![3.0; MAX_PROCESS_FRAMES + 1]);
        assert_samples(&oversized_right, &vec![4.0; MAX_PROCESS_FRAMES + 1]);
    }

    #[test]
    fn validation_failure_does_not_touch_output_or_advance_state() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let target = [parameters(LinearGain::SILENCE, Pan::CENTER, true)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let source = [1.0; 2];
        let mut invalid_left = [7.0; 2];
        let mut invalid_right = [8.0; 1];

        let error = mixer
            .process(
                &[InputBlock::Mono(&source)],
                &target,
                LinearGain::SILENCE,
                StereoOutput {
                    left: &mut invalid_left,
                    right: &mut invalid_right,
                },
            )
            .unwrap_err();
        assert_eq!(
            error,
            ProcessError::OutputLengthMismatch { left: 2, right: 1 }
        );
        assert_samples(&invalid_left, &[7.0; 2]);
        assert_samples(&invalid_right, &[8.0; 1]);

        let mut left = [0.0; 2];
        let mut right = [0.0; 2];
        mixer
            .process(
                &[InputBlock::Mono(&source)],
                &target,
                LinearGain::SILENCE,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();
        assert!(left[0] > 0.0);
        assert!(left[1].abs() <= EPSILON);
        assert_samples(&left, &right);
    }

    #[test]
    fn controlled_processing_is_sample_exact_and_block_size_independent() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let controls = [
            parameters(LinearGain::SILENCE, Pan::CENTER, true),
            parameters(LinearGain::new(0.25).unwrap(), Pan::CENTER, true),
            parameters(LinearGain::new(0.5).unwrap(), Pan::CENTER, true),
            parameters(LinearGain::new(0.75).unwrap(), Pan::CENTER, true),
            parameters(LinearGain::UNITY, Pan::CENTER, false),
            parameters(LinearGain::new(1.25).unwrap(), Pan::CENTER, true),
            parameters(LinearGain::new(1.5).unwrap(), Pan::CENTER, true),
            parameters(LinearGain::new(1.75).unwrap(), Pan::CENTER, true),
        ];
        let master = [LinearGain::UNITY; 8];
        let source_left = [1.0; 8];
        let source_right = [1.0; 8];
        let input = InputBlock::Stereo {
            left: &source_left,
            right: &source_right,
        };
        let mut whole = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let mut whole_left = [9.0; 8];
        let mut whole_right = [9.0; 8];
        whole
            .process_controlled(
                &[ControlledInputBlock {
                    input,
                    parameters: &controls,
                }],
                &master,
                StereoOutput {
                    left: &mut whole_left,
                    right: &mut whole_right,
                },
            )
            .unwrap();

        let mut split = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let mut split_left = [9.0; 8];
        let mut split_right = [9.0; 8];
        for range in [0..3, 3..8] {
            split
                .process_controlled(
                    &[ControlledInputBlock {
                        input: InputBlock::Stereo {
                            left: &source_left[range.clone()],
                            right: &source_right[range.clone()],
                        },
                        parameters: &controls[range.clone()],
                    }],
                    &master[range.clone()],
                    StereoOutput {
                        left: &mut split_left[range.clone()],
                        right: &mut split_right[range],
                    },
                )
                .unwrap();
        }
        let expected = [0.0, 0.25, 0.5, 0.75, 0.0, 1.25, 1.5, 1.75];
        assert_samples(&whole_left, &expected);
        assert_samples(&whole_right, &expected);
        assert_samples(&split_left, &expected);
        assert_samples(&split_right, &expected);
    }

    #[test]
    fn controlled_processing_applies_pan_at_each_sample() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let controls = [
            parameters(LinearGain::UNITY, Pan::HARD_LEFT, true),
            parameters(LinearGain::UNITY, Pan::CENTER, true),
            parameters(LinearGain::UNITY, Pan::HARD_RIGHT, true),
        ];
        let source = [1.0; 3];
        let mut left = [9.0; 3];
        let mut right = [9.0; 3];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();

        mixer
            .process_controlled(
                &[ControlledInputBlock {
                    input: InputBlock::Mono(&source),
                    parameters: &controls,
                }],
                &[LinearGain::UNITY; 3],
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();

        let (center_left, center_right) = mono_pan_coefficients(Pan::CENTER.value());
        assert_samples(&left, &[1.0, center_left, 0.0]);
        assert_samples(&right, &[0.0, center_right, 1.0]);
    }

    #[test]
    fn zero_frame_block_does_not_advance_ramps() {
        let initial = [parameters(LinearGain::UNITY, Pan::CENTER, true)];
        let target = [parameters(LinearGain::SILENCE, Pan::CENTER, true)];
        let mut mixer = StereoMixer::try_new(&initial, LinearGain::UNITY).unwrap();
        let mut empty_left = [];
        let mut empty_right = [];
        mixer
            .process(
                &[InputBlock::Mono(&[])],
                &target,
                LinearGain::SILENCE,
                StereoOutput {
                    left: &mut empty_left,
                    right: &mut empty_right,
                },
            )
            .unwrap();

        let source = [1.0; 2];
        let mut left = [0.0; 2];
        let mut right = [0.0; 2];
        mixer
            .process(
                &[InputBlock::Mono(&source)],
                &target,
                LinearGain::SILENCE,
                StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
            )
            .unwrap();
        assert!(left[0] > 0.0);
        assert!(left[1].abs() <= EPSILON);
    }
}
