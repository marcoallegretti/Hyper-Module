//! Bounded identity resolution for explicit arrangement placements.

use core::cmp::Ordering;
use core::error::Error;
use core::fmt;

use hyper_clip_engine::ConsolidationReason;

use crate::{
    ClipCompileSpec, ClipTimebase, CompileError, CompiledPlacement, PlacementCompileSpec,
    TickRange, Timeline, WarpMarker, compile_placement,
};

/// Maximum number of persisted placement intervals that may overlap on one
/// Version-1 audio track.
///
/// The bound applies before preparation so the native renderer can retain a
/// fixed source count at each callback boundary.
pub const MAX_CONCURRENT_PLACEMENTS_PER_TRACK: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackKind {
    Audio,
    Midi,
    CvGate,
    ExternalInput,
    Group,
    Return,
}

#[derive(Clone, Copy, Debug)]
pub struct TrackResource<'a> {
    pub id: &'a str,
    pub kind: TrackKind,
    pub active: bool,
    pub clip_ids: &'a [&'a str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssetResource<'a> {
    pub id: &'a str,
    pub sample_rate_hz: u32,
    pub frame_count: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ClipResource<'a> {
    pub id: &'a str,
    pub track_id: &'a str,
    pub asset_id: &'a str,
    pub enabled: bool,
    pub timebase: ClipTimebase,
    pub duration_ticks: u64,
    pub source_start_frame: usize,
    pub source_frame_count: usize,
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
pub struct PlacementResource<'a> {
    pub id: &'a str,
    pub track_id: &'a str,
    pub clip_id: &'a str,
    pub start_tick: u64,
    pub clip_start_tick: u64,
    pub duration_ticks: u64,
    pub enabled: bool,
    pub gain_decibels: f64,
    pub fade_in_ticks: u64,
    pub fade_out_ticks: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct ArrangementResource<'a> {
    pub enabled: bool,
    pub placements: &'a [PlacementResource<'a>],
    pub scene_launch_count: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct GraphInput<'a> {
    pub tracks: &'a [TrackResource<'a>],
    pub clips: &'a [ClipResource<'a>],
    pub assets: &'a [AssetResource<'a>],
    pub arrangement: ArrangementResource<'a>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GraphLimits {
    pub max_tracks: usize,
    pub max_clips: usize,
    pub max_assets: usize,
    pub max_placements: usize,
}

