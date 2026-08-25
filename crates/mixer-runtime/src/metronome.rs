use core::error::Error;
use core::fmt;

use hyper_playback_compiler::{FrameTimeline, FrameTimelineError, MeterEvent, TempoEvent};

use crate::StereoOutput;

const NORMAL_CLICK_FREQUENCY_HZ: f32 = 1_600.0;
const ACCENT_CLICK_FREQUENCY_HZ: f32 = 2_400.0;
const NORMAL_CLICK_AMPLITUDE: f32 = 0.10;
const ACCENT_CLICK_AMPLITUDE: f32 = 0.14;
const CLICK_DURATION_DIVISOR: usize = 80;
const MAX_CLICK_FRAMES: usize = 8_192;

/// Preallocated timing and voice data for one transport metronome.
///
/// The plan is built outside the audio callback from an already validated
/// project timeline. Its two click voices and frame-to-tick index are retained
/// by the transport, so callback processing never allocates or reads files.
#[derive(Clone, Debug)]
pub struct MetronomePlan {
    frames: FrameTimeline,
    meter_events: Vec<MeterEvent>,
    normal_voice: Vec<f32>,
    accent_voice: Vec<f32>,
}

impl MetronomePlan {
    /// Creates a metronome plan for one exact project timeline.
    ///
    /// # Errors
    ///
    /// Returns an error when the tempo or meter map is outside the supported
    /// version-one domain, when voice storage cannot be allocated, or when the
    /// selected sample rate would make a click voice exceed its fixed bound.
    pub fn try_new(
        sample_rate_hz: u32,
        tempo_events: &[TempoEvent],
        meter_events: &[MeterEvent],
    ) -> Result<Self, MetronomePlanError> {
        let frames = FrameTimeline::try_new(sample_rate_hz, tempo_events)
            .map_err(MetronomePlanError::Timeline)?;
        validate_meter_events(meter_events)?;
        let voice_frames = click_frame_count(sample_rate_hz)?;
        let meter_events = try_copy(meter_events).map_err(|()| MetronomePlanError::Capacity)?;
        let normal_voice = build_voice(
            sample_rate_hz,
            voice_frames,
            NORMAL_CLICK_FREQUENCY_HZ,
            NORMAL_CLICK_AMPLITUDE,
        )?;
        let accent_voice = build_voice(
            sample_rate_hz,
            voice_frames,
            ACCENT_CLICK_FREQUENCY_HZ,
            ACCENT_CLICK_AMPLITUDE,
        )?;
        Ok(Self {
            frames,
            meter_events,
            normal_voice,
            accent_voice,
        })
    }
}

/// A construction failure for [`MetronomePlan`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetronomePlanError {
    Timeline(FrameTimelineError),
    EmptyMeterMap,
    FirstMeterNotAtOrigin,
    MeterNotStrictlyIncreasing,
    MeterOutsideVersionOneDomain,
    ClickTooLong { frames: usize, maximum: usize },
    Capacity,
}

impl fmt::Display for MetronomePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeline(source) => write!(formatter, "metronome timeline is invalid: {source}"),
            Self::EmptyMeterMap => formatter.write_str("metronome meter map is empty"),
            Self::FirstMeterNotAtOrigin => {
                formatter.write_str("metronome meter map does not begin at tick zero")
            }
            Self::MeterNotStrictlyIncreasing => {
                formatter.write_str("metronome meter events are not strictly increasing")
            }
            Self::MeterOutsideVersionOneDomain => {
                formatter.write_str("metronome meter event is outside the version-one domain")
            }
            Self::ClickTooLong { frames, maximum } => write!(
                formatter,
                "metronome click needs {frames} frames; maximum is {maximum}"
            ),
            Self::Capacity => formatter.write_str("metronome preparation capacity is unavailable"),
        }
    }
}

impl Error for MetronomePlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::EmptyMeterMap
            | Self::FirstMeterNotAtOrigin
            | Self::MeterNotStrictlyIncreasing
            | Self::MeterOutsideVersionOneDomain
            | Self::ClickTooLong { .. }
            | Self::Capacity => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetronomeProcessError {
    FrameTimeline(FrameTimelineError),
    FrameRangeOverflow,
}

impl fmt::Display for MetronomeProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTimeline(source) => write!(formatter, "metronome frame conversion failed: {source}"),
            Self::FrameRangeOverflow => formatter.write_str("metronome frame range overflowed"),
        }
    }
}

