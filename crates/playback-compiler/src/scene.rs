//! Bounded compilation and sample-exact execution of recorded scene authority.

use core::error::Error;
use core::fmt;

use crate::{Timeline, TimelineError};

pub const VERSION_1_MAX_SCENE_TRACKS: usize = 8;
pub const VERSION_1_MAX_SCENES: usize = 8;
pub const VERSION_1_MAX_SCENE_SLOTS: usize = 8;
pub const VERSION_1_MAX_SCENE_LAUNCHES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchQuantization {
    Immediate,
    Grid { quantum_ticks: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneSlotAction<'a> {
    Play { clip_id: &'a str },
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneSlotResource<'a> {
    pub track_id: &'a str,
    pub action: SceneSlotAction<'a>,
}

#[derive(Clone, Copy, Debug)]
pub struct SceneResource<'a> {
    pub id: &'a str,
    pub enabled: bool,
    pub order: u8,
    pub launch_quantization: LaunchQuantization,
    pub slots: &'a [SceneSlotResource<'a>],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneLaunchResource<'a> {
    pub id: &'a str,
    pub tick: u64,
    pub scene_id: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneTrackResource<'a> {
    pub id: &'a str,
    pub active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneClipResource<'a> {
    pub id: &'a str,
    pub track_id: &'a str,
    pub enabled: bool,
    pub duration_ticks: u64,
    pub loop_enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct ScenePerformanceInput<'a> {
    pub enabled: bool,
    pub tracks: &'a [SceneTrackResource<'a>],
    pub clips: &'a [SceneClipResource<'a>],
    pub scenes: &'a [SceneResource<'a>],
    pub launches: &'a [SceneLaunchResource<'a>],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneLimits {
    pub max_tracks: usize,
    pub max_clips: usize,
    pub max_scenes: usize,
    pub max_slots_per_scene: usize,
    pub max_launches: usize,
}

impl SceneLimits {
    pub const VERSION_1: Self = Self {
        max_tracks: VERSION_1_MAX_SCENE_TRACKS,
        max_clips: 4_096,
        max_scenes: VERSION_1_MAX_SCENES,
        max_slots_per_scene: VERSION_1_MAX_SCENE_SLOTS,
        max_launches: VERSION_1_MAX_SCENE_LAUNCHES,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneCollection {
    Track,
    Clip,
    Scene,
    Slot,
    Launch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneCompileError {
    LimitExceeded {
        collection: SceneCollection,
        actual: usize,
        maximum: usize,
    },
    CapacityUnavailable,
    EmptyIdentifier {
        collection: SceneCollection,
        index: usize,
    },
    DuplicateIdentifier {
        collection: SceneCollection,
        index: usize,
    },
    DuplicateSceneOrder {
        scene_index: usize,
    },
    InvalidSceneOrder {
        scene_index: usize,
    },
    InvalidQuantization {
        scene_index: usize,
    },
    DuplicateSlotTrack {
        scene_index: usize,
        slot_index: usize,
    },
    MissingSlotTrack {
        scene_index: usize,
        slot_index: usize,
    },
    MissingSlotClip {
        scene_index: usize,
        slot_index: usize,
    },
    SlotClipOwnerMismatch {
        scene_index: usize,
        slot_index: usize,
    },
    InvalidClipDuration {
        clip_index: usize,
    },
    MissingClipTrack {
        clip_index: usize,
    },
    LaunchesNotStrictlyIncreasing {
        launch_index: usize,
    },
    MissingLaunchScene {
        launch_index: usize,
    },
    LaunchOffGrid {
        launch_index: usize,
    },
    TickOverflow {
        launch_index: usize,
        slot_index: usize,
    },
    Timeline(TimelineError),
}

impl fmt::Display for SceneCompileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded {
                collection,
                actual,
                maximum,
            } => write!(
                formatter,
                "{collection:?} count {actual} exceeds the limit {maximum}"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("scene authority capacity is unavailable")
            }
            Self::EmptyIdentifier { collection, index } => {
                write!(formatter, "{collection:?} {index} has an empty identifier")
            }
            Self::DuplicateIdentifier { collection, index } => {
                write!(formatter, "{collection:?} {index} duplicates an identifier")
            }
            Self::DuplicateSceneOrder { scene_index } => {
                write!(formatter, "scene {scene_index} duplicates an order value")
            }
            Self::InvalidSceneOrder { scene_index } => {
                write!(formatter, "scene {scene_index} has an invalid order value")
            }
            Self::InvalidQuantization { scene_index } => {
                write!(formatter, "scene {scene_index} has invalid quantization")
            }
            Self::DuplicateSlotTrack {
                scene_index,
                slot_index,
            } => write!(
                formatter,
                "scene {scene_index} slot {slot_index} duplicates a track action"
            ),
            Self::MissingSlotTrack {
                scene_index,
                slot_index,
            } => write!(
                formatter,
                "scene {scene_index} slot {slot_index} references a missing track"
            ),
            Self::MissingSlotClip {
                scene_index,
                slot_index,
            } => write!(
                formatter,
                "scene {scene_index} slot {slot_index} references a missing clip"
            ),
            Self::SlotClipOwnerMismatch {
                scene_index,
                slot_index,
            } => write!(
                formatter,
                "scene {scene_index} slot {slot_index} references a clip owned by another track"
            ),
            Self::InvalidClipDuration { clip_index } => {
                write!(formatter, "scene clip {clip_index} has zero duration")
            }
            Self::MissingClipTrack { clip_index } => {
                write!(
                    formatter,
                    "scene clip {clip_index} references a missing track"
                )
            }
            Self::LaunchesNotStrictlyIncreasing { launch_index } => write!(
                formatter,
                "scene launch {launch_index} is not strictly ordered by tick"
            ),
            Self::MissingLaunchScene { launch_index } => {
                write!(
                    formatter,
                    "scene launch {launch_index} references a missing scene"
                )
            }
            Self::LaunchOffGrid { launch_index } => {
                write!(formatter, "scene launch {launch_index} is off its grid")
            }
            Self::TickOverflow {
                launch_index,
                slot_index,
            } => write!(
                formatter,
                "scene launch {launch_index} slot {slot_index} exceeds the tick domain"
            ),
            Self::Timeline(error) => write!(formatter, "scene timeline is invalid: {error}"),
        }
    }
}