impl GraphLimits {
    pub const VERSION_1: Self = Self {
        max_tracks: 8,
        max_clips: 4_096,
        max_assets: 4_096,
        max_placements: 65_536,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CollectionKind {
    Track,
    Clip,
    Asset,
    Placement,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GraphError {
    LimitExceeded {
        collection: CollectionKind,
        actual: usize,
        maximum: usize,
    },
    CapacityUnavailable,
    EmptyEntityId {
        collection: CollectionKind,
        index: usize,
    },
    DuplicateEntityId {
        first_collection: CollectionKind,
        first_index: usize,
        second_collection: CollectionKind,
        second_index: usize,
    },
    UnsupportedSceneLaunches,
    MissingTrackForClip {
        clip_index: usize,
    },
    MissingAssetForClip {
        clip_index: usize,
    },
    NonAudioClipOwner {
        clip_index: usize,
    },
    InvalidAsset {
        asset_index: usize,
    },
    SourceOutsideAsset {
        clip_index: usize,
    },
    MissingTrackClipReference {
        track_index: usize,
        reference_index: usize,
    },
    TrackClipOwnerMismatch {
        track_index: usize,
        reference_index: usize,
    },
    ClipReferenceCount {
        clip_index: usize,
        actual: usize,
    },
    PlacementsNotSorted {
        index: usize,
    },
    MissingPlacementTrack {
        placement_index: usize,
    },
    MissingPlacementClip {
        placement_index: usize,
    },
    PlacementOwnerMismatch {
        placement_index: usize,
    },
    ConcurrentPlacementsExceeded {
        placement_index: usize,
        maximum: usize,
    },
    PlacementCompilation {
        placement_index: usize,
        error: CompileError,
    },
}

impl fmt::Display for GraphError {
    #[allow(
        clippy::too_many_lines,
        reason = "exhaustive diagnostics keep every graph failure independently actionable"
    )]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                collection,
                actual,
                maximum,
            } => format_limit_error(formatter, *collection, *actual, *maximum),
            Self::CapacityUnavailable => {
                formatter.write_str("graph index or schedule capacity is unavailable")
            }
            Self::EmptyEntityId { collection, index } => {
                write!(
                    formatter,
                    "{collection:?} at index {index} has an empty identifier"
                )
            }
            Self::DuplicateEntityId {
                first_collection,
                first_index,
                second_collection,
                second_index,
            } => format_duplicate_error(
                formatter,
                *first_collection,
                *first_index,
                *second_collection,
                *second_index,
            ),
            Self::UnsupportedSceneLaunches => {
                formatter.write_str("recorded scene-launch authority is not executable")
            }
            Self::MissingTrackForClip { clip_index } => {
                write!(formatter, "clip {clip_index} references a missing track")
            }
            Self::MissingAssetForClip { clip_index } => {
                write!(formatter, "clip {clip_index} references a missing asset")
            }
            Self::NonAudioClipOwner { clip_index } => {
                write!(
                    formatter,
                    "clip {clip_index} is not owned by an audio track"
                )
            }
            Self::InvalidAsset { asset_index } => {
                write!(formatter, "asset {asset_index} has invalid audio metadata")
            }
            Self::SourceOutsideAsset { clip_index } => {
                write!(
                    formatter,
                    "clip {clip_index} selects audio outside its asset"
                )
            }
            Self::MissingTrackClipReference {
                track_index,
                reference_index,
            } => write!(
                formatter,
                "track {track_index} clip reference {reference_index} is missing"
            ),
            Self::TrackClipOwnerMismatch {
                track_index,
                reference_index,
            } => write!(
                formatter,
                "track {track_index} clip reference {reference_index} names a foreign clip"
            ),
            Self::ClipReferenceCount { clip_index, actual } => write!(
                formatter,
                "clip {clip_index} is referenced by {actual} tracks; expected exactly one"
            ),
            Self::PlacementsNotSorted { index } => {
                write!(formatter, "placement {index} is outside canonical order")
            }
            Self::MissingPlacementTrack { placement_index } => {
                write!(
                    formatter,
                    "placement {placement_index} references a missing track"
                )
            }
            Self::MissingPlacementClip { placement_index } => {
                write!(
                    formatter,
                    "placement {placement_index} references a missing clip"
                )
            }
            Self::PlacementOwnerMismatch { placement_index } => write!(
                formatter,
                "placement {placement_index} track differs from its clip owner"
            ),
            Self::ConcurrentPlacementsExceeded {
                placement_index,
                maximum,
            } => {
                write!(
                    formatter,
                    "placement {placement_index} exceeds the {maximum}-voice track overlap limit"
                )
            }
            Self::PlacementCompilation {
                placement_index,
                error,
            } => write!(
                formatter,
                "placement {placement_index} failed compilation: {error}"
            ),
        }
    }
}

fn format_limit_error(
    formatter: &mut fmt::Formatter<'_>,
    collection: CollectionKind,
    actual: usize,
    maximum: usize,
) -> fmt::Result {
    write!(
        formatter,
        "{collection:?} count {actual} exceeds the limit {maximum}"
    )
}

fn format_duplicate_error(
    formatter: &mut fmt::Formatter<'_>,
    first_collection: CollectionKind,
    first_index: usize,
    second_collection: CollectionKind,
    second_index: usize,
) -> fmt::Result {
    write!(
        formatter,
        "{first_collection:?} {first_index} and {second_collection:?} {second_index} share an identifier"
    )
}

