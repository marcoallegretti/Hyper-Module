#![doc = "Bounded immutable decoded-media binding for Hyper Module playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{ClipOutput, ClipPlacement, ClipPlanError, ClipRenderPlan};
use hyper_playback_compiler::PrepareError;
use hyper_playback_json::{OwnedCompiledSchedule, OwnedScheduleEntry};

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
        Ok(PreparedSchedule { entries })
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
    entries: Vec<PreparedScheduleEntry<'cache, 'schedule>>,
}

impl<'cache, 'schedule> PreparedSchedule<'cache, 'schedule> {
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