impl Error for SceneCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TimelineError> for SceneCompileError {
    fn from(error: TimelineError) -> Self {
        Self::Timeline(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayingSceneClip<'a> {
    launch_id: &'a str,
    scene_id: &'a str,
    clip_id: &'a str,
    started_at_frame: usize,
    natural_end_frame: Option<usize>,
    enabled: bool,
}

impl<'a> PlayingSceneClip<'a> {
    #[must_use]
    pub const fn launch_id(self) -> &'a str {
        self.launch_id
    }

    #[must_use]
    pub const fn scene_id(self) -> &'a str {
        self.scene_id
    }

    #[must_use]
    pub const fn clip_id(self) -> &'a str {
        self.clip_id
    }

    #[must_use]
    pub const fn started_at_frame(self) -> usize {
        self.started_at_frame
    }

    #[must_use]
    pub const fn natural_end_frame(self) -> Option<usize> {
        self.natural_end_frame
    }

    #[must_use]
    pub const fn enabled(self) -> bool {
        self.enabled
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompiledAction<'a> {
    Play(PlayingSceneClip<'a>),
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TrackAction<'a> {
    frame: usize,
    action: CompiledAction<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompiledSceneTrack<'a> {
    id: &'a str,
    actions: Vec<TrackAction<'a>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledScenePerformance<'a> {
    tracks: Vec<CompiledSceneTrack<'a>>,
}

impl CompiledScenePerformance<'_> {
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    #[must_use]
    pub fn track_id(&self, index: usize) -> Option<&str> {
        self.tracks.get(index).map(|track| track.id)
    }

    /// Creates runtime state positioned immediately before rendering `frame`.
    ///
    /// # Errors
    ///
    /// Returns [`SceneRuntimeError::CapacityUnavailable`] if fixed runtime
    /// state cannot be allocated.
    pub fn runtime_at(&self, frame: usize) -> Result<SceneRuntime<'_, '_>, SceneRuntimeError> {
        SceneRuntime::try_new(self, frame)
    }
}

/// Resolves recorded scene launches into per-track frame-domain actions.
///
/// Construction is a control-plane operation. The resulting runtime processes
/// contiguous windows without allocation.
///
/// # Errors
///
/// Returns [`SceneCompileError`] for bounds, identity, order, reference,
/// quantization, duration, arithmetic, or timeline failures.
pub fn compile_scene_performance<'a>(
    timeline: Timeline<'_>,
    input: ScenePerformanceInput<'a>,
    limits: SceneLimits,
) -> Result<CompiledScenePerformance<'a>, SceneCompileError> {
    enforce_limit(
        SceneCollection::Track,
        input.tracks.len(),
        limits.max_tracks,
    )?;
    enforce_limit(SceneCollection::Clip, input.clips.len(), limits.max_clips)?;
    enforce_limit(
        SceneCollection::Scene,
        input.scenes.len(),
        limits.max_scenes,
    )?;
    enforce_limit(
        SceneCollection::Launch,
        input.launches.len(),
        limits.max_launches,
    )?;
    validate_identities(input)?;
    validate_resources(input, limits)?;

    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(input.tracks.len())
        .map_err(|_| SceneCompileError::CapacityUnavailable)?;
    for track in input.tracks {
        tracks.push(CompiledSceneTrack {
            id: track.id,
            actions: Vec::new(),
        });
    }

    compile_launches(timeline, input, &mut tracks)?;
    Ok(CompiledScenePerformance { tracks })
}