impl Error for GraphError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::PlacementCompilation { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScheduleEntry<'a> {
    pub placement_id: &'a str,
    pub track_id: &'a str,
    pub clip_id: &'a str,
    pub asset_id: &'a str,
    pub compiled: CompiledPlacement,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompiledSchedule<'a> {
    entries: Vec<ScheduleEntry<'a>>,
}

impl<'a> CompiledSchedule<'a> {
    #[must_use]
    pub fn entries(&self) -> &[ScheduleEntry<'a>] {
        &self.entries
    }

    #[must_use]
    pub fn into_entries(self) -> Vec<ScheduleEntry<'a>> {
        self.entries
    }
}

#[derive(Clone, Copy)]
struct Identity<'a> {
    id: &'a str,
    collection: CollectionKind,
    index: usize,
}

#[derive(Clone, Copy)]
struct IndexEntry<'a> {
    id: &'a str,
    index: usize,
}

struct ResolvedIndexes<'a> {
    tracks: Vec<IndexEntry<'a>>,
    clips: Vec<IndexEntry<'a>>,
    assets: Vec<IndexEntry<'a>>,
}

struct ResolvedPlacement<'a> {
    track_index: usize,
    end_tick: u64,
    entry: ScheduleEntry<'a>,
}

/// Resolves and compiles every explicit placement in one verified arrangement.
///
/// Output preserves canonical placement order. Allocation occurs only while
/// constructing control-plane indexes and the immutable schedule.
///
/// # Errors
///
/// Returns [`GraphError`] for exceeded bounds, identity or ownership failures,
/// scene-launch authority, excessive concurrent placement intervals, invalid
/// asset selection, allocation failure, or a placement that cannot be
/// compiled natively.
pub fn compile_arrangement<'a>(
    timeline: Timeline<'_>,
    input: GraphInput<'a>,
    limits: GraphLimits,
) -> Result<CompiledSchedule<'a>, GraphError> {
    let indexes = validate_graph(input, limits)?;
    validate_placement_order(input.arrangement.placements)?;
    compile_entries(timeline, input, &indexes)
}

fn validate_graph(
    input: GraphInput<'_>,
    limits: GraphLimits,
) -> Result<ResolvedIndexes<'_>, GraphError> {
    enforce_limit(CollectionKind::Track, input.tracks.len(), limits.max_tracks)?;
    enforce_limit(CollectionKind::Clip, input.clips.len(), limits.max_clips)?;
    enforce_limit(CollectionKind::Asset, input.assets.len(), limits.max_assets)?;
    enforce_limit(
        CollectionKind::Placement,
        input.arrangement.placements.len(),
        limits.max_placements,
    )?;
    if input.arrangement.scene_launch_count != 0 {
        return Err(GraphError::UnsupportedSceneLaunches);
    }

    validate_global_identities(input)?;
    let indexes = ResolvedIndexes {
        tracks: build_index(input.tracks.iter().map(|track| track.id))?,
        clips: build_index(input.clips.iter().map(|clip| clip.id))?,
        assets: build_index(input.assets.iter().map(|asset| asset.id))?,
    };

    validate_assets(input.assets)?;
    validate_clip_ownership(input, &indexes)?;
    Ok(indexes)
}

