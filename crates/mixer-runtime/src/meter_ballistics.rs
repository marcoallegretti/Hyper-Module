use core::error::Error;
use core::fmt;

use super::OutputMeterSnapshot;

/// Explicit policy for persistent sample-peak display behavior.
///
/// Falloff is linear in dB over successfully processed output frames. This is
/// a sample-peak display policy, not a true-peak, loudness, or VU meter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SamplePeakBallisticsConfig {
    sample_rate_hz: u32,
    floor_dbfs: f32,
    falloff_db_per_second: f32,
    peak_hold_frames: u64,
}

impl SamplePeakBallisticsConfig {
    /// Creates one frame-time sample-peak policy.
    ///
    /// `sample_rate_hz` must match the output stream that produces the meter
    /// snapshots.
    ///
    /// A zero falloff keeps displayed peaks indefinitely. A zero hold duration
    /// makes the held marker begin falling on the next processed frame.
    ///
    /// # Errors
    ///
    /// Returns [`SamplePeakBallisticsConfigError`] when the sample rate is
    /// zero, the floor is not finite and below 0 dBFS, or falloff is negative
    /// or non-finite.
    pub fn try_new(
        sample_rate_hz: u32,
        floor_dbfs: f32,
        falloff_db_per_second: f32,
        peak_hold_frames: u64,
    ) -> Result<Self, SamplePeakBallisticsConfigError> {
        if sample_rate_hz == 0 {
            return Err(SamplePeakBallisticsConfigError::ZeroSampleRate);
        }
        if !floor_dbfs.is_finite() || floor_dbfs >= 0.0 {
            return Err(SamplePeakBallisticsConfigError::InvalidFloor);
        }
        if !falloff_db_per_second.is_finite() || falloff_db_per_second < 0.0 {
            return Err(SamplePeakBallisticsConfigError::InvalidFalloff);
        }
        Ok(Self {
            sample_rate_hz,
            floor_dbfs,
            falloff_db_per_second,
            peak_hold_frames,
        })
    }

    #[must_use]
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn floor_dbfs(self) -> f32 {
        self.floor_dbfs
    }

    #[must_use]
    pub const fn falloff_db_per_second(self) -> f32 {
        self.falloff_db_per_second
    }

    #[must_use]
    pub const fn peak_hold_frames(self) -> u64 {
        self.peak_hold_frames
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SamplePeakBallisticsConfigError {
    ZeroSampleRate,
    InvalidFloor,
    InvalidFalloff,
}

impl fmt::Display for SamplePeakBallisticsConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ZeroSampleRate => "sample-peak ballistics require a nonzero sample rate",
            Self::InvalidFloor => "sample-peak ballistics require a finite floor below 0 dBFS",
            Self::InvalidFalloff => "sample-peak ballistics require a finite nonnegative falloff",
        })
    }
}

impl Error for SamplePeakBallisticsConfigError {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ChannelState {
    level_dbfs: f32,
    held_peak_dbfs: f32,
    hold_until_frame: u64,
}

impl ChannelState {
    const fn at_floor(floor_dbfs: f32) -> Self {
        Self {
            level_dbfs: floor_dbfs,
            held_peak_dbfs: floor_dbfs,
            hold_until_frame: 0,
        }
    }

