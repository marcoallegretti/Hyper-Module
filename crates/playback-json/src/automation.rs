use core::error::Error;
use core::fmt;

use hyper_playback_compiler::{FrameTickPosition, FrameTimeline, FrameTimelineError};
use serde::Deserialize;
use serde::de::IgnoredAny;

use super::{
    CollectionLimit, CompiledMixerGraph, DocumentKind, DocumentLocation, LoadError, LoadedMixer,
    LoadedPlaybackProject, LoaderLimits, TrackKind, capacity_error, domain_error,
    enforce_loader_limit, parse_document, project_contains_identity, validate_header,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationChannelParameter {
    Gain,
    Pan,
    Mute,
    Solo,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationSendParameter {
    Gain,
    Pan,
    Enabled,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationMainParameter {
    Gain,
    Mute,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AutomationTarget {
    Channel {
        slot: usize,
        parameter: AutomationChannelParameter,
    },
    Send {
        slot: usize,
        send: usize,
        parameter: AutomationSendParameter,
    },
    Main {
        parameter: AutomationMainParameter,
    },
    Cue {
        parameter: AutomationMainParameter,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationInterpolation {
    Hold,
    Linear,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationValue {
    Integer(i32),
    Boolean(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationEvaluationError {
    TickOutOfRange,
}

impl fmt::Display for AutomationEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("automation evaluation tick exceeds the interoperable integer domain")
    }
}

impl Error for AutomationEvaluationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomationPoint {
    tick: u64,
    value: AutomationValue,
}

impl AutomationPoint {
    #[must_use]
    pub const fn tick(self) -> u64 {
        self.tick
    }

    #[must_use]
    pub const fn value(self) -> AutomationValue {
        self.value
    }
}

#[derive(Clone, Debug)]
pub struct AutomationLane {
    id: String,
    target: AutomationTarget,
    interpolation: AutomationInterpolation,
    points: Vec<AutomationPoint>,
}

impl AutomationLane {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn target(&self) -> AutomationTarget {
        self.target
    }

    #[must_use]
    pub const fn interpolation(&self) -> AutomationInterpolation {
        self.interpolation
    }

    #[must_use]
    pub fn points(&self) -> &[AutomationPoint] {
        &self.points
    }

    /// Evaluates the published integer-tick automation function.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationEvaluationError::TickOutOfRange`] when `tick`
    /// exceeds the version 1 interoperable integer domain.
    pub fn value_at(&self, tick: u64) -> Result<AutomationValue, AutomationEvaluationError> {
        if tick > super::MAX_SAFE_INTEGER {
            return Err(AutomationEvaluationError::TickOutOfRange);
        }
        let first = self.points[0];
        if tick <= first.tick {
            return Ok(first.value);
        }
        let right_index = self.points.partition_point(|point| point.tick <= tick);
        if right_index == self.points.len() {
            return Ok(self.points[self.points.len() - 1].value);
        }
        let left = self.points[right_index - 1];
        let right = self.points[right_index];
        if self.interpolation == AutomationInterpolation::Hold {
            return Ok(left.value);
        }
        let (AutomationValue::Integer(left_value), AutomationValue::Integer(right_value)) =
            (left.value, right.value)
        else {
            unreachable!("validated linear lanes contain only integer points");
        };
        let elapsed = i128::from(tick - left.tick);
        let duration = i128::from(right.tick - left.tick);
        let delta = i128::from(right_value) - i128::from(left_value);
        let value = i128::from(left_value) + delta * elapsed / duration;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "linear interpolation remains between two validated i32 endpoints"
        )]
        Ok(AutomationValue::Integer(value as i32))
    }
}

#[derive(Clone, Debug)]
pub struct AutomationTake {
    id: String,
    name: String,
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
    lanes: Vec<AutomationLane>,
}

impl AutomationTake {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn start_tick(&self) -> u64 {
        self.start_tick
    }

    #[must_use]
    pub const fn end_tick(&self) -> u64 {
        self.end_tick
    }

    #[must_use]
    pub const fn is_active_at(&self, tick: u64) -> bool {
        self.enabled && self.start_tick <= tick && tick < self.end_tick
    }

    #[must_use]
    pub fn lanes(&self) -> &[AutomationLane] {
        &self.lanes
    }
}

#[derive(Clone, Debug)]
pub struct LoadedAutomationSet {
    takes: Vec<AutomationTake>,
}

impl LoadedAutomationSet {
    #[must_use]
    pub fn takes(&self) -> &[AutomationTake] {
        &self.takes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationCompileError {
    CapacityUnavailable,
    UnsupportedCueTarget,
    TargetMismatch,
    Timeline(FrameTimelineError),
}

impl fmt::Display for AutomationCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("compiled automation capacity is unavailable")
            }
            Self::UnsupportedCueTarget => {
                formatter.write_str("cue automation is unavailable in the main-output runtime")
            }
            Self::TargetMismatch => {
                formatter.write_str("automation target does not match the compiled mixer graph")
            }
            Self::Timeline(source) => write!(formatter, "automation timeline failed: {source}"),
        }
    }
}

impl Error for AutomationCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::CapacityUnavailable | Self::UnsupportedCueTarget | Self::TargetMismatch => None,
        }
    }
}

