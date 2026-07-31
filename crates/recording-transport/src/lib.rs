#![doc = "Bounded real-time transfer for interleaved stereo recording data."]

use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use std::collections::TryReserveError;
use std::sync::{Arc, OnceLock};
use std::thread::Thread;

pub const RECORDING_CHANNELS: usize = 2;

#[derive(Debug)]
pub enum RecordingTransportBuildError {
    ZeroCapacity,
    SampleCapacityOverflow,
    Allocation(TryReserveError),
}

impl fmt::Display for RecordingTransportBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroCapacity => {
                formatter.write_str("recording transport capacity must be nonzero")
            }
            Self::SampleCapacityOverflow => {
                formatter.write_str("recording transport sample capacity overflowed")
            }
            Self::Allocation(_) => {
                formatter.write_str("recording transport capacity is unavailable")
            }
        }
    }
}

impl Error for RecordingTransportBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Allocation(source) => Some(source),
            Self::ZeroCapacity | Self::SampleCapacityOverflow => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingPushError {
    SampleCountNotStereoAligned {
        samples: usize,
    },
    NonFiniteInput,
    BlockExceedsCapacity {
        frames: usize,
        capacity_frames: usize,
    },
    Saturated {
        frames: usize,
        available_frames: usize,
    },
    ConsumerClosed {
        frames: usize,
    },
}

impl fmt::Display for RecordingPushError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleCountNotStereoAligned { samples } => write!(
                formatter,
                "recording block has {samples} samples; expected interleaved stereo"
            ),
            Self::NonFiniteInput => {
                formatter.write_str("recording block contains a non-finite sample")
            }
            Self::BlockExceedsCapacity {
                frames,
                capacity_frames,
            } => write!(
                formatter,
                "recording block has {frames} frames; transport capacity is {capacity_frames}"
            ),
            Self::Saturated {
                frames,
                available_frames,
            } => write!(
                formatter,
                "recording transport cannot accept {frames} frames; {available_frames} are free"
            ),
            Self::ConsumerClosed { frames } => write!(
                formatter,
                "recording consumer closed before accepting {frames} frames"
            ),
        }
    }
}

impl Error for RecordingPushError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDrainError {
    EmptyOutput,
    SampleCountNotStereoAligned { samples: usize },
}

impl fmt::Display for RecordingDrainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyOutput => formatter.write_str("recording drain output must not be empty"),
            Self::SampleCountNotStereoAligned { samples } => write!(
                formatter,
                "recording drain output has {samples} samples; expected interleaved stereo"
            ),
        }
    }
}

impl Error for RecordingDrainError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingWakeRegistrationError {
    AlreadyRegistered,
}

impl fmt::Display for RecordingWakeRegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a recording consumer wake thread is already registered")
    }
}

impl Error for RecordingWakeRegistrationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordingDrain {
    pub frames: usize,
    pub producer_closed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordingTransportSnapshot {
    pub push_attempt_count: u64,
    pub accepted_block_count: u64,
    pub accepted_frame_count: u64,
    pub rejected_block_count: u64,
    pub rejected_frame_count: u64,
    pub drained_frame_count: u64,
}

#[derive(Debug, Default)]
struct RecordingTransportState {
    push_attempts: AtomicU64,
    accepted_blocks: AtomicU64,
    accepted_frames: AtomicU64,
    rejected_blocks: AtomicU64,
    rejected_frames: AtomicU64,
    drained_frames: AtomicU64,
    wake_thread: OnceLock<Thread>,
}

#[derive(Clone, Debug, Default)]
pub struct RecordingTransportCounters {
    state: Arc<RecordingTransportState>,
}

impl RecordingTransportCounters {
    #[must_use]
    pub fn snapshot(&self) -> RecordingTransportSnapshot {
        RecordingTransportSnapshot {
            push_attempt_count: self.state.push_attempts.load(Ordering::Relaxed),
            accepted_block_count: self.state.accepted_blocks.load(Ordering::Relaxed),
            accepted_frame_count: self.state.accepted_frames.load(Ordering::Relaxed),
            rejected_block_count: self.state.rejected_blocks.load(Ordering::Relaxed),
            rejected_frame_count: self.state.rejected_frames.load(Ordering::Relaxed),
            drained_frame_count: self.state.drained_frames.load(Ordering::Relaxed),
        }
    }

