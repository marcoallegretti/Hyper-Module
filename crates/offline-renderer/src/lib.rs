#![doc = "Deterministic bounded offline rendering for aligned audio sources."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_audio_engine::{
    ChannelParameters, InputBlock, LinearGain, MAX_PROCESS_FRAMES, MixerCreationError,
    ProcessError, StereoMixer, StereoOutput,
};
use hyper_audio_source::{DecodeError, DecodedAudio};

#[derive(Clone, Copy, Debug)]
pub struct AlignedInput<'a> {
    pub source: &'a DecodedAudio,
    pub parameters: ChannelParameters,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderSpec {
    pub sample_rate_hz: u32,
    pub frame_count: usize,
    pub block_frames: usize,
}

#[derive(Debug)]
pub enum RenderError {
    InvalidSpec(&'static str),
    SourceRateMismatch {
        input: usize,
        expected: u32,
        actual: u32,
    },
    SourceFrameMismatch {
        input: usize,
        expected: usize,
        actual: usize,
    },
    Mixer(ProcessError),
    MixerCreation(MixerCreationError),
    Output(DecodeError),
    CapacityUnavailable,
}

impl fmt::Display for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSpec(reason) => {
                write!(formatter, "invalid render specification: {reason}")
            }
            Self::SourceRateMismatch {
                input,
                expected,
                actual,
            } => write!(
                formatter,
                "input {input} has sample rate {actual} Hz; expected {expected} Hz"
            ),
            Self::SourceFrameMismatch {
                input,
                expected,
                actual,
            } => write!(
                formatter,
                "input {input} has {actual} frames; expected {expected}"
            ),
            Self::Mixer(error) => write!(formatter, "audio mixing failed: {error}"),
            Self::MixerCreation(error) => write!(formatter, "audio mixer creation failed: {error}"),
            Self::Output(error) => write!(formatter, "rendered output is invalid: {error}"),
            Self::CapacityUnavailable => {
                formatter.write_str("offline render capacity cannot be reserved")
            }
        }
    }
}

