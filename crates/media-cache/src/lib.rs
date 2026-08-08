#![doc = "Bounded immutable decoded-media binding for Hyper Module playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{ClipOutput, ClipPlacement, ClipPlanError, ClipRenderPlan};
use hyper_playback_compiler::scene::{
    CompiledScenePerformance, PlayingSceneClip, ScenePlaybackSource,
};
use hyper_playback_compiler::{CompiledSceneAudioClip, PrepareError};
use hyper_playback_json::{
    CompiledPlaybackGraph, InteractiveSceneAudioAvailability, MediaAssetFact,
    OwnedCompiledSchedule, OwnedScheduleEntry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheLimits {
    /// Maximum number of unique decoded assets retained by the cache.
    pub max_assets: usize,
    /// Maximum decoded frame count accepted for any single asset.
    pub max_frames_per_asset: usize,
    /// Aggregate byte limit for decoded `f32` sample planes only.
    pub max_decoded_sample_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheBuildError {
    InvalidLimits,
    EmptyAssetId,
    DuplicateAssetId,
    AssetCountLimit { maximum: usize },
    AssetFrameLimit { actual: usize, maximum: usize },
    DecodedSampleByteLimit { attempted: usize, maximum: usize },
    ArithmeticOverflow,
    CapacityUnavailable,
}

impl fmt::Display for CacheBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("media cache limits must be positive"),
            Self::EmptyAssetId => formatter.write_str("media cache asset identity is empty"),
            Self::DuplicateAssetId => {
                formatter.write_str("media cache asset identity is duplicated")
            }
            Self::AssetCountLimit { maximum } => {
                write!(formatter, "media cache exceeds its {maximum}-asset limit")
            }
            Self::AssetFrameLimit { actual, maximum } => write!(
                formatter,
                "decoded asset has {actual} frames; maximum is {maximum}"
            ),
            Self::DecodedSampleByteLimit { attempted, maximum } => write!(
                formatter,
                "decoded sample payload would use {attempted} bytes; maximum is {maximum}"
            ),
            Self::ArithmeticOverflow => {
                formatter.write_str("decoded media cache arithmetic overflowed")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("decoded media cache capacity is unavailable")
            }
        }
    }
}

impl Error for CacheBuildError {}

#[derive(Debug)]
pub struct InsertFailure {
    error: CacheBuildError,
    audio: DecodedAudio,
}

impl InsertFailure {
    #[must_use]
    pub const fn error(&self) -> CacheBuildError {
        self.error
    }

    #[must_use]
    pub fn into_audio(self) -> DecodedAudio {
        self.audio
    }
}

impl fmt::Display for InsertFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for InsertFailure {}

#[derive(Debug)]
struct CacheEntry {
    asset_id: String,
    audio: DecodedAudio,
}

#[derive(Debug)]
pub struct DecodedMediaCacheBuilder {
    limits: CacheLimits,
    entries: Vec<CacheEntry>,
    decoded_sample_bytes: usize,
}

