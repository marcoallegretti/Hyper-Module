//! Bounded compilation and sample-exact execution of recorded scene authority.

use core::convert::Infallible;
use core::error::Error;
use core::fmt;

use crate::{FrameTimeline, FrameTimelineError, Timeline, TimelineError};

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
pub struct SceneResource<'input, 'resource> {
    pub id: &'resource str,
    pub enabled: bool,
    pub order: u8,
    pub launch_quantization: LaunchQuantization,
    pub slots: &'input [SceneSlotResource<'resource>],
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
pub struct ScenePerformanceInput<'input, 'resource> {
    pub enabled: bool,
    pub tracks: &'input [SceneTrackResource<'resource>],
    pub clips: &'input [SceneClipResource<'resource>],
    pub scenes: &'input [SceneResource<'input, 'resource>],
    pub launches: &'input [SceneLaunchResource<'resource>],
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
    FrameTimeline(FrameTimelineError),
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
            Self::FrameTimeline(error) => {
                write!(formatter, "scene frame timeline is invalid: {error}")
            }
        }
    }
}

impl Error for SceneCompileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(error) => Some(error),
            Self::FrameTimeline(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TimelineError> for SceneCompileError {
    fn from(error: TimelineError) -> Self {
        Self::Timeline(error)
    }
}

impl From<FrameTimelineError> for SceneCompileError {
    fn from(error: FrameTimelineError) -> Self {
        Self::FrameTimeline(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScenePlaybackSource<'a> {
    Recorded {
        launch_id: &'a str,
    },
    Interactive {
        scene_index: usize,
        slot_index: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayingSceneClip<'a> {
    source: ScenePlaybackSource<'a>,
    scene_id: &'a str,
    clip_id: &'a str,
    started_at_tick: u64,
    started_at_frame: usize,
    natural_end_frame: Option<usize>,
    enabled: bool,
}

impl<'a> PlayingSceneClip<'a> {
    #[must_use]
    pub const fn source(self) -> ScenePlaybackSource<'a> {
        self.source
    }

    #[must_use]
    pub const fn launch_id(self) -> Option<&'a str> {
        match self.source {
            ScenePlaybackSource::Recorded { launch_id } => Some(launch_id),
            ScenePlaybackSource::Interactive { .. } => None,
        }
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
    pub const fn started_at_tick(self) -> u64 {
        self.started_at_tick
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompiledTemplateAction<'a> {
    Play {
        clip_id: &'a str,
        duration_frames: usize,
        loop_enabled: bool,
        enabled: bool,
    },
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompiledSceneTemplateSlot<'a> {
    track_index: usize,
    action: CompiledTemplateAction<'a>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompiledSceneTemplate<'a> {
    id: &'a str,
    enabled: bool,
    launch_quantization: LaunchQuantization,
    slots: Vec<CompiledSceneTemplateSlot<'a>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledScenePerformance<'a> {
    tracks: Vec<CompiledSceneTrack<'a>>,
    scenes: Vec<CompiledSceneTemplate<'a>>,
    timeline: FrameTimeline,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordedSceneExtent {
    Empty,
    Finite { end_frame: usize },
    Unbounded,
}

impl<'resource> CompiledScenePerformance<'resource> {
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// Returns the exact extent of recorded scene authority. Interactive scene
    /// templates do not contribute until they are launched at runtime.
    #[must_use]
    pub fn recorded_extent(&self) -> RecordedSceneExtent {
        let mut end_frame = None::<usize>;
        for track in &self.tracks {
            let Some(action) = track.actions.last() else {
                continue;
            };
            let track_end = match action.action {
                CompiledAction::Stop => action.frame,
                CompiledAction::Play(playing) if !playing.enabled() => action.frame,
                CompiledAction::Play(playing) => {
                    let Some(end_frame) = playing.natural_end_frame() else {
                        return RecordedSceneExtent::Unbounded;
                    };
                    end_frame
                }
            };
            end_frame = Some(end_frame.map_or(track_end, |current| current.max(track_end)));
        }
        end_frame.map_or(RecordedSceneExtent::Empty, |end_frame| {
            RecordedSceneExtent::Finite { end_frame }
        })
    }

    #[must_use]
    pub fn track_id(&self, index: usize) -> Option<&'resource str> {
        self.tracks.get(index).map(|track| track.id)
    }

    #[must_use]
    pub fn track_has_actions(&self, index: usize) -> Option<bool> {
        self.tracks
            .get(index)
            .map(|track| !track.actions.is_empty())
    }

    #[must_use]
    pub fn track_has_scene_slots(&self, index: usize) -> Option<bool> {
        self.tracks.get(index).map(|_| {
            self.scenes
                .iter()
                .any(|scene| scene.slots.iter().any(|slot| slot.track_index == index))
        })
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.scenes.len()
    }

    #[must_use]
    pub fn scene_id(&self, index: usize) -> Option<&'resource str> {
        self.scenes.get(index).map(|scene| scene.id)
    }

    /// Converts an exact project tick through the same compiled tempo map used
    /// by recorded and interactive scene scheduling.
    ///
    /// # Errors
    ///
    /// Returns [`FrameTimelineError`] if checked arithmetic fails or the
    /// resulting frame lies outside the platform range.
    pub fn tick_to_frame_ceil(&self, tick: u64) -> Result<usize, FrameTimelineError> {
        self.timeline.tick_to_frame_ceil(tick)
    }

    #[must_use]
    pub fn scene_enabled(&self, index: usize) -> Option<bool> {
        self.scenes.get(index).map(|scene| scene.enabled)
    }

    pub fn interactive_clips(
        &self,
    ) -> impl Iterator<Item = (usize, usize, &'resource str, &'resource str, bool)> + '_ {
        self.scenes
            .iter()
            .enumerate()
            .flat_map(move |(scene_index, scene)| {
                scene
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(slot_index, slot)| {
                        if let CompiledTemplateAction::Play {
                            clip_id, enabled, ..
                        } = slot.action
                        {
                            Some((
                                scene_index,
                                slot_index,
                                self.tracks[slot.track_index].id,
                                clip_id,
                                enabled,
                            ))
                        } else {
                            None
                        }
                    })
            })
    }

    pub fn playing_clips(
        &self,
    ) -> impl Iterator<Item = (&'resource str, PlayingSceneClip<'resource>)> + '_ {
        self.tracks.iter().flat_map(|track| {
            track.actions.iter().filter_map(move |action| {
                if let CompiledAction::Play(playing) = action.action {
                    Some((track.id, playing))
                } else {
                    None
                }
            })
        })
    }

    /// Creates runtime state positioned immediately before rendering `frame`.
    ///
    /// # Errors
    ///
    /// Returns [`SceneRuntimeError::CapacityUnavailable`] if fixed runtime
    /// state cannot be allocated.
    pub fn runtime_at<'plan>(
        &'plan self,
        frame: usize,
    ) -> Result<SceneRuntime<'plan, 'resource>, SceneRuntimeError> {
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
pub fn compile_scene_performance<'resource>(
    timeline: Timeline<'_>,
    input: ScenePerformanceInput<'_, 'resource>,
    limits: SceneLimits,
) -> Result<CompiledScenePerformance<'resource>, SceneCompileError> {
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

    let scenes = compile_scene_templates(timeline, input)?;
    compile_launches(timeline, input, &mut tracks)?;
    let frame_timeline = FrameTimeline::try_new(timeline.sample_rate_hz(), timeline.tempo_events)?;
    Ok(CompiledScenePerformance {
        tracks,
        scenes,
        timeline: frame_timeline,
    })
}

fn compile_scene_templates<'resource>(
    timeline: Timeline<'_>,
    input: ScenePerformanceInput<'_, 'resource>,
) -> Result<Vec<CompiledSceneTemplate<'resource>>, SceneCompileError> {
    let mut scenes = Vec::new();
    scenes
        .try_reserve_exact(input.scenes.len())
        .map_err(|_| SceneCompileError::CapacityUnavailable)?;
    for (scene_index, scene) in input.scenes.iter().enumerate() {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(scene.slots.len())
            .map_err(|_| SceneCompileError::CapacityUnavailable)?;
        for (slot_index, slot) in scene.slots.iter().enumerate() {
            let track_index = find_track(input.tracks, slot.track_id).ok_or(
                SceneCompileError::MissingSlotTrack {
                    scene_index,
                    slot_index,
                },
            )?;
            let action = match slot.action {
                SceneSlotAction::Stop => CompiledTemplateAction::Stop,
                SceneSlotAction::Play { clip_id } => {
                    let clip_index = find_clip(input.clips, clip_id).ok_or(
                        SceneCompileError::MissingSlotClip {
                            scene_index,
                            slot_index,
                        },
                    )?;
                    let clip = input.clips[clip_index];
                    CompiledTemplateAction::Play {
                        clip_id,
                        duration_frames: timeline.tick_to_frame_ceil(clip.duration_ticks)?,
                        loop_enabled: clip.loop_enabled,
                        enabled: input.enabled
                            && scene.enabled
                            && input.tracks[track_index].active
                            && clip.enabled,
                    }
                }
            };
            slots.push(CompiledSceneTemplateSlot {
                track_index,
                action,
            });
        }
        scenes.push(CompiledSceneTemplate {
            id: scene.id,
            enabled: input.enabled && scene.enabled,
            launch_quantization: scene.launch_quantization,
            slots,
        });
    }
    Ok(scenes)
}

