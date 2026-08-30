#![doc = "Exact project-tick to native clip-playback compilation for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{
    ClipPlacement, ClipPlanError, ClipRenderPlan, ConsolidationReason, FrameRange, NativeClipSpec,
    PlacementSpec, TimeMapping,
};

pub mod graph;
pub mod scene;

pub const TICKS_PER_QUARTER: u64 = 1_920;
const MICROSECONDS_PER_SECOND: u128 = 1_000_000;
const MAX_TEMPO_MICROSECONDS_PER_QUARTER: u32 = 16_777_215;
const VERSION_1_METER_DENOMINATORS: [u16; 9] = [1, 2, 4, 8, 16, 32, 64, 128, 256];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TempoEvent {
    pub tick: u64,
    pub microseconds_per_quarter: u32,
}

/// One entry of the step meter map. Validated by the loader: the first sits at
/// tick zero, ticks strictly increase, the numerator is `1..=255`, and the
/// denominator is a power of two up to 256.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeterEvent {
    pub tick: u64,
    pub numerator: u16,
    pub denominator: u16,
}

impl MeterEvent {
    /// Ticks in one beat. A 4/4 bar counts quarters, a 6/8 bar counts eighths,
    /// which follows from the denominator alone.
    #[must_use]
    pub const fn ticks_per_beat(self) -> u64 {
        TICKS_PER_QUARTER * 4 / self.denominator as u64
    }

    #[must_use]
    pub const fn ticks_per_bar(self) -> u64 {
        self.ticks_per_beat() * self.numerator as u64
    }
}

/// Where the playhead sits musically. Bars and beats are one-based, as every
/// digital audio workstation presents them; the tick remainder is zero-based.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MusicalPosition {
    bar: u64,
    beat: u64,
    tick: u64,
}

impl MusicalPosition {
    #[must_use]
    pub const fn bar(self) -> u64 {
        self.bar
    }

    #[must_use]
    pub const fn beat(self) -> u64 {
        self.beat
    }

    #[must_use]
    pub const fn tick(self) -> u64 {
        self.tick
    }
}

/// The musical position of a sample frame, combining both halves of the
/// timeline: the tempo map places the frame on a tick, and the meter map groups
/// that tick into bars and beats.
///
/// # Errors
///
/// Returns [`TimelineError`] when either map is unusable or the arithmetic
/// would overflow.
pub fn musical_position_at_frame(
    sample_rate_hz: u32,
    tempo_events: &[TempoEvent],
    meter_events: &[MeterEvent],
    frame: usize,
) -> Result<MusicalPosition, TimelineError> {
    let timeline = FrameTimeline::try_new(sample_rate_hz, tempo_events)
        .map_err(|_| TimelineError::ArithmeticOverflow)?;
    let tick = timeline
        .position_at_frame(frame)
        .map_err(|_| TimelineError::ArithmeticOverflow)?
        .whole_tick();
    musical_position(meter_events, tick)
}

/// Converts a project tick to bars and beats through the step meter map.
///
/// Meter does not affect playback timing; it groups a tempo's pulse into bars,
/// so this is a presentation conversion only.
///
/// # Errors
///
/// Returns [`TimelineError::EmptyMeterMap`] when the map is empty and
/// [`TimelineError::ArithmeticOverflow`] when the arithmetic would overflow.
pub fn musical_position(
    meter_events: &[MeterEvent],
    tick: u64,
) -> Result<MusicalPosition, TimelineError> {
    let first = *meter_events.first().ok_or(TimelineError::EmptyMeterMap)?;
    let mut bars_before = 0_u64;
    let mut cursor_tick = 0_u64;
    let mut current = first;
    for &next in meter_events.iter().skip(1) {
        if next.tick > tick {
            break;
        }
        let span = next
            .tick
            .checked_sub(cursor_tick)
            .ok_or(TimelineError::ArithmeticOverflow)?;
        bars_before = bars_before
            .checked_add(span / current.ticks_per_bar())
            .ok_or(TimelineError::ArithmeticOverflow)?;
        cursor_tick = next.tick;
        current = next;
    }
    let elapsed = tick
        .checked_sub(cursor_tick)
        .ok_or(TimelineError::ArithmeticOverflow)?;
    let bar_offset = elapsed / current.ticks_per_bar();
    let within_bar = elapsed % current.ticks_per_bar();
    Ok(MusicalPosition {
        bar: bars_before
            .checked_add(bar_offset)
            .and_then(|bars| bars.checked_add(1))
            .ok_or(TimelineError::ArithmeticOverflow)?,
        beat: within_bar / current.ticks_per_beat() + 1,
        tick: within_bar % current.ticks_per_beat(),
    })
}

/// Resolves an exact one-based bar and beat plus zero-based tick remainder to
/// its unique project tick through the verified meter map.
///
/// A meter change may occur inside a bar. In that case the forward
/// [`musical_position`] conversion can produce the same displayed coordinate
/// for multiple project ticks. This function refuses that ambiguity rather
/// than choosing a durable edit position implicitly.
///
/// # Errors
///
/// Returns [`MusicalPositionResolveError::InvalidPosition`] when the supplied
/// components do not describe a position in the map,
/// [`MusicalPositionResolveError::AmbiguousPosition`] when more than one
/// meter-map segment describes them, and a named error for an invalid map or
/// checked arithmetic failure.
pub fn tick_at_musical_position(
    meter_events: &[MeterEvent],
    bar: u64,
    beat: u64,
    tick: u64,
) -> Result<u64, MusicalPositionResolveError> {
    if bar == 0 || beat == 0 {
        return Err(MusicalPositionResolveError::InvalidPosition);
    }
    validate_meter_events_for_position_resolution(meter_events)?;

    let mut bars_before = 0_u64;
    let mut found = None;
    for (index, current) in meter_events.iter().copied().enumerate() {
        let segment_end = meter_events.get(index + 1).map(|event| event.tick);
        if let Some(candidate) =
            tick_in_meter_segment(current, bars_before, segment_end, bar, beat, tick)?
            && found.replace(candidate).is_some()
        {
            return Err(MusicalPositionResolveError::AmbiguousPosition);
        }

        if let Some(segment_end) = segment_end {
            let completed_bars = segment_end
                .checked_sub(current.tick)
                .ok_or(MusicalPositionResolveError::InvalidMeterMap)?
                / current.ticks_per_bar();
            bars_before = bars_before
                .checked_add(completed_bars)
                .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
        }
    }

    found.ok_or(MusicalPositionResolveError::InvalidPosition)
}

fn validate_meter_events_for_position_resolution(
    meter_events: &[MeterEvent],
) -> Result<(), MusicalPositionResolveError> {
    let Some(first) = meter_events.first() else {
        return Err(MusicalPositionResolveError::EmptyMeterMap);
    };
    if first.tick != 0 {
        return Err(MusicalPositionResolveError::InvalidMeterMap);
    }

    let mut previous_tick = None;
    for event in meter_events {
        if event.numerator == 0
            || event.numerator > 255
            || !VERSION_1_METER_DENOMINATORS.contains(&event.denominator)
            || previous_tick.is_some_and(|previous| event.tick <= previous)
        {
            return Err(MusicalPositionResolveError::InvalidMeterMap);
        }
        previous_tick = Some(event.tick);
    }
    Ok(())
}

fn tick_in_meter_segment(
    meter: MeterEvent,
    bars_before: u64,
    segment_end: Option<u64>,
    bar: u64,
    beat: u64,
    tick: u64,
) -> Result<Option<u64>, MusicalPositionResolveError> {
    let first_bar = bars_before
        .checked_add(1)
        .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
    let ticks_per_beat = meter.ticks_per_beat();
    if bar < first_bar || beat > u64::from(meter.numerator) || tick >= ticks_per_beat {
        return Ok(None);
    }

    let within_bar = beat
        .checked_sub(1)
        .and_then(|beat_offset| beat_offset.checked_mul(ticks_per_beat))
        .and_then(|beat_offset| beat_offset.checked_add(tick))
        .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
    let bar_offset = bar
        .checked_sub(first_bar)
        .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
    let ticks_per_bar = meter.ticks_per_bar();
    let segment_span = segment_end
        .map(|segment_end| {
            segment_end
                .checked_sub(meter.tick)
                .ok_or(MusicalPositionResolveError::InvalidMeterMap)
        })
        .transpose()?;
    if let Some(span) = segment_span {
        let final_segment_relative = span
            .checked_sub(1)
            .ok_or(MusicalPositionResolveError::InvalidMeterMap)?;
        let maximum_bar_offset = final_segment_relative / ticks_per_bar;
        if bar_offset > maximum_bar_offset {
            return Ok(None);
        }
        let final_bar_tick_limit = final_segment_relative % ticks_per_bar;
        if bar_offset == maximum_bar_offset && within_bar > final_bar_tick_limit {
            return Ok(None);
        }
    }
    let relative = bar_offset
        .checked_mul(ticks_per_bar)
        .and_then(|bar_offset| bar_offset.checked_add(within_bar))
        .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
    let candidate = meter
        .tick
        .checked_add(relative)
        .ok_or(MusicalPositionResolveError::ArithmeticOverflow)?;
    if segment_end.is_some_and(|end| candidate >= end) {
        return Ok(None);
    }
    Ok(Some(candidate))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
    EmptyMeterMap,
    InvalidSampleRate,
    EmptyTempoMap,
    FirstTempoNotAtOrigin,
    InvalidTempo,
    TempoNotStrictlyIncreasing,
    ArithmeticOverflow,
    FrameOutOfRange,
}