impl DecodedMediaCacheBuilder {
    /// Allocates all entry slots before any asset can be inserted.
    ///
    /// # Errors
    ///
    /// Returns [`CacheBuildError`] for zero limits or unavailable capacity.
    pub fn try_new(limits: CacheLimits) -> Result<Self, CacheBuildError> {
        if limits.max_assets == 0
            || limits.max_frames_per_asset == 0
            || limits.max_decoded_sample_bytes == 0
        {
            return Err(CacheBuildError::InvalidLimits);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(limits.max_assets)
            .map_err(|_| CacheBuildError::CapacityUnavailable)?;
        Ok(Self {
            limits,
            entries,
            decoded_sample_bytes: 0,
        })
    }

    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn decoded_sample_bytes(&self) -> usize {
        self.decoded_sample_bytes
    }

    /// Inserts one fully decoded asset without replacement or eviction.
    ///
    /// On failure, ownership of `audio` is returned through
    /// [`InsertFailure::into_audio`], and the builder remains unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`InsertFailure`] for invalid identity, duplicate identity,
    /// exceeded bounds, arithmetic overflow, or identity allocation failure.
    pub fn try_insert(&mut self, asset_id: &str, audio: DecodedAudio) -> Result<(), InsertFailure> {
        let result = self.preflight_insert(asset_id, &audio);
        let (insert_at, next_total) = match result {
            Ok(values) => values,
            Err(error) => return Err(InsertFailure { error, audio }),
        };

        let mut owned_id = String::new();
        if owned_id.try_reserve_exact(asset_id.len()).is_err() {
            return Err(InsertFailure {
                error: CacheBuildError::CapacityUnavailable,
                audio,
            });
        }
        owned_id.push_str(asset_id);

        self.entries.insert(
            insert_at,
            CacheEntry {
                asset_id: owned_id,
                audio,
            },
        );
        self.decoded_sample_bytes = next_total;
        Ok(())
    }

    fn preflight_insert(
        &self,
        asset_id: &str,
        audio: &DecodedAudio,
    ) -> Result<(usize, usize), CacheBuildError> {
        if asset_id.is_empty() {
            return Err(CacheBuildError::EmptyAssetId);
        }
        let Err(insert_at) = self
            .entries
            .binary_search_by(|entry| entry.asset_id.as_str().cmp(asset_id))
        else {
            return Err(CacheBuildError::DuplicateAssetId);
        };
        if self.entries.len() >= self.limits.max_assets {
            return Err(CacheBuildError::AssetCountLimit {
                maximum: self.limits.max_assets,
            });
        }
        let frame_count = audio.frame_count();
        if frame_count > self.limits.max_frames_per_asset {
            return Err(CacheBuildError::AssetFrameLimit {
                actual: frame_count,
                maximum: self.limits.max_frames_per_asset,
            });
        }
        let decoded_sample_bytes = frame_count
            .checked_mul(audio.channels())
            .and_then(|samples| samples.checked_mul(size_of::<f32>()))
            .ok_or(CacheBuildError::ArithmeticOverflow)?;
        let next_total = self
            .decoded_sample_bytes
            .checked_add(decoded_sample_bytes)
            .ok_or(CacheBuildError::ArithmeticOverflow)?;
        if next_total > self.limits.max_decoded_sample_bytes {
            return Err(CacheBuildError::DecodedSampleByteLimit {
                attempted: next_total,
                maximum: self.limits.max_decoded_sample_bytes,
            });
        }
        Ok((insert_at, next_total))
    }

    #[must_use]
    pub fn seal(self) -> DecodedMediaCache {
        DecodedMediaCache {
            entries: self.entries,
            decoded_sample_bytes: self.decoded_sample_bytes,
        }
    }
}

#[derive(Debug)]
pub struct DecodedMediaCache {
    entries: Vec<CacheEntry>,
    decoded_sample_bytes: usize,
}

impl DecodedMediaCache {
    #[must_use]
    pub fn asset_count(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub const fn decoded_sample_bytes(&self) -> usize {
        self.decoded_sample_bytes
    }

    #[must_use]
    pub fn contains(&self, asset_id: &str) -> bool {
        self.find(asset_id).is_some()
    }

    /// Returns immutable decoded audio for one verified asset identity.
    ///
    /// The cache retains ownership; callers cannot replace or mutate media
    /// while a playback or audition owner borrows it.
    #[must_use]
    pub fn decoded_audio(&self, asset_id: &str) -> Option<&DecodedAudio> {
        self.find(asset_id).map(|entry| &entry.audio)
    }

    /// Returns facts derived from the exact decoded assets in this cache.
    #[must_use]
    pub fn media_asset_facts(&self) -> impl ExactSizeIterator<Item = MediaAssetFact<'_>> + '_ {
        self.entries.iter().map(|entry| MediaAssetFact {
            id: &entry.asset_id,
            sample_rate_hz: entry.audio.sample_rate_hz(),
            frame_count: entry.audio.frame_count(),
        })
    }

    fn find(&self, asset_id: &str) -> Option<&CacheEntry> {
        self.entries
            .binary_search_by(|entry| entry.asset_id.as_str().cmp(asset_id))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Atomically binds every scheduled placement to immutable decoded media.
    ///
    /// No partially prepared schedule is observable on failure. Successful
    /// preparation owns all callback-time entry storage up front.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareScheduleError`] for unavailable schedule capacity,
    /// missing media, or a decoded source that fails compiled-plan validation.
    pub fn prepare_schedule<'cache, 'schedule>(
        &'cache self,
        schedule: &'schedule OwnedCompiledSchedule,
    ) -> Result<PreparedSchedule<'cache, 'schedule>, PrepareScheduleError> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(schedule.entries().len())
            .map_err(|_| PrepareScheduleError::CapacityUnavailable)?;

        for (schedule_index, entry) in schedule.entries().iter().enumerate() {
            let cached = self
                .find(&entry.asset_id)
                .ok_or(PrepareScheduleError::MissingAsset { schedule_index })?;
            let (plan, placement) = entry.compiled.prepare(&cached.audio).map_err(|source| {
                PrepareScheduleError::InvalidAsset {
                    schedule_index,
                    source,
                }
            })?;
            entries.push(PreparedScheduleEntry {
                schedule: entry,
                plan,
                placement,
            });
        }
        Ok(PreparedSchedule {
            project_id: schedule.project_id(),
            entries,
        })
    }

    /// Atomically binds every compiled scene play action to immutable media.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareSceneAudioError`] for mismatched authority, duplicate
    /// bindings, missing media, or invalid compiled clip data.
    pub fn prepare_scene_audio<'cache, 'resource>(
        &'cache self,
        authority: &CompiledScenePerformance<'_>,
        bindings: &[SceneAudioBinding<'resource>],
    ) -> Result<PreparedSceneAudio<'cache, 'resource>, PrepareSceneAudioError> {
        let play_count = authority.playing_clips().count();
        if bindings.len() != play_count {
            return Err(PrepareSceneAudioError::BindingCountMismatch {
                expected: play_count,
                actual: bindings.len(),
            });
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(bindings.len())
            .map_err(|_| PrepareSceneAudioError::CapacityUnavailable)?;
        for (binding_index, binding) in bindings.iter().enumerate() {
            if bindings[..binding_index]
                .iter()
                .any(|previous| previous.key() == binding.key())
            {
                return Err(PrepareSceneAudioError::DuplicateBinding { binding_index });
            }
            let playing = authority
                .playing_clips()
                .find(|(track_id, playing)| binding.matches(track_id, *playing))
                .ok_or(PrepareSceneAudioError::AuthorityMismatch { binding_index })?
                .1;
            if binding.compiled.launch_frame() != playing.started_at_frame() {
                return Err(PrepareSceneAudioError::LaunchFrameMismatch { binding_index });
            }
            let cached = self
                .find(binding.asset_id)
                .ok_or(PrepareSceneAudioError::MissingAsset { binding_index })?;
            let (plan, placement) = binding.compiled.prepare(&cached.audio).map_err(|source| {
                PrepareSceneAudioError::InvalidAsset {
                    binding_index,
                    source,
                }
            })?;
            entries.push(PreparedSceneAudioEntry {
                binding: *binding,
                plan,
                placement,
            });
        }
        let mut scene_availability = Vec::new();
        scene_availability
            .try_reserve_exact(authority.scene_count())
            .map_err(|_| PrepareSceneAudioError::CapacityUnavailable)?;
        scene_availability.resize(
            authority.scene_count(),
            PreparedSceneAvailability::NotPrepared,
        );
        Ok(PreparedSceneAudio {
            entries,
            scene_availability,
        })
    }

    /// Atomically binds every scene play action in a compiled playback graph.
    ///
    /// # Errors
    ///
    /// Returns [`PrepareSceneAudioError`] for capacity, authority, media, or
    /// decoded-source mismatches.
    pub fn prepare_playback_scenes<'cache, 'project>(
        &'cache self,
        graph: &CompiledPlaybackGraph<'project>,
    ) -> Result<PreparedSceneAudio<'cache, 'project>, PrepareSceneAudioError> {
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(graph.scene_audio().len())
            .map_err(|_| PrepareSceneAudioError::CapacityUnavailable)?;
        bindings.extend(graph.scene_audio().iter().map(|entry| SceneAudioBinding {
            track_id: entry.track_id,
            source: ScenePlaybackSource::Recorded {
                launch_id: entry.launch_id,
            },
            scene_id: entry.scene_id,
            clip_id: entry.clip_id,
            asset_id: entry.asset_id,
            compiled: entry.compiled,
        }));
        let mut prepared = self.prepare_scene_audio(graph.scene_authority(), &bindings)?;
        prepared.scene_availability.clear();
        prepared
            .scene_availability
            .try_reserve_exact(graph.scene_authority().scene_count())
            .map_err(|_| PrepareSceneAudioError::CapacityUnavailable)?;
        prepared
            .scene_availability
            .extend(
                (0..graph.scene_authority().scene_count()).map(|scene_index| {
                    if graph.scene_authority().scene_enabled(scene_index) == Some(true) {
                        PreparedSceneAvailability::Ready
                    } else {
                        PreparedSceneAvailability::Disabled
                    }
                }),
            );
        prepared
            .entries
            .try_reserve_exact(graph.interactive_scene_audio().len())
            .map_err(|_| PrepareSceneAudioError::CapacityUnavailable)?;
        for (binding_index, binding) in graph.interactive_scene_audio().iter().enumerate() {
            let Some(compiled) = binding.compiled else {
                if matches!(
                    binding.availability,
                    InteractiveSceneAudioAvailability::ConsolidationRequired
                ) {
                    if prepared.scene_availability[binding.scene_index]
                        == PreparedSceneAvailability::Ready
                    {
                        prepared.scene_availability[binding.scene_index] =
                            PreparedSceneAvailability::ConsolidationRequired;
                    }
                    continue;
                }
                return Err(PrepareSceneAudioError::AuthorityMismatch { binding_index });
            };
            let cached = self
                .find(binding.asset_id)
                .ok_or(PrepareSceneAudioError::MissingAsset { binding_index })?;
            let (plan, placement) = compiled.prepare(&cached.audio).map_err(|source| {
                PrepareSceneAudioError::InvalidAsset {
                    binding_index,
                    source,
                }
            })?;
            prepared.entries.push(PreparedSceneAudioEntry {
                binding: SceneAudioBinding {
                    track_id: binding.track_id,
                    source: ScenePlaybackSource::Interactive {
                        scene_index: binding.scene_index,
                        slot_index: binding.slot_index,
                    },
                    scene_id: binding.scene_id,
                    clip_id: binding.clip_id,
                    asset_id: binding.asset_id,
                    compiled,
                },
                plan,
                placement,
            });
        }
        Ok(prepared)
    }

    /// Atomically binds every arrangement placement and scene play action in a
    /// compiled playback graph to this immutable cache.
    ///
    /// # Errors
    ///
    /// Returns [`PreparePlaybackError`] without publishing partial prepared
    /// state when either arrangement or scene media cannot be bound.
    pub fn prepare_playback<'cache, 'graph, 'project>(
        &'cache self,
        graph: &'graph CompiledPlaybackGraph<'project>,
    ) -> Result<PreparedPlayback<'cache, 'graph, 'project>, PreparePlaybackError> {
        let schedule = self
            .prepare_schedule(graph.arrangement())
            .map_err(PreparePlaybackError::Schedule)?;
        let scene_audio = self
            .prepare_playback_scenes(graph)
            .map_err(PreparePlaybackError::SceneAudio)?;
        Ok(PreparedPlayback {
            project_id: graph.project_id(),
            authority: graph.scene_authority(),
            schedule,
            scene_audio,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneAudioBinding<'a> {
    pub track_id: &'a str,
    pub source: ScenePlaybackSource<'a>,
    pub scene_id: &'a str,
    pub clip_id: &'a str,
    pub asset_id: &'a str,
    pub compiled: CompiledSceneAudioClip,
}

impl SceneAudioBinding<'_> {
    fn key(&self) -> (&str, ScenePlaybackSource<'_>, &str) {
        (self.track_id, self.source, self.clip_id)
    }

    fn matches(&self, track_id: &str, playing: PlayingSceneClip<'_>) -> bool {
        self.track_id == track_id
            && self.source == playing.source()
            && self.scene_id == playing.scene_id()
            && self.clip_id == playing.clip_id()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareSceneAudioError {
    CapacityUnavailable,
    BindingCountMismatch {
        expected: usize,
        actual: usize,
    },
    DuplicateBinding {
        binding_index: usize,
    },
    AuthorityMismatch {
        binding_index: usize,
    },
    LaunchFrameMismatch {
        binding_index: usize,
    },
    MissingAsset {
        binding_index: usize,
    },
    InvalidAsset {
        binding_index: usize,
        source: PrepareError,
    },
}

impl fmt::Display for PrepareSceneAudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("prepared scene-audio capacity is unavailable")
            }
            Self::BindingCountMismatch { expected, actual } => write!(
                formatter,
                "scene authority requires {expected} audio bindings; {actual} were provided"
            ),
            Self::DuplicateBinding { binding_index } => {
                write!(
                    formatter,
                    "scene audio binding {binding_index} is duplicated"
                )
            }
            Self::AuthorityMismatch { binding_index } => write!(
                formatter,
                "scene audio binding {binding_index} has no matching play action"
            ),
            Self::LaunchFrameMismatch { binding_index } => write!(
                formatter,
                "scene audio binding {binding_index} has the wrong launch frame"
            ),
            Self::MissingAsset { binding_index } => write!(
                formatter,
                "scene audio binding {binding_index} references missing media"
            ),
            Self::InvalidAsset {
                binding_index,
                source,
            } => write!(
                formatter,
                "scene audio binding {binding_index} cannot bind its media: {source}"
            ),
        }
    }
}

impl Error for PrepareSceneAudioError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidAsset { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparePlaybackError {
    Schedule(PrepareScheduleError),
    SceneAudio(PrepareSceneAudioError),
}

impl fmt::Display for PreparePlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schedule(source) => write!(formatter, "arrangement preparation failed: {source}"),
            Self::SceneAudio(source) => write!(formatter, "scene preparation failed: {source}"),
        }
    }
}

