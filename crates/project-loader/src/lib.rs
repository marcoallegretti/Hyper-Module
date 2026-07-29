#![doc = "Provenance-preserving project-package loading for native playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::path::Path;

use hyper_audio_source::{AudioContainer, DecodeError, DecodeLimits, decode_bytes};
use hyper_playback_session::{
    DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyError,
    PlaybackSessionAssemblyInput, PlaybackSessionAssemblyLimits, PreparedPlaybackSession,
    assemble_playback_session,
};
use hyper_project_model::{AssetRole, AudioMediaType, ResourceDescriptor, ResourceRole};
use hyper_storage_engine::{StorageError, StructurallyVerifiedProject, verify_project_package};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectPlaybackLimits {
    pub max_total_resource_bytes: usize,
    pub audio: DecodeLimits,
    pub session: PlaybackSessionAssemblyLimits,
}

#[derive(Debug)]
pub struct PreparedProjectPlayback {
    package: StructurallyVerifiedProject,
    session: PreparedPlaybackSession,
}

impl PreparedProjectPlayback {
    #[must_use]
    pub const fn package(&self) -> &StructurallyVerifiedProject {
        &self.package
    }

    #[must_use]
    pub const fn session(&self) -> &PreparedPlaybackSession {
        &self.session
    }

    #[must_use]
    pub fn into_parts(self) -> (StructurallyVerifiedProject, PreparedPlaybackSession) {
        (self.package, self.session)
    }
}

#[derive(Debug)]
pub enum ProjectPlaybackError {
    InvalidLimits,
    ResourceBytesLimit {
        actual: u64,
        maximum: usize,
    },
    AssetCountLimit {
        actual: usize,
        maximum: usize,
    },
    AssetFrameLimit {
        asset_id: String,
        actual: usize,
        maximum: usize,
    },
    DecodedBytesLimit {
        actual: usize,
        maximum: usize,
    },
    ResourceCapacityUnavailable,
    MissingMixer,
    MultipleArrangements,
    MultipleMixers,
    UnsupportedResourceRole(ResourceRole),
    UnsupportedAssetRole(AssetRole),
    Storage(StorageError),
    Audio {
        asset_id: String,
        source: DecodeError,
    },
    Assembly(PlaybackSessionAssemblyError),
}

impl fmt::Display for ProjectPlaybackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("project playback limits must be positive"),
            Self::ResourceBytesLimit { actual, maximum } => write!(
                formatter,
                "project resources require {actual} bytes; maximum is {maximum}"
            ),
            Self::AssetCountLimit { actual, maximum } => {
                write!(
                    formatter,
                    "project has {actual} assets; maximum is {maximum}"
                )
            }
            Self::AssetFrameLimit {
                asset_id,
                actual,
                maximum,
            } => write!(
                formatter,
                "asset {asset_id} has {actual} decoded frames; maximum is {maximum}"
            ),
            Self::DecodedBytesLimit { actual, maximum } => write!(
                formatter,
                "project assets require {actual} decoded bytes; maximum is {maximum}"
            ),
            Self::ResourceCapacityUnavailable => {
                formatter.write_str("project resource capacity is unavailable")
            }
            Self::MissingMixer => formatter.write_str("playback package has no mixer resource"),
            Self::MultipleArrangements => {
                formatter.write_str("playback package has multiple arrangement resources")
            }
            Self::MultipleMixers => {
                formatter.write_str("playback package has multiple mixer resources")
            }
            Self::UnsupportedResourceRole(role) => {
                write!(formatter, "resource role {role:?} is not a playback input")
            }
            Self::UnsupportedAssetRole(role) => {
                write!(formatter, "asset role {role:?} is not a playback input")
            }
            Self::Storage(source) => write!(formatter, "package verification failed: {source}"),
            Self::Audio { asset_id, source } => {
                write!(formatter, "asset {asset_id} could not be decoded: {source}")
            }
            Self::Assembly(source) => write!(formatter, "playback assembly failed: {source}"),
        }
    }
}