impl fmt::Display for TimelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate => formatter.write_str("sample rate must be positive"),
            Self::EmptyTempoMap => formatter.write_str("tempo map must not be empty"),
            Self::EmptyMeterMap => formatter.write_str("meter map must not be empty"),
            Self::FirstTempoNotAtOrigin => formatter.write_str("tempo map must begin at tick zero"),
            Self::InvalidTempo => formatter.write_str("tempo is outside the version 1 domain"),
            Self::TempoNotStrictlyIncreasing => {
                formatter.write_str("tempo-event ticks must be strictly increasing")
            }
            Self::ArithmeticOverflow => formatter.write_str("timeline arithmetic overflowed"),
            Self::FrameOutOfRange => formatter.write_str("frame is outside the platform range"),
        }
    }
}

impl Error for TimelineError {}

/// Failure while resolving a displayed musical position back to a project tick.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MusicalPositionResolveError {
    EmptyMeterMap,
    InvalidMeterMap,
    InvalidPosition,
    AmbiguousPosition,
    ArithmeticOverflow,
}

impl fmt::Display for MusicalPositionResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyMeterMap => formatter.write_str("meter map must not be empty"),
            Self::InvalidMeterMap => {
                formatter.write_str("meter map is outside the supported position domain")
            }
            Self::InvalidPosition => {
                formatter.write_str("musical position is not represented by the meter map")
            }
            Self::AmbiguousPosition => {
                formatter.write_str("musical position is ambiguous in the meter map")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("musical position exceeds the supported timeline domain")
            }
        }
    }
}

impl Error for MusicalPositionResolveError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameTimelineError {
    Timeline(TimelineError),
    CapacityUnavailable,
    ArithmeticOverflow,
    TickOutOfRange,
}

impl fmt::Display for FrameTimelineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeline(source) => write!(formatter, "timeline is invalid: {source}"),
            Self::CapacityUnavailable => {
                formatter.write_str("frame timeline capacity is unavailable")
            }
            Self::ArithmeticOverflow => formatter.write_str("frame timeline arithmetic overflowed"),
            Self::TickOutOfRange => {
                formatter.write_str("sample frame maps outside the project tick domain")
            }
        }
    }
}

impl Error for FrameTimelineError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::CapacityUnavailable | Self::ArithmeticOverflow | Self::TickOutOfRange => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FrameTempoSegment {
    start_tick: u64,
    start_frame_numerator: u128,
    tick_denominator: u128,
}

/// Exact musical position at the start of one sample frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameTickPosition {
    tick: u64,
    remainder: u128,
    denominator: u128,
}

impl FrameTickPosition {
    #[must_use]
    pub const fn whole_tick(self) -> u64 {
        self.tick
    }

    #[must_use]
    pub const fn is_exact(self) -> bool {
        self.remainder == 0
    }

    /// Resolves the next grid tick at or after this exact position.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] for a zero quantum or tick overflow.
    pub fn quantize_forward(self, quantum_ticks: u64) -> Result<u64, FrameTimelineError> {
        if quantum_ticks == 0 {
            return Err(FrameTimelineError::TickOutOfRange);
        }
        if self.remainder == 0 && self.tick.is_multiple_of(quantum_ticks) {
            return Ok(self.tick);
        }
        self.tick
            .checked_div(quantum_ticks)
            .and_then(|quotient| quotient.checked_add(1))
            .and_then(|quotient| quotient.checked_mul(quantum_ticks))
            .ok_or(FrameTimelineError::TickOutOfRange)
    }

    #[must_use]
    pub const fn is_at_or_after(self, tick: u64) -> bool {
        self.tick >= tick
    }

    #[must_use]
    pub const fn is_before(self, tick: u64) -> bool {
        self.tick < tick
    }

    /// Interpolates an integer value at this exact fractional-tick position.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] unless this position lies in the closed
    /// interval between two strictly ordered ticks, or if checked arithmetic
    /// fails.
    pub fn interpolate_i32(
        self,
        left_tick: u64,
        left_value: i32,
        right_tick: u64,
        right_value: i32,
    ) -> Result<i32, FrameTimelineError> {
        if left_tick >= right_tick
            || self.is_before(left_tick)
            || self.is_at_or_after(right_tick) && self.tick != right_tick
            || self.tick == right_tick && self.remainder != 0
        {
            return Err(FrameTimelineError::TickOutOfRange);
        }
        let whole_elapsed = u128::from(self.tick - left_tick)
            .checked_mul(self.denominator)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let elapsed = whole_elapsed
            .checked_add(self.remainder)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let duration = u128::from(right_tick - left_tick)
            .checked_mul(self.denominator)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let elapsed =
            i128::try_from(elapsed).map_err(|_| FrameTimelineError::ArithmeticOverflow)?;
        let duration =
            i128::try_from(duration).map_err(|_| FrameTimelineError::ArithmeticOverflow)?;
        let delta = i128::from(right_value) - i128::from(left_value);
        let value = i128::from(left_value)
            .checked_add(
                delta
                    .checked_mul(elapsed)
                    .ok_or(FrameTimelineError::ArithmeticOverflow)?
                    / duration,
            )
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        i32::try_from(value).map_err(|_| FrameTimelineError::ArithmeticOverflow)
    }
}

/// Owned step-tempo index for allocation-free sample-frame to tick conversion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameTimeline {
    segments: Vec<FrameTempoSegment>,
}

impl FrameTimeline {
    /// Builds exact tempo-segment boundaries outside the real-time path.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] for an invalid timeline, unavailable
    /// capacity, or checked arithmetic failure.
    pub fn try_new(
        sample_rate_hz: u32,
        tempo_events: &[TempoEvent],
    ) -> Result<Self, FrameTimelineError> {
        let timeline = Timeline::try_new(sample_rate_hz, tempo_events)
            .map_err(FrameTimelineError::Timeline)?;
        let mut segments = Vec::new();
        segments
            .try_reserve_exact(tempo_events.len())
            .map_err(|_| FrameTimelineError::CapacityUnavailable)?;
        for event in tempo_events {
            let tick_denominator = u128::from(sample_rate_hz)
                .checked_mul(u128::from(event.microseconds_per_quarter))
                .ok_or(FrameTimelineError::ArithmeticOverflow)?;
            segments.push(FrameTempoSegment {
                start_tick: event.tick,
                start_frame_numerator: timeline
                    .exact_frame_numerator(event.tick)
                    .map_err(FrameTimelineError::Timeline)?,
                tick_denominator,
            });
        }
        Ok(Self { segments })
    }

    /// Returns the exact musical position at the start of `frame`.
    ///
    /// A tick boundary inside a sample frame therefore takes effect on the
    /// first sample whose start is at or after that boundary.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] for checked arithmetic or if the result
    /// exceeds the project tick domain.
    pub fn position_at_frame(&self, frame: usize) -> Result<FrameTickPosition, FrameTimelineError> {
        let timeline_denominator = u128::from(TICKS_PER_QUARTER) * MICROSECONDS_PER_SECOND;
        let frame_numerator = u128::try_from(frame)
            .map_err(|_| FrameTimelineError::ArithmeticOverflow)?
            .checked_mul(timeline_denominator)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_frame_numerator <= frame_numerator)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let elapsed = frame_numerator
            .checked_sub(segment.start_frame_numerator)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let whole_elapsed = elapsed / segment.tick_denominator;
        let tick = u128::from(segment.start_tick)
            .checked_add(whole_elapsed)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        Ok(FrameTickPosition {
            tick: u64::try_from(tick).map_err(|_| FrameTimelineError::TickOutOfRange)?,
            remainder: elapsed % segment.tick_denominator,
            denominator: segment.tick_denominator,
        })
    }

    /// Converts an absolute project tick to the first sample frame whose
    /// start is at or after that tick.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] if checked arithmetic fails or the
    /// resulting frame lies outside the platform range.
    pub fn tick_to_frame_ceil(&self, tick: u64) -> Result<usize, FrameTimelineError> {
        let segment_index = self
            .segments
            .partition_point(|segment| segment.start_tick <= tick)
            .saturating_sub(1);
        let segment = self.segments[segment_index];
        let elapsed = u128::from(tick - segment.start_tick)
            .checked_mul(segment.tick_denominator)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let numerator = segment
            .start_frame_numerator
            .checked_add(elapsed)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?;
        let denominator = u128::from(TICKS_PER_QUARTER) * MICROSECONDS_PER_SECOND;
        let frame = numerator
            .checked_add(denominator - 1)
            .ok_or(FrameTimelineError::ArithmeticOverflow)?
            / denominator;
        usize::try_from(frame).map_err(|_| FrameTimelineError::TickOutOfRange)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Timeline<'a> {
    sample_rate_hz: u32,
    tempo_events: &'a [TempoEvent],
}