impl Error for PreparePlaybackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schedule(source) => Some(source),
            Self::SceneAudio(source) => Some(source),
        }
    }
}

/// Immutable cache bindings for one exact compiled playback graph.
#[derive(Debug)]
pub struct PreparedPlayback<'cache, 'graph, 'project> {
    project_id: &'project str,
    authority: &'graph CompiledScenePerformance<'project>,
    schedule: PreparedSchedule<'cache, 'graph>,
    scene_audio: PreparedSceneAudio<'cache, 'project>,
}

impl<'cache, 'graph, 'project> PreparedPlayback<'cache, 'graph, 'project> {
    #[must_use]
    pub const fn project_id(&self) -> &'project str {
        self.project_id
    }

    #[must_use]
    pub const fn authority(&self) -> &'graph CompiledScenePerformance<'project> {
        self.authority
    }

    #[must_use]
    pub const fn schedule(&self) -> &PreparedSchedule<'cache, 'graph> {
        &self.schedule
    }

    #[must_use]
    pub const fn scene_audio(&self) -> &PreparedSceneAudio<'cache, 'project> {
        &self.scene_audio
    }
}

#[derive(Debug)]
pub struct PreparedSceneAudio<'cache, 'binding> {
    entries: Vec<PreparedSceneAudioEntry<'cache, 'binding>>,
    scene_availability: Vec<PreparedSceneAvailability>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedSceneAvailability {
    Ready,
    Disabled,
    ConsolidationRequired,
    NotPrepared,
}

impl<'cache, 'binding> PreparedSceneAudio<'cache, 'binding> {
    #[must_use]
    pub fn entries(&self) -> &[PreparedSceneAudioEntry<'cache, 'binding>] {
        &self.entries
    }