impl Error for ProjectPlaybackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Storage(source) => Some(source),
            Self::Audio { source, .. } => Some(source),
            Self::Assembly(source) => Some(source),
            Self::InvalidLimits
            | Self::ResourceBytesLimit { .. }
            | Self::AssetCountLimit { .. }
            | Self::AssetFrameLimit { .. }
            | Self::DecodedBytesLimit { .. }
            | Self::ResourceCapacityUnavailable
            | Self::MissingMixer
            | Self::MultipleArrangements
            | Self::MultipleMixers
            | Self::UnsupportedResourceRole(_)
            | Self::UnsupportedAssetRole(_) => None,
        }
    }
}

impl From<StorageError> for ProjectPlaybackError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Verifies, snapshots, semantically resolves, and decodes one playback-ready
/// project package.
///
/// Mapping and recording resources are rejected explicitly because they are
/// not inputs to the native playback runtime. No resource role is silently
/// ignored.
///
/// # Errors
///
/// Returns a fail-closed rejection for structural, capacity, role, media, or
/// cross-resource semantic failure. No partial session is published.
pub fn prepare_project_playback(
    root: impl AsRef<Path>,
    limits: ProjectPlaybackLimits,
) -> Result<PreparedProjectPlayback, ProjectPlaybackError> {
    let cache_limits = limits.session.media_cache;
    if limits.max_total_resource_bytes == 0
        || cache_limits.max_assets == 0
        || cache_limits.max_frames_per_asset == 0
        || cache_limits.max_decoded_sample_bytes == 0
    {
        return Err(ProjectPlaybackError::InvalidLimits);
    }
    let package = verify_project_package(root)?;
    let inventory = ResourceInventory::new(&package, limits.max_total_resource_bytes)?;
    let documents = inventory.read(&package)?;

    let asset_count = package.manifest().assets().len();
    if asset_count > cache_limits.max_assets {
        return Err(ProjectPlaybackError::AssetCountLimit {
            actual: asset_count,
            maximum: cache_limits.max_assets,
        });
    }
    let mut decoded_assets = Vec::new();
    decoded_assets
        .try_reserve_exact(asset_count)
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    let mut decoded_bytes = 0_usize;
    for descriptor in package.manifest().assets() {
        if descriptor.role() != AssetRole::Sample {
            return Err(ProjectPlaybackError::UnsupportedAssetRole(
                descriptor.role(),
            ));
        }
        let bytes = package.read_asset(descriptor.id(), limits.audio.max_file_bytes())?;
        let audio = decode_bytes(
            bytes,
            audio_container(descriptor.media_type()),
            package.manifest().sample_rate_hz(),
            limits.audio,
        )
        .map_err(|source| ProjectPlaybackError::Audio {
            asset_id: descriptor.id().to_owned(),
            source,
        })?;
        let frame_count = audio.frame_count();
        if frame_count > cache_limits.max_frames_per_asset {
            return Err(ProjectPlaybackError::AssetFrameLimit {
                asset_id: descriptor.id().to_owned(),
                actual: frame_count,
                maximum: cache_limits.max_frames_per_asset,
            });
        }
        let asset_bytes = frame_count
            .checked_mul(audio.channels())
            .and_then(|samples| samples.checked_mul(size_of::<f32>()))
            .ok_or(ProjectPlaybackError::DecodedBytesLimit {
                actual: usize::MAX,
                maximum: cache_limits.max_decoded_sample_bytes,
            })?;
        decoded_bytes = decoded_bytes.checked_add(asset_bytes).ok_or(
            ProjectPlaybackError::DecodedBytesLimit {
                actual: usize::MAX,
                maximum: cache_limits.max_decoded_sample_bytes,
            },
        )?;
        if decoded_bytes > cache_limits.max_decoded_sample_bytes {
            return Err(ProjectPlaybackError::DecodedBytesLimit {
                actual: decoded_bytes,
                maximum: cache_limits.max_decoded_sample_bytes,
            });
        }
        decoded_assets.push(DecodedPlaybackAsset::new(descriptor.id().to_owned(), audio));
    }

    let session = assemble_playback_session(PlaybackSessionAssemblyInput {
        documents,
        decoded_assets,
        limits: limits.session,
    })
    .map_err(ProjectPlaybackError::Assembly)?;
    package.confirm_unchanged()?;
    Ok(PreparedProjectPlayback { package, session })
}

fn audio_container(media_type: AudioMediaType) -> AudioContainer {
    match media_type {
        AudioMediaType::AudioFlac => AudioContainer::Flac,
        AudioMediaType::AudioWav => AudioContainer::Wav,
    }
}

