use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};
use std::sync::Arc;

use hyper_mixer_runtime::{
    StereoSamplePeakDisplayBallistics, StereoSamplePeakDisplayBallisticsSnapshot,
};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use super::STEREO_CHANNELS;

/// Maximum number of accepted-input meter blocks waiting for a consumer.
///
/// This is display telemetry only. Queue saturation never rejects capture,
/// recording, or monitoring work.
pub const MAX_QUEUED_INPUT_METER_SNAPSHOTS: usize = 64;

static NEXT_INPUT_METER_CHANNEL_ID: AtomicU64 = AtomicU64::new(1);

fn next_input_meter_channel_id() -> u64 {
    NEXT_INPUT_METER_CHANNEL_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("input meter channel identity space exhausted")
}

/// Immutable sample-peak evidence from one accepted desktop input block.
///
/// The stereo values are maximum absolute `f32` samples from the block after
/// capture validation and before cue gain, mute, buffering, resampling, or
/// playback mixing. They are neither hardware-calibrated level, true peak,
/// loudness, nor evidence that a recording file was finalized.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputMeterSnapshot {
    accepted_frame_end: u64,
    frame_count: usize,
    left_sample_peak: f32,
    right_sample_peak: f32,
    dropped_before: usize,
}

impl InputMeterSnapshot {
    /// Returns the monotonic accepted-input endpoint after this block.
    #[must_use]
    pub const fn accepted_frame_end(self) -> u64 {
        self.accepted_frame_end
    }

    /// Returns the number of stereo frames measured in this block.
    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.frame_count
    }

    /// Returns the maximum absolute left sample in this block.
    #[must_use]
    pub const fn left_sample_peak(self) -> f32 {
        self.left_sample_peak
    }

    /// Returns the maximum absolute right sample in this block.
    #[must_use]
    pub const fn right_sample_peak(self) -> f32 {
        self.right_sample_peak
    }

    /// Returns meter snapshots rejected before this one because the display
    /// queue was full.
    #[must_use]
    pub const fn dropped_before(self) -> usize {
        self.dropped_before
    }
}

/// Result of a nonblocking accepted-input meter publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputMeterPublication {
    NoFrames,
    Published,
    QueueFull,
    ReceiverDisconnected,
}

/// Test-only equivalent of one accepted-input meter publication result.
///
/// This type is unavailable in production builds, where only a validated
/// capture callback can publish accepted-input meter evidence.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMeterTestPublication {
    NoFrames,
    Published,
    QueueFull,
    ReceiverDisconnected,
}

#[cfg(feature = "test-support")]
impl From<InputMeterPublication> for InputMeterTestPublication {
    fn from(value: InputMeterPublication) -> Self {
        match value {
            InputMeterPublication::NoFrames => Self::NoFrames,
            InputMeterPublication::Published => Self::Published,
            InputMeterPublication::QueueFull => Self::QueueFull,
            InputMeterPublication::ReceiverDisconnected => Self::ReceiverDisconnected,
        }
    }
}

/// Audio-thread half of the accepted-input meter transport.
pub(crate) struct InputMeterSender {
    producer: HeapProd<InputMeterSnapshot>,
    accepted_frame_end: u64,
    dropped_snapshot_count: usize,
    pending_dropped_snapshot_count: Arc<AtomicUsize>,
}

/// Test-only sender for deterministic application consumers of the meter.
///
/// It exists solely behind the `test-support` feature so dependent tests can
/// verify presentation lifecycle behavior without a native capture device.
#[cfg(feature = "test-support")]
#[derive(Debug)]
pub struct InputMeterTestPublisher {
    sender: InputMeterSender,
}

#[cfg(feature = "test-support")]
impl InputMeterTestPublisher {
    /// Publishes one synthetic already-accepted stereo block for a test.
    #[must_use]
    pub fn publish(&mut self, samples: &[f32]) -> InputMeterTestPublication {
        self.sender.publish(samples).into()
    }
}

impl fmt::Debug for InputMeterSender {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputMeterSender")
            .field("accepted_frame_end", &self.accepted_frame_end)
            .field("dropped_snapshot_count", &self.dropped_snapshot_count)
            .finish_non_exhaustive()
    }
}