fn validate_assets(assets: &[AssetResource<'_>]) -> Result<(), GraphError> {
    for (asset_index, asset) in assets.iter().enumerate() {
        if asset.sample_rate_hz == 0 || asset.frame_count == 0 {
            return Err(GraphError::InvalidAsset { asset_index });
        }
    }
    Ok(())
}

fn validate_clip_ownership(
    input: GraphInput<'_>,
    indexes: &ResolvedIndexes<'_>,
) -> Result<(), GraphError> {
    let mut clip_reference_counts = fallible_filled(input.clips.len(), 0_usize)?;
    for (track_index_value, track) in input.tracks.iter().enumerate() {
        for (reference_index, clip_id) in track.clip_ids.iter().enumerate() {
            let referenced_index =
                lookup(&indexes.clips, clip_id).ok_or(GraphError::MissingTrackClipReference {
                    track_index: track_index_value,
                    reference_index,
                })?;
            if input.clips[referenced_index].track_id != track.id {
                return Err(GraphError::TrackClipOwnerMismatch {
                    track_index: track_index_value,
                    reference_index,
                });
            }
            clip_reference_counts[referenced_index] = clip_reference_counts[referenced_index]
                .checked_add(1)
                .ok_or(GraphError::CapacityUnavailable)?;
        }
    }

    for (clip_index_value, clip) in input.clips.iter().enumerate() {
        let owner_index =
            lookup(&indexes.tracks, clip.track_id).ok_or(GraphError::MissingTrackForClip {
                clip_index: clip_index_value,
            })?;
        if input.tracks[owner_index].kind != TrackKind::Audio {
            return Err(GraphError::NonAudioClipOwner {
                clip_index: clip_index_value,
            });
        }
        let referenced_asset_index =
            lookup(&indexes.assets, clip.asset_id).ok_or(GraphError::MissingAssetForClip {
                clip_index: clip_index_value,
            })?;
        let asset = input.assets[referenced_asset_index];
        let source_end = clip
            .source_start_frame
            .checked_add(clip.source_frame_count)
            .ok_or(GraphError::SourceOutsideAsset {
                clip_index: clip_index_value,
            })?;
        if clip.source_frame_count == 0 || source_end > asset.frame_count {
            return Err(GraphError::SourceOutsideAsset {
                clip_index: clip_index_value,
            });
        }
        if clip_reference_counts[clip_index_value] != 1 {
            return Err(GraphError::ClipReferenceCount {
                clip_index: clip_index_value,
                actual: clip_reference_counts[clip_index_value],
            });
        }
    }
    Ok(())
}

fn compile_entries<'a>(
    timeline: Timeline<'_>,
    input: GraphInput<'a>,
    indexes: &ResolvedIndexes<'_>,
) -> Result<CompiledSchedule<'a>, GraphError> {
    let mut track_tick_ends = fallible_filled(
        input.tracks.len(),
        [None; MAX_CONCURRENT_PLACEMENTS_PER_TRACK],
    )?;
    let mut track_frame_ends = fallible_filled(
        input.tracks.len(),
        [None; MAX_CONCURRENT_PLACEMENTS_PER_TRACK],
    )?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(input.arrangement.placements.len())
        .map_err(|_| GraphError::CapacityUnavailable)?;

    for (placement_index, placement) in input.arrangement.placements.iter().enumerate() {
        let resolved = resolve_placement(timeline, input, indexes, placement_index, *placement)?;
        if !reserve_concurrent_interval(
            &mut track_tick_ends[resolved.track_index],
            placement.start_tick,
            resolved.end_tick,
        ) || !reserve_concurrent_interval(
            &mut track_frame_ends[resolved.track_index],
            resolved.entry.compiled.arrangement_start_frame(),
            resolved.entry.compiled.arrangement_end_frame(),
        ) {
            return Err(GraphError::ConcurrentPlacementsExceeded {
                placement_index,
                maximum: MAX_CONCURRENT_PLACEMENTS_PER_TRACK,
            });
        }
        entries.push(resolved.entry);
    }

    Ok(CompiledSchedule { entries })
}

fn reserve_concurrent_interval<T: Copy + Ord>(
    active_ends: &mut [Option<T>; MAX_CONCURRENT_PLACEMENTS_PER_TRACK],
    start: T,
    end: T,
) -> bool {
    for active_end in active_ends.iter_mut() {
        if active_end.is_some_and(|value| value <= start) {
            *active_end = None;
        }
    }
    let Some(slot) = active_ends.iter_mut().find(|end| end.is_none()) else {
        return false;
    };
    *slot = Some(end);
    true
}