struct ResourceInventory<'manifest> {
    timeline: &'manifest ResourceDescriptor,
    tracks: Vec<&'manifest ResourceDescriptor>,
    clips: Vec<&'manifest ResourceDescriptor>,
    scenes: Vec<&'manifest ResourceDescriptor>,
    arrangement: Option<&'manifest ResourceDescriptor>,
    mixer: &'manifest ResourceDescriptor,
    automation_takes: Vec<&'manifest ResourceDescriptor>,
}

impl<'manifest> ResourceInventory<'manifest> {
    fn new(
        package: &'manifest StructurallyVerifiedProject,
        maximum_bytes: usize,
    ) -> Result<Self, ProjectPlaybackError> {
        let resources = package.manifest().resources();
        let actual_bytes = resources.iter().try_fold(0_u64, |total, descriptor| {
            total.checked_add(descriptor.byte_length())
        });
        let actual_bytes = actual_bytes.ok_or(ProjectPlaybackError::ResourceBytesLimit {
            actual: u64::MAX,
            maximum: maximum_bytes,
        })?;
        if actual_bytes > maximum_bytes as u64 {
            return Err(ProjectPlaybackError::ResourceBytesLimit {
                actual: actual_bytes,
                maximum: maximum_bytes,
            });
        }

        let timeline = resources
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Timeline)
            .expect("validated manifests contain exactly one timeline");
        let mut tracks = Vec::new();
        let mut clips = Vec::new();
        let mut scenes = Vec::new();
        let mut arrangement = None;
        let mut mixer = None;
        let mut automation_takes = Vec::new();
        for descriptor in resources {
            match descriptor.role() {
                ResourceRole::Timeline => {}
                ResourceRole::Track => push_descriptor(&mut tracks, descriptor)?,
                ResourceRole::Clip => push_descriptor(&mut clips, descriptor)?,
                ResourceRole::Scene => push_descriptor(&mut scenes, descriptor)?,
                ResourceRole::Arrangement => {
                    if arrangement.replace(descriptor).is_some() {
                        return Err(ProjectPlaybackError::MultipleArrangements);
                    }
                }
                ResourceRole::Mixer => {
                    if mixer.replace(descriptor).is_some() {
                        return Err(ProjectPlaybackError::MultipleMixers);
                    }
                }
                ResourceRole::AutomationTake => {
                    push_descriptor(&mut automation_takes, descriptor)?;
                }
                ResourceRole::Mapping | ResourceRole::RecordingMetadata => {
                    return Err(ProjectPlaybackError::UnsupportedResourceRole(
                        descriptor.role(),
                    ));
                }
            }
        }
        let mixer = mixer.ok_or(ProjectPlaybackError::MissingMixer)?;
        Ok(Self {
            timeline,
            tracks,
            clips,
            scenes,
            arrangement,
            mixer,
            automation_takes,
        })
    }

    fn read(
        &self,
        package: &StructurallyVerifiedProject,
    ) -> Result<PlaybackDocumentBytes, ProjectPlaybackError> {
        Ok(PlaybackDocumentBytes {
            project_id: package.manifest().project_id().to_owned(),
            sample_rate_hz: package.manifest().sample_rate_hz(),
            timeline: package.read_resource(self.timeline.id())?,
            tracks: read_documents(package, &self.tracks)?,
            clips: read_documents(package, &self.clips)?,
            scenes: read_documents(package, &self.scenes)?,
            arrangement: self
                .arrangement
                .map(|descriptor| package.read_resource(descriptor.id()))
                .transpose()?,
            mixer: package.read_resource(self.mixer.id())?,
            automation_takes: read_documents(package, &self.automation_takes)?,
        })
    }
}

fn push_descriptor<'manifest>(
    output: &mut Vec<&'manifest ResourceDescriptor>,
    descriptor: &'manifest ResourceDescriptor,
) -> Result<(), ProjectPlaybackError> {
    output
        .try_reserve(1)
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    output.push(descriptor);
    Ok(())
}

fn read_documents(
    package: &StructurallyVerifiedProject,
    descriptors: &[&ResourceDescriptor],
) -> Result<Vec<Vec<u8>>, ProjectPlaybackError> {
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(descriptors.len())
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    for descriptor in descriptors {
        documents.push(package.read_resource(descriptor.id())?);
    }
    Ok(documents)
}
