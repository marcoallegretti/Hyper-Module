#![doc = "Bounded frame-domain audio clip execution for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_audio_source::{DecodeError, DecodedAudio};

const MAX_NATIVE_CLIP_GAIN: f32 = 2.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeMapping {
    NativeRate,
    ResamplingRequired,
    TimeStretchRequired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConsolidationReason {
    Transposition,
    Resampling,
    TimeStretch,
    ClipGainOutsideNativeRange,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameRange {
    pub start: usize,
    pub end: usize,
}

impl FrameRange {
    #[must_use]
    pub const fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeClipSpec {
    pub enabled: bool,
    pub source: FrameRange,
    pub loop_range: Option<FrameRange>,
    pub reverse: bool,
    pub gain_decibels: f64,
    pub fade_in_frames: usize,
    pub fade_out_frames: usize,
    pub transpose_cents: i32,
    pub time_mapping: TimeMapping,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementSpec {
    pub clip_start_frame: usize,
    pub frame_count: usize,
    /// Linear-amplitude fade-in measured from this placement's start edge.
    pub fade_in_frames: usize,
    /// Linear-amplitude fade-out measured from this placement's end edge.
    pub fade_out_frames: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipPlacement {
    clip_start_frame: usize,
    frame_count: usize,
    fade_in_frames: usize,
    fade_out_frames: usize,
}

impl ClipPlacement {
    #[must_use]
    pub const fn clip_start_frame(self) -> usize {
        self.clip_start_frame
    }

    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    #[must_use]
    pub const fn fade_in_frames(self) -> usize {
        self.fade_in_frames
    }

    #[must_use]
    pub const fn fade_out_frames(self) -> usize {
        self.fade_out_frames
    }

    fn fade_gain(self, local_frame: usize) -> f32 {
        let mut gain = 1.0_f32;
        if self.fade_in_frames > 0 && local_frame < self.fade_in_frames {
            #[allow(clippy::cast_precision_loss)]
            let fade = local_frame as f32 / self.fade_in_frames as f32;
            gain *= fade;
        }
        let fade_out_start = self.frame_count - self.fade_out_frames;
        if self.fade_out_frames > 0 && local_frame >= fade_out_start {
            #[allow(clippy::cast_precision_loss)]
            let fade = (self.frame_count - local_frame) as f32 / self.fade_out_frames as f32;
            gain *= fade;
        }
        gain
    }
}

#[derive(Debug)]
pub struct ClipRenderPlan<'a> {
    source: &'a DecodedAudio,
    enabled: bool,
    source_range: FrameRange,
    loop_range: Option<FrameRange>,
    reverse: bool,
    linear_gain: f32,
    fade_in_frames: usize,
    fade_out_frames: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipPlanError {
    ConsolidationRequired(ConsolidationReason),
    InvalidSourceRange,
    InvalidLoopRange,
    InvalidFades,
    InvalidGain,
    SampleGainOverflow,
    RenderedAudioInvalid,
    InvalidPlacement,
    InvalidPlacementFades,
    ArithmeticOverflow,
    OutputChannelMismatch { expected: usize, actual: usize },
    OutputLengthMismatch { left: usize, right: usize },
    BlockOutsidePlacement,
    CapacityUnavailable,
}

/// Compiles a decibel value into the exact native `f32` clip-gain domain.
///
/// # Errors
///
/// Returns [`ClipPlanError::InvalidGain`] for a non-finite conversion and
/// [`ClipPlanError::ConsolidationRequired`] when the native gain ceiling is
/// exceeded.
pub fn compile_native_clip_gain(gain_decibels: f64) -> Result<f32, ClipPlanError> {
    let linear_gain_f64 = 10.0_f64.powf(gain_decibels / 20.0);
    if !linear_gain_f64.is_finite() || linear_gain_f64 < 0.0 {
        return Err(ClipPlanError::InvalidGain);
    }
    #[allow(clippy::cast_possible_truncation)]
    let linear_gain = linear_gain_f64 as f32;
    if !linear_gain.is_finite() || linear_gain > MAX_NATIVE_CLIP_GAIN {
        return Err(ClipPlanError::ConsolidationRequired(
            ConsolidationReason::ClipGainOutsideNativeRange,
        ));
    }
    Ok(linear_gain)
}

/// Validates the exact decoded source view against a candidate native gain.
///
/// # Errors
///
/// Returns a classified clip-plan rejection for an invalid source range,
/// unsupported gain, or a finite decoded sample that would overflow after
/// native `f32` gain multiplication.
pub fn validate_native_clip_gain(
    source: &DecodedAudio,
    source_range: FrameRange,
    gain_decibels: f64,
) -> Result<f32, ClipPlanError> {
    if source_range.is_empty() || source_range.end > source.frame_count() {
        return Err(ClipPlanError::InvalidSourceRange);
    }
    let linear_gain = compile_native_clip_gain(gain_decibels)?;
    for channel in 0..source.channels() {
        let plane = source
            .plane(channel)
            .ok_or(ClipPlanError::InvalidSourceRange)?;
        if plane[source_range.start..source_range.end]
            .iter()
            .any(|sample| !(*sample * linear_gain).is_finite())
        {
            return Err(ClipPlanError::SampleGainOverflow);
        }
    }
    Ok(linear_gain)
}

impl fmt::Display for ClipPlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsolidationRequired(reason) => {
                write!(formatter, "clip requires consolidation: {reason:?}")
            }
            Self::InvalidSourceRange => formatter.write_str("clip source range is invalid"),
            Self::InvalidLoopRange => formatter.write_str("clip loop range is invalid"),
            Self::InvalidFades => formatter.write_str("clip fade ranges are invalid"),
            Self::InvalidGain => formatter.write_str("clip gain is not finite"),
            Self::SampleGainOverflow => {
                formatter.write_str("clip source and gain produce a non-finite sample")
            }
            Self::RenderedAudioInvalid => {
                formatter.write_str("rendered clip audio failed validation")
            }
            Self::InvalidPlacement => formatter.write_str("clip placement is invalid"),
            Self::InvalidPlacementFades => {
                formatter.write_str("clip placement fade ranges are invalid")
            }
            Self::ArithmeticOverflow => formatter.write_str("clip frame arithmetic overflowed"),
            Self::OutputChannelMismatch { expected, actual } => write!(
                formatter,
                "clip output has {actual} channels; expected {expected}"
            ),
            Self::OutputLengthMismatch { left, right } => write!(
                formatter,
                "clip stereo output lengths differ: left={left}, right={right}"
            ),
            Self::BlockOutsidePlacement => {
                formatter.write_str("clip render block lies outside the placement")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("clip output capacity cannot be reserved")
            }
        }
    }
}

impl Error for ClipPlanError {}

#[derive(Debug)]
pub enum ClipOutput<'a> {
    Mono(&'a mut [f32]),
    Stereo {
        left: &'a mut [f32],
        right: &'a mut [f32],
    },
}

impl ClipOutput<'_> {
    fn channels(&self) -> usize {
        match self {
            Self::Mono(_) => 1,
            Self::Stereo { .. } => 2,
        }
    }

    fn frame_count(&self) -> Result<usize, ClipPlanError> {
        match self {
            Self::Mono(samples) => Ok(samples.len()),
            Self::Stereo { left, right } if left.len() == right.len() => Ok(left.len()),
            Self::Stereo { left, right } => Err(ClipPlanError::OutputLengthMismatch {
                left: left.len(),
                right: right.len(),
            }),
        }
    }

    fn write<const ADDITIVE: bool>(
        &mut self,
        frame: usize,
        left_sample: f32,
        right_sample: Option<f32>,
    ) {
        match self {
            Self::Mono(samples) => {
                if ADDITIVE {
                    samples[frame] += left_sample;
                } else {
                    samples[frame] = left_sample;
                }
            }
            Self::Stereo { left, right } => {
                let right_sample = right_sample.unwrap_or(0.0);
                if ADDITIVE {
                    left[frame] += left_sample;
                    right[frame] += right_sample;
                } else {
                    left[frame] = left_sample;
                    right[frame] = right_sample;
                }
            }
        }
    }
}

impl<'a> ClipRenderPlan<'a> {
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Compiles a decoded source view into the native frame-domain subset.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError::ConsolidationRequired`] for transpose,
    /// resampling, time stretching, or gain beyond the native range. Invalid
    /// source, loop, fade, or gain data is rejected separately.
    pub fn try_new(source: &'a DecodedAudio, spec: NativeClipSpec) -> Result<Self, ClipPlanError> {
        assess_execution(spec)?;
        if spec.source.is_empty() || spec.source.end > source.frame_count() {
            return Err(ClipPlanError::InvalidSourceRange);
        }
        let duration = spec.source.len();
        if let Some(loop_range) = spec.loop_range
            && (loop_range.is_empty() || loop_range.end > duration)
        {
            return Err(ClipPlanError::InvalidLoopRange);
        }
        if spec.fade_out_frames > duration || spec.fade_in_frames > duration - spec.fade_out_frames
        {
            return Err(ClipPlanError::InvalidFades);
        }

        let linear_gain = validate_native_clip_gain(source, spec.source, spec.gain_decibels)?;

        Ok(Self {
            source,
            enabled: spec.enabled,
            source_range: spec.source,
            loop_range: spec.loop_range,
            reverse: spec.reverse,
            linear_gain,
            fade_in_frames: spec.fade_in_frames,
            fade_out_frames: spec.fade_out_frames,
        })
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.source.channels()
    }

    #[must_use]
    pub const fn duration_frames(&self) -> usize {
        self.source_range.len()
    }

    /// Validates placement offset, length, and loop extension rules.
    ///
    /// A looped placement keeps its offset unfolded. Rendering folds every
    /// local frame into the loop range, so an offset beyond the first pass
    /// remains reversible when an editor later extends the placement front.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError::InvalidPlacement`] for an empty or out-of-range
    /// placement and [`ClipPlanError::ArithmeticOverflow`] on range overflow.
    pub fn place(&self, spec: PlacementSpec) -> Result<ClipPlacement, ClipPlanError> {
        if spec.frame_count == 0 {
            return Err(ClipPlanError::InvalidPlacement);
        }
        if spec.fade_out_frames > spec.frame_count
            || spec.fade_in_frames > spec.frame_count - spec.fade_out_frames
        {
            return Err(ClipPlanError::InvalidPlacementFades);
        }
        let requested_end = spec
            .clip_start_frame
            .checked_add(spec.frame_count)
            .ok_or(ClipPlanError::ArithmeticOverflow)?;
        if self.loop_range.is_none() && requested_end > self.duration_frames() {
            return Err(ClipPlanError::InvalidPlacement);
        }
        Ok(ClipPlacement {
            clip_start_frame: spec.clip_start_frame,
            frame_count: spec.frame_count,
            fade_in_frames: spec.fade_in_frames,
            fade_out_frames: spec.fade_out_frames,
        })
    }

    /// Renders a block at an offset inside a validated placement.
    ///
    /// The call performs no allocation, file I/O, logging, locking, or dynamic
    /// dispatch. Structural errors are returned before output mutation.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError`] when output shape differs from the source or
    /// the requested block extends outside the placement.
    pub fn render_block(
        &self,
        placement: ClipPlacement,
        placement_frame_offset: usize,
        output: ClipOutput<'_>,
    ) -> Result<(), ClipPlanError> {
        self.render_block_inner::<false>(placement, placement_frame_offset, output)
    }

    /// Adds a block at an offset inside a validated placement to initialized
    /// output planes.
    ///
    /// The caller owns clearing or otherwise initializing the output. The
    /// call performs no allocation, file I/O, logging, locking, or dynamic
    /// dispatch. Structural errors are returned before output mutation.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError`] when output shape differs from the source or
    /// the requested block extends outside the placement.
    pub fn render_block_additive(
        &self,
        placement: ClipPlacement,
        placement_frame_offset: usize,
        output: ClipOutput<'_>,
    ) -> Result<(), ClipPlanError> {
        self.render_block_inner::<true>(placement, placement_frame_offset, output)
    }

    fn render_block_inner<const ADDITIVE: bool>(
        &self,
        placement: ClipPlacement,
        placement_frame_offset: usize,
        mut output: ClipOutput<'_>,
    ) -> Result<(), ClipPlanError> {
        let output_frames = output.frame_count()?;
        if output.channels() != self.channels() {
            return Err(ClipPlanError::OutputChannelMismatch {
                expected: self.channels(),
                actual: output.channels(),
            });
        }
        let block_end = placement_frame_offset
            .checked_add(output_frames)
            .ok_or(ClipPlanError::ArithmeticOverflow)?;
        if block_end > placement.frame_count {
            return Err(ClipPlanError::BlockOutsidePlacement);
        }
        let left_plane = self
            .source
            .plane(0)
            .ok_or(ClipPlanError::OutputChannelMismatch {
                expected: self.channels(),
                actual: 0,
            })?;
        let right_plane = if self.channels() == 2 {
            Some(
                self.source
                    .plane(1)
                    .ok_or(ClipPlanError::OutputChannelMismatch {
                        expected: 2,
                        actual: 1,
                    })?,
            )
        } else {
            None
        };

        for output_frame in 0..output_frames {
            if !self.enabled {
                output.write::<ADDITIVE>(output_frame, 0.0, Some(0.0));
                continue;
            }
            let unfolded_local = placement
                .clip_start_frame
                .checked_add(placement_frame_offset)
                .and_then(|value| value.checked_add(output_frame))
                .ok_or(ClipPlanError::ArithmeticOverflow)?;
            let local_frame = self.fold_loop(unfolded_local);
            let source_frame = self.source_frame(local_frame);
            let placement_local_frame = placement_frame_offset
                .checked_add(output_frame)
                .ok_or(ClipPlanError::ArithmeticOverflow)?;
            let amplitude = self.linear_gain
                * self.fade_gain(local_frame)
                * placement.fade_gain(placement_local_frame);
            let left = left_plane[source_frame] * amplitude;
            let right = right_plane.map(|plane| plane[source_frame] * amplitude);
            output.write::<ADDITIVE>(output_frame, left, right);
        }
        Ok(())
    }

    /// Allocates and renders one complete validated placement offline.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError`] when output capacity cannot be reserved or
    /// block rendering fails.
    pub fn render_offline(&self, placement: ClipPlacement) -> Result<DecodedAudio, ClipPlanError> {
        let mut left = allocate_silence(placement.frame_count)?;
        if self.channels() == 1 {
            self.render_block(placement, 0, ClipOutput::Mono(&mut left))?;
            DecodedAudio::from_mono(self.source.sample_rate_hz(), left)
                .map_err(|error| map_decoded_output_error(&error))
        } else {
            let mut right = allocate_silence(placement.frame_count)?;
            self.render_block(
                placement,
                0,
                ClipOutput::Stereo {
                    left: &mut left,
                    right: &mut right,
                },
            )?;
            DecodedAudio::from_stereo(self.source.sample_rate_hz(), left, right)
                .map_err(|error| map_decoded_output_error(&error))
        }
    }

    fn fold_loop(&self, local_frame: usize) -> usize {
        if let Some(loop_range) = self.loop_range
            && local_frame >= loop_range.end
        {
            return loop_range.start + (local_frame - loop_range.end) % loop_range.len();
        }
        local_frame
    }

    fn source_frame(&self, local_frame: usize) -> usize {
        if self.reverse {
            self.source_range.end - 1 - local_frame
        } else {
            self.source_range.start + local_frame
        }
    }

    fn fade_gain(&self, local_frame: usize) -> f32 {
        let mut gain = 1.0_f32;
        if self.fade_in_frames > 0 && local_frame < self.fade_in_frames {
            #[allow(clippy::cast_precision_loss)]
            let fade = local_frame as f32 / self.fade_in_frames as f32;
            gain *= fade;
        }
        let fade_out_start = self.duration_frames() - self.fade_out_frames;
        if self.fade_out_frames > 0 && local_frame >= fade_out_start {
            #[allow(clippy::cast_precision_loss)]
            let fade = (self.duration_frames() - local_frame) as f32 / self.fade_out_frames as f32;
            gain *= fade;
        }
        gain
    }
}

