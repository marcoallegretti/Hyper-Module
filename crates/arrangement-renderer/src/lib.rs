#![doc = "Bounded callback-window scheduling for prepared arrangement placements."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_audio_engine::{InputBlock, MAX_PROCESS_FRAMES};
use hyper_clip_engine::{ClipOutput, ClipPlanError};
use hyper_media_cache::{PreparedSchedule, PreparedScheduleEntry};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RendererCreationError {
    CapacityUnavailable,
    PlacementArithmetic { entry_index: usize },
    EmptyTrackId { track_index: usize },
    DuplicateTrackId { track_index: usize },
    MissingScheduleTrack { entry_index: usize },
    InconsistentTrackChannels { entry_index: usize },
}

impl fmt::Display for RendererCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("arrangement render index capacity is unavailable")
            }
            Self::PlacementArithmetic { entry_index } => {
                write!(
                    formatter,
                    "schedule entry {entry_index} frame range overflowed"
                )
            }
            Self::EmptyTrackId { track_index } => {
                write!(
                    formatter,
                    "renderer track {track_index} has an empty identity"
                )
            }
            Self::DuplicateTrackId { track_index } => {
                write!(formatter, "renderer track {track_index} is duplicated")
            }
            Self::MissingScheduleTrack { entry_index } => {
                write!(
                    formatter,
                    "schedule entry {entry_index} has no renderer track"
                )
            }
            Self::InconsistentTrackChannels { entry_index } => write!(
                formatter,
                "schedule entry {entry_index} changes its track channel layout"
            ),
        }
    }
}

impl Error for RendererCreationError {}

#[derive(Debug)]
pub struct ArrangementRenderer<'prepared, 'cache, 'schedule> {
    schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
    lanes: Vec<TrackLane>,
}

#[derive(Debug)]
struct TrackLane {
    placements: Vec<IndexedPlacement>,
    channels: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
struct IndexedPlacement {
    entry_index: usize,
    end_frame: usize,
}

#[derive(Clone, Copy)]
struct LaneBuild {
    representative_entry: usize,
    placement_count: usize,
}

impl<'prepared, 'cache, 'schedule> ArrangementRenderer<'prepared, 'cache, 'schedule> {
    /// Builds a per-track placement index outside the callback path.
    ///
    /// # Errors
    ///
    /// Returns [`RendererCreationError`] for unavailable index capacity or
    /// overflowing placement frame ranges.
    pub fn try_new(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
    ) -> Result<Self, RendererCreationError> {
        let mut builds: Vec<LaneBuild> = Vec::new();
        builds
            .try_reserve_exact(schedule.entries().len())
            .map_err(|_| RendererCreationError::CapacityUnavailable)?;
        for (entry_index, entry) in schedule.entries().iter().enumerate() {
            if let Some(build) = builds.iter_mut().find(|build| {
                schedule.entries()[build.representative_entry]
                    .schedule_entry()
                    .track_id
                    == entry.schedule_entry().track_id
            }) {
                build.placement_count = build
                    .placement_count
                    .checked_add(1)
                    .ok_or(RendererCreationError::CapacityUnavailable)?;
            } else {
                builds.push(LaneBuild {
                    representative_entry: entry_index,
                    placement_count: 1,
                });
            }
        }

        let mut lanes = Vec::new();
        lanes
            .try_reserve_exact(builds.len())
            .map_err(|_| RendererCreationError::CapacityUnavailable)?;
        for build in &builds {
            let mut placements = Vec::new();
            placements
                .try_reserve_exact(build.placement_count)
                .map_err(|_| RendererCreationError::CapacityUnavailable)?;
            lanes.push(TrackLane {
                placements,
                channels: None,
            });
        }

        for (entry_index, entry) in schedule.entries().iter().enumerate() {
            let lane_index = builds
                .iter()
                .position(|build| {
                    schedule.entries()[build.representative_entry]
                        .schedule_entry()
                        .track_id
                        == entry.schedule_entry().track_id
                })
                .ok_or(RendererCreationError::CapacityUnavailable)?;
            let start_frame = entry.schedule_entry().compiled.arrangement_start_frame();
            let end_frame = start_frame
                .checked_add(entry.placement().frame_count())
                .ok_or(RendererCreationError::PlacementArithmetic { entry_index })?;
            if lanes[lane_index]
                .channels
                .is_some_and(|channels| channels != entry.channels())
            {
                return Err(RendererCreationError::InconsistentTrackChannels { entry_index });
            }
            lanes[lane_index].channels = Some(entry.channels());
            lanes[lane_index].placements.push(IndexedPlacement {
                entry_index,
                end_frame,
            });
        }

        Ok(Self { schedule, lanes })
    }