fn compile_launches<'resource>(
    timeline: Timeline<'_>,
    input: ScenePerformanceInput<'_, 'resource>,
    tracks: &mut [CompiledSceneTrack<'resource>],
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
                        source: ScenePlaybackSource::Recorded {
                            launch_id: launch.id,
                        },
                        scene_id: scene.id,
                        clip_id,
                        started_at_tick: launch.tick,
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

fn validate_identities(input: ScenePerformanceInput<'_, '_>) -> Result<(), SceneCompileError> {
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
    input: ScenePerformanceInput<'_, '_>,
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

fn find_scene(scenes: &[SceneResource<'_, '_>], id: &str) -> Option<usize> {
    scenes.iter().position(|scene| scene.id == id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SceneRuntimeError {
    CapacityUnavailable,
    WindowNotContiguous { expected: usize, actual: usize },
    FrameOverflow,
    MissingScene { scene_index: usize },
    SceneDisabled { scene_index: usize },
    Timeline(FrameTimelineError),
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
            Self::MissingScene { scene_index } => {
                write!(formatter, "scene {scene_index} is missing")
            }
            Self::SceneDisabled { scene_index } => {
                write!(formatter, "scene {scene_index} is disabled")
            }
            Self::Timeline(source) => write!(formatter, "scene launch timing failed: {source}"),
        }
    }
}

impl Error for SceneRuntimeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::CapacityUnavailable
            | Self::WindowNotContiguous { .. }
            | Self::FrameOverflow
            | Self::MissingScene { .. }
            | Self::SceneDisabled { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum SceneProcessError<E> {
    Runtime(SceneRuntimeError),
    Observer(E),
}

#[derive(Clone, Copy, Debug)]
pub struct SceneSegment<'state, 'resource> {
    pub offset_frames: usize,
    pub frame_count: usize,
    pub track_states: &'state [Option<PlayingSceneClip<'resource>>],
    pub track_controlled: &'state [bool],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingSceneLaunch {
    scene_index: usize,
    tick: u64,
    frame: usize,
}

/// Committed interactive scene-launch state at one runtime position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneLaunchState {
    queued_scene_index: u8,
    last_launched_scene_index: u8,
    queued_launch_frame: usize,
}

impl SceneLaunchState {
    const NO_SCENE: u8 = u8::MAX;

    #[must_use]
    pub const fn queued_scene_index(self) -> Option<usize> {
        if self.queued_scene_index == Self::NO_SCENE {
            None
        } else {
            Some(self.queued_scene_index as usize)
        }
    }

    #[must_use]
    pub const fn queued_launch_frame(self) -> Option<usize> {
        if self.queued_scene_index == Self::NO_SCENE {
            None
        } else {
            Some(self.queued_launch_frame)
        }
    }

    #[must_use]
    pub const fn last_launched_scene_index(self) -> Option<usize> {
        if self.last_launched_scene_index == Self::NO_SCENE {
            None
        } else {
            Some(self.last_launched_scene_index as usize)
        }
    }
}

impl Default for SceneLaunchState {
    fn default() -> Self {
        Self {
            queued_scene_index: Self::NO_SCENE,
            last_launched_scene_index: Self::NO_SCENE,
            queued_launch_frame: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SceneRuntimeCheckpoint<'resource> {
    states: [Option<PlayingSceneClip<'resource>>; VERSION_1_MAX_SCENE_TRACKS],
    cursors: [usize; VERSION_1_MAX_SCENE_TRACKS],
    controlled: [bool; VERSION_1_MAX_SCENE_TRACKS],
    track_count: usize,
    position: usize,
    pending: Option<PendingSceneLaunch>,
    last_launched_scene_index: Option<usize>,
}

#[derive(Debug)]
pub struct SceneRuntime<'plan, 'resource> {
    plan: &'plan CompiledScenePerformance<'resource>,
    states: Vec<Option<PlayingSceneClip<'resource>>>,
    cursors: Vec<usize>,
    controlled: Vec<bool>,
    position: usize,
    pending: Option<PendingSceneLaunch>,
    last_launched_scene_index: Option<usize>,
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
        let mut controlled = Vec::new();
        controlled
            .try_reserve_exact(plan.tracks.len())
            .map_err(|_| SceneRuntimeError::CapacityUnavailable)?;
        controlled.resize(plan.tracks.len(), false);
        let mut runtime = Self {
            plan,
            states,
            cursors,
            controlled,
            position: frame,
            pending: None,
            last_launched_scene_index: None,
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

    #[must_use]
    pub fn track_controlled(&self) -> &[bool] {
        &self.controlled
    }

    #[must_use]
    pub fn scene_count(&self) -> usize {
        self.plan.scene_count()
    }

    /// Returns the queued launch and the most recently applied interactive scene.
    ///
    /// # Panics
    ///
    /// Panics only if an internally compiled scene index exceeds the version-1
    /// scene limit, which compilation rejects before runtime construction.
    #[must_use]
    pub fn launch_state(&self) -> SceneLaunchState {
        let (queued_scene_index, queued_launch_frame) = match self.pending {
            Some(pending) => (
                u8::try_from(pending.scene_index)
                    .expect("compiled scene indices fit the version-1 scene limit"),
                pending.frame,
            ),
            None => (SceneLaunchState::NO_SCENE, 0),
        };
        SceneLaunchState {
            queued_scene_index,
            queued_launch_frame,
            last_launched_scene_index: self.last_launched_scene_index.map_or(
                SceneLaunchState::NO_SCENE,
                |scene_index| {
                    u8::try_from(scene_index)
                        .expect("compiled scene indices fit the version-1 scene limit")
                },
            ),
        }
    }

    #[must_use]
    pub fn checkpoint(&self) -> SceneRuntimeCheckpoint<'resource> {
        let mut states = [None; VERSION_1_MAX_SCENE_TRACKS];
        states[..self.states.len()].copy_from_slice(&self.states);
        let mut cursors = [0; VERSION_1_MAX_SCENE_TRACKS];
        cursors[..self.cursors.len()].copy_from_slice(&self.cursors);
        let mut controlled = [false; VERSION_1_MAX_SCENE_TRACKS];
        controlled[..self.controlled.len()].copy_from_slice(&self.controlled);
        SceneRuntimeCheckpoint {
            states,
            cursors,
            controlled,
            track_count: self.states.len(),
            position: self.position,
            pending: self.pending,
            last_launched_scene_index: self.last_launched_scene_index,
        }
    }

    pub fn restore(&mut self, checkpoint: SceneRuntimeCheckpoint<'resource>) {
        debug_assert_eq!(checkpoint.track_count, self.states.len());
        self.states
            .copy_from_slice(&checkpoint.states[..checkpoint.track_count]);
        self.cursors
            .copy_from_slice(&checkpoint.cursors[..checkpoint.track_count]);
        self.controlled
            .copy_from_slice(&checkpoint.controlled[..checkpoint.track_count]);
        self.position = checkpoint.position;
        self.pending = checkpoint.pending;
        self.last_launched_scene_index = checkpoint.last_launched_scene_index;
    }

    /// Replaces any pending interactive launch with the latest request.
    ///
    /// The launch executes at the current callback boundary for immediate
    /// scenes or at the first exact grid boundary at or after it.
    ///
    /// # Errors
    ///
    /// Returns [`SceneRuntimeError`] for an unknown or disabled scene, or if
    /// its quantized execution position exceeds the timeline domain.
    pub fn queue_scene_launch(&mut self, scene_index: usize) -> Result<(), SceneRuntimeError> {
        let scene = self
            .plan
            .scenes
            .get(scene_index)
            .ok_or(SceneRuntimeError::MissingScene { scene_index })?;
        if !scene.enabled {
            return Err(SceneRuntimeError::SceneDisabled { scene_index });
        }
        let position = self
            .plan
            .timeline
            .position_at_frame(self.position)
            .map_err(SceneRuntimeError::Timeline)?;
        let (tick, frame) = match scene.launch_quantization {
            LaunchQuantization::Immediate => (position.whole_tick(), self.position),
            LaunchQuantization::Grid { quantum_ticks } => {
                let tick = position
                    .quantize_forward(quantum_ticks)
                    .map_err(SceneRuntimeError::Timeline)?;
                let frame = if position.is_exact() && tick == position.whole_tick() {
                    self.position
                } else {
                    self.plan
                        .timeline
                        .tick_to_frame_ceil(tick)
                        .map_err(SceneRuntimeError::Timeline)?
                };
                (tick, frame)
            }
        };
        for slot in &scene.slots {
            if let CompiledTemplateAction::Play {
                duration_frames,
                loop_enabled: false,
                ..
            } = slot.action
            {
                frame
                    .checked_add(duration_frames)
                    .ok_or(SceneRuntimeError::FrameOverflow)?;
            }
        }
        self.pending = Some(PendingSceneLaunch {
            scene_index,
            tick,
            frame,
        });
        Ok(())
    }

    /// Reconstructs per-track state at an arbitrary sample frame.
    pub fn seek(&mut self, frame: usize) {
        self.position = frame;
        self.pending = None;
        self.last_launched_scene_index = None;
        for (track_index, track) in self.plan.tracks.iter().enumerate() {
            self.controlled[track_index] = !track.actions.is_empty();
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
        match self.try_process_window(start_frame, frame_count, |segment| {
            observe(segment);
            Ok::<(), Infallible>(())
        }) {
            Ok(count) => Ok(count),
            Err(SceneProcessError::Runtime(error)) => Err(error),
            Err(SceneProcessError::Observer(error)) => match error {},
        }
    }

    /// Fallibly visits each segment and restores runtime state if observation
    /// fails.
    ///
    /// # Errors
    ///
    /// Returns [`SceneProcessError::Runtime`] for invalid windows and
    /// [`SceneProcessError::Observer`] for observer failures.
    pub fn try_process_window<E>(
        &mut self,
        start_frame: usize,
        frame_count: usize,
        mut observe: impl FnMut(SceneSegment<'_, 'resource>) -> Result<(), E>,
    ) -> Result<usize, SceneProcessError<E>> {
        if start_frame != self.position {
            return Err(SceneProcessError::Runtime(
                SceneRuntimeError::WindowNotContiguous {
                    expected: self.position,
                    actual: start_frame,
                },
            ));
        }
        let end_frame = start_frame
            .checked_add(frame_count)
            .ok_or(SceneProcessError::Runtime(SceneRuntimeError::FrameOverflow))?;
        let saved_position = self.position;
        let saved_pending = self.pending;
        let saved_last_launched_scene_index = self.last_launched_scene_index;
        let track_count = self.states.len();
        let mut saved_states = [None; VERSION_1_MAX_SCENE_TRACKS];
        saved_states[..track_count].copy_from_slice(&self.states);
        let mut saved_cursors = [0_usize; VERSION_1_MAX_SCENE_TRACKS];
        saved_cursors[..track_count].copy_from_slice(&self.cursors);
        let mut saved_controlled = [false; VERSION_1_MAX_SCENE_TRACKS];
        saved_controlled[..track_count].copy_from_slice(&self.controlled);
        let mut segment_count = 0;
        while self.position < end_frame {
            self.apply_boundary();
            let next_boundary = self.next_boundary(end_frame);
            if let Err(error) = observe(SceneSegment {
                offset_frames: self.position - start_frame,
                frame_count: next_boundary - self.position,
                track_states: &self.states,
                track_controlled: &self.controlled,
            }) {
                self.position = saved_position;
                self.states.copy_from_slice(&saved_states[..track_count]);
                self.cursors.copy_from_slice(&saved_cursors[..track_count]);
                self.controlled
                    .copy_from_slice(&saved_controlled[..track_count]);
                self.pending = saved_pending;
                self.last_launched_scene_index = saved_last_launched_scene_index;
                return Err(SceneProcessError::Observer(error));
            }
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
        if let Some(pending) = self
            .pending
            .filter(|pending| pending.frame <= self.position)
        {
            let scene = &self.plan.scenes[pending.scene_index];
            for (slot_index, slot) in scene.slots.iter().enumerate() {
                self.controlled[slot.track_index] = true;
                self.states[slot.track_index] = match slot.action {
                    CompiledTemplateAction::Stop => None,
                    CompiledTemplateAction::Play {
                        clip_id,
                        duration_frames,
                        loop_enabled,
                        enabled,
                        ..
                    } => Some(PlayingSceneClip {
                        source: ScenePlaybackSource::Interactive {
                            scene_index: pending.scene_index,
                            slot_index,
                        },
                        scene_id: scene.id,
                        clip_id,
                        started_at_tick: pending.tick,
                        started_at_frame: pending.frame,
                        natural_end_frame: (!loop_enabled).then(|| pending.frame + duration_frames),
                        enabled,
                    }),
                };
            }
            self.last_launched_scene_index = Some(pending.scene_index);
            self.pending = None;
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
        if let Some(pending) = self.pending.filter(|pending| pending.frame > self.position) {
            boundary = boundary.min(pending.frame);
        }
        boundary
    }
}