#[derive(Clone, Debug)]
struct CompiledAutomationInterval {
    start_tick: u64,
    end_tick: u64,
    interpolation: AutomationInterpolation,
    points: Vec<AutomationPoint>,
}

impl CompiledAutomationInterval {
    fn value_at(&self, position: FrameTickPosition) -> Result<AutomationValue, FrameTimelineError> {
        let first = self.points[0];
        if position.is_before(first.tick) {
            return Ok(first.value);
        }
        let right_index = self
            .points
            .partition_point(|point| position.is_at_or_after(point.tick));
        if right_index == self.points.len() {
            return Ok(self.points[self.points.len() - 1].value);
        }
        let left = self.points[right_index - 1];
        let right = self.points[right_index];
        if self.interpolation == AutomationInterpolation::Hold {
            return Ok(left.value);
        }
        let (AutomationValue::Integer(left_value), AutomationValue::Integer(right_value)) =
            (left.value, right.value)
        else {
            unreachable!("validated linear lanes contain only integer points");
        };
        position
            .interpolate_i32(left.tick, left_value, right.tick, right_value)
            .map(AutomationValue::Integer)
    }
}

#[derive(Clone, Debug)]
struct CompiledAutomationTarget {
    target: AutomationTarget,
    intervals: Vec<CompiledAutomationInterval>,
}

impl CompiledAutomationTarget {
    fn value_at(
        &self,
        position: FrameTickPosition,
    ) -> Result<Option<AutomationValue>, FrameTimelineError> {
        let right = self
            .intervals
            .partition_point(|interval| position.is_at_or_after(interval.start_tick));
        let Some(index) = right.checked_sub(1) else {
            return Ok(None);
        };
        let interval = &self.intervals[index];
        if !position.is_before(interval.end_tick) {
            return Ok(None);
        }
        interval.value_at(position).map(Some)
    }
}

/// Precompiled enabled automation indexed by resolved mixer target.
#[derive(Clone, Debug)]
pub struct CompiledAutomation {
    timeline: FrameTimeline,
    targets: Vec<CompiledAutomationTarget>,
}

impl CompiledAutomation {
    #[must_use]
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    /// Visits every active target value at the exact start of one sample frame.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] for checked timeline or interpolation
    /// arithmetic failure.
    pub fn visit_frame_values(
        &self,
        frame: usize,
        mut visitor: impl FnMut(AutomationTarget, AutomationValue),
    ) -> Result<(), FrameTimelineError> {
        let position = self.timeline.position_at_frame(frame)?;
        for target in &self.targets {
            if let Some(value) = target.value_at(position)? {
                visitor(target.target, value);
            }
        }
        Ok(())
    }
}