    /// Builds fixed renderer lanes in caller-provided mixer-track order.
    ///
    /// Tracks without arrangement placements retain a silent lane. Every
    /// placement on one track must use the same mono or stereo layout because
    /// a mixer input has one layout for the complete callback block.
    ///
    /// # Errors
    ///
    /// Returns [`RendererCreationError`] for invalid track identities,
    /// missing schedule tracks, inconsistent source layouts, arithmetic
    /// overflow, or unavailable capacity.
    pub fn try_new_for_tracks(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        track_ids: &[&str],
    ) -> Result<Self, RendererCreationError> {
        for (track_index, track_id) in track_ids.iter().enumerate() {
            if track_id.is_empty() {
                return Err(RendererCreationError::EmptyTrackId { track_index });
            }
            if track_ids[..track_index].contains(track_id) {
                return Err(RendererCreationError::DuplicateTrackId { track_index });
            }
        }

        let mut counts = Vec::new();
        counts
            .try_reserve_exact(track_ids.len())
            .map_err(|_| RendererCreationError::CapacityUnavailable)?;
        counts.resize(track_ids.len(), 0_usize);
        for (entry_index, entry) in schedule.entries().iter().enumerate() {
            let lane_index = track_ids
                .iter()
                .position(|track_id| *track_id == entry.schedule_entry().track_id)
                .ok_or(RendererCreationError::MissingScheduleTrack { entry_index })?;
            counts[lane_index] = counts[lane_index]
                .checked_add(1)
                .ok_or(RendererCreationError::CapacityUnavailable)?;
        }

        let mut lanes = Vec::new();
        lanes
            .try_reserve_exact(track_ids.len())
            .map_err(|_| RendererCreationError::CapacityUnavailable)?;
        for count in counts {
            let mut placements = Vec::new();
            placements
                .try_reserve_exact(count)
                .map_err(|_| RendererCreationError::CapacityUnavailable)?;
            lanes.push(TrackLane {
                placements,
                channels: None,
            });
        }
        for (entry_index, entry) in schedule.entries().iter().enumerate() {
            let lane_index = track_ids
                .iter()
                .position(|track_id| *track_id == entry.schedule_entry().track_id)
                .ok_or(RendererCreationError::MissingScheduleTrack { entry_index })?;
            if lanes[lane_index]
                .channels
                .is_some_and(|channels| channels != entry.channels())
            {
                return Err(RendererCreationError::InconsistentTrackChannels { entry_index });
            }
            let start_frame = entry.schedule_entry().compiled.arrangement_start_frame();
            let end_frame = start_frame
                .checked_add(entry.placement().frame_count())
                .ok_or(RendererCreationError::PlacementArithmetic { entry_index })?;
            lanes[lane_index].channels = Some(entry.channels());
            lanes[lane_index].placements.push(IndexedPlacement {
                entry_index,
                end_frame,
            });
        }
        Ok(Self { schedule, lanes })
    }

    #[must_use]
    pub fn track_lane_count(&self) -> usize {
        self.lanes.len()
    }