impl Error for MetronomeProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FrameTimeline(source) => Some(source),
            Self::FrameRangeOverflow => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MetronomeState {
    enabled: bool,
    active_voice: Option<ActiveVoice>,
    last_trigger_tick: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveVoice {
    accent: bool,
    offset: usize,
}

/// One callback-safe metronome bound to a prepared project timeline.
#[derive(Debug)]
pub(crate) struct Metronome {
    plan: MetronomePlan,
    state: MetronomeState,
}

impl Metronome {
    pub(crate) const fn new(plan: MetronomePlan) -> Self {
        Self {
            plan,
            state: MetronomeState {
                enabled: false,
                active_voice: None,
                last_trigger_tick: None,
            },
        }
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.state.enabled
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        self.state.enabled = enabled;
        if !enabled {
            self.clear_voice();
        }
    }

    pub(crate) const fn checkpoint(&self) -> MetronomeState {
        self.state
    }

    pub(crate) fn restore(&mut self, state: MetronomeState) {
        self.state = state;
    }

    pub(crate) fn clear_voice(&mut self) {
        self.state.active_voice = None;
        self.state.last_trigger_tick = None;
    }

    /// Allows a beat at a new transport position while retaining an active
    /// click across a loop seam.
    pub(crate) fn reset_position(&mut self) {
        self.state.last_trigger_tick = None;
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "the callback receives the exclusive stereo planes as one short-lived view"
    )]
    pub(crate) fn mix_window(
        &mut self,
        start_frame: usize,
        output: StereoOutput<'_>,
    ) -> Result<(), MetronomeProcessError> {
        if !self.state.enabled {
            return Ok(());
        }
        for offset in 0..output.left.len() {
            let frame = start_frame
                .checked_add(offset)
                .ok_or(MetronomeProcessError::FrameRangeOverflow)?;
            let tick = self
                .plan
                .frames
                .position_at_frame(frame)
                .map_err(MetronomeProcessError::FrameTimeline)?
                .whole_tick();
            if self.is_beat_start(tick) && self.state.last_trigger_tick != Some(tick) {
                self.state.active_voice = Some(ActiveVoice {
                    accent: self.is_bar_start(tick),
                    offset: 0,
                });
                self.state.last_trigger_tick = Some(tick);
            }
            let Some(active) = self.state.active_voice else {
                continue;
            };
            let voice = if active.accent {
                &self.plan.accent_voice
            } else {
                &self.plan.normal_voice
            };
            let sample = voice[active.offset];
            output.left[offset] += sample;
            output.right[offset] += sample;
            let next_offset = active.offset + 1;
            self.state.active_voice = (next_offset < voice.len()).then_some(ActiveVoice {
                accent: active.accent,
                offset: next_offset,
            });
        }
        Ok(())
    }

    fn is_beat_start(&self, tick: u64) -> bool {
        let event = self.meter_at(tick);
        (tick - event.tick).is_multiple_of(event.ticks_per_beat())
    }

    fn is_bar_start(&self, tick: u64) -> bool {
        let event = self.meter_at(tick);
        (tick - event.tick).is_multiple_of(event.ticks_per_bar())
    }

    fn meter_at(&self, tick: u64) -> MeterEvent {
        let index = self
            .plan
            .meter_events
            .partition_point(|event| event.tick <= tick)
            .saturating_sub(1);
        self.plan.meter_events[index]
    }
}

fn validate_meter_events(events: &[MeterEvent]) -> Result<(), MetronomePlanError> {
    let Some(first) = events.first() else {
        return Err(MetronomePlanError::EmptyMeterMap);
    };
    if first.tick != 0 {
        return Err(MetronomePlanError::FirstMeterNotAtOrigin);
    }
    let mut previous_tick = None;
    for event in events {
        if event.numerator == 0
            || event.denominator == 0
            || event.denominator > 256
            || !event.denominator.is_power_of_two()
        {
            return Err(MetronomePlanError::MeterOutsideVersionOneDomain);
        }
        if previous_tick.is_some_and(|previous| event.tick <= previous) {
            return Err(MetronomePlanError::MeterNotStrictlyIncreasing);
        }
        previous_tick = Some(event.tick);
    }
    Ok(())
}