impl LoadedPlaybackProject {
    /// Loads, resolves, and cross-validates post-verified automation resources.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError`] for malformed strict JSON, limits, identity,
    /// target, domain, ordering, interval, or enabled-overlap failures.
    pub fn load_automation_takes(
        &self,
        mixer: &LoadedMixer,
        documents: &[&[u8]],
        limits: LoaderLimits,
    ) -> Result<LoadedAutomationSet, LoadError> {
        enforce_loader_limit(
            CollectionLimit::AutomationDocuments,
            documents.len(),
            limits.max_automation_documents,
            None,
        )?;
        let mut takes = Vec::new();
        takes
            .try_reserve_exact(documents.len())
            .map_err(|_| capacity_error())?;
        let mut lane_count = 0_usize;
        let mut point_count = 0_usize;
        for (index, bytes) in documents.iter().enumerate() {
            let location = DocumentLocation {
                kind: DocumentKind::AutomationTake,
                index,
            };
            let document: AutomationTakeDocument =
                parse_document(bytes, location, limits.max_document_bytes)?;
            validate_header(
                &document.format,
                document.format_version,
                "hyper-automation-take",
                &document.project_id,
                &self.project_id,
                location,
            )?;
            lane_count = lane_count
                .checked_add(document.lanes.len())
                .ok_or_else(capacity_error)?;
            enforce_loader_limit(
                CollectionLimit::AutomationLanes,
                lane_count,
                limits.max_automation_lanes,
                Some(location),
            )?;
            for lane in &document.lanes {
                point_count = point_count
                    .checked_add(lane.points.len())
                    .ok_or_else(capacity_error)?;
            }
            enforce_loader_limit(
                CollectionLimit::AutomationPoints,
                point_count,
                limits.max_automation_points,
                Some(location),
            )?;
            validate_take_identity(self, mixer, &takes, &document, location)?;
            takes.push(load_take(self, mixer, document, location)?);
        }
        validate_enabled_overlaps(&takes)?;
        Ok(LoadedAutomationSet { takes })
    }

    /// Compiles enabled automation for the resolved main-output mixer graph.
    ///
    /// # Errors
    ///
    /// Returns [`AutomationCompileError`] for unavailable capacity, cue
    /// targets, graph mismatch, or timeline compilation failure.
    pub fn compile_main_automation(
        &self,
        mixer: &CompiledMixerGraph,
        automation: &LoadedAutomationSet,
    ) -> Result<CompiledAutomation, AutomationCompileError> {
        let timeline = FrameTimeline::try_new(self.sample_rate_hz, &self.tempo_events)
            .map_err(AutomationCompileError::Timeline)?;
        let enabled_lane_count = automation
            .takes
            .iter()
            .filter(|take| take.enabled)
            .try_fold(0_usize, |count, take| count.checked_add(take.lanes.len()))
            .ok_or(AutomationCompileError::CapacityUnavailable)?;
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(enabled_lane_count)
            .map_err(|_| AutomationCompileError::CapacityUnavailable)?;
        for take in automation.takes.iter().filter(|take| take.enabled) {
            for lane in &take.lanes {
                validate_compiled_target(mixer, lane.target)?;
                pending.push(PendingAutomationLane {
                    target: lane.target,
                    start_tick: take.start_tick,
                    end_tick: take.end_tick,
                    lane,
                });
            }
        }
        pending.sort_unstable_by_key(|lane| (lane.target, lane.start_tick));

        let mut targets: Vec<CompiledAutomationTarget> = Vec::new();
        for lane in pending {
            if targets
                .last()
                .is_none_or(|target| target.target != lane.target)
            {
                targets
                    .try_reserve(1)
                    .map_err(|_| AutomationCompileError::CapacityUnavailable)?;
                targets.push(CompiledAutomationTarget {
                    target: lane.target,
                    intervals: Vec::new(),
                });
            }
            let intervals = &mut targets
                .last_mut()
                .ok_or(AutomationCompileError::CapacityUnavailable)?
                .intervals;
            intervals
                .try_reserve(1)
                .map_err(|_| AutomationCompileError::CapacityUnavailable)?;
            let mut points = Vec::new();
            points
                .try_reserve_exact(lane.lane.points.len())
                .map_err(|_| AutomationCompileError::CapacityUnavailable)?;
            points.extend_from_slice(&lane.lane.points);
            intervals.push(CompiledAutomationInterval {
                start_tick: lane.start_tick,
                end_tick: lane.end_tick,
                interpolation: lane.lane.interpolation,
                points,
            });
        }
        Ok(CompiledAutomation { timeline, targets })
    }
}

#[derive(Clone, Copy)]
struct PendingAutomationLane<'a> {
    target: AutomationTarget,
    start_tick: u64,
    end_tick: u64,
    lane: &'a AutomationLane,
}