impl<'a> Timeline<'a> {
    /// Validates a version 1 step-tempo map.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError`] for a zero sample rate, empty or non-origin
    /// map, invalid tempo, or non-increasing event ticks.
    pub fn try_new(
        sample_rate_hz: u32,
        tempo_events: &'a [TempoEvent],
    ) -> Result<Self, TimelineError> {
        if sample_rate_hz == 0 {
            return Err(TimelineError::InvalidSampleRate);
        }
        let first = tempo_events.first().ok_or(TimelineError::EmptyTempoMap)?;
        if first.tick != 0 {
            return Err(TimelineError::FirstTempoNotAtOrigin);
        }
        let mut previous_tick = None;
        for event in tempo_events {
            if event.microseconds_per_quarter == 0
                || event.microseconds_per_quarter > MAX_TEMPO_MICROSECONDS_PER_QUARTER
            {
                return Err(TimelineError::InvalidTempo);
            }
            if previous_tick.is_some_and(|tick| event.tick <= tick) {
                return Err(TimelineError::TempoNotStrictlyIncreasing);
            }
            previous_tick = Some(event.tick);
        }
        Ok(Self {
            sample_rate_hz,
            tempo_events,
        })
    }

    #[must_use]
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    /// Converts an absolute project tick to the containing sample frame.
    ///
    /// Exact rational numerators are accumulated across tempo boundaries and
    /// floored only once at the final absolute position.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError`] if checked arithmetic or platform frame
    /// conversion fails.
    pub fn tick_to_frame(self, tick: u64) -> Result<usize, TimelineError> {
        let numerator = self.exact_frame_numerator(tick)?;
        let denominator = u128::from(TICKS_PER_QUARTER) * MICROSECONDS_PER_SECOND;
        usize::try_from(numerator / denominator).map_err(|_| TimelineError::FrameOutOfRange)
    }

    /// Converts an absolute project tick to the first sample frame whose start
    /// is at or after that tick.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError`] if checked arithmetic or platform frame
    /// conversion fails.
    pub fn tick_to_frame_ceil(self, tick: u64) -> Result<usize, TimelineError> {
        let numerator = self.exact_frame_numerator(tick)?;
        let denominator = u128::from(TICKS_PER_QUARTER) * MICROSECONDS_PER_SECOND;
        let quotient = numerator / denominator;
        let rounded = if numerator % denominator == 0 {
            quotient
        } else {
            quotient
                .checked_add(1)
                .ok_or(TimelineError::ArithmeticOverflow)?
        };
        usize::try_from(rounded).map_err(|_| TimelineError::FrameOutOfRange)
    }

    fn exact_frame_numerator(self, tick: u64) -> Result<u128, TimelineError> {
        let mut numerator = 0_u128;
        let mut cursor_tick = 0_u64;
        let mut tempo_index = 0_usize;

        while tempo_index + 1 < self.tempo_events.len()
            && self.tempo_events[tempo_index + 1].tick <= tick
        {
            let current = self.tempo_events[tempo_index];
            let next_tick = self.tempo_events[tempo_index + 1].tick;
            numerator = add_timeline_segment(
                numerator,
                next_tick - cursor_tick,
                current.microseconds_per_quarter,
                self.sample_rate_hz,
            )?;
            cursor_tick = next_tick;
            tempo_index += 1;
        }

        add_timeline_segment(
            numerator,
            tick - cursor_tick,
            self.tempo_events[tempo_index].microseconds_per_quarter,
            self.sample_rate_hz,
        )
    }

    fn tempo_at(self, tick: u64) -> TempoEvent {
        let index = self
            .tempo_events
            .partition_point(|event| event.tick <= tick)
            .saturating_sub(1);
        self.tempo_events[index]
    }