    #[must_use]
    pub fn scene_availability(&self, scene_index: usize) -> PreparedSceneAvailability {
        self.scene_availability
            .get(scene_index)
            .copied()
            .unwrap_or(PreparedSceneAvailability::NotPrepared)
    }

    #[must_use]
    pub fn find(
        &self,
        track_id: &str,
        playing: PlayingSceneClip<'_>,
    ) -> Option<&PreparedSceneAudioEntry<'cache, 'binding>> {
        self.find_index(track_id, playing).map(|(_, entry)| entry)
    }

    #[must_use]
    pub fn find_index(
        &self,
        track_id: &str,
        playing: PlayingSceneClip<'_>,
    ) -> Option<(usize, &PreparedSceneAudioEntry<'cache, 'binding>)> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, entry)| entry.binding.matches(track_id, playing))
    }
}

#[derive(Debug)]
pub struct PreparedSceneAudioEntry<'cache, 'binding> {
    binding: SceneAudioBinding<'binding>,
    plan: ClipRenderPlan<'cache>,
    placement: ClipPlacement,
}

impl PreparedSceneAudioEntry<'_, '_> {
    #[must_use]
    pub const fn binding(&self) -> &SceneAudioBinding<'_> {
        &self.binding
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.plan.channels()
    }

    /// Renders a scene clip block relative to its launch frame.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError`] if the block exceeds the prepared native clip.
    pub fn render_block(
        &self,
        launch_frame_offset: usize,
        output: ClipOutput<'_>,
    ) -> Result<(), ClipPlanError> {
        self.plan
            .render_block(self.placement, launch_frame_offset, output)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrepareScheduleError {
    CapacityUnavailable,
    MissingAsset {
        schedule_index: usize,
    },
    InvalidAsset {
        schedule_index: usize,
        source: PrepareError,
    },
}

impl fmt::Display for PrepareScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("prepared schedule capacity is unavailable")
            }
            Self::MissingAsset { schedule_index } => write!(
                formatter,
                "schedule entry {schedule_index} references an asset missing from the cache"
            ),
            Self::InvalidAsset {
                schedule_index,
                source,
            } => write!(
                formatter,
                "schedule entry {schedule_index} cannot bind its cached asset: {source}"
            ),
        }
    }
}