fn resolve_placement<'a>(
    timeline: Timeline<'_>,
    input: GraphInput<'a>,
    indexes: &ResolvedIndexes<'_>,
    placement_index: usize,
    placement: PlacementResource<'a>,
) -> Result<ResolvedPlacement<'a>, GraphError> {
    let track_index = lookup(&indexes.tracks, placement.track_id)
        .ok_or(GraphError::MissingPlacementTrack { placement_index })?;
    let clip_index = lookup(&indexes.clips, placement.clip_id)
        .ok_or(GraphError::MissingPlacementClip { placement_index })?;
    let track = input.tracks[track_index];
    let clip = input.clips[clip_index];
    if clip.track_id != placement.track_id {
        return Err(GraphError::PlacementOwnerMismatch { placement_index });
    }
    let end_tick = placement
        .start_tick
        .checked_add(placement.duration_ticks)
        .ok_or(GraphError::PlacementCompilation {
            placement_index,
            error: CompileError::ArithmeticOverflow,
        })?;
    let asset_index = lookup(&indexes.assets, clip.asset_id)
        .ok_or(GraphError::MissingAssetForClip { clip_index })?;
    let asset = input.assets[asset_index];
    let compiled = compile_placement(
        timeline,
        ClipCompileSpec {
            enabled: input.arrangement.enabled && track.active && clip.enabled,
            timebase: clip.timebase,
            duration_ticks: clip.duration_ticks,
            source_start_frame: clip.source_start_frame,
            source_frame_count: clip.source_frame_count,
            asset_frame_count: asset.frame_count,
            source_sample_rate_hz: asset.sample_rate_hz,
            loop_enabled: clip.loop_enabled,
            loop_range: clip.loop_range,
            reverse: clip.reverse,
            gain_decibels: clip.gain_decibels,
            transpose_cents: clip.transpose_cents,
            fade_in_ticks: clip.fade_in_ticks,
            fade_out_ticks: clip.fade_out_ticks,
            warp_markers: clip.warp_markers,
        },
        PlacementCompileSpec {
            enabled: placement.enabled,
            start_tick: placement.start_tick,
            clip_start_tick: placement.clip_start_tick,
            duration_ticks: placement.duration_ticks,
            gain_decibels: placement.gain_decibels,
            fade_in_ticks: placement.fade_in_ticks,
            fade_out_ticks: placement.fade_out_ticks,
        },
    )
    .map_err(|error| GraphError::PlacementCompilation {
        placement_index,
        error,
    })?;
    Ok(ResolvedPlacement {
        track_index,
        end_tick,
        entry: ScheduleEntry {
            placement_id: placement.id,
            track_id: placement.track_id,
            clip_id: placement.clip_id,
            asset_id: clip.asset_id,
            compiled,
        },
    })
}