impl Error for RenderError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(error) => Some(error),
            Self::MixerCreation(error) => Some(error),
            Self::Output(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProcessError> for RenderError {
    fn from(error: ProcessError) -> Self {
        Self::Mixer(error)
    }
}

impl From<MixerCreationError> for RenderError {
    fn from(error: MixerCreationError) -> Self {
        Self::MixerCreation(error)
    }
}

/// Renders complete, frame-aligned sources through the production stereo mixer.
///
/// This function intentionally performs no clip placement, looping, warping,
/// transposition, file decoding, routing, or device I/O.
///
/// # Errors
///
/// Returns [`RenderError`] for an invalid specification, source rate or length
/// mismatch, mixer rejection, or invalid rendered samples.
pub fn render_aligned(
    inputs: &[AlignedInput<'_>],
    spec: RenderSpec,
    master_gain: LinearGain,
) -> Result<DecodedAudio, RenderError> {
    validate_spec(spec)?;
    for (index, input) in inputs.iter().enumerate() {
        if input.source.sample_rate_hz() != spec.sample_rate_hz {
            return Err(RenderError::SourceRateMismatch {
                input: index,
                expected: spec.sample_rate_hz,
                actual: input.source.sample_rate_hz(),
            });
        }
        if input.source.frame_count() != spec.frame_count {
            return Err(RenderError::SourceFrameMismatch {
                input: index,
                expected: spec.frame_count,
                actual: input.source.frame_count(),
            });
        }
    }

    let mut initial_parameters = Vec::new();
    initial_parameters
        .try_reserve_exact(inputs.len())
        .map_err(|_| RenderError::CapacityUnavailable)?;
    initial_parameters.extend(inputs.iter().map(|input| input.parameters));
    let mut mixer = StereoMixer::try_new(&initial_parameters, master_gain)?;
    let mut left = allocate_silence(spec.frame_count)?;
    let mut right = allocate_silence(spec.frame_count)?;
    let mut block_inputs = Vec::new();
    block_inputs
        .try_reserve_exact(inputs.len())
        .map_err(|_| RenderError::CapacityUnavailable)?;

    for start in (0..spec.frame_count).step_by(spec.block_frames) {
        let end = start
            .saturating_add(spec.block_frames)
            .min(spec.frame_count);
        block_inputs.clear();
        for input in inputs {
            let left_plane = input
                .source
                .plane(0)
                .ok_or(RenderError::InvalidSpec("source has no first plane"))?;
            if input.source.channels() == 1 {
                block_inputs.push(InputBlock::Mono(&left_plane[start..end]));
            } else {
                let right_plane = input
                    .source
                    .plane(1)
                    .ok_or(RenderError::InvalidSpec("stereo source has no right plane"))?;
                block_inputs.push(InputBlock::Stereo {
                    left: &left_plane[start..end],
                    right: &right_plane[start..end],
                });
            }
        }
        mixer.process(
            &block_inputs,
            &initial_parameters,
            master_gain,
            StereoOutput {
                left: &mut left[start..end],
                right: &mut right[start..end],
            },
        )?;
    }

    DecodedAudio::from_stereo(spec.sample_rate_hz, left, right).map_err(RenderError::Output)
}

fn validate_spec(spec: RenderSpec) -> Result<(), RenderError> {
    if spec.sample_rate_hz == 0 {
        return Err(RenderError::InvalidSpec("sample rate must be positive"));
    }
    if spec.frame_count == 0 {
        return Err(RenderError::InvalidSpec("frame count must be positive"));
    }
    if spec.block_frames == 0 || spec.block_frames > MAX_PROCESS_FRAMES {
        return Err(RenderError::InvalidSpec("block size must be in 1..=65536"));
    }
    Ok(())
}

fn allocate_silence(frame_count: usize) -> Result<Vec<f32>, RenderError> {
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(frame_count)
        .map_err(|_| RenderError::CapacityUnavailable)?;
    samples.resize(frame_count, 0.0);
    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_audio_engine::{LinearGain, Pan};

    const EPSILON: f32 = 1.0e-6;

    fn parameters(gain: LinearGain, pan: Pan, audible: bool) -> ChannelParameters {
        ChannelParameters::new(gain, pan, audible)
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
    fn renders_mono_and_stereo_sources_through_the_shared_mixer() {
        let mono = DecodedAudio::from_mono(48_000, vec![1.0, 0.5, -1.0, -0.5]).unwrap();
        let stereo = DecodedAudio::from_stereo(
            48_000,
            vec![0.25, 0.25, 0.25, 0.25],
            vec![-0.25, -0.25, -0.25, -0.25],
        )
        .unwrap();
        let inputs = [
            AlignedInput {
                source: &mono,
                parameters: parameters(LinearGain::UNITY, Pan::HARD_LEFT, true),
            },
            AlignedInput {
                source: &stereo,
                parameters: parameters(LinearGain::UNITY, Pan::CENTER, true),
            },
        ];
        let output = render_aligned(
            &inputs,
            RenderSpec {
                sample_rate_hz: 48_000,
                frame_count: 4,
                block_frames: 3,
            },
            LinearGain::new(0.5).unwrap(),
        )
        .unwrap();

        assert_samples(output.plane(0).unwrap(), &[0.625, 0.375, -0.375, -0.125]);
        assert_samples(output.plane(1).unwrap(), &[-0.125; 4]);
    }

    #[test]
    fn constant_parameter_render_is_block_size_independent() {
        let source = DecodedAudio::from_mono(48_000, vec![0.25, -0.5, 0.75, -1.0, 0.5]).unwrap();
        let inputs = [AlignedInput {
            source: &source,
            parameters: parameters(LinearGain::new(1.5).unwrap(), Pan::CENTER, true),
        }];
        let render = |block_frames| {
            render_aligned(
                &inputs,
                RenderSpec {
                    sample_rate_hz: 48_000,
                    frame_count: 5,
                    block_frames,
                },
                LinearGain::UNITY,
            )
            .unwrap()
        };

        assert_eq!(render(1), render(4));
    }

    #[test]
    fn empty_input_set_renders_bounded_silence() {
        let output = render_aligned(
            &[],
            RenderSpec {
                sample_rate_hz: 48_000,
                frame_count: 4,
                block_frames: 2,
            },
            LinearGain::UNITY,
        )
        .unwrap();
        assert_samples(output.plane(0).unwrap(), &[0.0; 4]);
        assert_samples(output.plane(1).unwrap(), &[0.0; 4]);
    }

    #[test]
    fn rejects_invalid_specs_and_misaligned_sources() {
        let source = DecodedAudio::from_mono(44_100, vec![0.0; 3]).unwrap();
        let input = [AlignedInput {
            source: &source,
            parameters: parameters(LinearGain::UNITY, Pan::CENTER, true),
        }];
        assert!(matches!(
            render_aligned(
                &input,
                RenderSpec {
                    sample_rate_hz: 48_000,
                    frame_count: 3,
                    block_frames: 2,
                },
                LinearGain::UNITY
            ),
            Err(RenderError::SourceRateMismatch { input: 0, .. })
        ));

        let source = DecodedAudio::from_mono(48_000, vec![0.0; 3]).unwrap();
        let input = [AlignedInput {
            source: &source,
            parameters: parameters(LinearGain::UNITY, Pan::CENTER, true),
        }];
        assert!(matches!(
            render_aligned(
                &input,
                RenderSpec {
                    sample_rate_hz: 48_000,
                    frame_count: 4,
                    block_frames: 2,
                },
                LinearGain::UNITY
            ),
            Err(RenderError::SourceFrameMismatch { input: 0, .. })
        ));
        for invalid_spec in [
            RenderSpec {
                sample_rate_hz: 0,
                frame_count: 1,
                block_frames: 1,
            },
            RenderSpec {
                sample_rate_hz: 48_000,
                frame_count: 0,
                block_frames: 1,
            },
            RenderSpec {
                sample_rate_hz: 48_000,
                frame_count: 1,
                block_frames: 0,
            },
            RenderSpec {
                sample_rate_hz: 48_000,
                frame_count: 1,
                block_frames: MAX_PROCESS_FRAMES + 1,
            },
        ] {
            assert!(matches!(
                render_aligned(&[], invalid_spec, LinearGain::UNITY),
                Err(RenderError::InvalidSpec(_))
            ));
        }
    }
}