fn compile_launches<'a>(
    timeline: Timeline<'_>,
    input: ScenePerformanceInput<'a>,
    tracks: &mut [CompiledSceneTrack<'a>],
) -> Result<(), SceneCompileError> {
    let mut previous_tick = None;
    for (launch_index, launch) in input.launches.iter().enumerate() {
        if previous_tick.is_some_and(|tick| launch.tick <= tick) {
            return Err(SceneCompileError::LaunchesNotStrictlyIncreasing { launch_index });
        }
        previous_tick = Some(launch.tick);
        let scene_index = find_scene(input.scenes, launch.scene_id)
            .ok_or(SceneCompileError::MissingLaunchScene { launch_index })?;
        let scene = input.scenes[scene_index];
        if matches!(
            scene.launch_quantization,
            LaunchQuantization::Grid { quantum_ticks } if launch.tick % quantum_ticks != 0
        ) {
            return Err(SceneCompileError::LaunchOffGrid { launch_index });
        }
        if !input.enabled || !scene.enabled {
            continue;
        }
        let start_frame = timeline.tick_to_frame_ceil(launch.tick)?;
        for (slot_index, slot) in scene.slots.iter().enumerate() {
            let track_index = find_track(input.tracks, slot.track_id).ok_or(
                SceneCompileError::MissingSlotTrack {
                    scene_index,
                    slot_index,
                },
            )?;
            let action = match slot.action {
                SceneSlotAction::Stop => CompiledAction::Stop,
                SceneSlotAction::Play { clip_id } => {
                    let clip_index = find_clip(input.clips, clip_id).ok_or(
                        SceneCompileError::MissingSlotClip {
                            scene_index,
                            slot_index,
                        },
                    )?;
                    let clip = input.clips[clip_index];
                    if clip.track_id != slot.track_id {
                        return Err(SceneCompileError::SlotClipOwnerMismatch {
                            scene_index,
                            slot_index,
                        });
                    }
                    let natural_end_frame = if clip.loop_enabled {
                        None
                    } else {
                        let end_tick = launch.tick.checked_add(clip.duration_ticks).ok_or(
                            SceneCompileError::TickOverflow {
                                launch_index,
                                slot_index,
                            },
                        )?;
                        Some(timeline.tick_to_frame_ceil(end_tick)?)
                    };
                    CompiledAction::Play(PlayingSceneClip {
                        launch_id: launch.id,
                        scene_id: scene.id,
                        clip_id,
                        started_at_frame: start_frame,
                        natural_end_frame,
                        enabled: input.tracks[track_index].active && clip.enabled,
                    })
                }
            };
            tracks[track_index]
                .actions
                .try_reserve(1)
                .map_err(|_| SceneCompileError::CapacityUnavailable)?;
            tracks[track_index].actions.push(TrackAction {
                frame: start_frame,
                action,
            });
        }
    }
    Ok(())
}