    fn add(counter: &AtomicU64, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }
}

pub struct RecordingProducer {
    inner: HeapProd<f32>,
    capacity_frames: usize,
    counters: RecordingTransportCounters,
}

impl fmt::Debug for RecordingProducer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingProducer")
            .field("capacity_frames", &self.capacity_frames)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl RecordingProducer {
    /// Atomically transfers one complete stereo block.
    ///
    /// Saturation rejects the entire block; no partial block is published.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingPushError`] for malformed samples, saturation, or a
    /// closed consumer.
    pub fn push(&mut self, samples: &[f32]) -> Result<(), RecordingPushError> {
        RecordingTransportCounters::add(&self.counters.state.push_attempts, 1);
        let frames = samples.len() / RECORDING_CHANNELS;
        if !samples.len().is_multiple_of(RECORDING_CHANNELS) {
            return self.reject(
                frames,
                RecordingPushError::SampleCountNotStereoAligned {
                    samples: samples.len(),
                },
            );
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            return self.reject(frames, RecordingPushError::NonFiniteInput);
        }
        if frames > self.capacity_frames {
            return self.reject(
                frames,
                RecordingPushError::BlockExceedsCapacity {
                    frames,
                    capacity_frames: self.capacity_frames,
                },
            );
        }
        if !self.inner.read_is_held() {
            return self.reject(frames, RecordingPushError::ConsumerClosed { frames });
        }
        let available_samples = self.inner.vacant_len();
        if available_samples < samples.len() {
            return self.reject(
                frames,
                RecordingPushError::Saturated {
                    frames,
                    available_frames: available_samples / RECORDING_CHANNELS,
                },
            );
        }
        let pushed = self.inner.push_slice(samples);
        debug_assert_eq!(pushed, samples.len());
        RecordingTransportCounters::add(&self.counters.state.accepted_blocks, 1);
        RecordingTransportCounters::add(&self.counters.state.accepted_frames, frames);
        self.counters.state.wake_writer();
        Ok(())
    }

    #[must_use]
    pub const fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    #[must_use]
    pub fn counters(&self) -> RecordingTransportCounters {
        self.counters.clone()
    }

    fn reject(&self, frames: usize, error: RecordingPushError) -> Result<(), RecordingPushError> {
        RecordingTransportCounters::add(&self.counters.state.rejected_blocks, 1);
        RecordingTransportCounters::add(&self.counters.state.rejected_frames, frames);
        Err(error)
    }
}

pub struct RecordingConsumer {
    inner: HeapCons<f32>,
    capacity_frames: usize,
    counters: RecordingTransportCounters,
}

impl fmt::Debug for RecordingConsumer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordingConsumer")
            .field("capacity_frames", &self.capacity_frames)
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

impl RecordingConsumer {
    /// Registers the current non-real-time writer thread for producer wakeups.
    ///
    /// Registration is permanent for this transport. A successful producer
    /// push unparks this thread after publishing the complete block.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingWakeRegistrationError::AlreadyRegistered`] if any
    /// thread was registered previously.
    pub fn register_current_thread_waker(&self) -> Result<(), RecordingWakeRegistrationError> {
        self.register_thread_waker(std::thread::current())
    }

    /// Registers a non-real-time writer thread for producer wakeups.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingWakeRegistrationError::AlreadyRegistered`] if any
    /// thread was registered previously.
    pub fn register_thread_waker(
        &self,
        thread: Thread,
    ) -> Result<(), RecordingWakeRegistrationError> {
        self.counters
            .state
            .wake_thread
            .set(thread)
            .map_err(|_| RecordingWakeRegistrationError::AlreadyRegistered)
    }