fn validate_compiled_target(
    mixer: &CompiledMixerGraph,
    target: AutomationTarget,
) -> Result<(), AutomationCompileError> {
    match target {
        AutomationTarget::Channel { slot, .. } if slot < mixer.slots().len() => Ok(()),
        AutomationTarget::Send { slot, send, .. }
            if mixer
                .slots()
                .get(slot)
                .is_some_and(|channel| send < channel.sends().len()) =>
        {
            Ok(())
        }
        AutomationTarget::Main { .. } => Ok(()),
        AutomationTarget::Cue { .. } => Err(AutomationCompileError::UnsupportedCueTarget),
        AutomationTarget::Channel { .. } | AutomationTarget::Send { .. } => {
            Err(AutomationCompileError::TargetMismatch)
        }
    }
}

fn load_take(
    project: &LoadedPlaybackProject,
    mixer: &LoadedMixer,
    document: AutomationTakeDocument,
    location: DocumentLocation,
) -> Result<AutomationTake, LoadError> {
    if document.name.is_empty()
        || document.name.chars().count() > 120
        || document.start_tick >= document.end_tick
        || document.end_tick > super::MAX_SAFE_INTEGER
        || !canonical_utc(&document.captured_at_utc)
        || document.lanes.is_empty()
    {
        return domain_error(location, "automation take has an invalid version-1 domain");
    }
    let mut lanes = Vec::new();
    lanes
        .try_reserve_exact(document.lanes.len())
        .map_err(|_| capacity_error())?;
    let mut previous_lane_id: Option<&str> = None;
    let mut targets = Vec::new();
    targets
        .try_reserve_exact(document.lanes.len())
        .map_err(|_| capacity_error())?;
    for lane in &document.lanes {
        if lane.id.is_empty()
            || previous_lane_id.is_some_and(|previous| previous >= lane.id.as_str())
            || identity_collides(project, mixer, &document.id, &lane.id)
        {
            return domain_error(location, "automation lane identity is invalid or collides");
        }
        previous_lane_id = Some(&lane.id);
        let target = resolve_target(project, mixer, &lane.target, location)?;
        if targets.contains(&target) {
            return domain_error(location, "automation target is duplicated within a take");
        }
        targets.push(target);
        lanes.push(load_lane(
            lane,
            target,
            document.start_tick,
            document.end_tick,
            location,
        )?);
    }
    Ok(AutomationTake {
        id: document.id,
        name: document.name,
        enabled: document.enabled,
        start_tick: document.start_tick,
        end_tick: document.end_tick,
        lanes,
    })
}

fn load_lane(
    document: &AutomationLaneDocument,
    target: AutomationTarget,
    start_tick: u64,
    end_tick: u64,
    location: DocumentLocation,
) -> Result<AutomationLane, LoadError> {
    let interpolation = match document.interpolation {
        AutomationInterpolationDocument::Hold => AutomationInterpolation::Hold,
        AutomationInterpolationDocument::Linear => AutomationInterpolation::Linear,
    };
    let boolean = target_is_boolean(target);
    if document.points.is_empty() || (boolean && interpolation != AutomationInterpolation::Hold) {
        return domain_error(location, "automation interpolation or point set is invalid");
    }
    let mut points = Vec::new();
    points
        .try_reserve_exact(document.points.len())
        .map_err(|_| capacity_error())?;
    let mut previous_tick = None;
    for point in &document.points {
        if point.tick < start_tick
            || point.tick >= end_tick
            || point.tick > super::MAX_SAFE_INTEGER
            || previous_tick.is_some_and(|previous| previous >= point.tick)
        {
            return domain_error(
                location,
                "automation points are outside their ordered interval",
            );
        }
        previous_tick = Some(point.tick);
        let value = match point.value {
            AutomationValueDocument::Boolean(value) if boolean => AutomationValue::Boolean(value),
            AutomationValueDocument::Integer(value) if !boolean => {
                let (minimum, maximum) = target_bounds(target);
                if !(minimum..=maximum).contains(&value) {
                    return domain_error(location, "automation point exceeds its target domain");
                }
                AutomationValue::Integer(value)
            }
            AutomationValueDocument::Boolean(_) | AutomationValueDocument::Integer(_) => {
                return domain_error(location, "automation point type differs from its target");
            }
        };
        points.push(AutomationPoint {
            tick: point.tick,
            value,
        });
    }
    Ok(AutomationLane {
        id: document.id.clone(),
        target,
        interpolation,
        points,
    })
}