fn enforce_limit(
    collection: CollectionKind,
    actual: usize,
    maximum: usize,
) -> Result<(), GraphError> {
    if actual > maximum {
        return Err(GraphError::LimitExceeded {
            collection,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_global_identities(input: GraphInput<'_>) -> Result<(), GraphError> {
    let capacity = input
        .tracks
        .len()
        .checked_add(input.clips.len())
        .and_then(|value| value.checked_add(input.assets.len()))
        .and_then(|value| value.checked_add(input.arrangement.placements.len()))
        .ok_or(GraphError::CapacityUnavailable)?;
    let mut identities = Vec::new();
    identities
        .try_reserve_exact(capacity)
        .map_err(|_| GraphError::CapacityUnavailable)?;
    append_identities(
        &mut identities,
        CollectionKind::Track,
        input.tracks.iter().map(|v| v.id),
    )?;
    append_identities(
        &mut identities,
        CollectionKind::Clip,
        input.clips.iter().map(|v| v.id),
    )?;
    append_identities(
        &mut identities,
        CollectionKind::Asset,
        input.assets.iter().map(|v| v.id),
    )?;
    append_identities(
        &mut identities,
        CollectionKind::Placement,
        input.arrangement.placements.iter().map(|v| v.id),
    )?;
    identities.sort_unstable_by(|left, right| left.id.cmp(right.id));
    for pair in identities.windows(2) {
        if pair[0].id == pair[1].id {
            return Err(GraphError::DuplicateEntityId {
                first_collection: pair[0].collection,
                first_index: pair[0].index,
                second_collection: pair[1].collection,
                second_index: pair[1].index,
            });
        }
    }
    Ok(())
}

fn append_identities<'a>(
    output: &mut Vec<Identity<'a>>,
    collection: CollectionKind,
    ids: impl Iterator<Item = &'a str>,
) -> Result<(), GraphError> {
    for (index, id) in ids.enumerate() {
        if id.is_empty() {
            return Err(GraphError::EmptyEntityId { collection, index });
        }
        output.push(Identity {
            id,
            collection,
            index,
        });
    }
    Ok(())
}

fn build_index<'a>(
    ids: impl ExactSizeIterator<Item = &'a str>,
) -> Result<Vec<IndexEntry<'a>>, GraphError> {
    let mut index = Vec::new();
    index
        .try_reserve_exact(ids.len())
        .map_err(|_| GraphError::CapacityUnavailable)?;
    for (position, id) in ids.enumerate() {
        index.push(IndexEntry {
            id,
            index: position,
        });
    }
    index.sort_unstable_by(|left, right| left.id.cmp(right.id));
    Ok(index)
}

fn lookup(index: &[IndexEntry<'_>], id: &str) -> Option<usize> {
    index
        .binary_search_by(|entry| entry.id.cmp(id))
        .ok()
        .map(|position| index[position].index)
}

fn validate_placement_order(placements: &[PlacementResource<'_>]) -> Result<(), GraphError> {
    for (index, pair) in placements.windows(2).enumerate() {
        if compare_placements(pair[0], pair[1]) != Ordering::Less {
            return Err(GraphError::PlacementsNotSorted { index: index + 1 });
        }
    }
    Ok(())
}

fn compare_placements(left: PlacementResource<'_>, right: PlacementResource<'_>) -> Ordering {
    left.start_tick
        .cmp(&right.start_tick)
        .then_with(|| left.track_id.cmp(right.track_id))
        .then_with(|| left.id.cmp(right.id))
}

fn fallible_filled<T: Clone>(length: usize, value: T) -> Result<Vec<T>, GraphError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| GraphError::CapacityUnavailable)?;
    output.resize(length, value);
    Ok(output)
}

#[must_use]
pub const fn consolidation_reason(error: GraphError) -> Option<ConsolidationReason> {
    match error {
        GraphError::PlacementCompilation {
            error: CompileError::ConsolidationRequired(reason),
            ..
        } => Some(reason),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TempoEvent;

    const CLIP_IDS: [&str; 1] = ["clip"];
    const MARKERS: [WarpMarker; 2] = [
        WarpMarker {
            tick: 0,
            source_frame_offset: 0,
        },
        WarpMarker {
            tick: 4,
            source_frame_offset: 100,
        },
    ];
    const TRACKS: [TrackResource<'static>; 1] = [TrackResource {
        id: "track",
        kind: TrackKind::Audio,
        active: true,
        clip_ids: &CLIP_IDS,
    }];
    const ASSETS: [AssetResource<'static>; 1] = [AssetResource {
        id: "asset",
        sample_rate_hz: 48_000,
        frame_count: 100,
    }];
    const CLIPS: [ClipResource<'static>; 1] = [ClipResource {
        id: "clip",
        track_id: "track",
        asset_id: "asset",
        enabled: true,
        timebase: ClipTimebase::Musical,
        duration_ticks: 4,
        source_start_frame: 0,
        source_frame_count: 100,
        loop_enabled: false,
        loop_range: TickRange { start: 0, end: 4 },
        reverse: false,
        gain_decibels: 0.0,
        transpose_cents: 0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
        warp_markers: &MARKERS,
    }];
    const PLACEMENTS: [PlacementResource<'static>; 1] = [PlacementResource {
        id: "placement",
        track_id: "track",
        clip_id: "clip",
        start_tick: 0,
        clip_start_tick: 0,
        duration_ticks: 4,
        enabled: true,
        gain_decibels: 0.0,
        fade_in_ticks: 0,
        fade_out_ticks: 0,
    }];
    const TEMPOS: [TempoEvent; 1] = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 1_000_000,
    }];

    fn timeline() -> Timeline<'static> {
        Timeline::try_new(48_000, &TEMPOS).unwrap()
    }

    fn input<'a>(
        tracks: &'a [TrackResource<'a>],
        clips: &'a [ClipResource<'a>],
        assets: &'a [AssetResource<'a>],
        placements: &'a [PlacementResource<'a>],
    ) -> GraphInput<'a> {
        GraphInput {
            tracks,
            clips,
            assets,
            arrangement: ArrangementResource {
                enabled: true,
                placements,
                scene_launch_count: 0,
            },
        }
    }

    #[test]
    fn resolves_a_complete_bounded_graph() {
        let schedule = compile_arrangement(
            timeline(),
            input(&TRACKS, &CLIPS, &ASSETS, &PLACEMENTS),
            GraphLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(schedule.entries().len(), 1);
        assert_eq!(schedule.entries()[0].asset_id, "asset");
        assert_eq!(schedule.entries()[0].compiled.frame_count(), 100);
    }

    #[test]
    fn rejects_limits_collisions_and_scene_authority() {
        let mut limits = GraphLimits::VERSION_1;
        limits.max_placements = 0;
        assert!(matches!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &CLIPS, &ASSETS, &PLACEMENTS),
                limits
            ),
            Err(GraphError::LimitExceeded {
                collection: CollectionKind::Placement,
                ..
            })
        ));

        let colliding_assets = [AssetResource {
            id: "clip",
            ..ASSETS[0]
        }];
        assert!(matches!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &CLIPS, &colliding_assets, &PLACEMENTS),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::DuplicateEntityId { .. })
        ));

        let mut graph = input(&TRACKS, &CLIPS, &ASSETS, &PLACEMENTS);
        graph.arrangement.scene_launch_count = 1;
        assert_eq!(
            compile_arrangement(timeline(), graph, GraphLimits::VERSION_1),
            Err(GraphError::UnsupportedSceneLaunches)
        );
    }

    #[test]
    fn rejects_broken_bidirectional_ownership_and_asset_bounds() {
        let no_clip_ids: [&str; 0] = [];
        let tracks = [TrackResource {
            clip_ids: &no_clip_ids,
            ..TRACKS[0]
        }];
        assert!(matches!(
            compile_arrangement(
                timeline(),
                input(&tracks, &CLIPS, &ASSETS, &PLACEMENTS),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::ClipReferenceCount {
                clip_index: 0,
                actual: 0
            })
        ));

        let clips = [ClipResource {
            source_frame_count: 101,
            warp_markers: &[
                WarpMarker {
                    tick: 0,
                    source_frame_offset: 0,
                },
                WarpMarker {
                    tick: 4,
                    source_frame_offset: 101,
                },
            ],
            ..CLIPS[0]
        }];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &clips, &ASSETS, &PLACEMENTS),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::SourceOutsideAsset { clip_index: 0 })
        );
    }

    #[test]
    fn validates_canonical_order_bounded_overlap_and_missing_references() {
        let two = [
            PlacementResource {
                id: "z",
                start_tick: 4,
                ..PLACEMENTS[0]
            },
            PLACEMENTS[0],
        ];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &CLIPS, &ASSETS, &two),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::PlacementsNotSorted { index: 1 })
        );

        let overlapping_pair = [
            PLACEMENTS[0],
            PlacementResource {
                id: "placement-2",
                start_tick: 2,
                ..PLACEMENTS[0]
            },
        ];
        let schedule = compile_arrangement(
            timeline(),
            input(&TRACKS, &CLIPS, &ASSETS, &overlapping_pair),
            GraphLimits::VERSION_1,
        )
        .unwrap();
        assert_eq!(schedule.entries().len(), 2);

        let overlapping_triple = [
            overlapping_pair[0],
            overlapping_pair[1],
            PlacementResource {
                id: "placement-3",
                start_tick: 3,
                ..PLACEMENTS[0]
            },
        ];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &CLIPS, &ASSETS, &overlapping_triple),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::ConcurrentPlacementsExceeded {
                placement_index: 2,
                maximum: MAX_CONCURRENT_PLACEMENTS_PER_TRACK,
            })
        );

        let missing = [PlacementResource {
            clip_id: "missing",
            ..PLACEMENTS[0]
        }];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &CLIPS, &ASSETS, &missing),
                GraphLimits::VERSION_1
            ),
            Err(GraphError::MissingPlacementClip { placement_index: 0 })
        );
    }

    #[test]
    fn rejects_a_third_native_frame_overlap_even_when_ticks_are_adjacent() {
        const SAMPLE_LOCKED_MARKERS: [WarpMarker; 2] = [
            WarpMarker {
                tick: 0,
                source_frame_offset: 0,
            },
            WarpMarker {
                tick: 1,
                source_frame_offset: 100,
            },
        ];
        let clips = [ClipResource {
            timebase: ClipTimebase::SampleLocked,
            duration_ticks: 1,
            source_frame_count: 100,
            loop_range: TickRange { start: 0, end: 1 },
            warp_markers: &SAMPLE_LOCKED_MARKERS,
            ..CLIPS[0]
        }];
        let pair = [
            PlacementResource {
                duration_ticks: 1,
                ..PLACEMENTS[0]
            },
            PlacementResource {
                id: "placement-2",
                start_tick: 1,
                duration_ticks: 1,
                ..PLACEMENTS[0]
            },
        ];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &clips, &ASSETS, &pair),
                GraphLimits::VERSION_1,
            )
            .unwrap()
            .entries()
            .len(),
            2
        );

        let triple = [
            pair[0],
            pair[1],
            PlacementResource {
                id: "placement-3",
                start_tick: 2,
                duration_ticks: 1,
                ..PLACEMENTS[0]
            },
        ];
        assert_eq!(
            compile_arrangement(
                timeline(),
                input(&TRACKS, &clips, &ASSETS, &triple),
                GraphLimits::VERSION_1,
            ),
            Err(GraphError::ConcurrentPlacementsExceeded {
                placement_index: 2,
                maximum: MAX_CONCURRENT_PLACEMENTS_PER_TRACK,
            })
        );
    }

    #[test]
    fn propagates_consolidation_without_partial_schedule() {
        let clips = [ClipResource {
            transpose_cents: 100,
            ..CLIPS[0]
        }];
        let error = compile_arrangement(
            timeline(),
            input(&TRACKS, &clips, &ASSETS, &PLACEMENTS),
            GraphLimits::VERSION_1,
        )
        .unwrap_err();
        assert_eq!(
            consolidation_reason(error),
            Some(ConsolidationReason::Transposition)
        );
    }

    #[test]
    fn combines_arrangement_track_clip_and_placement_activation() {
        let inactive_tracks = [TrackResource {
            active: false,
            ..TRACKS[0]
        }];
        let schedule = compile_arrangement(
            timeline(),
            input(&inactive_tracks, &CLIPS, &ASSETS, &PLACEMENTS),
            GraphLimits::VERSION_1,
        )
        .unwrap();
        assert!(!schedule.entries()[0].compiled.clip_spec().enabled);
    }
}