fn enforce_limit(
    collection: SceneCollection,
    actual: usize,
    maximum: usize,
) -> Result<(), SceneCompileError> {
    if actual > maximum {
        return Err(SceneCompileError::LimitExceeded {
            collection,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_identities(input: ScenePerformanceInput<'_>) -> Result<(), SceneCompileError> {
    validate_ids(
        SceneCollection::Track,
        input.tracks.iter().map(|resource| resource.id),
    )?;
    validate_ids(
        SceneCollection::Clip,
        input.clips.iter().map(|resource| resource.id),
    )?;
    validate_ids(
        SceneCollection::Scene,
        input.scenes.iter().map(|resource| resource.id),
    )?;
    validate_ids(
        SceneCollection::Launch,
        input.launches.iter().map(|resource| resource.id),
    )
}

fn validate_ids<'a>(
    collection: SceneCollection,
    ids: impl Iterator<Item = &'a str>,
) -> Result<(), SceneCompileError> {
    let mut seen = Vec::new();
    for (index, id) in ids.enumerate() {
        if id.is_empty() {
            return Err(SceneCompileError::EmptyIdentifier { collection, index });
        }
        if seen.contains(&id) {
            return Err(SceneCompileError::DuplicateIdentifier { collection, index });
        }
        seen.try_reserve(1)
            .map_err(|_| SceneCompileError::CapacityUnavailable)?;
        seen.push(id);
    }
    Ok(())
}

fn validate_resources(
    input: ScenePerformanceInput<'_>,
    limits: SceneLimits,
) -> Result<(), SceneCompileError> {
    for (clip_index, clip) in input.clips.iter().enumerate() {
        if clip.duration_ticks == 0 {
            return Err(SceneCompileError::InvalidClipDuration { clip_index });
        }
        if find_track(input.tracks, clip.track_id).is_none() {
            return Err(SceneCompileError::MissingClipTrack { clip_index });
        }
    }
    let mut scene_orders = [false; VERSION_1_MAX_SCENES];
    for (scene_index, scene) in input.scenes.iter().enumerate() {
        enforce_limit(
            SceneCollection::Slot,
            scene.slots.len(),
            limits.max_slots_per_scene,
        )?;
        let order = usize::from(scene.order);
        if order >= VERSION_1_MAX_SCENES {
            return Err(SceneCompileError::InvalidSceneOrder { scene_index });
        }
        if scene_orders[order] {
            return Err(SceneCompileError::DuplicateSceneOrder { scene_index });
        }
        scene_orders[order] = true;
        if matches!(
            scene.launch_quantization,
            LaunchQuantization::Grid { quantum_ticks: 0 }
        ) {
            return Err(SceneCompileError::InvalidQuantization { scene_index });
        }
        for (slot_index, slot) in scene.slots.iter().enumerate() {
            if scene.slots[..slot_index]
                .iter()
                .any(|previous| previous.track_id == slot.track_id)
            {
                return Err(SceneCompileError::DuplicateSlotTrack {
                    scene_index,
                    slot_index,
                });
            }
            let track_index = find_track(input.tracks, slot.track_id).ok_or(
                SceneCompileError::MissingSlotTrack {
                    scene_index,
                    slot_index,
                },
            )?;
            if let SceneSlotAction::Play { clip_id } = slot.action {
                let clip_index =
                    find_clip(input.clips, clip_id).ok_or(SceneCompileError::MissingSlotClip {
                        scene_index,
                        slot_index,
                    })?;
                if input.clips[clip_index].track_id != input.tracks[track_index].id {
                    return Err(SceneCompileError::SlotClipOwnerMismatch {
                        scene_index,
                        slot_index,
                    });
                }
            }
        }
    }
    Ok(())
}

fn find_track(tracks: &[SceneTrackResource<'_>], id: &str) -> Option<usize> {
    tracks.iter().position(|track| track.id == id)
}

fn find_clip(clips: &[SceneClipResource<'_>], id: &str) -> Option<usize> {
    clips.iter().position(|clip| clip.id == id)
}

fn find_scene(scenes: &[SceneResource<'_>], id: &str) -> Option<usize> {
    scenes.iter().position(|scene| scene.id == id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneRuntimeError {
    CapacityUnavailable,
    WindowNotContiguous { expected: usize, actual: usize },
    FrameOverflow,
}

impl fmt::Display for SceneRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("scene runtime capacity is unavailable")
            }
            Self::WindowNotContiguous { expected, actual } => write!(
                formatter,
                "scene window begins at frame {actual}; expected {expected}"
            ),
            Self::FrameOverflow => formatter.write_str("scene window exceeds the frame domain"),
        }
    }
}

impl Error for SceneRuntimeError {}

#[derive(Clone, Copy, Debug)]
pub struct SceneSegment<'state, 'resource> {
    pub offset_frames: usize,
    pub frame_count: usize,
    pub track_states: &'state [Option<PlayingSceneClip<'resource>>],
}

#[derive(Debug)]
pub struct SceneRuntime<'plan, 'resource> {
    plan: &'plan CompiledScenePerformance<'resource>,
    states: Vec<Option<PlayingSceneClip<'resource>>>,
    cursors: Vec<usize>,
    position: usize,
}

impl<'plan, 'resource> SceneRuntime<'plan, 'resource> {
    fn try_new(
        plan: &'plan CompiledScenePerformance<'resource>,
        frame: usize,
    ) -> Result<Self, SceneRuntimeError> {
        let mut states = Vec::new();
        states
            .try_reserve_exact(plan.tracks.len())
            .map_err(|_| SceneRuntimeError::CapacityUnavailable)?;
        states.resize(plan.tracks.len(), None);
        let mut cursors = Vec::new();
        cursors
            .try_reserve_exact(plan.tracks.len())
            .map_err(|_| SceneRuntimeError::CapacityUnavailable)?;
        cursors.resize(plan.tracks.len(), 0);
        let mut runtime = Self {
            plan,
            states,
            cursors,
            position: frame,
        };
        runtime.seek(frame);
        Ok(runtime)
    }

    #[must_use]
    pub const fn position(&self) -> usize {
        self.position
    }

    #[must_use]
    pub fn track_states(&self) -> &[Option<PlayingSceneClip<'resource>>] {
        &self.states
    }

    /// Reconstructs per-track state at an arbitrary sample frame.
    pub fn seek(&mut self, frame: usize) {
        self.position = frame;
        for (track_index, track) in self.plan.tracks.iter().enumerate() {
            let cursor = track
                .actions
                .partition_point(|action| action.frame <= frame);
            self.cursors[track_index] = cursor;
            self.states[track_index] =
                track.actions[..cursor]
                    .last()
                    .and_then(|action| match action.action {
                        CompiledAction::Play(playing)
                            if playing.natural_end_frame.is_none_or(|end| end > frame) =>
                        {
                            Some(playing)
                        }
                        CompiledAction::Play(_) | CompiledAction::Stop => None,
                    });
        }
    }

    /// Visits each constant-state segment in one contiguous processing window.
    ///
    /// The observer is called synchronously and the method performs no
    /// allocation.
    ///
    /// # Errors
    ///
    /// Returns [`SceneRuntimeError`] for a noncontiguous window or frame-range
    /// overflow.
    pub fn process_window(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        mut observe: impl FnMut(SceneSegment<'_, 'resource>),
    ) -> Result<usize, SceneRuntimeError> {
        if start_frame != self.position {
            return Err(SceneRuntimeError::WindowNotContiguous {
                expected: self.position,
                actual: start_frame,
            });
        }
        let end_frame = start_frame
            .checked_add(frame_count)
            .ok_or(SceneRuntimeError::FrameOverflow)?;
        let mut segment_count = 0;
        while self.position < end_frame {
            self.apply_boundary();
            let next_boundary = self.next_boundary(end_frame);
            observe(SceneSegment {
                offset_frames: self.position - start_frame,
                frame_count: next_boundary - self.position,
                track_states: &self.states,
            });
            segment_count += 1;
            self.position = next_boundary;
        }
        Ok(segment_count)
    }

    fn apply_boundary(&mut self) {
        for state in &mut self.states {
            if state.is_some_and(|playing| {
                playing
                    .natural_end_frame
                    .is_some_and(|end| end <= self.position)
            }) {
                *state = None;
            }
        }
        for (track_index, track) in self.plan.tracks.iter().enumerate() {
            while let Some(action) = track.actions.get(self.cursors[track_index]) {
                if action.frame > self.position {
                    break;
                }
                self.states[track_index] = match action.action {
                    CompiledAction::Play(playing) => Some(playing),
                    CompiledAction::Stop => None,
                };
                self.cursors[track_index] += 1;
            }
        }
    }

    fn next_boundary(&self, end_frame: usize) -> usize {
        let mut boundary = end_frame;
        for (track_index, track) in self.plan.tracks.iter().enumerate() {
            if let Some(action) = track.actions.get(self.cursors[track_index])
                && action.frame > self.position
            {
                boundary = boundary.min(action.frame);
            }
            if let Some(end) = self.states[track_index]
                .and_then(PlayingSceneClip::natural_end_frame)
                .filter(|end| *end > self.position)
            {
                boundary = boundary.min(end);
            }
        }
        boundary
    }
}