    fn placement_uses_rate(
        self,
        start_tick: u64,
        end_tick: u64,
        clip_duration_ticks: u64,
        clip_frame_count: usize,
    ) -> Result<bool, CompileError> {
        if !tempo_matches_clip_rate(
            self.sample_rate_hz,
            self.tempo_at(start_tick).microseconds_per_quarter,
            clip_duration_ticks,
            clip_frame_count,
        )? {
            return Ok(false);
        }
        for event in self.tempo_events {
            if event.tick > start_tick
                && event.tick < end_tick
                && !tempo_matches_clip_rate(
                    self.sample_rate_hz,
                    event.microseconds_per_quarter,
                    clip_duration_ticks,
                    clip_frame_count,
                )?
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn add_timeline_segment(
    numerator: u128,
    ticks: u64,
    microseconds_per_quarter: u32,
    sample_rate_hz: u32,
) -> Result<u128, TimelineError> {
    let segment = u128::from(ticks)
        .checked_mul(u128::from(microseconds_per_quarter))
        .and_then(|value| value.checked_mul(u128::from(sample_rate_hz)))
        .ok_or(TimelineError::ArithmeticOverflow)?;
    numerator
        .checked_add(segment)
        .ok_or(TimelineError::ArithmeticOverflow)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WarpMarker {
    pub tick: u64,
    pub source_frame_offset: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TickRange {
    pub start: u64,
    pub end: u64,
}

/// Time authority for an audio clip.
///
/// Musical clips derive their rendered duration from the project tempo map.
/// Sample-locked clips preserve their decoded source-frame duration exactly;
/// their tick duration is presentation metadata only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipTimebase {
    Musical,
    SampleLocked,
}

#[derive(Clone, Copy, Debug)]
pub struct ClipCompileSpec<'a> {
    pub enabled: bool,
    pub timebase: ClipTimebase,
    pub duration_ticks: u64,
    pub source_start_frame: usize,
    pub source_frame_count: usize,
    pub asset_frame_count: usize,
    pub source_sample_rate_hz: u32,
    pub loop_enabled: bool,
    pub loop_range: TickRange,
    pub reverse: bool,
    pub gain_decibels: f64,
    pub transpose_cents: i64,
    pub fade_in_ticks: u64,
    pub fade_out_ticks: u64,
    pub warp_markers: &'a [WarpMarker],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlacementCompileSpec {
    pub enabled: bool,
    pub start_tick: u64,
    pub clip_start_tick: u64,
    pub duration_ticks: u64,
    /// Constant decibel offset applied only by this arrangement placement.
    /// The compiler combines it with the reusable clip gain before building
    /// the one existing native clip-render plan.
    pub gain_decibels: f64,
    /// Linear-amplitude fade-in from this arrangement placement's start edge.
    pub fade_in_ticks: u64,
    /// Linear-amplitude fade-out from this arrangement placement's end edge.
    pub fade_out_ticks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileError {
    InvalidTimeline(TimelineError),
    InvalidClipDuration,
    InvalidSourceRange,
    InvalidLoopRange,
    InvalidFades,
    InvalidGain,
    InvalidPlacementGain,
    InvalidPlacementFades,
    InvalidWarpMap,
    InvalidPlacement,
    NonIntegralFrameBoundary,
    ArithmeticOverflow,
    FrameOutOfRange,
    ConsolidationRequired(ConsolidationReason),
}

impl fmt::Display for CompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTimeline(error) => write!(formatter, "timeline is invalid: {error}"),
            Self::InvalidClipDuration => formatter.write_str("clip duration must be positive"),
            Self::InvalidSourceRange => formatter.write_str("clip source range is invalid"),
            Self::InvalidLoopRange => formatter.write_str("clip loop range is invalid"),
            Self::InvalidFades => formatter.write_str("clip fade ranges are invalid"),
            Self::InvalidGain => formatter.write_str("effective clip gain must be finite"),
            Self::InvalidPlacementGain => formatter.write_str("placement gain must be finite"),
            Self::InvalidPlacementFades => formatter.write_str("placement fade ranges are invalid"),
            Self::InvalidWarpMap => formatter.write_str("clip warp map is invalid"),
            Self::InvalidPlacement => formatter.write_str("clip placement is invalid"),
            Self::NonIntegralFrameBoundary => {
                formatter.write_str("clip tick boundary does not map to an integer source frame")
            }
            Self::ArithmeticOverflow => {
                formatter.write_str("clip compilation arithmetic overflowed")
            }
            Self::FrameOutOfRange => {
                formatter.write_str("compiled frame is outside the platform range")
            }
            Self::ConsolidationRequired(reason) => {
                write!(formatter, "clip requires consolidation: {reason:?}")
            }
        }
    }
}

impl Error for CompileError {}

impl From<TimelineError> for CompileError {
    fn from(error: TimelineError) -> Self {
        match error {
            TimelineError::ArithmeticOverflow => Self::ArithmeticOverflow,
            TimelineError::FrameOutOfRange => Self::FrameOutOfRange,
            TimelineError::InvalidSampleRate
            | TimelineError::EmptyTempoMap
            | TimelineError::FirstTempoNotAtOrigin
            | TimelineError::InvalidTempo
            | TimelineError::TempoNotStrictlyIncreasing
            | TimelineError::EmptyMeterMap => Self::InvalidTimeline(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledPlacement {
    arrangement_start_frame: usize,
    arrangement_end_frame: usize,
    expected_source_sample_rate_hz: u32,
    expected_source_frame_count: usize,
    clip_spec: NativeClipSpec,
    placement_spec: PlacementSpec,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledSceneAudioClip {
    launch_frame: usize,
    expected_source_sample_rate_hz: u32,
    expected_source_frame_count: usize,
    clip_spec: NativeClipSpec,
    placement_spec: PlacementSpec,
}

impl CompiledSceneAudioClip {
    #[must_use]
    pub const fn launch_frame(self) -> usize {
        self.launch_frame
    }

    #[must_use]
    pub const fn is_looping(self) -> bool {
        self.clip_spec.loop_range.is_some()
    }

    /// Binds the compiled launch clip to its verified decoded source.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError`] for decoded-media mismatches or an unexpected
    /// native clip-plan failure.
    pub fn prepare(
        self,
        source: &DecodedAudio,
    ) -> Result<(ClipRenderPlan<'_>, ClipPlacement), PrepareError> {
        if source.sample_rate_hz() != self.expected_source_sample_rate_hz {
            return Err(PrepareError::SourceSampleRateMismatch {
                expected: self.expected_source_sample_rate_hz,
                actual: source.sample_rate_hz(),
            });
        }
        if source.frame_count() != self.expected_source_frame_count {
            return Err(PrepareError::SourceFrameCountMismatch {
                expected: self.expected_source_frame_count,
                actual: source.frame_count(),
            });
        }
        let plan = ClipRenderPlan::try_new(source, self.clip_spec).map_err(PrepareError::Clip)?;
        let placement = plan
            .place(self.placement_spec)
            .map_err(PrepareError::Clip)?;
        Ok((plan, placement))
    }
}

impl CompiledPlacement {
    #[must_use]
    pub const fn arrangement_start_frame(self) -> usize {
        self.arrangement_start_frame
    }

    #[must_use]
    pub const fn arrangement_end_frame(self) -> usize {
        self.arrangement_end_frame
    }

    #[must_use]
    pub const fn frame_count(self) -> usize {
        self.placement_spec.frame_count
    }

    #[must_use]
    pub const fn clip_spec(self) -> NativeClipSpec {
        self.clip_spec
    }

    #[must_use]
    pub const fn placement_spec(self) -> PlacementSpec {
        self.placement_spec
    }

    /// Binds compiled metadata to decoded media and validates the clip plan.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareError`] when decoded media has the wrong rate or the
    /// clip engine rejects the compiled source, gain, or placement.
    pub fn prepare(
        self,
        source: &DecodedAudio,
    ) -> Result<(ClipRenderPlan<'_>, ClipPlacement), PrepareError> {
        if source.sample_rate_hz() != self.expected_source_sample_rate_hz {
            return Err(PrepareError::SourceSampleRateMismatch {
                expected: self.expected_source_sample_rate_hz,
                actual: source.sample_rate_hz(),
            });
        }
        if source.frame_count() != self.expected_source_frame_count {
            return Err(PrepareError::SourceFrameCountMismatch {
                expected: self.expected_source_frame_count,
                actual: source.frame_count(),
            });
        }
        let plan = ClipRenderPlan::try_new(source, self.clip_spec).map_err(PrepareError::Clip)?;
        let placement = plan
            .place(self.placement_spec)
            .map_err(PrepareError::Clip)?;
        Ok((plan, placement))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareError {
    SourceSampleRateMismatch { expected: u32, actual: u32 },
    SourceFrameCountMismatch { expected: usize, actual: usize },
    Clip(ClipPlanError),
}

impl fmt::Display for PrepareError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceSampleRateMismatch { expected, actual } => write!(
                formatter,
                "decoded source rate is {actual} Hz; expected {expected} Hz"
            ),
            Self::SourceFrameCountMismatch { expected, actual } => write!(
                formatter,
                "decoded source has {actual} frames; expected {expected} frames"
            ),
            Self::Clip(error) => error.fmt(formatter),
        }
    }
}

impl Error for PrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceSampleRateMismatch { .. } | Self::SourceFrameCountMismatch { .. } => None,
            Self::Clip(error) => Some(error),
        }
    }
}

/// Compiles one verified clip placement into the native frame-domain engine.
///
/// This compiler intentionally accepts typed data and does not assert package
/// provenance.
/// It does not parse JSON or resolve project identifiers and assets.
///
/// # Errors
///
/// Structural violations return a specific [`CompileError`]. Valid clips that
/// need transpose, resampling, or time stretching return
/// [`CompileError::ConsolidationRequired`] rather than an approximation.
pub fn compile_placement(
    timeline: Timeline<'_>,
    clip: ClipCompileSpec<'_>,
    placement: PlacementCompileSpec,
) -> Result<CompiledPlacement, CompileError> {
    validate_clip(clip)?;
    validate_placement(clip, placement)?;
    let effective_gain_decibels = clip.gain_decibels + placement.gain_decibels;
    if !effective_gain_decibels.is_finite() {
        return Err(CompileError::InvalidGain);
    }
    let clip = ClipCompileSpec {
        gain_decibels: effective_gain_decibels,
        ..clip
    };

    if clip.transpose_cents != 0 {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::Transposition,
        ));
    }
    if clip.source_sample_rate_hz != timeline.sample_rate_hz() {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::Resampling,
        ));
    }
    if clip.timebase == ClipTimebase::SampleLocked {
        return compile_sample_locked_placement(timeline, clip, placement);
    }
    if !warp_is_linear(clip)? {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        ));
    }

    let placement_end_tick = placement
        .start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    if !timeline.placement_uses_rate(
        placement.start_tick,
        placement_end_tick,
        clip.duration_ticks,
        clip.source_frame_count,
    )? {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        ));
    }

    let source_end = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let clip_start_frame = clip_unfolded_tick_to_frame(clip, placement.clip_start_tick)?;
    let frame_count = clip_tick_span_to_frames(clip, placement.duration_ticks)?;
    let arrangement_start_frame = timeline.tick_to_frame(placement.start_tick)?;
    let arrangement_end_frame = timeline.tick_to_frame(placement_end_tick)?;
    let arrangement_frame_count = arrangement_end_frame
        .checked_sub(arrangement_start_frame)
        .ok_or(CompileError::ArithmeticOverflow)?;
    if arrangement_frame_count != frame_count {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        ));
    }

    let loop_range = compiled_loop_range(clip)?;
    let fade_in_frames = clip_tick_span_to_frames(clip, clip.fade_in_ticks)?;
    let fade_out_frames = clip_tick_span_to_frames(clip, clip.fade_out_ticks)?;
    let (placement_fade_in_frames, placement_fade_out_frames) =
        placement_fade_tick_spans_to_frames(
            timeline,
            placement,
            placement_end_tick,
            arrangement_start_frame,
            arrangement_end_frame,
        )?;

    Ok(CompiledPlacement {
        arrangement_start_frame,
        arrangement_end_frame,
        expected_source_sample_rate_hz: clip.source_sample_rate_hz,
        expected_source_frame_count: clip.asset_frame_count,
        clip_spec: NativeClipSpec {
            enabled: clip.enabled && placement.enabled,
            source: FrameRange {
                start: clip.source_start_frame,
                end: source_end,
            },
            loop_range,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            fade_in_frames,
            fade_out_frames,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
        placement_spec: PlacementSpec {
            clip_start_frame,
            frame_count,
            fade_in_frames: placement_fade_in_frames,
            fade_out_frames: placement_fade_out_frames,
        },
    })
}