fn assess_execution(spec: NativeClipSpec) -> Result<(), ClipPlanError> {
    if spec.transpose_cents != 0 {
        return Err(ClipPlanError::ConsolidationRequired(
            ConsolidationReason::Transposition,
        ));
    }
    match spec.time_mapping {
        TimeMapping::NativeRate => Ok(()),
        TimeMapping::ResamplingRequired => Err(ClipPlanError::ConsolidationRequired(
            ConsolidationReason::Resampling,
        )),
        TimeMapping::TimeStretchRequired => Err(ClipPlanError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        )),
    }
}

fn allocate_silence(frame_count: usize) -> Result<Vec<f32>, ClipPlanError> {
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(frame_count)
        .map_err(|_| ClipPlanError::CapacityUnavailable)?;
    samples.resize(frame_count, 0.0);
    Ok(samples)
}

fn map_decoded_output_error(error: &DecodeError) -> ClipPlanError {
    match error {
        DecodeError::CapacityUnavailable => ClipPlanError::CapacityUnavailable,
        DecodeError::NonFiniteSample { .. } => ClipPlanError::SampleGainOverflow,
        _ => ClipPlanError::RenderedAudioInvalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1.0e-6;

    fn mono(samples: &[f32]) -> DecodedAudio {
        DecodedAudio::from_mono(48_000, samples.to_vec()).unwrap()
    }

    fn spec(frame_count: usize) -> NativeClipSpec {
        NativeClipSpec {
            enabled: true,
            source: FrameRange {
                start: 0,
                end: frame_count,
            },
            loop_range: None,
            reverse: false,
            gain_decibels: 0.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        }
    }

    fn render(plan: &ClipRenderPlan<'_>, placement: ClipPlacement) -> Vec<f32> {
        plan.render_offline(placement)
            .unwrap()
            .plane(0)
            .unwrap()
            .to_vec()
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
    fn trims_and_offsets_a_forward_source_view() {
        let source = mono(&[10.0, 11.0, 12.0, 13.0, 14.0, 15.0]);
        let mut clip = spec(4);
        clip.source = FrameRange { start: 1, end: 5 };
        let plan = ClipRenderPlan::try_new(&source, clip).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 1,
                frame_count: 3,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();

        assert_samples(&render(&plan, placement), &[12.0, 13.0, 14.0]);
    }

    #[test]
    fn reverse_traverses_the_selected_view_without_reordering_loop_time() {
        let source = mono(&[10.0, 11.0, 12.0, 13.0, 14.0, 15.0]);
        let mut clip = spec(4);
        clip.source = FrameRange { start: 1, end: 5 };
        clip.reverse = true;
        let plan = ClipRenderPlan::try_new(&source, clip).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 4,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();

        assert_samples(&render(&plan, placement), &[14.0, 13.0, 12.0, 11.0]);
    }

    #[test]
    fn repeats_the_half_open_loop_after_the_first_pass() {
        let source = mono(&[0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        let mut clip = spec(6);
        clip.loop_range = Some(FrameRange { start: 2, end: 5 });
        let plan = ClipRenderPlan::try_new(&source, clip).unwrap();
        let full = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 10,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        assert_samples(
            &render(&plan, full),
            &[0.0, 1.0, 2.0, 3.0, 4.0, 2.0, 3.0, 4.0, 2.0, 3.0],
        );

        let offset = plan
            .place(PlacementSpec {
                clip_start_frame: 4,
                frame_count: 5,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        assert_samples(&render(&plan, offset), &[4.0, 2.0, 3.0, 4.0, 2.0]);

        let unfolded_offset = plan
            .place(PlacementSpec {
                clip_start_frame: 8,
                frame_count: 5,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        assert_samples(&render(&plan, unfolded_offset), &[2.0, 3.0, 4.0, 2.0, 3.0]);
    }

    #[test]
    fn applies_linear_boundary_fades_gain_and_enabled_state() {
        let source = mono(&[1.0; 6]);
        let mut clip = spec(6);
        clip.fade_in_frames = 2;
        clip.fade_out_frames = 2;
        clip.gain_decibels = 20.0 * f64::from(0.5_f32).log10();
        let plan = ClipRenderPlan::try_new(&source, clip).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 6,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        assert_samples(&render(&plan, placement), &[0.0, 0.25, 0.5, 0.5, 0.5, 0.25]);

        clip.enabled = false;
        let disabled = ClipRenderPlan::try_new(&source, clip).unwrap();
        assert_samples(&render(&disabled, placement), &[0.0; 6]);
    }

    #[test]
    fn execution_gate_requires_consolidation_instead_of_ignoring_features() {
        let source = mono(&[0.0; 4]);
        let mut clip = spec(4);
        clip.transpose_cents = 1;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::ConsolidationRequired(
                ConsolidationReason::Transposition
            ))
        ));

        clip.transpose_cents = 0;
        clip.time_mapping = TimeMapping::ResamplingRequired;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::ConsolidationRequired(
                ConsolidationReason::Resampling
            ))
        ));

        clip.time_mapping = TimeMapping::TimeStretchRequired;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::ConsolidationRequired(
                ConsolidationReason::TimeStretch
            ))
        ));

        clip.time_mapping = TimeMapping::NativeRate;
        clip.gain_decibels = 12.0;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::ConsolidationRequired(
                ConsolidationReason::ClipGainOutsideNativeRange
            ))
        ));
    }

    #[test]
    fn placement_fades_multiply_reusable_clip_fades() {
        let source = mono(&[1.0; 6]);
        let mut clip = spec(6);
        clip.fade_in_frames = 2;
        clip.fade_out_frames = 2;
        let plan = ClipRenderPlan::try_new(&source, clip).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 6,
                fade_in_frames: 3,
                fade_out_frames: 2,
            })
            .unwrap();

        assert_eq!(placement.fade_in_frames(), 3);
        assert_eq!(placement.fade_out_frames(), 2);
        assert_samples(
            &render(&plan, placement),
            &[0.0, 1.0 / 6.0, 2.0 / 3.0, 1.0, 1.0, 0.25],
        );
    }

    #[test]
    fn rejects_invalid_source_loop_fade_placement_and_sample_product() {
        let source = mono(&[0.0; 4]);
        let mut clip = spec(4);
        clip.source.end = 5;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::InvalidSourceRange)
        ));

        clip = spec(4);
        clip.loop_range = Some(FrameRange { start: 2, end: 2 });
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::InvalidLoopRange)
        ));

        clip = spec(4);
        clip.fade_in_frames = 3;
        clip.fade_out_frames = 2;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::InvalidFades)
        ));

        clip = spec(4);
        clip.fade_out_frames = 5;
        assert!(matches!(
            ClipRenderPlan::try_new(&source, clip),
            Err(ClipPlanError::InvalidFades)
        ));

        let plan = ClipRenderPlan::try_new(&source, spec(4)).unwrap();
        assert!(matches!(
            plan.place(PlacementSpec {
                clip_start_frame: 3,
                frame_count: 2,
                fade_in_frames: 0,
                fade_out_frames: 0,
            }),
            Err(ClipPlanError::InvalidPlacement)
        ));
        assert!(matches!(
            plan.place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 0,
                fade_in_frames: 0,
                fade_out_frames: 0,
            }),
            Err(ClipPlanError::InvalidPlacement)
        ));
        assert!(matches!(
            plan.place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 4,
                fade_in_frames: 3,
                fade_out_frames: 2,
            }),
            Err(ClipPlanError::InvalidPlacementFades)
        ));

        let loud_source = mono(&[f32::MAX]);
        let mut loud_clip = spec(1);
        loud_clip.gain_decibels = 20.0 * f64::from(2.0_f32).log10();
        assert!(matches!(
            ClipRenderPlan::try_new(&loud_source, loud_clip),
            Err(ClipPlanError::SampleGainOverflow)
        ));
    }

    #[test]
    fn structural_block_errors_leave_output_untouched() {
        let source = DecodedAudio::from_stereo(48_000, vec![1.0, 2.0], vec![3.0, 4.0]).unwrap();
        let plan = ClipRenderPlan::try_new(&source, spec(2)).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 2,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        let mut mono_output = [7.0; 2];
        assert!(matches!(
            plan.render_block(placement, 0, ClipOutput::Mono(&mut mono_output)),
            Err(ClipPlanError::OutputChannelMismatch {
                expected: 2,
                actual: 1
            })
        ));
        assert_samples(&mono_output, &[7.0; 2]);

        let mut left = [7.0; 2];
        let mut right = [8.0; 1];
        assert!(matches!(
            plan.render_block(
                placement,
                0,
                ClipOutput::Stereo {
                    left: &mut left,
                    right: &mut right
                }
            ),
            Err(ClipPlanError::OutputLengthMismatch { left: 2, right: 1 })
        ));
        assert_samples(&left, &[7.0; 2]);
        assert_samples(&right, &[8.0]);

        let mut left = [7.0; 2];
        let mut right = [8.0; 2];
        assert!(matches!(
            plan.render_block(
                placement,
                1,
                ClipOutput::Stereo {
                    left: &mut left,
                    right: &mut right
                }
            ),
            Err(ClipPlanError::BlockOutsidePlacement)
        ));
        assert_samples(&left, &[7.0; 2]);
        assert_samples(&right, &[8.0; 2]);
    }

    #[test]
    fn additive_block_rendering_preserves_initialized_samples() {
        let source = DecodedAudio::from_stereo(48_000, vec![0.25, 0.5], vec![-0.5, 0.25]).unwrap();
        let plan = ClipRenderPlan::try_new(&source, spec(2)).unwrap();
        let placement = plan
            .place(PlacementSpec {
                clip_start_frame: 0,
                frame_count: 2,
                fade_in_frames: 0,
                fade_out_frames: 0,
            })
            .unwrap();
        let mut left = [1.0_f32, -1.0];
        let mut right = [0.5_f32, 2.0];

        plan.render_block_additive(
            placement,
            0,
            ClipOutput::Stereo {
                left: &mut left,
                right: &mut right,
            },
        )
        .unwrap();

        assert_samples(&left, &[1.25, -0.5]);
        assert_samples(&right, &[0.0, 2.25]);
    }
}
