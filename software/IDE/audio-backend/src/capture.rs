use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::{MAXIMUM_BLOCK_FRAMES, STEREO_CHANNELS};

/// Consumes one borrowed interleaved stereo input block.
///
/// Implementations run on the native audio callback thread. They must not
/// allocate, block, retain `input`, or panic.
pub trait InterleavedStereoCaptureProcessor {
    type Error;

    /// Consumes one validated, finite interleaved stereo `f32` block.
    ///
    /// # Errors
    ///
    /// Returns an implementation-defined error when the block cannot be
    /// accepted. Native stream owners treat that failure as terminal.
    ///
    /// # Panics
    ///
    /// Implementations must not panic. Panic hooks and unwinding are not
    /// bounded operations on a real-time callback thread.
    fn process(&mut self, input: &[f32]) -> Result<(), Self::Error>;
}

impl<F, E> InterleavedStereoCaptureProcessor for F
where
    F: FnMut(&[f32]) -> Result<(), E>,
{
    type Error = E;

    fn process(&mut self, input: &[f32]) -> Result<(), Self::Error> {
        self(input)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureCallbackStatus {
    Captured,
    ProcessorFailed,
    SampleCountNotStereoAligned { samples: usize },
    BlockTooLarge { frames: usize, maximum: usize },
    NonFiniteInput,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CaptureStreamSnapshot {
    pub callback_count: u64,
    pub captured_callback_count: u64,
    pub captured_frame_count: u64,
    pub dropped_frame_count: u64,
    pub processor_failure_count: u64,
    pub invalid_shape_count: u64,
    pub oversized_block_count: u64,
    pub non_finite_block_count: u64,
    pub unavailable_buffer_count: u64,
    pub malformed_buffer_count: u64,
    pub discontinuity_count: u64,
    pub timestamp_error_count: u64,
    pub stream_error_count: u64,
}

#[derive(Debug, Default)]
struct CaptureStreamCounterState {
    callbacks: AtomicU64,
    captured_callbacks: AtomicU64,
    captured_frames: AtomicU64,
    dropped_frames: AtomicU64,
    processor_failures: AtomicU64,
    invalid_shapes: AtomicU64,
    oversized_blocks: AtomicU64,
    non_finite_blocks: AtomicU64,
    unavailable_buffers: AtomicU64,
    malformed_buffers: AtomicU64,
    discontinuities: AtomicU64,
    timestamp_errors: AtomicU64,
    stream_errors: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureStreamCounters {
    state: Arc<CaptureStreamCounterState>,
}

impl CaptureStreamCounters {
    #[must_use]
    pub fn snapshot(&self) -> CaptureStreamSnapshot {
        CaptureStreamSnapshot {
            callback_count: self.state.callbacks.load(Ordering::Relaxed),
            captured_callback_count: self.state.captured_callbacks.load(Ordering::Relaxed),
            captured_frame_count: self.state.captured_frames.load(Ordering::Relaxed),
            dropped_frame_count: self.state.dropped_frames.load(Ordering::Relaxed),
            processor_failure_count: self.state.processor_failures.load(Ordering::Relaxed),
            invalid_shape_count: self.state.invalid_shapes.load(Ordering::Relaxed),
            oversized_block_count: self.state.oversized_blocks.load(Ordering::Relaxed),
            non_finite_block_count: self.state.non_finite_blocks.load(Ordering::Relaxed),
            unavailable_buffer_count: self.state.unavailable_buffers.load(Ordering::Relaxed),
            malformed_buffer_count: self.state.malformed_buffers.load(Ordering::Relaxed),
            discontinuity_count: self.state.discontinuities.load(Ordering::Relaxed),
            timestamp_error_count: self.state.timestamp_errors.load(Ordering::Relaxed),
            stream_error_count: self.state.stream_errors.load(Ordering::Relaxed),
        }
    }

    fn add(counter: &AtomicU64, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    pub(crate) fn record_unavailable_buffer(&self) {
        Self::add(&self.state.unavailable_buffers, 1);
    }

    #[cfg(any(
        all(target_os = "linux", target_endian = "little"),
        target_os = "windows"
    ))]
    pub(crate) fn record_malformed_buffer(&self, frames: usize) {
        Self::add(&self.state.callbacks, 1);
        Self::add(&self.state.malformed_buffers, 1);
        Self::add(&self.state.dropped_frames, frames);
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn record_discontinuity(&self) {
        Self::add(&self.state.discontinuities, 1);
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn record_timestamp_error(&self) {
        Self::add(&self.state.timestamp_errors, 1);
    }

    #[cfg(any(
        all(target_os = "linux", target_endian = "little"),
        target_os = "windows"
    ))]
    pub(crate) fn record_stream_error(&self) {
        Self::add(&self.state.stream_errors, 1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureCallbackConfigError {
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
}

impl fmt::Display for CaptureCallbackConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "maximum capture block has {actual} frames; expected 1 through {maximum}"
            ),
        }
    }
}

impl Error for CaptureCallbackConfigError {}

#[derive(Debug)]
pub struct CaptureCallback<P> {
    processor: P,
    maximum_block_frames: usize,
    counters: CaptureStreamCounters,
}

impl<P: InterleavedStereoCaptureProcessor> CaptureCallback<P> {
    /// Creates a validating callback owner for interleaved stereo `f32` input.
    ///
    /// # Errors
    ///
    /// Returns [`CaptureCallbackConfigError`] when the callback capacity is
    /// zero or exceeds [`MAXIMUM_BLOCK_FRAMES`].
    pub fn try_new(
        processor: P,
        maximum_block_frames: usize,
    ) -> Result<Self, CaptureCallbackConfigError> {
        Self::try_new_with_counters(
            processor,
            maximum_block_frames,
            CaptureStreamCounters::default(),
        )
    }