    /// Drains complete stereo frames without blocking.
    ///
    /// # Errors
    ///
    /// Returns [`RecordingDrainError`] if `output` cannot contain complete
    /// stereo frames.
    pub fn drain(&mut self, output: &mut [f32]) -> Result<RecordingDrain, RecordingDrainError> {
        if output.is_empty() {
            return Err(RecordingDrainError::EmptyOutput);
        }
        if !output.len().is_multiple_of(RECORDING_CHANNELS) {
            return Err(RecordingDrainError::SampleCountNotStereoAligned {
                samples: output.len(),
            });
        }
        // Observe producer closure before sampling the final write index. When
        // closure is visible, its acquire ordering makes every preceding push
        // visible to the occupancy read below.
        let producer_closed = !self.inner.write_is_held();
        let available_samples = self.inner.occupied_len();
        let sample_count = available_samples.min(output.len());
        debug_assert_eq!(sample_count % RECORDING_CHANNELS, 0);
        let popped = self.inner.pop_slice(&mut output[..sample_count]);
        debug_assert_eq!(popped, sample_count);
        let frames = popped / RECORDING_CHANNELS;
        RecordingTransportCounters::add(&self.counters.state.drained_frames, frames);
        Ok(RecordingDrain {
            frames,
            producer_closed,
        })
    }

    #[must_use]
    pub const fn capacity_frames(&self) -> usize {
        self.capacity_frames
    }

