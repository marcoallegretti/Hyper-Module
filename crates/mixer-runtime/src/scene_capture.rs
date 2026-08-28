use core::sync::atomic::{AtomicUsize, Ordering, fence};
use std::sync::Arc;

use hyper_playback_compiler::scene::InteractiveSceneLaunch;
use rtrb::{Consumer, Producer, PushError, RingBuffer};

use super::{MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES, TransportCycle};

/// One exact interactive scene launch from a successfully committed callback.
///
/// The launch's timing comes directly from the scene runtime. `loop_enabled`
/// belongs to the same committed transport cycle, allowing a capture consumer
/// to reject timings that cannot be represented by the current arrangement
/// document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractiveSceneLaunchObservation {
    launch: InteractiveSceneLaunch,
    loop_enabled: bool,
}

impl InteractiveSceneLaunchObservation {
    #[must_use]
    pub const fn launch(self) -> InteractiveSceneLaunch {
        self.launch
    }

    #[must_use]
    pub const fn loop_enabled(self) -> bool {
        self.loop_enabled
    }
}

/// Publication result for one successful transport callback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractiveSceneLaunchPublication {
    /// This cycle did not accept a new interactive launch.
    NoLaunch,
    /// The accepted launch was enqueued for the control thread.
    Published,
    /// The fixed queue was full, so capture must be treated as incomplete.
    QueueFull,
    /// The control-thread receiver no longer exists.
    ReceiverDisconnected,
}

/// Audio-thread half of the committed interactive-scene launch channel.
///
/// The queue is allocated before callback processing. Publishing does not
/// allocate, block, or retry: an overflow is preserved as explicit evidence
/// for the consumer, which can then fail capture closed rather than inventing
/// an arrangement from a partial performance.
#[derive(Debug)]
pub struct InteractiveSceneLaunchPublisher {
    producer: Producer<InteractiveSceneLaunchObservation>,
    last_published_sequence: Option<u64>,
    total_dropped_event_count: Arc<AtomicUsize>,
}

impl InteractiveSceneLaunchPublisher {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES
    }

    #[must_use]
    pub fn is_disconnected(&self) -> bool {
        let disconnected = self.producer.is_abandoned();
        if disconnected {
            fence(Ordering::Acquire);
        }
        disconnected
    }

    /// Publishes a launch only when this cycle accepted a new scene boundary.
    ///
    /// Call this only after the output owner has committed the corresponding
    /// callback. Re-observing the same transport cycle is harmless because the
    /// runtime observation sequence is published once at most.
    pub fn publish_cycle(&mut self, cycle: TransportCycle) -> InteractiveSceneLaunchPublication {
        let Some(launch) = cycle.scene_launch_state().last_interactive_launch() else {
            return InteractiveSceneLaunchPublication::NoLaunch;
        };
        if self.last_published_sequence == Some(launch.sequence()) {
            return InteractiveSceneLaunchPublication::NoLaunch;
        }
        self.last_published_sequence = Some(launch.sequence());
        if self.is_disconnected() {
            return InteractiveSceneLaunchPublication::ReceiverDisconnected;
        }
        let observation = InteractiveSceneLaunchObservation {
            launch,
            loop_enabled: cycle.loop_enabled(),
        };
        match self.producer.push(observation) {
            Ok(()) => InteractiveSceneLaunchPublication::Published,
            Err(PushError::Full(_)) => {
                self.increment_dropped_event_count();
                InteractiveSceneLaunchPublication::QueueFull
            }
        }
    }

    fn increment_dropped_event_count(&self) {
        let _ = self.total_dropped_event_count.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |count| Some(count.saturating_add(1)),
        );
    }
}

/// Aggregate evidence from one control-thread launch-channel drain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InteractiveSceneLaunchDrain {
    delivered_event_count: usize,
    dropped_event_count: usize,
    sender_disconnected: bool,
}

impl InteractiveSceneLaunchDrain {
    #[must_use]
    pub const fn delivered_event_count(self) -> usize {
        self.delivered_event_count
    }

    /// Number of queue-rejected launches observed since the prior drain.
    #[must_use]
    pub const fn dropped_event_count(self) -> usize {
        self.dropped_event_count
    }

    #[must_use]
    pub const fn sender_disconnected(self) -> bool {
        self.sender_disconnected
    }
}

/// Control-thread half of the committed interactive-scene launch channel.
#[derive(Debug)]
pub struct InteractiveSceneLaunchReceiver {
    consumer: Consumer<InteractiveSceneLaunchObservation>,
    observed_total_dropped_event_count: usize,
    total_dropped_event_count: Arc<AtomicUsize>,
}

impl InteractiveSceneLaunchReceiver {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES
    }

    /// Drains every currently available launch in FIFO order.
    ///
    /// Queue overflows and a disconnected publisher are surfaced separately;
    /// either condition makes a lossless performance capture incomplete.
    #[must_use]
    pub fn drain(
        &mut self,
        mut observe: impl FnMut(InteractiveSceneLaunchObservation),
    ) -> InteractiveSceneLaunchDrain {
        let mut delivered_event_count = 0_usize;
        for _ in 0..MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES {
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
        InteractiveSceneLaunchDrain {
            delivered_event_count,
            dropped_event_count,
            sender_disconnected,
        }
    }
}

/// Allocates the fixed launch queue before callback processing starts.
#[must_use]
pub fn interactive_scene_launch_channel() -> (
    InteractiveSceneLaunchPublisher,
    InteractiveSceneLaunchReceiver,
) {
    let (producer, consumer) = RingBuffer::new(MAX_QUEUED_INTERACTIVE_SCENE_LAUNCHES);
    let total_dropped_event_count = Arc::new(AtomicUsize::new(0));
    (
        InteractiveSceneLaunchPublisher {
            producer,
            last_published_sequence: None,
            total_dropped_event_count: Arc::clone(&total_dropped_event_count),
        },
        InteractiveSceneLaunchReceiver {
            consumer,
            observed_total_dropped_event_count: 0,
            total_dropped_event_count,
        },
    )
}