impl InputMeterSender {
    /// Measures and attempts to publish one already validated stereo input
    /// block without waiting or allocating.
    pub(crate) fn publish(&mut self, samples: &[f32]) -> InputMeterPublication {
        debug_assert!(samples.len().is_multiple_of(STEREO_CHANNELS));
        let frame_count = samples.len() / STEREO_CHANNELS;
        if frame_count == 0 {
            return InputMeterPublication::NoFrames;
        }
        self.accepted_frame_end = self
            .accepted_frame_end
            .saturating_add(u64::try_from(frame_count).unwrap_or(u64::MAX));
        let (left_sample_peak, right_sample_peak) = stereo_sample_peaks(samples);
        let snapshot = InputMeterSnapshot {
            accepted_frame_end: self.accepted_frame_end,
            frame_count,
            left_sample_peak,
            right_sample_peak,
            dropped_before: self.dropped_snapshot_count,
        };
        if !self.producer.read_is_held() {
            return InputMeterPublication::ReceiverDisconnected;
        }
        if let Ok(()) = self.producer.try_push(snapshot) {
            if self.dropped_snapshot_count != 0 {
                self.dropped_snapshot_count = 0;
                self.pending_dropped_snapshot_count
                    .store(0, Ordering::Release);
            }
            InputMeterPublication::Published
        } else {
            self.dropped_snapshot_count = self.dropped_snapshot_count.saturating_add(1);
            self.pending_dropped_snapshot_count
                .store(self.dropped_snapshot_count, Ordering::Release);
            InputMeterPublication::QueueFull
        }
    }
}

fn stereo_sample_peaks(samples: &[f32]) -> (f32, f32) {
    let mut left = 0.0_f32;
    let mut right = 0.0_f32;
    for frame in samples.chunks_exact(STEREO_CHANNELS) {
        left = left.max(frame[0].abs());
        right = right.max(frame[1].abs());
    }
    (left, right)
}

/// Result of draining accepted-input meter evidence to its newest snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LatestInputMeterSnapshot {
    snapshot: Option<InputMeterSnapshot>,
    coalesced_snapshot_count: usize,
    dropped_snapshot_count: usize,
    sender_disconnected: bool,
}

impl LatestInputMeterSnapshot {
    /// Returns an empty observation before the first capture block arrives.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            snapshot: None,
            coalesced_snapshot_count: 0,
            dropped_snapshot_count: 0,
            sender_disconnected: false,
        }
    }

    #[must_use]
    pub const fn snapshot(self) -> Option<InputMeterSnapshot> {
        self.snapshot
    }

    /// Returns delivered snapshots superseded during this drain.
    #[must_use]
    pub const fn coalesced_snapshot_count(self) -> usize {
        self.coalesced_snapshot_count
    }

    /// Returns snapshots rejected before delivery because the display queue was
    /// full, including a pending count observed after sender teardown.
    #[must_use]
    pub const fn dropped_snapshot_count(self) -> usize {
        self.dropped_snapshot_count
    }

    /// Returns whether the accepted-input producer has ended.
    #[must_use]
    pub const fn sender_disconnected(self) -> bool {
        self.sender_disconnected
    }
}

impl Default for LatestInputMeterSnapshot {
    fn default() -> Self {
        Self::empty()
    }
}

/// Result of one input-meter drain with display-only sample-peak ballistics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputMeterDisplayRead {
    latest: LatestInputMeterSnapshot,
    display: Option<StereoSamplePeakDisplayBallisticsSnapshot>,
    display_reset: bool,
}

impl InputMeterDisplayRead {
    #[must_use]
    pub const fn latest(self) -> LatestInputMeterSnapshot {
        self.latest
    }

    /// Returns the newest display update delivered during this drain.
    #[must_use]
    pub const fn display(self) -> Option<StereoSamplePeakDisplayBallisticsSnapshot> {
        self.display
    }

    /// Returns whether a source identity or frame-clock boundary reset the
    /// display before a delivered snapshot was applied.
    #[must_use]
    pub const fn display_reset(self) -> bool {
        self.display_reset
    }
}

/// Control-thread half of the accepted-input meter transport.
pub struct InputMeterReceiver {
    consumer: HeapCons<InputMeterSnapshot>,
    channel_id: u64,
    pending_dropped_snapshot_count: Arc<AtomicUsize>,
}

impl fmt::Debug for InputMeterReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputMeterReceiver")
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

impl InputMeterReceiver {
    #[must_use]
    pub const fn capacity(&self) -> usize {
        MAX_QUEUED_INPUT_METER_SNAPSHOTS
    }

    /// Drains delivered snapshots and returns only the newest raw evidence.
    ///
    /// Coalesced snapshots were delivered but superseded during this call.
    /// Queue-rejected snapshots remain explicit through the dropped count.
    #[must_use]
    pub fn read_latest(&mut self) -> LatestInputMeterSnapshot {
        self.drain_with(|_| {})
    }

    /// Drains every delivered snapshot through display-only frame-time
    /// ballistics. The source identity remains private to this receiver, so a
    /// new input session cannot inherit a prior session's visual history.
    #[must_use]
    pub fn read_display(
        &mut self,
        display: &mut StereoSamplePeakDisplayBallistics,
    ) -> InputMeterDisplayRead {
        let channel_id = self.channel_id;
        let mut latest_display = None;
        let mut display_reset = false;
        let latest = self.drain_with(|snapshot| {
            let update = display.update(
                channel_id,
                snapshot.accepted_frame_end(),
                Some(snapshot.left_sample_peak()),
                Some(snapshot.right_sample_peak()),
            );
            latest_display = Some(update.snapshot());
            display_reset |= update.display_reset();
        });
        InputMeterDisplayRead {
            latest,
            display: latest_display,
            display_reset,
        }
    }