    fn update(
        &mut self,
        sample_peak: Option<f32>,
        previous_frame_end: u64,
        frame_end: u64,
        config: SamplePeakBallisticsConfig,
    ) -> ChannelSamplePeakBallistics {
        let elapsed_frames = frame_end.saturating_sub(previous_frame_end);
        let level_after_falloff = fall(self.level_dbfs, elapsed_frames, config);
        let held_falloff_start = previous_frame_end.max(self.hold_until_frame);
        let held_after_falloff = fall(
            self.held_peak_dbfs,
            frame_end.saturating_sub(held_falloff_start),
            config,
        );
        let measured_dbfs = sample_peak.map(|peak| sample_peak_dbfs(peak, config.floor_dbfs));

        self.level_dbfs = measured_dbfs.map_or(level_after_falloff, |measured| {
            measured.max(level_after_falloff)
        });
        if let Some(measured) = measured_dbfs
            && measured >= held_after_falloff
        {
            self.held_peak_dbfs = measured;
            self.hold_until_frame = frame_end.saturating_add(config.peak_hold_frames);
        } else {
            self.held_peak_dbfs = held_after_falloff;
        }

        ChannelSamplePeakBallistics {
            level_dbfs: self.level_dbfs,
            held_peak_dbfs: self.held_peak_dbfs,
            measurement_available: measured_dbfs.is_some(),
        }
    }
}

/// Persistent display values for one channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelSamplePeakBallistics {
    level_dbfs: f32,
    held_peak_dbfs: f32,
    measurement_available: bool,
}

impl ChannelSamplePeakBallistics {
    #[must_use]
    pub const fn level_dbfs(self) -> f32 {
        self.level_dbfs
    }

    #[must_use]
    pub const fn held_peak_dbfs(self) -> f32 {
        self.held_peak_dbfs
    }

    #[must_use]
    pub const fn measurement_available(self) -> bool {
        self.measurement_available
    }
}

/// Persistent stereo sample-peak values after one output snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoSamplePeakBallisticsSnapshot {
    processed_frame_end: u64,
    left: ChannelSamplePeakBallistics,
    right: ChannelSamplePeakBallistics,
}

impl StereoSamplePeakBallisticsSnapshot {
    #[must_use]
    pub const fn processed_frame_end(self) -> u64 {
        self.processed_frame_end
    }

    #[must_use]
    pub const fn left(self) -> ChannelSamplePeakBallistics {
        self.left
    }

    #[must_use]
    pub const fn right(self) -> ChannelSamplePeakBallistics {
        self.right
    }
}

/// Internal result of applying one output snapshot to persistent ballistics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SamplePeakBallisticsUpdate {
    snapshot: StereoSamplePeakBallisticsSnapshot,
    ballistics_reset: bool,
}

impl SamplePeakBallisticsUpdate {
    #[must_use]
    pub(crate) const fn snapshot(self) -> StereoSamplePeakBallisticsSnapshot {
        self.snapshot
    }

    #[must_use]
    pub(crate) const fn ballistics_reset(self) -> bool {
        self.ballistics_reset
    }
}

/// Frame-time persistent state for a stereo sample-peak display.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoSamplePeakBallistics {
    config: SamplePeakBallisticsConfig,
    channel_id: Option<u64>,
    last_frame_end: Option<u64>,
    left: ChannelState,
    right: ChannelState,
}

impl StereoSamplePeakBallistics {
    #[must_use]
    pub const fn new(config: SamplePeakBallisticsConfig) -> Self {
        Self {
            config,
            channel_id: None,
            last_frame_end: None,
            left: ChannelState::at_floor(config.floor_dbfs),
            right: ChannelState::at_floor(config.floor_dbfs),
        }
    }

    #[must_use]
    pub const fn config(self) -> SamplePeakBallisticsConfig {
        self.config
    }

    /// Clears accumulated display and frame-clock state.
    pub const fn reset(&mut self) {
        self.channel_id = None;
        self.clear_display();
    }

    const fn clear_display(&mut self) {
        self.last_frame_end = None;
        self.left = ChannelState::at_floor(self.config.floor_dbfs);
        self.right = ChannelState::at_floor(self.config.floor_dbfs);
    }