impl Error for PrepareScheduleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidAsset { source, .. } => Some(source),
            Self::CapacityUnavailable | Self::MissingAsset { .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct PreparedSchedule<'cache, 'schedule> {
    project_id: &'schedule str,
    entries: Vec<PreparedScheduleEntry<'cache, 'schedule>>,
}

impl<'cache, 'schedule> PreparedSchedule<'cache, 'schedule> {
    #[must_use]
    pub const fn project_id(&self) -> &'schedule str {
        self.project_id
    }

    #[must_use]
    pub fn entries(&self) -> &[PreparedScheduleEntry<'cache, 'schedule>] {
        &self.entries
    }
}

#[derive(Debug)]
pub struct PreparedScheduleEntry<'cache, 'schedule> {
    schedule: &'schedule OwnedScheduleEntry,
    plan: ClipRenderPlan<'cache>,
    placement: ClipPlacement,
}

impl PreparedScheduleEntry<'_, '_> {
    #[must_use]
    pub const fn schedule_entry(&self) -> &OwnedScheduleEntry {
        self.schedule
    }

    #[must_use]
    pub const fn placement(&self) -> ClipPlacement {
        self.placement
    }

    #[must_use]
    pub const fn channels(&self) -> usize {
        self.plan.channels()
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.plan.is_enabled()
    }