fn click_frame_count(sample_rate_hz: u32) -> Result<usize, MetronomePlanError> {
    let sample_rate = usize::try_from(sample_rate_hz).map_err(|_| MetronomePlanError::Capacity)?;
    let frames = sample_rate.div_ceil(CLICK_DURATION_DIVISOR).max(1);
    if frames > MAX_CLICK_FRAMES {
        return Err(MetronomePlanError::ClickTooLong {
            frames,
            maximum: MAX_CLICK_FRAMES,
        });
    }
    Ok(frames)
}

fn try_copy<T: Copy>(source: &[T]) -> Result<Vec<T>, ()> {
    let mut destination = Vec::new();
    destination.try_reserve_exact(source.len()).map_err(|_| ())?;
    destination.extend_from_slice(source);
    Ok(destination)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the voice frame bound is 8,192 and its compatible sample rate is below 2^24"
)]
fn build_voice(
    sample_rate_hz: u32,
    frame_count: usize,
    frequency_hz: f32,
    amplitude: f32,
) -> Result<Vec<f32>, MetronomePlanError> {
    let mut voice = Vec::new();
    voice
        .try_reserve_exact(frame_count)
        .map_err(|_| MetronomePlanError::Capacity)?;
    for frame in 0..frame_count {
        let phase = core::f32::consts::TAU * frequency_hz * frame as f32 / sample_rate_hz as f32;
        let envelope = 1.0 - frame as f32 / frame_count as f32;
        voice.push(amplitude * envelope * envelope * phase.sin());
    }
    Ok(voice)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPO: [TempoEvent; 1] = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 500_000,
    }];
    const FOUR_FOUR: [MeterEvent; 1] = [MeterEvent {
        tick: 0,
        numerator: 4,
        denominator: 4,
    }];

    #[test]
    fn plan_rejects_invalid_meter_authority() {
        assert_eq!(
            MetronomePlan::try_new(
                48_000,
                &TEMPO,
                &[MeterEvent {
                    tick: 1,
                    numerator: 4,
                    denominator: 4,
                }],
            )
            .unwrap_err(),
            MetronomePlanError::FirstMeterNotAtOrigin
        );
    }

    #[test]
    fn click_is_silent_until_enabled_and_accents_each_bar() {
        let mut metronome = Metronome::new(MetronomePlan::try_new(48_000, &TEMPO, &FOUR_FOUR).unwrap());
        let mut left = [0.0; 8];
        let mut right = [0.0; 8];
        metronome
            .mix_window(0, StereoOutput { left: &mut left, right: &mut right })
            .unwrap();
        assert!(left.iter().all(|sample| sample.abs() <= f32::EPSILON));

        metronome.set_enabled(true);
        metronome
            .mix_window(0, StereoOutput { left: &mut left, right: &mut right })
            .unwrap();
        assert!(
            left.iter()
                .zip(&right)
                .all(|(left, right)| (*left - *right).abs() <= f32::EPSILON)
        );
        assert!(left.iter().any(|sample| sample.abs() > f32::EPSILON));
    }

    #[test]
    fn meter_change_starts_a_new_bar_and_the_following_beat_uses_the_normal_voice() {
        let meter = [
            MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            },
            MeterEvent {
                tick: 1_920,
                numerator: 6,
                denominator: 8,
            },
        ];
        let mut metronome = Metronome::new(MetronomePlan::try_new(48_000, &TEMPO, &meter).unwrap());
        metronome.set_enabled(true);

        let mut bar_left = [0.0; 8];
        let mut bar_right = [0.0; 8];
        metronome
            .mix_window(
                24_000,
                StereoOutput {
                    left: &mut bar_left,
                    right: &mut bar_right,
                },
            )
            .unwrap();

        let mut beat_left = [0.0; 8];
        let mut beat_right = [0.0; 8];
        metronome
            .mix_window(
                36_000,
                StereoOutput {
                    left: &mut beat_left,
                    right: &mut beat_right,
                },
            )
            .unwrap();

        let bar_peak = bar_left.iter().copied().map(f32::abs).fold(0.0, f32::max);
        let beat_peak = beat_left.iter().copied().map(f32::abs).fold(0.0, f32::max);
        assert!(
            bar_left
                .iter()
                .zip(&bar_right)
                .all(|(left, right)| (*left - *right).abs() <= f32::EPSILON)
        );
        assert!(
            beat_left
                .iter()
                .zip(&beat_right)
                .all(|(left, right)| (*left - *right).abs() <= f32::EPSILON)
        );
        assert!(bar_peak > beat_peak);
    }
}