fn placement_fade_tick_spans_to_frames(
    timeline: Timeline<'_>,
    placement: PlacementCompileSpec,
    placement_end_tick: u64,
    arrangement_start_frame: usize,
    arrangement_end_frame: usize,
) -> Result<(usize, usize), CompileError> {
    let fade_in_end_tick = placement
        .start_tick
        .checked_add(placement.fade_in_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let fade_in_frames = timeline
        .tick_to_frame(fade_in_end_tick)?
        .checked_sub(arrangement_start_frame)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let fade_out_start_tick = placement_end_tick
        .checked_sub(placement.fade_out_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let fade_out_frames = arrangement_end_frame
        .checked_sub(timeline.tick_to_frame(fade_out_start_tick)?)
        .ok_or(CompileError::ArithmeticOverflow)?;
    Ok((fade_in_frames, fade_out_frames))
}

fn compiled_loop_range(clip: ClipCompileSpec<'_>) -> Result<Option<FrameRange>, CompileError> {
    if !clip.loop_enabled {
        return Ok(None);
    }
    Ok(Some(FrameRange {
        start: clip_tick_to_frame(clip, clip.loop_range.start)?,
        end: clip_tick_to_frame(clip, clip.loop_range.end)?,
    }))
}

/// Compiles one clip for playback from a recorded scene launch.
///
/// Open-ended loops are accepted only when the native clip rate matches every
/// remaining tempo segment. This prevents a later tempo change from being
/// silently ignored.
///
/// # Errors
///
/// Returns [`CompileError`] for invalid clip data, arithmetic failure, or a
/// native execution requirement that needs consolidation.
pub fn compile_scene_audio_clip(
    timeline: Timeline<'_>,
    clip: ClipCompileSpec<'_>,
    launch_tick: u64,
) -> Result<CompiledSceneAudioClip, CompileError> {
    validate_clip(clip)?;
    if clip.transpose_cents != 0 {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::Transposition,
        ));
    }
    if clip.source_sample_rate_hz != timeline.sample_rate_hz() {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::Resampling,
        ));
    }
    if clip.timebase == ClipTimebase::SampleLocked {
        return compile_sample_locked_scene_audio_clip(timeline, clip, launch_tick);
    }
    if !warp_is_linear(clip)? {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        ));
    }
    let end_tick = if clip.loop_enabled {
        u64::MAX
    } else {
        launch_tick
            .checked_add(clip.duration_ticks)
            .ok_or(CompileError::ArithmeticOverflow)?
    };
    if !timeline.placement_uses_rate(
        launch_tick,
        end_tick,
        clip.duration_ticks,
        clip.source_frame_count,
    )? {
        return Err(CompileError::ConsolidationRequired(
            ConsolidationReason::TimeStretch,
        ));
    }
    let source_end = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let loop_range = compiled_loop_range(clip)?;
    let placement_frame_count = if clip.loop_enabled {
        usize::MAX
    } else {
        clip.source_frame_count
    };
    Ok(CompiledSceneAudioClip {
        launch_frame: timeline.tick_to_frame_ceil(launch_tick)?,
        expected_source_sample_rate_hz: clip.source_sample_rate_hz,
        expected_source_frame_count: clip.asset_frame_count,
        clip_spec: NativeClipSpec {
            enabled: clip.enabled,
            source: FrameRange {
                start: clip.source_start_frame,
                end: source_end,
            },
            loop_range,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            fade_in_frames: clip_tick_span_to_frames(clip, clip.fade_in_ticks)?,
            fade_out_frames: clip_tick_span_to_frames(clip, clip.fade_out_ticks)?,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
        placement_spec: PlacementSpec {
            clip_start_frame: 0,
            frame_count: placement_frame_count,
            fade_in_frames: 0,
            fade_out_frames: 0,
        },
    })
}

/// Compiles a reusable native-rate clip for interactive scene launches.
///
/// Because a live request may be quantized anywhere on the project timeline,
/// every declared tempo segment must preserve the clip's native tick-to-frame
/// rate. Projects requiring time-stretching remain explicit consolidation
/// work instead of being approximated in the realtime path.
///
/// # Errors
///
/// Returns [`CompileError`] for invalid clip data, arithmetic failure, or a
/// native execution requirement that needs consolidation.
pub fn compile_interactive_scene_audio_clip(
    timeline: Timeline<'_>,
    clip: ClipCompileSpec<'_>,
) -> Result<CompiledSceneAudioClip, CompileError> {
    validate_clip(clip)?;
    if clip.timebase == ClipTimebase::SampleLocked {
        return compile_scene_audio_clip(timeline, clip, 0);
    }
    for event in timeline.tempo_events {
        if !tempo_matches_clip_rate(
            timeline.sample_rate_hz,
            event.microseconds_per_quarter,
            clip.duration_ticks,
            clip.source_frame_count,
        )? {
            return Err(CompileError::ConsolidationRequired(
                ConsolidationReason::TimeStretch,
            ));
        }
    }
    compile_scene_audio_clip(timeline, clip, 0)
}

fn validate_clip(clip: ClipCompileSpec<'_>) -> Result<(), CompileError> {
    if clip.duration_ticks == 0 {
        return Err(CompileError::InvalidClipDuration);
    }
    if clip.source_frame_count == 0
        || clip.asset_frame_count == 0
        || clip.source_sample_rate_hz == 0
    {
        return Err(CompileError::InvalidSourceRange);
    }
    let source_end = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    if source_end > clip.asset_frame_count {
        return Err(CompileError::InvalidSourceRange);
    }
    if clip.loop_range.start >= clip.loop_range.end || clip.loop_range.end > clip.duration_ticks {
        return Err(CompileError::InvalidLoopRange);
    }
    if clip.fade_out_ticks > clip.duration_ticks
        || clip.fade_in_ticks > clip.duration_ticks - clip.fade_out_ticks
    {
        return Err(CompileError::InvalidFades);
    }
    if !clip.gain_decibels.is_finite() {
        return Err(CompileError::InvalidGain);
    }
    if clip.warp_markers.len() < 2 {
        return Err(CompileError::InvalidWarpMap);
    }
    let first = clip.warp_markers[0];
    let last = clip.warp_markers[clip.warp_markers.len() - 1];
    if first.tick != 0
        || first.source_frame_offset != 0
        || last.tick != clip.duration_ticks
        || last.source_frame_offset != clip.source_frame_count
    {
        return Err(CompileError::InvalidWarpMap);
    }
    for markers in clip.warp_markers.windows(2) {
        if markers[1].tick <= markers[0].tick
            || markers[1].source_frame_offset <= markers[0].source_frame_offset
        {
            return Err(CompileError::InvalidWarpMap);
        }
    }
    if clip.timebase == ClipTimebase::SampleLocked {
        if clip.loop_enabled {
            return Err(CompileError::InvalidLoopRange);
        }
        if clip.fade_in_ticks != 0 || clip.fade_out_ticks != 0 {
            return Err(CompileError::InvalidFades);
        }
        if !warp_is_linear(clip)? {
            return Err(CompileError::ConsolidationRequired(
                ConsolidationReason::TimeStretch,
            ));
        }
    }
    Ok(())
}

fn validate_placement(
    clip: ClipCompileSpec<'_>,
    placement: PlacementCompileSpec,
) -> Result<(), CompileError> {
    if !placement.gain_decibels.is_finite() {
        return Err(CompileError::InvalidPlacementGain);
    }
    if placement.duration_ticks == 0 {
        return Err(CompileError::InvalidPlacement);
    }
    if placement.fade_out_ticks > placement.duration_ticks
        || placement.fade_in_ticks > placement.duration_ticks - placement.fade_out_ticks
    {
        return Err(CompileError::InvalidPlacementFades);
    }
    placement
        .start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let clip_requested_end = placement
        .clip_start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    if !clip.loop_enabled && clip_requested_end > clip.duration_ticks {
        return Err(CompileError::InvalidPlacement);
    }
    if clip.timebase == ClipTimebase::SampleLocked
        && (placement.clip_start_tick != 0 || placement.duration_ticks != clip.duration_ticks)
    {
        return Err(CompileError::InvalidPlacement);
    }
    if clip.timebase == ClipTimebase::SampleLocked
        && (placement.fade_in_ticks != 0 || placement.fade_out_ticks != 0)
    {
        return Err(CompileError::InvalidPlacementFades);
    }
    Ok(())
}