    #[must_use]
    pub fn counters(&self) -> RecordingTransportCounters {
        self.counters.clone()
    }
}

impl RecordingTransportState {
    fn wake_writer(&self) {
        if let Some(thread) = self.wake_thread.get() {
            thread.unpark();
        }
    }
}

/// Allocates one fixed-capacity stereo SPSC transport.
///
/// # Errors
///
/// Returns [`RecordingTransportBuildError`] for a zero or overflowing capacity
/// or if the fixed backing allocation is unavailable.
pub fn try_recording_transport(
    capacity_frames: usize,
) -> Result<(RecordingProducer, RecordingConsumer), RecordingTransportBuildError> {
    if capacity_frames == 0 {
        return Err(RecordingTransportBuildError::ZeroCapacity);
    }
    let sample_capacity = capacity_frames
        .checked_mul(RECORDING_CHANNELS)
        .ok_or(RecordingTransportBuildError::SampleCapacityOverflow)?;
    let ring = HeapRb::<f32>::try_new(sample_capacity)
        .map_err(RecordingTransportBuildError::Allocation)?;
    let (producer, consumer) = ring.split();
    let counters = RecordingTransportCounters::default();
    Ok((
        RecordingProducer {
            inner: producer,
            capacity_frames,
            counters: counters.clone(),
        },
        RecordingConsumer {
            inner: consumer,
            capacity_frames,
            counters,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_samples<const N: usize>(actual: [f32; N], expected: [f32; N]) {
        assert_eq!(actual.map(f32::to_bits), expected.map(f32::to_bits));
    }

    #[test]
    fn construction_rejects_invalid_capacities() {
        assert!(matches!(
            try_recording_transport(0),
            Err(RecordingTransportBuildError::ZeroCapacity)
        ));
        assert!(matches!(
            try_recording_transport(usize::MAX),
            Err(RecordingTransportBuildError::SampleCapacityOverflow)
        ));
    }

    #[test]
    fn complete_blocks_wrap_and_drain_in_order() {
        let (mut producer, mut consumer) = try_recording_transport(3).unwrap();
        producer.push(&[0.0, 1.0, 2.0, 3.0]).unwrap();
        let mut first = [0.0; 2];
        assert_eq!(
            consumer.drain(&mut first).unwrap(),
            RecordingDrain {
                frames: 1,
                producer_closed: false,
            }
        );
        producer.push(&[4.0, 5.0]).unwrap();
        let mut rest = [0.0; 4];
        assert_eq!(consumer.drain(&mut rest).unwrap().frames, 2);
        assert_samples(first, [0.0, 1.0]);
        assert_samples(rest, [2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn saturation_rejects_the_whole_block_and_counts_loss() {
        let (mut producer, mut consumer) = try_recording_transport(2).unwrap();
        producer.push(&[0.0, 1.0]).unwrap();
        assert_eq!(
            producer.push(&[2.0, 3.0, 4.0, 5.0]),
            Err(RecordingPushError::Saturated {
                frames: 2,
                available_frames: 1,
            })
        );
        let mut output = [9.0; 4];
        let drained = consumer.drain(&mut output).unwrap();
        assert_eq!(drained.frames, 1);
        assert_samples(output, [0.0, 1.0, 9.0, 9.0]);
        assert_eq!(
            producer.counters().snapshot(),
            RecordingTransportSnapshot {
                push_attempt_count: 2,
                accepted_block_count: 1,
                accepted_frame_count: 1,
                rejected_block_count: 1,
                rejected_frame_count: 2,
                drained_frame_count: 1,
            }
        );
    }

    #[test]
    fn malformed_and_closed_paths_are_explicit() {
        let (mut producer, consumer) = try_recording_transport(2).unwrap();
        assert_eq!(
            producer.push(&[0.0, 1.0, 2.0]),
            Err(RecordingPushError::SampleCountNotStereoAligned { samples: 3 })
        );
        assert_eq!(
            producer.push(&[f32::NAN, 0.0]),
            Err(RecordingPushError::NonFiniteInput)
        );
        assert_eq!(
            producer.push(&[0.0; 6]),
            Err(RecordingPushError::BlockExceedsCapacity {
                frames: 3,
                capacity_frames: 2,
            })
        );
        drop(consumer);
        assert_eq!(
            producer.push(&[0.0, 0.0]),
            Err(RecordingPushError::ConsumerClosed { frames: 1 })
        );
    }

    #[test]
    fn producer_close_is_observed_after_the_final_frames() {
        let (mut producer, mut consumer) = try_recording_transport(2).unwrap();
        producer.push(&[0.0, 1.0]).unwrap();
        drop(producer);
        let mut output = [0.0; 4];
        assert_eq!(
            consumer.drain(&mut output).unwrap(),
            RecordingDrain {
                frames: 1,
                producer_closed: true,
            }
        );
        assert_eq!(
            consumer.drain(&mut output).unwrap(),
            RecordingDrain {
                frames: 0,
                producer_closed: true,
            }
        );
    }

    #[test]
    fn drain_rejects_malformed_output_without_consuming() {
        let (mut producer, mut consumer) = try_recording_transport(1).unwrap();
        producer.push(&[0.0, 1.0]).unwrap();
        assert_eq!(
            consumer.drain(&mut []),
            Err(RecordingDrainError::EmptyOutput)
        );
        assert_eq!(
            consumer.drain(&mut [0.0; 3]),
            Err(RecordingDrainError::SampleCountNotStereoAligned { samples: 3 })
        );
        let mut output = [0.0; 2];
        assert_eq!(consumer.drain(&mut output).unwrap().frames, 1);
    }

    #[test]
    fn accepted_blocks_wake_the_registered_consumer_thread() {
        use std::sync::mpsc::sync_channel;
        use std::time::Duration;

        let (mut producer, consumer) = try_recording_transport(2).unwrap();
        let (ready_sender, ready_receiver) = sync_channel(1);
        let (samples_sender, samples_receiver) = sync_channel(1);
        let writer = std::thread::spawn(move || {
            consumer.register_current_thread_waker().unwrap();
            ready_sender.send(()).unwrap();
            std::thread::park();
            let mut consumer = consumer;
            let mut output = [0.0_f32; 4];
            let drained = consumer.drain(&mut output).unwrap();
            samples_sender
                .send((drained, output.map(f32::to_bits)))
                .unwrap();
        });

        ready_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        producer.push(&[0.25, -0.25]).unwrap();
        let (drained, samples) = samples_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(drained.frames, 1);
        assert_eq!(samples[..2], [0.25_f32.to_bits(), (-0.25_f32).to_bits()]);
        writer.join().unwrap();
    }

    #[test]
    fn consumer_wake_thread_can_only_be_registered_once() {
        let (_producer, consumer) = try_recording_transport(1).unwrap();
        consumer.register_current_thread_waker().unwrap();
        assert_eq!(
            consumer.register_current_thread_waker(),
            Err(RecordingWakeRegistrationError::AlreadyRegistered)
        );
    }
}