fn resolve_target(
    project: &LoadedPlaybackProject,
    mixer: &LoadedMixer,
    target: &AutomationTargetDocument,
    location: DocumentLocation,
) -> Result<AutomationTarget, LoadError> {
    match target {
        AutomationTargetDocument::Channel {
            track_id,
            parameter,
        } => {
            let slot = mixer
                .channels
                .iter()
                .position(|channel| channel.track_id == track_id.as_str())
                .ok_or_else(|| {
                    super::domain_error_value(location, "automation channel is missing")
                })?;
            if *parameter == AutomationChannelParameterDocument::Solo
                && !matches!(
                    project
                        .tracks
                        .iter()
                        .find(|track| track.id == track_id.as_str())
                        .map(|track| track.kind),
                    Some(TrackKind::Audio | TrackKind::ExternalInput)
                )
            {
                return domain_error(location, "automation solo target is not a source track");
            }
            Ok(AutomationTarget::Channel {
                slot,
                parameter: match parameter {
                    AutomationChannelParameterDocument::Gain => AutomationChannelParameter::Gain,
                    AutomationChannelParameterDocument::Pan => AutomationChannelParameter::Pan,
                    AutomationChannelParameterDocument::Mute => AutomationChannelParameter::Mute,
                    AutomationChannelParameterDocument::Solo => AutomationChannelParameter::Solo,
                },
            })
        }
        AutomationTargetDocument::Send { send_id, parameter } => {
            for (slot, channel) in mixer.channels.iter().enumerate() {
                if let Some(send) = channel
                    .sends
                    .iter()
                    .position(|send| send.id == send_id.as_str())
                {
                    return Ok(AutomationTarget::Send {
                        slot,
                        send,
                        parameter: match parameter {
                            AutomationSendParameterDocument::Gain => AutomationSendParameter::Gain,
                            AutomationSendParameterDocument::Pan => AutomationSendParameter::Pan,
                            AutomationSendParameterDocument::Enabled => {
                                AutomationSendParameter::Enabled
                            }
                        },
                    });
                }
            }
            domain_error(location, "automation send is missing")
        }
        AutomationTargetDocument::Main { parameter } => Ok(AutomationTarget::Main {
            parameter: main_parameter(*parameter),
        }),
        AutomationTargetDocument::Cue { parameter } => Ok(AutomationTarget::Cue {
            parameter: main_parameter(*parameter),
        }),
    }
}

const fn main_parameter(parameter: AutomationMainParameterDocument) -> AutomationMainParameter {
    match parameter {
        AutomationMainParameterDocument::Gain => AutomationMainParameter::Gain,
        AutomationMainParameterDocument::Mute => AutomationMainParameter::Mute,
    }
}

const fn target_is_boolean(target: AutomationTarget) -> bool {
    matches!(
        target,
        AutomationTarget::Channel {
            parameter: AutomationChannelParameter::Mute | AutomationChannelParameter::Solo,
            ..
        } | AutomationTarget::Send {
            parameter: AutomationSendParameter::Enabled,
            ..
        } | AutomationTarget::Main {
            parameter: AutomationMainParameter::Mute
        } | AutomationTarget::Cue {
            parameter: AutomationMainParameter::Mute
        }
    )
}

const fn target_bounds(target: AutomationTarget) -> (i32, i32) {
    if matches!(
        target,
        AutomationTarget::Channel {
            parameter: AutomationChannelParameter::Pan,
            ..
        } | AutomationTarget::Send {
            parameter: AutomationSendParameter::Pan,
            ..
        }
    ) {
        (-1_000_000, 1_000_000)
    } else {
        (0, 2_000_000)
    }
}

