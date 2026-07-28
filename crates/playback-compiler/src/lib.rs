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

pub const TICKS_PER_QUARTER: u64 = 1_920;
const MICROSECONDS_PER_SECOND: u128 = 1_000_000;
const MAX_TEMPO_MICROSECONDS_PER_QUARTER: u32 = 16_777_215;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TempoEvent {
    pub tick: u64,
    pub microseconds_per_quarter: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimelineError {
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
#[derive(Clone, Debug)]
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

#[derive(Clone, Copy, Debug)]
pub struct ClipCompileSpec<'a> {
    pub enabled: bool,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlacementCompileSpec {
    pub enabled: bool,
    pub start_tick: u64,
    pub clip_start_tick: u64,
    pub duration_ticks: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompileError {
    InvalidTimeline(TimelineError),
    InvalidClipDuration,
    InvalidSourceRange,
    InvalidLoopRange,
    InvalidFades,
    InvalidGain,
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
            Self::InvalidGain => formatter.write_str("clip gain must be finite"),
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
            | TimelineError::TempoNotStrictlyIncreasing => Self::InvalidTimeline(error),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompiledPlacement {
    arrangement_start_frame: usize,
    expected_source_sample_rate_hz: u32,
    expected_source_frame_count: usize,
    clip_spec: NativeClipSpec,
    placement_spec: PlacementSpec,
}

impl CompiledPlacement {
    #[must_use]
    pub const fn arrangement_start_frame(self) -> usize {
        self.arrangement_start_frame
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
/// This compiler intentionally accepts typed, already package-verified data.
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
    let clip_start_frame = clip_tick_to_frame(clip, placement.clip_start_tick)?;
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

    let loop_range = if clip.loop_enabled {
        Some(FrameRange {
            start: clip_tick_to_frame(clip, clip.loop_range.start)?,
            end: clip_tick_to_frame(clip, clip.loop_range.end)?,
        })
    } else {
        None
    };
    let fade_in_frames = clip_tick_span_to_frames(clip, clip.fade_in_ticks)?;
    let fade_out_frames = clip_tick_span_to_frames(clip, clip.fade_out_ticks)?;

    Ok(CompiledPlacement {
        arrangement_start_frame,
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
        },
    })
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
    if clip.fade_in_ticks > clip.duration_ticks.saturating_sub(clip.fade_out_ticks) {
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
    Ok(())
}

fn validate_placement(
    clip: ClipCompileSpec<'_>,
    placement: PlacementCompileSpec,
) -> Result<(), CompileError> {
    if placement.duration_ticks == 0 {
        return Err(CompileError::InvalidPlacement);
    }
    placement
        .start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    let clip_requested_end = placement
        .clip_start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(CompileError::ArithmeticOverflow)?;
    if clip.loop_enabled {
        if placement.clip_start_tick >= clip.loop_range.end {
            return Err(CompileError::InvalidPlacement);
        }
    } else if clip_requested_end > clip.duration_ticks {
        return Err(CompileError::InvalidPlacement);
    }
    Ok(())
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
    use super::*;

    fn timeline(events: &[TempoEvent]) -> Timeline<'_> {
        Timeline::try_new(48_000, events).unwrap()
    }

    fn native_clip(markers: &[WarpMarker]) -> ClipCompileSpec<'_> {
        ClipCompileSpec {
            enabled: true,
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
                frame_count: 250
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
        clip.warp_markers = &markers[..1];
        assert_eq!(
            compile_placement(timeline(&tempos), clip, placement()),
            Err(CompileError::InvalidWarpMap)
        );

        let mut invalid_placement = placement();
        invalid_placement.clip_start_tick = valid_clip.loop_range.end;
        assert_eq!(
            compile_placement(timeline(&tempos), valid_clip, invalid_placement),
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