    #[must_use]
    pub(crate) fn update(
        &mut self,
        channel_id: u64,
        snapshot: OutputMeterSnapshot,
    ) -> SamplePeakBallisticsUpdate {
        let frame_end = snapshot.processed_frame_end();
        let channel_changed = self
            .channel_id
            .is_some_and(|previous| channel_id != previous);
        let frame_clock_regressed = self
            .last_frame_end
            .is_some_and(|previous| !channel_changed && frame_end < previous);
        let ballistics_reset = channel_changed || frame_clock_regressed;
        if ballistics_reset {
            self.clear_display();
        }
        self.channel_id = Some(channel_id);
        let previous_frame_end = self.last_frame_end.unwrap_or(frame_end);
        let meter = snapshot.meter();
        let left = self.left.update(
            meter.left().sample_peak(),
            previous_frame_end,
            frame_end,
            self.config,
        );
        let right = self.right.update(
            meter.right().sample_peak(),
            previous_frame_end,
            frame_end,
            self.config,
        );
        self.last_frame_end = Some(frame_end);
        SamplePeakBallisticsUpdate {
            snapshot: StereoSamplePeakBallisticsSnapshot {
                processed_frame_end: frame_end,
                left,
                right,
            },
            ballistics_reset,
        }
    }
}

fn sample_peak_dbfs(sample_peak: f32, floor_dbfs: f32) -> f32 {
    if sample_peak <= 0.0 {
        floor_dbfs
    } else {
        (20.0 * sample_peak.log10()).max(floor_dbfs)
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "the bounded falloff is converted back to the public f32 display domain"
)]
fn fall(level_dbfs: f32, elapsed_frames: u64, config: SamplePeakBallisticsConfig) -> f32 {
    if elapsed_frames == 0 || config.falloff_db_per_second == 0.0 || level_dbfs <= config.floor_dbfs
    {
        return level_dbfs.max(config.floor_dbfs);
    }
    let available_db = f64::from(level_dbfs - config.floor_dbfs);
    let falloff_db = f64::from(config.falloff_db_per_second) * elapsed_frames as f64
        / f64::from(config.sample_rate_hz);
    if falloff_db >= available_db {
        config.floor_dbfs
    } else {
        level_dbfs - falloff_db as f32
    }
}

#[cfg(test)]
mod tests {
    use hyper_audio_engine::InterleavedStereoCallback;

    use super::*;
    use crate::{TransportCycle, TransportState};

    const EPSILON: f32 = 1.0e-5;

    fn snapshot(
        processed_frame_end: u64,
        frame_count: usize,
        left_peak: f32,
        right_peak: f32,
    ) -> OutputMeterSnapshot {
        let mut callback = InterleavedStereoCallback::try_new(frame_count, 48_000).unwrap();
        let mut interleaved = vec![0.0_f32; frame_count * 2];
        let processed = callback
            .process(&mut interleaved, |output| {
                output.left.fill(left_peak);
                output.right.fill(right_peak);
                Ok::<_, ()>(())
            })
            .unwrap();
        let ((), meter) = processed.into_parts();
        OutputMeterSnapshot {
            cycle: TransportCycle {
                state: TransportState::Rolling,
                start_frame: 0,
                frame_count,
                active_slot_count: 1,
                applied_command_count: 0,
                command_sender_disconnected: false,
            },
            meter,
            processed_frame_end,
            dropped_before: 0,
        }
    }

    fn config(falloff_db_per_second: f32, peak_hold_frames: u64) -> SamplePeakBallisticsConfig {
        SamplePeakBallisticsConfig::try_new(100, -60.0, falloff_db_per_second, peak_hold_frames)
            .unwrap()
    }

    fn assert_near(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn configuration_rejects_undefined_display_policies() {
        assert_eq!(
            SamplePeakBallisticsConfig::try_new(0, -60.0, 13.3, 0),
            Err(SamplePeakBallisticsConfigError::ZeroSampleRate)
        );
        for floor in [0.0, 1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                SamplePeakBallisticsConfig::try_new(48_000, floor, 13.3, 0),
                Err(SamplePeakBallisticsConfigError::InvalidFloor)
            );
        }
        for falloff in [-1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(
                SamplePeakBallisticsConfig::try_new(48_000, -90.0, falloff, 0),
                Err(SamplePeakBallisticsConfigError::InvalidFalloff)
            );
        }
        assert!(SamplePeakBallisticsConfig::try_new(48_000, -90.0, 0.0, 0).is_ok());
    }