    /// Renders enabled placements intersecting one half-open arrangement
    /// window.
    ///
    /// Active placements are written to scratch slots in canonical schedule
    /// order. Each slot remains in the source channel layout: mono uses `left`
    /// and leaves `right` silent; stereo uses both planes. The returned active
    /// count is the only initialized prefix callers may consume.
    ///
    /// Structural validation completes before scratch is touched. If a
    /// prepared clip unexpectedly fails, the initialized prefix is cleared
    /// before return.
    ///
    /// # Errors
    ///
    /// Returns [`RenderWindowError`] for an oversized or overflowing window,
    /// insufficient or malformed scratch, placement arithmetic overflow, or
    /// an unexpected prepared-clip render failure.
    pub fn render_window(
        &self,
        arrangement_start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RenderWindowError> {
        render_window(self, arrangement_start_frame, frame_count, scratch)
    }

    /// Renders one fixed scratch slot per indexed track lane.
    ///
    /// Sequential placements on one track are written into disjoint window
    /// regions in the same slot. Inactive and empty tracks remain silent.
    ///
    /// # Errors
    ///
    /// Returns [`RenderWindowError`] for invalid window or scratch structure,
    /// arithmetic overflow, or an unexpected prepared-clip failure.
    pub fn render_track_window(
        &self,
        arrangement_start_frame: usize,
        frame_count: usize,
        scratch: &mut [RenderScratchSlot<'_>],
    ) -> Result<RenderedWindow, RenderWindowError> {
        render_track_window(self, arrangement_start_frame, frame_count, scratch)
    }

    fn first_candidate_index(&self, window_start: usize) -> usize {
        self.lanes
            .iter()
            .filter_map(|lane| {
                let position = lane
                    .placements
                    .partition_point(|placement| placement.end_frame <= window_start);
                lane.placements
                    .get(position)
                    .map(|placement| placement.entry_index)
            })
            .min()
            .unwrap_or(self.schedule.entries().len())
    }
}

#[derive(Debug)]
pub struct RenderScratchSlot<'a> {
    left: &'a mut [f32],
    right: &'a mut [f32],
    entry_index: Option<usize>,
    channels: usize,
}

impl<'a> RenderScratchSlot<'a> {
    #[must_use]
    pub const fn new(left: &'a mut [f32], right: &'a mut [f32]) -> Self {
        Self {
            left,
            right,
            entry_index: None,
            channels: 0,
        }
    }

    #[must_use]
    pub const fn entry_index(&self) -> Option<usize> {
        self.entry_index
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.channels
    }

    #[must_use]
    pub fn left(&self) -> &[f32] {
        self.left
    }

    #[must_use]
    pub fn right(&self) -> &[f32] {
        self.right
    }

    #[must_use]
    pub fn input_block(&self) -> InputBlock<'_> {
        if self.channels == 2 {
            InputBlock::Stereo {
                left: &*self.left,
                right: &*self.right,
            }
        } else {
            InputBlock::Mono(&*self.left)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderedWindow {
    pub active_slot_count: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderWindowError {
    BlockTooLarge {
        actual: usize,
        maximum: usize,
    },
    WindowOverflow,
    ScratchSlotCountMismatch {
        expected: usize,
        actual: usize,
    },
    InsufficientScratchSlots {
        required: usize,
        available: usize,
    },
    ScratchLengthMismatch {
        slot: usize,
        expected: usize,
        left: usize,
        right: usize,
    },
    PlacementArithmetic {
        entry_index: usize,
    },
    Clip {
        entry_index: usize,
        source: ClipPlanError,
    },
}

impl fmt::Display for RenderWindowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlockTooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "render window has {actual} frames; maximum is {maximum}"
                )
            }
            Self::WindowOverflow => formatter.write_str("render window frame range overflowed"),
            Self::ScratchSlotCountMismatch { expected, actual } => write!(
                formatter,
                "fixed-track rendering requires exactly {expected} scratch slots; {actual} were provided"
            ),
            Self::InsufficientScratchSlots {
                required,
                available,
            } => write!(
                formatter,
                "render window requires {required} active slots; {available} are available"
            ),
            Self::ScratchLengthMismatch {
                slot,
                expected,
                left,
                right,
            } => write!(
                formatter,
                "scratch slot {slot} must have {expected} frames; left={left}, right={right}"
            ),
            Self::PlacementArithmetic { entry_index } => {
                write!(
                    formatter,
                    "schedule entry {entry_index} frame range overflowed"
                )
            }
            Self::Clip {
                entry_index,
                source,
            } => write!(
                formatter,
                "schedule entry {entry_index} failed window rendering: {source}"
            ),
        }
    }
}