fn compile_sample_locked_placement(
    timeline: Timeline<'_>,
    clip: ClipCompileSpec<'_>,
    placement: PlacementCompileSpec,
) -> Result<CompiledPlacement, CompileError> {
    let source_end = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let arrangement_start_frame = timeline.tick_to_frame_ceil(placement.start_tick)?;
    let arrangement_end_frame = arrangement_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    Ok(CompiledPlacement {
        arrangement_start_frame,
        arrangement_end_frame,
        expected_source_sample_rate_hz: clip.source_sample_rate_hz,
        expected_source_frame_count: clip.asset_frame_count,
        clip_spec: NativeClipSpec {
            enabled: clip.enabled && placement.enabled,
            source: FrameRange {
                start: clip.source_start_frame,
                end: source_end,
            },
            loop_range: None,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            fade_in_frames: 0,
            fade_out_frames: 0,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
        placement_spec: PlacementSpec {
            clip_start_frame: 0,
            frame_count: clip.source_frame_count,
            fade_in_frames: 0,
            fade_out_frames: 0,
        },
    })
}

fn compile_sample_locked_scene_audio_clip(
    timeline: Timeline<'_>,
    clip: ClipCompileSpec<'_>,
    launch_tick: u64,
) -> Result<CompiledSceneAudioClip, CompileError> {
    let source_end = clip
        .source_start_frame
        .checked_add(clip.source_frame_count)
        .ok_or(CompileError::ArithmeticOverflow)?;
    Ok(CompiledSceneAudioClip {
        launch_frame: timeline.tick_to_frame_ceil(launch_tick)?,
        expected_source_sample_rate_hz: clip.source_sample_rate_hz,
        expected_source_frame_count: clip.asset_frame_count,
        clip_spec: NativeClipSpec {
            enabled: clip.enabled,
            source: FrameRange {
                start: clip.source_start_frame,
                end: source_end,
            },
            loop_range: None,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            fade_in_frames: 0,
            fade_out_frames: 0,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
        placement_spec: PlacementSpec {
            clip_start_frame: 0,
            frame_count: clip.source_frame_count,
            fade_in_frames: 0,
            fade_out_frames: 0,
        },
    })
}

fn warp_is_linear(clip: ClipCompileSpec<'_>) -> Result<bool, CompileError> {
    let duration = u128::from(clip.duration_ticks);
    let frame_count =
        u128::try_from(clip.source_frame_count).map_err(|_| CompileError::FrameOutOfRange)?;
    for marker in clip.warp_markers {
        let marker_frame = u128::try_from(marker.source_frame_offset)
            .map_err(|_| CompileError::FrameOutOfRange)?;
        let left = marker_frame
            .checked_mul(duration)
            .ok_or(CompileError::ArithmeticOverflow)?;
        let right = u128::from(marker.tick)
            .checked_mul(frame_count)
            .ok_or(CompileError::ArithmeticOverflow)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn clip_tick_to_frame(clip: ClipCompileSpec<'_>, tick: u64) -> Result<usize, CompileError> {
    if tick > clip.duration_ticks {
        return Err(CompileError::InvalidPlacement);
    }
    exact_clip_ratio_frames(clip, tick)
}

fn clip_unfolded_tick_to_frame(
    clip: ClipCompileSpec<'_>,
    tick: u64,
) -> Result<usize, CompileError> {
    exact_clip_ratio_frames(clip, tick)
}

fn clip_tick_span_to_frames(clip: ClipCompileSpec<'_>, ticks: u64) -> Result<usize, CompileError> {
    exact_clip_ratio_frames(clip, ticks)
}

fn exact_clip_ratio_frames(clip: ClipCompileSpec<'_>, ticks: u64) -> Result<usize, CompileError> {
    let numerator = u128::from(ticks)
        .checked_mul(
            u128::try_from(clip.source_frame_count).map_err(|_| CompileError::FrameOutOfRange)?,
        )
        .ok_or(CompileError::ArithmeticOverflow)?;
    let denominator = u128::from(clip.duration_ticks);
    if numerator % denominator != 0 {
        return Err(CompileError::NonIntegralFrameBoundary);
    }
    usize::try_from(numerator / denominator).map_err(|_| CompileError::FrameOutOfRange)
}

fn tempo_matches_clip_rate(
    sample_rate_hz: u32,
    microseconds_per_quarter: u32,
    clip_duration_ticks: u64,
    clip_frame_count: usize,
) -> Result<bool, CompileError> {
    let left = u128::from(sample_rate_hz)
        .checked_mul(u128::from(microseconds_per_quarter))
        .and_then(|value| value.checked_mul(u128::from(clip_duration_ticks)))
        .ok_or(CompileError::ArithmeticOverflow)?;
    let right = u128::from(TICKS_PER_QUARTER)
        .checked_mul(MICROSECONDS_PER_SECOND)
        .and_then(|value| {
            u128::try_from(clip_frame_count)
                .ok()
                .and_then(|frames| value.checked_mul(frames))
        })
        .ok_or(CompileError::ArithmeticOverflow)?;
    Ok(left == right)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_musical_position_counts_bars_and_beats_from_one() {
        let four_four = [MeterEvent {
            tick: 0,
            numerator: 4,
            denominator: 4,
        }];
        assert_eq!(four_four[0].ticks_per_beat(), 1_920);
        assert_eq!(four_four[0].ticks_per_bar(), 7_680);

        let at = |tick| musical_position(&four_four, tick).unwrap();
        let origin = at(0);
        assert_eq!(
            (origin.bar(), origin.beat(), origin.tick()),
            (1, 1, 0),
            "the origin is bar one beat one, not bar zero"
        );
        let second_beat = at(1_920);
        assert_eq!(
            (second_beat.bar(), second_beat.beat(), second_beat.tick()),
            (1, 2, 0)
        );
        let second_bar = at(7_680);
        assert_eq!(
            (second_bar.bar(), second_bar.beat(), second_bar.tick()),
            (2, 1, 0)
        );
        let inside = at(7_680 + 1_920 + 960);
        assert_eq!(
            (inside.bar(), inside.beat(), inside.tick()),
            (2, 2, 960),
            "the tick remainder inside a beat is zero-based"
        );
    }

    #[test]
    fn a_musical_position_follows_the_meter_map_across_a_change() {
        let events = [
            MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            },
            MeterEvent {
                tick: 15_360,
                numerator: 3,
                denominator: 4,
            },
        ];
        assert_eq!(
            events[1].ticks_per_bar(),
            5_760,
            "a 3/4 bar is three quarters"
        );

        let at = |tick| musical_position(&events, tick).unwrap();
        let boundary = at(15_360);
        assert_eq!(
            (boundary.bar(), boundary.beat()),
            (3, 1),
            "two 4/4 bars precede the change, so it begins bar three"
        );
        let after = at(15_360 + 5_760);
        assert_eq!(
            (after.bar(), after.beat()),
            (4, 1),
            "and the next bar arrives three quarters later, not four"
        );
        let third_beat = at(15_360 + 3_840);
        assert_eq!((third_beat.bar(), third_beat.beat()), (3, 3));
    }

    /// A meter change is only required to sit on a strictly increasing tick, not
    /// on a bar line. The bar-boundary case above cannot distinguish whether the
    /// walk stops at or before the change, so this places one mid-bar.
    #[test]
    fn a_meter_change_off_a_bar_line_takes_effect_at_its_own_tick() {
        let events = [
            MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            },
            MeterEvent {
                tick: 3_840,
                numerator: 3,
                denominator: 4,
            },
        ];
        let at = |tick| musical_position(&events, tick).unwrap();

        let change = at(3_840);
        assert_eq!(
            (change.bar(), change.beat(), change.tick()),
            (1, 1, 0),
            "half a 4/4 bar elapsed is zero whole bars, so the new meter starts              bar one again at its own tick"
        );
        let before = at(3_839);
        assert_eq!(
            (before.bar(), before.beat()),
            (1, 2),
            "and the tick before it is still counted by the old meter"
        );
        let after = at(3_840 + 5_760);
        assert_eq!(
            (after.bar(), after.beat()),
            (2, 1),
            "the first 3/4 bar completes three quarters after the change"
        );
    }

    #[test]
    fn musical_positions_resolve_back_to_unique_project_ticks() {
        let events = [
            MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            },
            MeterEvent {
                tick: 15_360,
                numerator: 3,
                denominator: 4,
            },
        ];

        assert_eq!(
            tick_at_musical_position(&events, 1, 1, 0),
            Ok(0),
            "the one-based origin round trips"
        );
        assert_eq!(
            tick_at_musical_position(&events, 2, 2, 960),
            Ok(10_560),
            "the 4/4 segment resolves its beat-local remainder"
        );
        assert_eq!(
            tick_at_musical_position(&events, 3, 1, 0),
            Ok(15_360),
            "a bar-boundary meter change has one exact owner"
        );
        assert_eq!(
            tick_at_musical_position(&events, 4, 3, 0),
            Ok(24_960),
            "the following 3/4 bar retains its own beat width"
        );

        for tick in [0, 1_920, 10_560, 15_360, 24_960] {
            let position = musical_position(&events, tick).unwrap();
            assert_eq!(
                tick_at_musical_position(&events, position.bar(), position.beat(), position.tick(),),
                Ok(tick),
                "every unambiguous displayed position round trips"
            );
        }
    }

    #[test]
    fn musical_position_resolution_observes_compound_meters() {
        let six_eight = [MeterEvent {
            tick: 0,
            numerator: 6,
            denominator: 8,
        }];

        assert_eq!(tick_at_musical_position(&six_eight, 1, 6, 0), Ok(4_800));
        assert_eq!(tick_at_musical_position(&six_eight, 2, 1, 0), Ok(5_760));
        assert_eq!(
            tick_at_musical_position(&six_eight, 1, 7, 0),
            Err(MusicalPositionResolveError::InvalidPosition)
        );
        assert_eq!(
            tick_at_musical_position(&six_eight, 1, 1, 960),
            Err(MusicalPositionResolveError::InvalidPosition)
        );
    }

    #[test]
    fn musical_position_resolution_skips_finite_nonowners_before_checked_arithmetic() {
        let events = [
            MeterEvent {
                tick: 0,
                numerator: 255,
                denominator: 1,
            },
            MeterEvent {
                tick: 1,
                numerator: 1,
                denominator: 256,
            },
        ];

        assert_eq!(
            tick_at_musical_position(&events, 300_000_000_000_000, 1, 0),
            Ok(8_999_999_999_999_971),
            "a finite earlier segment must not overflow while rejecting a later segment's valid position"
        );

        let final_partial_bar = [
            MeterEvent {
                tick: 0,
                numerator: 255,
                denominator: 1,
            },
            MeterEvent {
                tick: u64::MAX - 10_000,
                numerator: 255,
                denominator: 256,
            },
        ];
        let expected_tick = u64::MAX - 2_351;
        let later_position = musical_position(&final_partial_bar, expected_tick)
            .expect("the later segment remains within the exact tick domain");
        assert_eq!(
            (
                later_position.bar(),
                later_position.beat(),
                later_position.tick()
            ),
            (9_419_293_338_292, 255, 29)
        );
        assert_eq!(
            tick_at_musical_position(&final_partial_bar, 9_419_293_338_292, 255, 29),
            Ok(expected_tick),
            "a finite final partial bar must be rejected before its non-owning coordinate can overflow"
        );
    }

    #[test]
    fn musical_position_resolution_refuses_off_bar_meter_ambiguity() {
        let events = [
            MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            },
            MeterEvent {
                tick: 3_840,
                numerator: 3,
                denominator: 4,
            },
        ];

        assert_eq!(
            tick_at_musical_position(&events, 1, 1, 0),
            Err(MusicalPositionResolveError::AmbiguousPosition),
            "the origin label is reused at the off-bar change and cannot choose a tick"
        );
        assert_eq!(
            tick_at_musical_position(&events, 1, 3, 0),
            Ok(7_680),
            "a coordinate present only after the change remains usable"
        );
    }

    #[test]
    fn musical_position_resolution_refuses_invalid_maps_and_checked_overflow() {
        let valid = [MeterEvent {
            tick: 0,
            numerator: 4,
            denominator: 4,
        }];
        assert_eq!(
            tick_at_musical_position(&[], 1, 1, 0),
            Err(MusicalPositionResolveError::EmptyMeterMap)
        );
        assert_eq!(
            tick_at_musical_position(
                &[MeterEvent {
                    tick: 1,
                    numerator: 4,
                    denominator: 4,
                }],
                1,
                1,
                0,
            ),
            Err(MusicalPositionResolveError::InvalidMeterMap)
        );
        assert_eq!(
            tick_at_musical_position(
                &[
                    MeterEvent {
                        tick: 0,
                        numerator: 4,
                        denominator: 4,
                    },
                    MeterEvent {
                        tick: 0,
                        numerator: 3,
                        denominator: 4,
                    },
                ],
                1,
                1,
                0,
            ),
            Err(MusicalPositionResolveError::InvalidMeterMap)
        );
        assert_eq!(
            tick_at_musical_position(
                &[MeterEvent {
                    tick: 0,
                    numerator: 4,
                    denominator: 3,
                }],
                1,
                1,
                0,
            ),
            Err(MusicalPositionResolveError::InvalidMeterMap)
        );
        assert_eq!(
            tick_at_musical_position(&valid, 0, 1, 0),
            Err(MusicalPositionResolveError::InvalidPosition)
        );
        assert_eq!(
            tick_at_musical_position(&valid, u64::MAX, 1, 0),
            Err(MusicalPositionResolveError::ArithmeticOverflow)
        );
    }

    #[test]
    fn a_frame_reads_as_the_bar_and_beat_the_tempo_places_it_on() {
        let tempo = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_000,
        }];
        let meter = [MeterEvent {
            tick: 0,
            numerator: 4,
            denominator: 4,
        }];
        let at = |frame| musical_position_at_frame(48_000, &tempo, &meter, frame).unwrap();

        let origin = at(0);
        assert_eq!((origin.bar(), origin.beat()), (1, 1));

        // 120 bpm: a quarter is half a second, so 24000 frames is one beat.
        let beat_two = at(24_000);
        assert_eq!(
            (beat_two.bar(), beat_two.beat()),
            (1, 2),
            "one beat at 120 bpm is 24000 frames at 48 kHz"
        );
        let bar_two = at(96_000);
        assert_eq!(
            (bar_two.bar(), bar_two.beat()),
            (2, 1),
            "and a 4/4 bar is four of them"
        );

        let faster = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 250_000,
        }];
        let doubled = musical_position_at_frame(48_000, &faster, &meter, 96_000).unwrap();
        assert_eq!(
            (doubled.bar(), doubled.beat()),
            (3, 1),
            "at twice the tempo the same frame is twice as far through the music"
        );
    }

    #[test]
    fn a_compound_meter_counts_the_denominator_as_the_beat() {
        let six_eight = [MeterEvent {
            tick: 0,
            numerator: 6,
            denominator: 8,
        }];
        assert_eq!(
            six_eight[0].ticks_per_beat(),
            960,
            "an eighth is half a quarter"
        );
        assert_eq!(six_eight[0].ticks_per_bar(), 5_760);

        let sixth = musical_position(&six_eight, 4_800).unwrap();
        assert_eq!((sixth.bar(), sixth.beat()), (1, 6));
        let next_bar = musical_position(&six_eight, 5_760).unwrap();
        assert_eq!((next_bar.bar(), next_bar.beat()), (2, 1));

        assert_eq!(
            musical_position(&[], 0),
            Err(TimelineError::EmptyMeterMap),
            "a project with no meter has no musical position to show"
        );
    }

    use super::*;

    fn timeline(events: &[TempoEvent]) -> Timeline<'_> {
        Timeline::try_new(48_000, events).unwrap()
    }

    fn native_clip(markers: &[WarpMarker]) -> ClipCompileSpec<'_> {
        ClipCompileSpec {
            enabled: true,
            timebase: ClipTimebase::Musical,
            duration_ticks: 8,
            source_start_frame: 2,
            source_frame_count: 200,
            asset_frame_count: 202,
            source_sample_rate_hz: 48_000,
            loop_enabled: true,
            loop_range: TickRange { start: 2, end: 8 },
            reverse: false,
            gain_decibels: 0.0,
            transpose_cents: 0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            warp_markers: markers,
        }
    }

    fn placement() -> PlacementCompileSpec {
        PlacementCompileSpec {
            enabled: true,
            start_tick: 0,
            clip_start_tick: 0,
            duration_ticks: 10,
            gain_decibels: 0.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
        }
    }

    #[test]
    fn reproduces_exact_absolute_timeline_vectors() {
        let events = [
            TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_000,
            },
            TempoEvent {
                tick: 3_840,
                microseconds_per_quarter: 1_000_000,
            },
        ];
        let project_timeline = timeline(&events);
        assert_eq!(project_timeline.tick_to_frame(0), Ok(0));
        assert_eq!(project_timeline.tick_to_frame(480), Ok(6_000));
        assert_eq!(project_timeline.tick_to_frame(1_920), Ok(24_000));
        assert_eq!(project_timeline.tick_to_frame(3_840), Ok(48_000));
        assert_eq!(project_timeline.tick_to_frame(5_760), Ok(96_000));

        let fractional = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_001,
        }];
        let fractional = timeline(&fractional);
        assert_eq!(fractional.tick_to_frame(1), Ok(12));
        assert_eq!(fractional.tick_to_frame(1_920), Ok(24_000));

        let boundary = [
            TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_001,
            },
            TempoEvent {
                tick: 1,
                microseconds_per_quarter: 500_003,
            },
        ];
        assert_eq!(timeline(&boundary).tick_to_frame(2), Ok(25));
    }

    #[test]
    fn maps_sample_starts_to_exact_fractional_ticks() {
        let events = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_000,
        }];
        let frames = FrameTimeline::try_new(48_000, &events).unwrap();
        assert_eq!(
            frames.position_at_frame(0),
            Ok(FrameTickPosition {
                tick: 0,
                remainder: 0,
                denominator: 24_000_000_000,
            })
        );
        assert_eq!(
            frames.position_at_frame(12),
            Ok(FrameTickPosition {
                tick: 0,
                remainder: 23_040_000_000,
                denominator: 24_000_000_000,
            })
        );
        assert_eq!(frames.position_at_frame(13).unwrap().whole_tick(), 1);

        let decreasing = frames
            .position_at_frame(25)
            .unwrap()
            .interpolate_i32(0, 1_000_000, 3, 0)
            .unwrap();
        assert_eq!(decreasing, 333_334);
    }

    #[test]
    fn selects_tempo_by_the_exact_sample_start_boundary() {
        let events = [
            TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_001,
            },
            TempoEvent {
                tick: 1,
                microseconds_per_quarter: 1_000_000,
            },
        ];
        let frames = FrameTimeline::try_new(48_000, &events).unwrap();
        let before = frames.position_at_frame(12).unwrap();
        let after = frames.position_at_frame(13).unwrap();
        assert_eq!(before.whole_tick(), 0);
        assert_eq!(after.whole_tick(), 1);
        assert!(after.remainder > 0);
        assert_eq!(after.denominator, 48_000_000_000);
    }

    #[test]
    fn rejects_invalid_tempo_maps() {
        assert!(matches!(
            Timeline::try_new(
                0,
                &[TempoEvent {
                    tick: 0,
                    microseconds_per_quarter: 500_000
                }]
            ),
            Err(TimelineError::InvalidSampleRate)
        ));
        assert!(matches!(
            Timeline::try_new(48_000, &[]),
            Err(TimelineError::EmptyTempoMap)
        ));
        assert!(matches!(
            Timeline::try_new(
                48_000,
                &[TempoEvent {
                    tick: 1,
                    microseconds_per_quarter: 500_000
                }]
            ),
            Err(TimelineError::FirstTempoNotAtOrigin)
        ));
        assert!(matches!(
            Timeline::try_new(
                48_000,
                &[
                    TempoEvent {
                        tick: 0,
                        microseconds_per_quarter: 500_000
                    },
                    TempoEvent {
                        tick: 0,
                        microseconds_per_quarter: 600_000
                    }
                ]
            ),
            Err(TimelineError::TempoNotStrictlyIncreasing)
        ));
    }

    #[test]
    fn compiles_exact_native_boundaries() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 1_000_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 4,
                source_frame_offset: 100,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        let mut clip = native_clip(&markers);
        clip.fade_in_ticks = 2;
        clip.fade_out_ticks = 2;
        let offset_placement = PlacementCompileSpec {
            enabled: true,
            start_tick: 8,
            clip_start_tick: 2,
            duration_ticks: 10,
            gain_decibels: 0.0,
            fade_in_ticks: 2,
            fade_out_ticks: 4,
        };
        let compiled = compile_placement(timeline(&tempos), clip, offset_placement).unwrap();
        assert_eq!(compiled.arrangement_start_frame(), 200);
        assert_eq!(compiled.frame_count(), 250);
        assert_eq!(
            compiled.clip_spec(),
            NativeClipSpec {
                enabled: true,
                source: FrameRange { start: 2, end: 202 },
                loop_range: Some(FrameRange {
                    start: 50,
                    end: 200
                }),
                reverse: false,
                gain_decibels: 0.0,
                fade_in_frames: 50,
                fade_out_frames: 50,
                transpose_cents: 0,
                time_mapping: TimeMapping::NativeRate,
            }
        );
        assert_eq!(
            compiled.placement_spec(),
            PlacementSpec {
                clip_start_frame: 50,
                frame_count: 250,
                fade_in_frames: 50,
                fade_out_frames: 100,
            }
        );

        let unfolded = compile_placement(
            timeline(&tempos),
            clip,
            PlacementCompileSpec {
                clip_start_tick: 10,
                ..offset_placement
            },
        )
        .unwrap();
        assert_eq!(
            unfolded.placement_spec(),
            PlacementSpec {
                clip_start_frame: 250,
                frame_count: 250,
                fade_in_frames: 50,
                fade_out_frames: 100,
            }
        );
    }

    #[test]
    fn classifies_every_unsupported_execution_mapping() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 1_000_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];

        let mut clip = native_clip(&markers);
        clip.transpose_cents = 1;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::ConsolidationRequired(
                ConsolidationReason::Transposition
            ))
        );

        clip.transpose_cents = 0;
        clip.source_sample_rate_hz = 44_100;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::ConsolidationRequired(
                ConsolidationReason::Resampling
            ))
        );

        clip.source_sample_rate_hz = 48_000;
        let nonlinear = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 4,
                source_frame_offset: 90,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        clip.warp_markers = &nonlinear;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::ConsolidationRequired(
                ConsolidationReason::TimeStretch
            ))
        );

        clip.warp_markers = &markers;
        let mismatched_tempo = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_000,
        }];
        assert_eq!(
            compile_placement(timeline(&mismatched_tempo), clip, placement()),
            Err(CompileError::ConsolidationRequired(
                ConsolidationReason::TimeStretch
            ))
        );
    }

    #[test]
    fn tempo_change_inside_placement_must_preserve_the_native_rate() {
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        let matching = [
            TempoEvent {
                tick: 0,
                microseconds_per_quarter: 1_000_000,
            },
            TempoEvent {
                tick: 5,
                microseconds_per_quarter: 1_000_000,
            },
        ];
        assert!(compile_placement(timeline(&matching), native_clip(&markers), placement()).is_ok());

        let changed = [
            TempoEvent {
                tick: 0,
                microseconds_per_quarter: 1_000_000,
            },
            TempoEvent {
                tick: 5,
                microseconds_per_quarter: 500_000,
            },
        ];
        assert_eq!(
            compile_placement(timeline(&changed), native_clip(&markers), placement()),
            Err(CompileError::ConsolidationRequired(
                ConsolidationReason::TimeStretch
            ))
        );
    }

    #[test]
    fn arrangement_placement_gain_combines_with_the_reusable_clip_gain_once() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 1_000_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        let mut clip = native_clip(&markers);
        clip.gain_decibels = -3.0;
        let mut placement = placement();
        placement.gain_decibels = 6.0;

        let compiled = compile_placement(timeline(&tempos), clip, placement).unwrap();
        assert!((compiled.clip_spec().gain_decibels - 3.0).abs() < f64::EPSILON);

        placement.gain_decibels = f64::NAN;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement),
            Err(CompileError::InvalidPlacementGain)
        );

        clip.gain_decibels = f64::MAX;
        placement.gain_decibels = f64::MAX;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement),
            Err(CompileError::InvalidGain)
        );
    }

    #[test]
    fn rejects_nonintegral_and_invalid_boundaries() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 500_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 2,
                source_frame_offset: 25,
            },
        ];
        let mut clip = ClipCompileSpec {
            duration_ticks: 2,
            source_frame_count: 25,
            loop_enabled: false,
            loop_range: TickRange { start: 0, end: 2 },
            warp_markers: &markers,
            ..native_clip(&markers)
        };
        let nonintegral = PlacementCompileSpec {
            enabled: true,
            start_tick: 0,
            clip_start_tick: 1,
            duration_ticks: 1,
            gain_decibels: 0.0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
        };
        assert_eq!(
            compile_placement(timeline(&tempos), clip, nonintegral),
            Err(CompileError::NonIntegralFrameBoundary)
        );

        clip.duration_ticks = 0;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, nonintegral),
            Err(CompileError::InvalidClipDuration)
        );
    }

    #[test]
    fn rejects_each_invalid_clip_and_placement_structure() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 1_000_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        let valid_clip = native_clip(&markers);

        let mut clip = valid_clip;
        clip.source_frame_count = 0;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidSourceRange)
        );

        clip = valid_clip;
        clip.asset_frame_count = 201;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidSourceRange)
        );

        clip = valid_clip;
        clip.loop_range.start = clip.loop_range.end;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidLoopRange)
        );

        clip = valid_clip;
        clip.fade_in_ticks = 5;
        clip.fade_out_ticks = 4;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidFades)
        );

        clip = valid_clip;
        clip.fade_out_ticks = clip.duration_ticks + 1;
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidFades)
        );

        let mut invalid_placement = placement();
        invalid_placement.fade_in_ticks = 7;
        invalid_placement.fade_out_ticks = 4;
        assert_eq!(
            compile_placement(timeline(&tempos), valid_clip, invalid_placement),
            Err(CompileError::InvalidPlacementFades)
        );

        let mut sample_locked_clip = valid_clip;
        sample_locked_clip.timebase = ClipTimebase::SampleLocked;
        sample_locked_clip.loop_enabled = false;
        let mut sample_locked_placement = placement();
        sample_locked_placement.duration_ticks = sample_locked_clip.duration_ticks;
        sample_locked_placement.fade_in_ticks = 1;
        assert_eq!(
            compile_placement(
                timeline(&tempos),
                sample_locked_clip,
                sample_locked_placement,
            ),
            Err(CompileError::InvalidPlacementFades)
        );

        clip = valid_clip;
        clip.warp_markers = &markers[..1];
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidWarpMap)
        );

        let mut invalid_placement = placement();
        invalid_placement.clip_start_tick = valid_clip.loop_range.end;
        let mut non_looping_clip = valid_clip;
        non_looping_clip.loop_enabled = false;
        assert_eq!(
            compile_placement(timeline(&tempos), non_looping_clip, invalid_placement),
            Err(CompileError::InvalidPlacement)
        );

        invalid_placement = placement();
        invalid_placement.start_tick = u64::MAX;
        assert_eq!(
            compile_placement(timeline(&tempos), valid_clip, invalid_placement),
            Err(CompileError::ArithmeticOverflow)
        );
    }

    #[test]
    fn validates_decoded_source_rate_and_length_during_preparation() {
        let tempos = [TempoEvent {
            tick: 0,
            microseconds_per_quarter: 1_000_000,
        }];
        let markers = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 8,
                source_frame_offset: 200,
            },
        ];
        let compiled =
            compile_placement(timeline(&tempos), native_clip(&markers), placement()).unwrap();
        let source = DecodedAudio::from_mono(44_100, vec![0.0; 202]).unwrap();
        assert_eq!(
            compiled.prepare(&source).unwrap_err(),
            PrepareError::SourceSampleRateMismatch {
                expected: 48_000,
                actual: 44_100
            }
        );

        let source = DecodedAudio::from_mono(48_000, vec![0.0; 201]).unwrap();
        assert_eq!(
            compiled.prepare(&source).unwrap_err(),
            PrepareError::SourceFrameCountMismatch {
                expected: 202,
                actual: 201
            }
        );
    }
}