    #[test]
    fn falloff_depends_on_elapsed_frames_not_snapshot_partitioning() {
        let policy = config(20.0, 0);
        let mut one_block = StereoSamplePeakBallistics::new(policy);
        let _ = one_block.update(1, snapshot(10, 10, 1.0, 1.0));
        let one_block_result = one_block.update(1, snapshot(60, 50, 0.0, 0.0)).snapshot();

        let mut partitioned = StereoSamplePeakBallistics::new(policy);
        let _ = partitioned.update(1, snapshot(10, 10, 1.0, 1.0));
        let mut partitioned_result = None;
        for frame_end in [20, 30, 40, 50, 60] {
            partitioned_result = Some(
                partitioned
                    .update(1, snapshot(frame_end, 10, 0.0, 0.0))
                    .snapshot(),
            );
        }
        let partitioned_result = partitioned_result.unwrap();

        assert_near(one_block_result.left().level_dbfs(), -10.0);
        assert_near(partitioned_result.left().level_dbfs(), -10.0);
        assert_near(
            one_block_result.left().level_dbfs(),
            partitioned_result.left().level_dbfs(),
        );
    }

    #[test]
    fn peak_hold_expires_in_frame_time_before_falling() {
        let mut ballistics = StereoSamplePeakBallistics::new(config(20.0, 50));
        let _ = ballistics.update(1, snapshot(10, 10, 1.0, 1.0));

        let during_hold = ballistics.update(1, snapshot(40, 30, 0.0, 0.0)).snapshot();
        assert_near(during_hold.left().level_dbfs(), -6.0);
        assert_near(during_hold.left().held_peak_dbfs(), 0.0);

        let after_hold = ballistics.update(1, snapshot(80, 40, 0.0, 0.0)).snapshot();
        assert_near(after_hold.left().level_dbfs(), -14.0);
        assert_near(after_hold.left().held_peak_dbfs(), -4.0);
    }

    #[test]
    fn regressed_frame_clock_resets_state_and_reports_the_new_epoch() {
        let mut ballistics = StereoSamplePeakBallistics::new(config(20.0, 50));
        let _ = ballistics.update(1, snapshot(100, 10, 1.0, 1.0));

        let update = ballistics.update(1, snapshot(10, 10, 0.1, 0.1));

        assert!(update.ballistics_reset());
        assert_eq!(update.snapshot().processed_frame_end(), 10);
        assert_near(update.snapshot().left().level_dbfs(), -20.0);
        assert_near(update.snapshot().left().held_peak_dbfs(), -20.0);
    }

    #[test]
    fn different_meter_channel_resets_even_when_its_clock_is_newer() {
        let mut ballistics = StereoSamplePeakBallistics::new(config(20.0, 50));
        let _ = ballistics.update(1, snapshot(10, 10, 1.0, 1.0));

        let update = ballistics.update(2, snapshot(1_000, 10, 0.1, 0.1));

        assert!(update.ballistics_reset());
        assert_eq!(update.snapshot().processed_frame_end(), 1_000);
        assert_near(update.snapshot().left().level_dbfs(), -20.0);
        assert_near(update.snapshot().left().held_peak_dbfs(), -20.0);
    }

    #[test]
    fn receiver_channel_identity_resets_reused_ballistics() {
        let mut ballistics = StereoSamplePeakBallistics::new(config(20.0, 50));
        let (mut first_sender, mut first_receiver) = crate::output_meter_channel();
        first_sender.try_publish(snapshot(0, 10, 1.0, 1.0)).unwrap();
        let first = first_receiver.read_ballistics(&mut ballistics);
        assert!(!first.ballistics_reset());
        assert_near(first.ballistics().unwrap().left().level_dbfs(), 0.0);

        let (mut second_sender, mut second_receiver) = crate::output_meter_channel();
        second_sender
            .try_publish(snapshot(0, 10, 0.1, 0.1))
            .unwrap();
        let second = second_receiver.read_ballistics(&mut ballistics);

        assert!(second.ballistics_reset());
        assert_near(second.ballistics().unwrap().left().level_dbfs(), -20.0);
    }
}