impl Error for RenderWindowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Clip { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn render_window(
    renderer: &ArrangementRenderer<'_, '_, '_>,
    arrangement_start_frame: usize,
    frame_count: usize,
    scratch: &mut [RenderScratchSlot<'_>],
) -> Result<RenderedWindow, RenderWindowError> {
    if frame_count > MAX_PROCESS_FRAMES {
        return Err(RenderWindowError::BlockTooLarge {
            actual: frame_count,
            maximum: MAX_PROCESS_FRAMES,
        });
    }
    let arrangement_end_frame = arrangement_start_frame
        .checked_add(frame_count)
        .ok_or(RenderWindowError::WindowOverflow)?;
    if frame_count == 0 {
        return Ok(RenderedWindow {
            active_slot_count: 0,
        });
    }
    let first_candidate = renderer.first_candidate_index(arrangement_start_frame);

    let required = count_active(
        renderer.schedule.entries(),
        first_candidate,
        arrangement_start_frame,
        arrangement_end_frame,
    )?;
    if required > scratch.len() {
        return Err(RenderWindowError::InsufficientScratchSlots {
            required,
            available: scratch.len(),
        });
    }
    for (slot_index, slot) in scratch.iter().take(required).enumerate() {
        if slot.left.len() != frame_count || slot.right.len() != frame_count {
            return Err(RenderWindowError::ScratchLengthMismatch {
                slot: slot_index,
                expected: frame_count,
                left: slot.left.len(),
                right: slot.right.len(),
            });
        }
    }

    for slot in scratch.iter_mut().take(required) {
        clear_slot(slot);
    }

    let mut slot_index = 0_usize;
    for (entry_index, entry) in renderer
        .schedule
        .entries()
        .iter()
        .enumerate()
        .skip(first_candidate)
    {
        let Some(intersection) = intersection(
            entry,
            entry_index,
            arrangement_start_frame,
            arrangement_end_frame,
        )?
        else {
            if entry.schedule_entry().compiled.arrangement_start_frame() >= arrangement_end_frame {
                break;
            }
            continue;
        };
        if !entry.is_enabled() {
            continue;
        }

        let slot = &mut scratch[slot_index];
        let render_result = if entry.channels() == 1 {
            entry.render_block(
                intersection.placement_offset,
                ClipOutput::Mono(
                    &mut slot.left[intersection.window_offset..intersection.window_end],
                ),
            )
        } else {
            entry.render_block(
                intersection.placement_offset,
                ClipOutput::Stereo {
                    left: &mut slot.left[intersection.window_offset..intersection.window_end],
                    right: &mut slot.right[intersection.window_offset..intersection.window_end],
                },
            )
        };
        if let Err(source) = render_result {
            for initialized in scratch.iter_mut().take(required) {
                clear_slot(initialized);
            }
            return Err(RenderWindowError::Clip {
                entry_index,
                source,
            });
        }
        slot.entry_index = Some(entry_index);
        slot.channels = entry.channels();
        slot_index += 1;
    }

    debug_assert_eq!(slot_index, required);
    Ok(RenderedWindow {
        active_slot_count: required,
    })
}

fn render_track_window(
    renderer: &ArrangementRenderer<'_, '_, '_>,
    arrangement_start_frame: usize,
    frame_count: usize,
    scratch: &mut [RenderScratchSlot<'_>],
) -> Result<RenderedWindow, RenderWindowError> {
    if frame_count > MAX_PROCESS_FRAMES {
        return Err(RenderWindowError::BlockTooLarge {
            actual: frame_count,
            maximum: MAX_PROCESS_FRAMES,
        });
    }
    let arrangement_end_frame = arrangement_start_frame
        .checked_add(frame_count)
        .ok_or(RenderWindowError::WindowOverflow)?;
    if scratch.len() != renderer.lanes.len() {
        return Err(RenderWindowError::ScratchSlotCountMismatch {
            expected: renderer.lanes.len(),
            actual: scratch.len(),
        });
    }
    for (slot_index, slot) in scratch.iter().enumerate() {
        if slot.left.len() != frame_count || slot.right.len() != frame_count {
            return Err(RenderWindowError::ScratchLengthMismatch {
                slot: slot_index,
                expected: frame_count,
                left: slot.left.len(),
                right: slot.right.len(),
            });
        }
    }

    for (lane, slot) in renderer.lanes.iter().zip(scratch.iter_mut()) {
        clear_slot(slot);
        slot.channels = lane.channels.unwrap_or(1);
    }
    if frame_count == 0 {
        return Ok(RenderedWindow {
            active_slot_count: 0,
        });
    }

    let mut active_slot_count = 0_usize;
    for (lane, slot) in renderer.lanes.iter().zip(scratch.iter_mut()) {
        let first_candidate = lane
            .placements
            .partition_point(|placement| placement.end_frame <= arrangement_start_frame);
        let mut active = false;
        for placement in &lane.placements[first_candidate..] {
            let entry = &renderer.schedule.entries()[placement.entry_index];
            if entry.schedule_entry().compiled.arrangement_start_frame() >= arrangement_end_frame {
                break;
            }
            if !entry.is_enabled() {
                continue;
            }
            let Some(intersection) = intersection(
                entry,
                placement.entry_index,
                arrangement_start_frame,
                arrangement_end_frame,
            )?
            else {
                continue;
            };
            let render_result = if entry.channels() == 1 {
                entry.render_block(
                    intersection.placement_offset,
                    ClipOutput::Mono(
                        &mut slot.left[intersection.window_offset..intersection.window_end],
                    ),
                )
            } else {
                entry.render_block(
                    intersection.placement_offset,
                    ClipOutput::Stereo {
                        left: &mut slot.left[intersection.window_offset..intersection.window_end],
                        right: &mut slot.right[intersection.window_offset..intersection.window_end],
                    },
                )
            };
            if let Err(source) = render_result {
                for (clear_lane, initialized) in renderer.lanes.iter().zip(scratch.iter_mut()) {
                    clear_slot(initialized);
                    initialized.channels = clear_lane.channels.unwrap_or(1);
                }
                return Err(RenderWindowError::Clip {
                    entry_index: placement.entry_index,
                    source,
                });
            }
            if !active {
                slot.entry_index = Some(placement.entry_index);
                active = true;
            }
        }
        if active {
            active_slot_count = active_slot_count
                .checked_add(1)
                .ok_or(RenderWindowError::WindowOverflow)?;
        }
    }

    Ok(RenderedWindow { active_slot_count })
}

#[derive(Clone, Copy)]
struct Intersection {
    placement_offset: usize,
    window_offset: usize,
    window_end: usize,
}

fn count_active(
    entries: &[PreparedScheduleEntry<'_, '_>],
    first_candidate: usize,
    window_start: usize,
    window_end: usize,
) -> Result<usize, RenderWindowError> {
    let mut count = 0_usize;
    for (entry_index, entry) in entries.iter().enumerate().skip(first_candidate) {
        if entry.schedule_entry().compiled.arrangement_start_frame() >= window_end {
            break;
        }
        if entry.is_enabled()
            && intersection(entry, entry_index, window_start, window_end)?.is_some()
        {
            count = count
                .checked_add(1)
                .ok_or(RenderWindowError::WindowOverflow)?;
        }
    }
    Ok(count)
}

fn intersection(
    entry: &PreparedScheduleEntry<'_, '_>,
    entry_index: usize,
    window_start: usize,
    window_end: usize,
) -> Result<Option<Intersection>, RenderWindowError> {
    let placement_start = entry.schedule_entry().compiled.arrangement_start_frame();
    let placement_end = placement_start
        .checked_add(entry.placement().frame_count())
        .ok_or(RenderWindowError::PlacementArithmetic { entry_index })?;
    let start = placement_start.max(window_start);
    let end = placement_end.min(window_end);
    if start >= end {
        return Ok(None);
    }
    Ok(Some(Intersection {
        placement_offset: start - placement_start,
        window_offset: start - window_start,
        window_end: end - window_start,
    }))
}

fn clear_slot(slot: &mut RenderScratchSlot<'_>) {
    slot.left.fill(0.0);
    slot.right.fill(0.0);
    slot.entry_index = None;
    slot.channels = 0;
}