    fn drain_with(
        &mut self,
        mut observe: impl FnMut(InputMeterSnapshot),
    ) -> LatestInputMeterSnapshot {
        let mut snapshot = None;
        let mut delivered_snapshot_count = 0_usize;
        let mut dropped_snapshot_count = 0_usize;
        for _ in 0..MAX_QUEUED_INPUT_METER_SNAPSHOTS {
            let Some(next) = self.consumer.try_pop() else {
                break;
            };
            dropped_snapshot_count = dropped_snapshot_count.saturating_add(next.dropped_before());
            observe(next);
            snapshot = Some(next);
            delivered_snapshot_count += 1;
        }
        let sender_disconnected = !self.consumer.write_is_held();
        if sender_disconnected {
            fence(Ordering::Acquire);
            dropped_snapshot_count = dropped_snapshot_count.saturating_add(
                self.pending_dropped_snapshot_count
                    .swap(0, Ordering::AcqRel),
            );
        }
        LatestInputMeterSnapshot {
            snapshot,
            coalesced_snapshot_count: delivered_snapshot_count.saturating_sub(1),
            dropped_snapshot_count,
            sender_disconnected,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputMeterTransportBuildError {
    Allocation,
}

/// Test-only failure while allocating a bounded meter transport.
#[cfg(feature = "test-support")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMeterTestTransportError {
    Allocation,
}

pub(crate) fn try_input_meter_channel()
-> Result<(InputMeterSender, InputMeterReceiver), InputMeterTransportBuildError> {
    let ring = HeapRb::<InputMeterSnapshot>::try_new(MAX_QUEUED_INPUT_METER_SNAPSHOTS)
        .map_err(|_| InputMeterTransportBuildError::Allocation)?;
    let (producer, consumer) = ring.split();
    let pending_dropped_snapshot_count = Arc::new(AtomicUsize::new(0));
    Ok((
        InputMeterSender {
            producer,
            accepted_frame_end: 0,
            dropped_snapshot_count: 0,
            pending_dropped_snapshot_count: Arc::clone(&pending_dropped_snapshot_count),
        },
        InputMeterReceiver {
            consumer,
            channel_id: next_input_meter_channel_id(),
            pending_dropped_snapshot_count,
        },
    ))
}

/// Allocates one isolated accepted-input meter transport for a dependent test.
///
/// Production code cannot call this function. The production transport remains
/// owned by the monitoring session after capture validation.
///
/// # Errors
///
/// Returns [`InputMeterTestTransportError::Allocation`] if the fixed test
/// transport cannot be allocated.
#[cfg(feature = "test-support")]
pub fn input_meter_channel_for_test()
-> Result<(InputMeterTestPublisher, InputMeterReceiver), InputMeterTestTransportError> {
    let (sender, receiver) =
        try_input_meter_channel().map_err(|_| InputMeterTestTransportError::Allocation)?;
    Ok((InputMeterTestPublisher { sender }, receiver))
}

#[cfg(test)]
mod tests {
    use hyper_mixer_runtime::SamplePeakBallisticsConfig;

    use super::*;

    fn channel() -> (InputMeterSender, InputMeterReceiver) {
        try_input_meter_channel().unwrap()
    }

    #[test]
    fn measures_stereo_absolute_peaks_and_accepted_frame_end() {
        let (mut sender, mut receiver) = channel();
        assert_eq!(
            sender.publish(&[-0.25, 0.5, 0.75, -1.0]),
            InputMeterPublication::Published
        );

        let read = receiver.read_latest();
        let snapshot = read.snapshot().expect("published meter snapshot");
        assert_eq!(snapshot.accepted_frame_end(), 2);
        assert_eq!(snapshot.frame_count(), 2);
        assert_eq!(snapshot.left_sample_peak().to_bits(), 0.75_f32.to_bits());
        assert_eq!(snapshot.right_sample_peak().to_bits(), 1.0_f32.to_bits());
        assert_eq!(snapshot.dropped_before(), 0);
        assert_eq!(read.coalesced_snapshot_count(), 0);
        assert_eq!(read.dropped_snapshot_count(), 0);
        assert!(!read.sender_disconnected());
    }

    #[test]
    fn empty_input_cannot_fabricate_a_measurement_or_advance_the_clock() {
        let (mut sender, mut receiver) = channel();
        assert_eq!(sender.publish(&[]), InputMeterPublication::NoFrames);
        assert!(receiver.read_latest().snapshot().is_none());

        assert_eq!(
            sender.publish(&[0.0, 0.0]),
            InputMeterPublication::Published
        );
        assert_eq!(
            receiver
                .read_latest()
                .snapshot()
                .expect("nonempty input meter snapshot")
                .accepted_frame_end(),
            1
        );
    }

    #[test]
    fn drain_coalesces_delivered_snapshots_without_hiding_queue_rejections() {
        let (mut sender, mut receiver) = channel();
        for _ in 0..MAX_QUEUED_INPUT_METER_SNAPSHOTS {
            assert_eq!(
                sender.publish(&[0.25, 0.5]),
                InputMeterPublication::Published
            );
        }
        assert_eq!(
            sender.publish(&[0.25, 0.5]),
            InputMeterPublication::QueueFull
        );

        let first = receiver.read_latest();
        assert_eq!(
            first.coalesced_snapshot_count(),
            MAX_QUEUED_INPUT_METER_SNAPSHOTS - 1
        );
        assert_eq!(first.dropped_snapshot_count(), 0);
        assert_eq!(
            first
                .snapshot()
                .expect("newest delivered snapshot")
                .accepted_frame_end(),
            u64::try_from(MAX_QUEUED_INPUT_METER_SNAPSHOTS).unwrap()
        );

        assert_eq!(
            sender.publish(&[0.5, 0.25]),
            InputMeterPublication::Published
        );
        let recovered = receiver.read_latest();
        assert_eq!(recovered.dropped_snapshot_count(), 1);
        assert_eq!(
            recovered
                .snapshot()
                .expect("post-overflow snapshot")
                .accepted_frame_end(),
            u64::try_from(MAX_QUEUED_INPUT_METER_SNAPSHOTS + 2).unwrap()
        );
    }

    #[test]
    fn terminal_receiver_read_includes_pending_rejected_snapshots() {
        let (mut sender, mut receiver) = channel();
        for _ in 0..MAX_QUEUED_INPUT_METER_SNAPSHOTS {
            assert_eq!(
                sender.publish(&[0.25, 0.5]),
                InputMeterPublication::Published
            );
        }
        assert_eq!(
            sender.publish(&[0.25, 0.5]),
            InputMeterPublication::QueueFull
        );
        drop(sender);

        let terminal = receiver.read_latest();
        assert!(terminal.sender_disconnected());
        assert_eq!(terminal.dropped_snapshot_count(), 1);
    }

    #[test]
    fn display_consumes_every_delivered_snapshot_and_clears_on_terminal_authority() {
        let (mut sender, mut receiver) = channel();
        let config = SamplePeakBallisticsConfig::try_new(100, -60.0, 20.0, 0).unwrap();
        let mut display = StereoSamplePeakDisplayBallistics::new(config);
        assert_eq!(
            sender.publish(&[1.0, 0.1]),
            InputMeterPublication::Published
        );
        assert_eq!(
            sender.publish(&[0.0, 0.0]),
            InputMeterPublication::Published
        );

        let read = receiver.read_display(&mut display);
        let snapshot = read.display().expect("display update");
        assert_eq!(read.latest().coalesced_snapshot_count(), 1);
        assert_eq!(snapshot.frame_end(), 2);
        assert!(snapshot.left().measurement_available());
        assert!(snapshot.right().measurement_available());
        assert!((snapshot.left().level_dbfs() + 0.2).abs() < 0.001);
        assert!((snapshot.right().level_dbfs() + 20.2).abs() < 0.001);

        drop(sender);
        assert!(
            receiver
                .read_display(&mut display)
                .latest()
                .sender_disconnected()
        );
    }

    #[test]
    fn receiver_channel_identity_resets_reused_display_state() {
        let config = SamplePeakBallisticsConfig::try_new(100, -60.0, 20.0, 0).unwrap();
        let mut display = StereoSamplePeakDisplayBallistics::new(config);
        let (mut first_sender, mut first_receiver) = channel();
        assert_eq!(
            first_sender.publish(&[1.0, 1.0]),
            InputMeterPublication::Published
        );

        let first = first_receiver.read_display(&mut display);
        assert!(!first.display_reset());
        assert!(
            first
                .display()
                .expect("first input-meter display update")
                .left()
                .level_dbfs()
                .abs()
                < 0.001
        );

        let (mut second_sender, mut second_receiver) = channel();
        assert_eq!(
            second_sender.publish(&[0.1, 0.01]),
            InputMeterPublication::Published
        );

        let second = second_receiver.read_display(&mut display);
        let snapshot = second.display().expect("second input-meter display update");
        assert!(second.display_reset());
        assert!((snapshot.left().level_dbfs() + 20.0).abs() < 0.001);
        assert!((snapshot.right().level_dbfs() + 40.0).abs() < 0.001);
    }
}