fn validate_take_identity(
    project: &LoadedPlaybackProject,
    mixer: &LoadedMixer,
    takes: &[AutomationTake],
    document: &AutomationTakeDocument,
    location: DocumentLocation,
) -> Result<(), LoadError> {
    if document.id.is_empty()
        || identity_collides(project, mixer, "", &document.id)
        || takes.iter().any(|take| take.id == document.id)
        || takes
            .iter()
            .flat_map(|take| &take.lanes)
            .any(|lane| lane.id == document.id)
    {
        return domain_error(location, "automation take identity is invalid or collides");
    }
    for lane in &document.lanes {
        if takes.iter().any(|take| take.id == lane.id)
            || takes
                .iter()
                .flat_map(|take| &take.lanes)
                .any(|loaded| loaded.id == lane.id)
        {
            return domain_error(location, "automation lane identity collides across takes");
        }
    }
    Ok(())
}

fn identity_collides(
    project: &LoadedPlaybackProject,
    mixer: &LoadedMixer,
    take_id: &str,
    id: &str,
) -> bool {
    id == take_id
        || project_contains_identity(project, id)
        || project.clips.iter().any(|clip| clip.asset_id == id)
        || mixer.id == id
        || mixer
            .channels
            .iter()
            .flat_map(|channel| &channel.sends)
            .any(|send| send.id == id)
}

fn validate_enabled_overlaps(takes: &[AutomationTake]) -> Result<(), LoadError> {
    for (take_index, take) in takes.iter().enumerate().filter(|(_, take)| take.enabled) {
        for lane in &take.lanes {
            for earlier in takes[..take_index].iter().filter(|earlier| earlier.enabled) {
                if earlier.start_tick < take.end_tick
                    && take.start_tick < earlier.end_tick
                    && earlier
                        .lanes
                        .iter()
                        .any(|candidate| candidate.target == lane.target)
                {
                    return domain_error(
                        DocumentLocation {
                            kind: DocumentKind::AutomationTake,
                            index: take_index,
                        },
                        "enabled automation intervals overlap on one target",
                    );
                }
            }
        }
    }
    Ok(())
}

fn canonical_utc(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return false;
    }
    let Some(year) = decimal(bytes, 0, 4) else {
        return false;
    };
    let Some(month) = decimal(bytes, 5, 2) else {
        return false;
    };
    let Some(day) = decimal(bytes, 8, 2) else {
        return false;
    };
    let Some(hour) = decimal(bytes, 11, 2) else {
        return false;
    };
    let Some(minute) = decimal(bytes, 14, 2) else {
        return false;
    };
    let Some(second) = decimal(bytes, 17, 2) else {
        return false;
    };
    if decimal(bytes, 20, 3).is_none() || !(1..=12).contains(&month) {
        return false;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

fn decimal(bytes: &[u8], start: usize, count: usize) -> Option<u32> {
    let mut value = 0_u32;
    for byte in &bytes[start..start + count] {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(*byte - b'0'))?;
    }
    Some(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationTakeDocument {
    format: String,
    format_version: u32,
    id: String,
    project_id: String,
    name: String,
    enabled: bool,
    captured_at_utc: String,
    start_tick: u64,
    end_tick: u64,
    lanes: Vec<AutomationLaneDocument>,
    #[serde(rename = "extensions")]
    _extensions: IgnoredAny,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationLaneDocument {
    id: String,
    target: AutomationTargetDocument,
    interpolation: AutomationInterpolationDocument,
    points: Vec<AutomationPointDocument>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AutomationInterpolationDocument {
    Hold,
    Linear,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomationPointDocument {
    tick: u64,
    value: AutomationValueDocument,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(untagged)]
enum AutomationValueDocument {
    Integer(i32),
    Boolean(bool),
}

#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum AutomationTargetDocument {
    #[serde(rename = "mixer-channel")]
    Channel {
        track_id: String,
        parameter: AutomationChannelParameterDocument,
    },
    #[serde(rename = "mixer-send")]
    Send {
        send_id: String,
        parameter: AutomationSendParameterDocument,
    },
    #[serde(rename = "mixer-main")]
    Main {
        parameter: AutomationMainParameterDocument,
    },
    #[serde(rename = "mixer-cue")]
    Cue {
        parameter: AutomationMainParameterDocument,
    },
}

#[derive(Clone, Copy, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AutomationChannelParameterDocument {
    Gain,
    Pan,
    Mute,
    Solo,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AutomationSendParameterDocument {
    Gain,
    Pan,
    Enabled,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AutomationMainParameterDocument {
    Gain,
    Mute,
}