    /// Renders a block through the pre-bound clip plan without cache mutation.
    ///
    /// # Errors
    ///
    /// Returns [`ClipPlanError`] when the requested block or output layout is
    /// incompatible with the prepared placement.
    pub fn render_block(
        &self,
        placement_frame_offset: usize,
        output: ClipOutput<'_>,
    ) -> Result<(), ClipPlanError> {
        self.plan
            .render_block(self.placement, placement_frame_offset, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(frames: usize) -> DecodedAudio {
        DecodedAudio::from_mono(48_000, vec![0.25; frames]).unwrap()
    }

    #[test]
    fn insertion_is_sorted_and_accounted_by_decoded_planes() {
        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 2,
            max_frames_per_asset: 4,
            max_decoded_sample_bytes: 32,
        })
        .unwrap();
        builder.try_insert("z", mono(2)).unwrap();
        let stereo = DecodedAudio::from_stereo(48_000, vec![0.0; 2], vec![0.0; 2]).unwrap();
        builder.try_insert("a", stereo).unwrap();

        let cache = builder.seal();
        assert_eq!(cache.asset_count(), 2);
        assert_eq!(cache.decoded_sample_bytes(), 24);
        assert!(cache.contains("a"));
        assert!(cache.contains("z"));
    }

    #[test]
    fn playback_facts_come_from_sorted_decoded_assets() {
        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 2,
            max_frames_per_asset: 4,
            max_decoded_sample_bytes: 32,
        })
        .unwrap();
        builder.try_insert("z", mono(2)).unwrap();
        let stereo = DecodedAudio::from_stereo(48_000, vec![0.0; 3], vec![0.0; 3]).unwrap();
        builder.try_insert("a", stereo).unwrap();

        let cache = builder.seal();
        let facts: Vec<_> = cache.media_asset_facts().collect();

        assert_eq!(
            facts,
            [
                MediaAssetFact {
                    id: "a",
                    sample_rate_hz: 48_000,
                    frame_count: 3,
                },
                MediaAssetFact {
                    id: "z",
                    sample_rate_hz: 48_000,
                    frame_count: 2,
                },
            ]
        );
    }

    #[test]
    fn rejected_insert_returns_audio_and_preserves_builder() {
        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 1,
            max_frames_per_asset: 2,
            max_decoded_sample_bytes: 8,
        })
        .unwrap();
        builder.try_insert("asset", mono(2)).unwrap();

        let failure = builder.try_insert("other", mono(1)).unwrap_err();
        assert_eq!(
            failure.error(),
            CacheBuildError::AssetCountLimit { maximum: 1 }
        );
        assert_eq!(failure.into_audio().frame_count(), 1);
        assert_eq!(builder.asset_count(), 1);
        assert_eq!(builder.decoded_sample_bytes(), 8);
    }

    #[test]
    fn duplicate_precedes_capacity_and_does_not_replace() {
        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 1,
            max_frames_per_asset: 2,
            max_decoded_sample_bytes: 8,
        })
        .unwrap();
        builder.try_insert("asset", mono(2)).unwrap();

        let failure = builder.try_insert("asset", mono(1)).unwrap_err();
        assert_eq!(failure.error(), CacheBuildError::DuplicateAssetId);
        assert_eq!(builder.decoded_sample_bytes(), 8);
    }

    #[test]
    fn frame_and_total_byte_limits_are_independent() {
        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 2,
            max_frames_per_asset: 2,
            max_decoded_sample_bytes: 12,
        })
        .unwrap();
        let frame_failure = builder.try_insert("large", mono(3)).unwrap_err();
        assert_eq!(
            frame_failure.error(),
            CacheBuildError::AssetFrameLimit {
                actual: 3,
                maximum: 2
            }
        );

        builder.try_insert("first", mono(2)).unwrap();
        let bytes_failure = builder.try_insert("second", mono(2)).unwrap_err();
        assert_eq!(
            bytes_failure.error(),
            CacheBuildError::DecodedSampleByteLimit {
                attempted: 16,
                maximum: 12
            }
        );
    }

    #[test]
    fn zero_limits_and_empty_identity_are_rejected_without_mutation() {
        assert!(matches!(
            DecodedMediaCacheBuilder::try_new(CacheLimits {
                max_assets: 0,
                max_frames_per_asset: 1,
                max_decoded_sample_bytes: 4,
            }),
            Err(CacheBuildError::InvalidLimits)
        ));

        let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
            max_assets: 1,
            max_frames_per_asset: 1,
            max_decoded_sample_bytes: 4,
        })
        .unwrap();
        let failure = builder.try_insert("", mono(1)).unwrap_err();
        assert_eq!(failure.error(), CacheBuildError::EmptyAssetId);
        assert_eq!(failure.into_audio().frame_count(), 1);
        assert_eq!(builder.asset_count(), 0);
    }
}