    pub(crate) fn try_new_with_counters(
        processor: P,
        maximum_block_frames: usize,
        counters: CaptureStreamCounters,
    ) -> Result<Self, CaptureCallbackConfigError> {
        if !(1..=MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(CaptureCallbackConfigError::InvalidMaximumBlockFrames {
                actual: maximum_block_frames,
                maximum: MAXIMUM_BLOCK_FRAMES,
            });
        }
        Ok(Self {
            processor,
            maximum_block_frames,
            counters,
        })
    }

    #[must_use]
    pub fn counters(&self) -> CaptureStreamCounters {
        self.counters.clone()
    }

    #[must_use]
    pub const fn maximum_block_frames(&self) -> usize {
        self.maximum_block_frames
    }

    pub fn process(&mut self, input: &[f32]) -> CaptureCallbackStatus {
        let samples = input.len();
        Self::record(&self.counters.state.callbacks, 1);
        if !samples.is_multiple_of(STEREO_CHANNELS) {
            return self.reject(
                &self.counters.state.invalid_shapes,
                0,
                CaptureCallbackStatus::SampleCountNotStereoAligned { samples },
            );
        }

        let frames = samples / STEREO_CHANNELS;
        if frames > self.maximum_block_frames {
            return self.reject(
                &self.counters.state.oversized_blocks,
                frames,
                CaptureCallbackStatus::BlockTooLarge {
                    frames,
                    maximum: self.maximum_block_frames,
                },
            );
        }
        if input.iter().any(|sample| !sample.is_finite()) {
            return self.reject(
                &self.counters.state.non_finite_blocks,
                frames,
                CaptureCallbackStatus::NonFiniteInput,
            );
        }

        if self.processor.process(input).is_ok() {
            Self::record(&self.counters.state.captured_callbacks, 1);
            Self::record(&self.counters.state.captured_frames, frames);
            CaptureCallbackStatus::Captured
        } else {
            Self::record(&self.counters.state.processor_failures, 1);
            Self::record(&self.counters.state.dropped_frames, frames);
            CaptureCallbackStatus::ProcessorFailed
        }
    }

    fn reject(
        &self,
        outcome: &AtomicU64,
        frames: usize,
        status: CaptureCallbackStatus,
    ) -> CaptureCallbackStatus {
        Self::record(outcome, 1);
        Self::record(&self.counters.state.dropped_frames, frames);
        status
    }

    fn record(counter: &AtomicU64, amount: usize) {
        CaptureStreamCounters::add(counter, amount);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_validates_input_and_counts_every_outcome() {
        let call = 0_u8;
        let mut callback = CaptureCallback::try_new(
            {
                let mut call = call;
                move |_: &[f32]| -> Result<(), &'static str> {
                    call += 1;
                    if call == 1 { Ok(()) } else { Err("failure") }
                }
            },
            2,
        )
        .unwrap();
        let counters = callback.counters();

        assert_eq!(
            callback.process(&[0.25, -0.25, 0.5, -0.5]),
            CaptureCallbackStatus::Captured
        );
        assert_eq!(
            callback.process(&[0.25, -0.25]),
            CaptureCallbackStatus::ProcessorFailed
        );
        assert_eq!(
            callback.process(&[0.0; 3]),
            CaptureCallbackStatus::SampleCountNotStereoAligned { samples: 3 }
        );
        assert_eq!(
            callback.process(&[0.0; 6]),
            CaptureCallbackStatus::BlockTooLarge {
                frames: 3,
                maximum: 2,
            }
        );
        assert_eq!(
            callback.process(&[f32::NAN, 0.0]),
            CaptureCallbackStatus::NonFiniteInput
        );

        assert_eq!(
            counters.snapshot(),
            CaptureStreamSnapshot {
                callback_count: 5,
                captured_callback_count: 1,
                captured_frame_count: 2,
                dropped_frame_count: 5,
                processor_failure_count: 1,
                invalid_shape_count: 1,
                oversized_block_count: 1,
                non_finite_block_count: 1,
                ..CaptureStreamSnapshot::default()
            }
        );
    }

    #[test]
    fn callback_capacity_is_strict() {
        let processor = |_: &[f32]| Ok::<_, ()>(());
        assert!(matches!(
            CaptureCallback::try_new(processor, 0),
            Err(CaptureCallbackConfigError::InvalidMaximumBlockFrames {
                actual: 0,
                maximum: MAXIMUM_BLOCK_FRAMES,
            })
        ));
        let processor = |_: &[f32]| Ok::<_, ()>(());
        assert!(matches!(
            CaptureCallback::try_new(processor, MAXIMUM_BLOCK_FRAMES + 1),
            Err(CaptureCallbackConfigError::InvalidMaximumBlockFrames {
                actual,
                maximum: MAXIMUM_BLOCK_FRAMES,
            }) if actual == MAXIMUM_BLOCK_FRAMES + 1
        ));
    }

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    #[test]
    fn backend_telemetry_is_explicit_and_saturating() {
        let counters = CaptureStreamCounters::default();
        counters.record_unavailable_buffer();
        counters.record_malformed_buffer(17);
        counters.record_stream_error();

        assert_eq!(
            counters.snapshot(),
            CaptureStreamSnapshot {
                callback_count: 1,
                dropped_frame_count: 17,
                unavailable_buffer_count: 1,
                malformed_buffer_count: 1,
                stream_error_count: 1,
                ..CaptureStreamSnapshot::default()
            }
        );
    }
}
