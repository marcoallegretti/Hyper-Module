use core::sync::atomic::{AtomicUsize, Ordering, fence};
use std::sync::Arc;

use hyper_playback_json::{AutomationTarget, AutomationValue};
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use super::{InterleavedTransportCycle, MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS};

/// One live mixer parameter accepted by a successfully committed audio callback.
///
/// The observation is quantized to the version-1 automation value domain at
/// the callback boundary. `start_frame`, `transport_rolling`, and
/// `loop_enabled` belong to that same callback, allowing the control thread to
/// preserve timing and exclude changes committed before capture transport
/// actually starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractiveMixerParameterObservation {
    start_frame: usize,
    transport_rolling: bool,
    loop_enabled: bool,
    target: AutomationTarget,
    value: AutomationValue,
}

impl InteractiveMixerParameterObservation {
    #[must_use]
    pub const fn start_frame(self) -> usize {
        self.start_frame
    }

    /// Whether the callback that committed this change was rolling.
    #[must_use]
    pub const fn transport_rolling(self) -> bool {
        self.transport_rolling
    }

    #[must_use]
    pub const fn loop_enabled(self) -> bool {
        self.loop_enabled
    }

    #[must_use]
    pub const fn target(self) -> AutomationTarget {
        self.target
    }

    #[must_use]
    pub const fn value(self) -> AutomationValue {
        self.value
    }
}

/// Publication result for one successful transport callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractiveMixerParameterPublication {
    /// This callback applied no live mixer parameters.
    NoChanges,
    /// Every applied parameter was enqueued for the control thread.
    Published,
    /// The fixed queue was full, so capture must be treated as incomplete.
    QueueFull,
    /// The control-thread receiver no longer exists.
    ReceiverDisconnected,
}

/// Audio-thread half of the committed interactive-mixer parameter channel.
///
/// The queue is allocated before callback processing. Publishing does not
/// allocate, block, or retry. An overflow is recorded as evidence for the
/// control thread, which can fail a capture closed instead of persisting an
/// incomplete automation take.
#[derive(Debug)]
pub struct InteractiveMixerParameterPublisher {
    producer: Producer<InteractiveMixerParameterObservation>,
    last_published_sequence: Option<u64>,
    total_dropped_event_count: Arc<AtomicUsize>,
}

impl InteractiveMixerParameterPublisher {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Publishes every live mixer parameter accepted by this callback.
    ///
    /// Call this only after the output owner has committed the corresponding
    /// callback. The cycle carries no changes after an unsuccessful callback,
    /// so failed audio work cannot create automation evidence.
    ///
    /// # Panics
    ///
    /// Panics if a non-empty committed parameter batch has no committed
    /// sequence. That would violate the runtime's callback-commit invariant
    /// and cannot be represented safely as capture evidence.
    pub fn publish_cycle(
        &mut self,
        cycle: &InterleavedTransportCycle,
    ) -> InteractiveMixerParameterPublication {
        let changes = cycle.committed_mixer_parameter_changes();
        if changes.is_empty() {
            return InteractiveMixerParameterPublication::NoChanges;
        }
        let sequence = changes
            .sequence()
            .expect("every committed mixer parameter batch has a sequence");
        if self.last_published_sequence == Some(sequence) {
            return InteractiveMixerParameterPublication::NoChanges;
        }
        self.last_published_sequence = Some(sequence);
        if self.is_disconnected() {
            return InteractiveMixerParameterPublication::ReceiverDisconnected;
        }
        let transport = cycle.snapshot().cycle();
        for (index, change) in changes.iter().enumerate() {
            let observation = InteractiveMixerParameterObservation {
                start_frame: transport.start_frame(),
                transport_rolling: transport.state() == super::TransportState::Rolling,
                loop_enabled: transport.loop_enabled(),
                target: change.target,
                value: change.value,
            };
            if let Err(PushError::Full(_)) = self.producer.push(observation) {
                self.increment_dropped_event_count(changes.len().saturating_sub(index));
                return InteractiveMixerParameterPublication::QueueFull;
            }
        }
        InteractiveMixerParameterPublication::Published
    }

    fn increment_dropped_event_count(&self, dropped: usize) {
        let _ = self.total_dropped_event_count.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |count| Some(count.saturating_add(dropped)),
        );
    }
}

/// Aggregate evidence from one control-thread mixer-parameter drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractiveMixerParameterDrain {
    delivered_event_count: usize,
    dropped_event_count: usize,
    sender_disconnected: bool,
}

impl InteractiveMixerParameterDrain {
    #[must_use]
    pub const fn delivered_event_count(self) -> usize {
        self.delivered_event_count
    }

    /// Number of queue-rejected parameters observed since the prior drain.
    #[must_use]
    pub const fn dropped_event_count(self) -> usize {
        self.dropped_event_count
    }

    #[must_use]
    pub const fn sender_disconnected(self) -> bool {
        self.sender_disconnected
    }
}

/// Control-thread half of the committed interactive-mixer parameter channel.
#[derive(Debug)]
pub struct InteractiveMixerParameterReceiver {
    consumer: Consumer<InteractiveMixerParameterObservation>,
    observed_total_dropped_event_count: usize,
    total_dropped_event_count: Arc<AtomicUsize>,
}

impl InteractiveMixerParameterReceiver {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS
    }

    /// Drains every currently available parameter in FIFO order.
    ///
    /// Queue overflows and a disconnected publisher are surfaced separately;
    /// either condition makes a lossless automation capture incomplete.
    #[must_use]
    pub fn drain(
        &mut self,
        mut observe: impl FnMut(InteractiveMixerParameterObservation),
    ) -> InteractiveMixerParameterDrain {
        let mut delivered_event_count = 0_usize;
        for _ in 0..MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS {
            let Ok(observation) = self.consumer.pop() else {
                break;
            };
            observe(observation);
            delivered_event_count = delivered_event_count.saturating_add(1);
        }
        let sender_disconnected = self.consumer.is_abandoned();
        if sender_disconnected {
            fence(Ordering::Acquire);
        }
        let total_dropped_event_count = self.total_dropped_event_count.load(Ordering::Acquire);
        let dropped_event_count =
            total_dropped_event_count.saturating_sub(self.observed_total_dropped_event_count);
        self.observed_total_dropped_event_count = total_dropped_event_count;
        InteractiveMixerParameterDrain {
            delivered_event_count,
            dropped_event_count,
            sender_disconnected,
        }
    }
}

/// Allocates the fixed mixer-parameter capture queue before callback processing starts.
#[must_use]
pub fn interactive_mixer_parameter_channel() -> (
    InteractiveMixerParameterPublisher,
    InteractiveMixerParameterReceiver,
) {
    let (producer, consumer) = RingBuffer::new(MAX_QUEUED_INTERACTIVE_MIXER_PARAMETER_OBSERVATIONS);
    let total_dropped_event_count = Arc::new(AtomicUsize::new(0));
    (
        InteractiveMixerParameterPublisher {
            producer,
            last_published_sequence: None,
            total_dropped_event_count: Arc::clone(&total_dropped_event_count),
        },
        InteractiveMixerParameterReceiver {
            consumer,
            observed_total_dropped_event_count: 0,
            total_dropped_event_count,
        },
    )
}
