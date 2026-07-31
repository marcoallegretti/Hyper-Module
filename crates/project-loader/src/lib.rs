#![doc = "Provenance-preserving project-package loading for native playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::path::Path;

use hyper_audio_source::{AudioContainer, DecodeError, DecodeLimits, decode_bytes};
use hyper_playback_session::{
    DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyError,
    PlaybackSessionAssemblyInput, PlaybackSessionAssemblyLimits, PreparedPlaybackSession,
    assemble_playback_session,
};
use hyper_project_model::{AssetRole, AudioMediaType, ResourceDescriptor, ResourceRole};
use hyper_recording_metadata::{RecordingMetadataDocument, RecordingMetadataParseError};
use hyper_storage_engine::{
    ProjectPackageLimit, ProjectPackageVerificationLimits, StorageError,
    StructurallyVerifiedProject, verify_project_package_with_limits,
};
use hyper_strict_json::StrictJsonError;
use serde_json::Value;

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
    recording_metadata: Vec<RecordingMetadataDocument>,
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
    pub fn recording_metadata(&self) -> &[RecordingMetadataDocument] {
        &self.recording_metadata
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        StructurallyVerifiedProject,
        PreparedPlaybackSession,
        Vec<RecordingMetadataDocument>,
    ) {
        (self.package, self.session, self.recording_metadata)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingBindingErrorKind {
    CountMismatch { metadata: usize, assets: usize },
    ResourceIdMismatch,
    IdentityCollision,
    ProjectIdMismatch,
    DuplicateAssetReference,
    AssetNotRecording,
    TrackNotAudio,
    InputUnavailable { index: u8, available: u8 },
    SampleRateMismatch { metadata: u32, project: u32 },
    ChannelMismatch { metadata: u8, audio: usize },
    FrameCountMismatch { metadata: u64, audio: usize },
}

#[derive(Debug)]
pub struct RecordingBindingError {
    metadata_id: Option<String>,
    kind: RecordingBindingErrorKind,
}

impl RecordingBindingError {
    #[must_use]
    pub const fn kind(&self) -> RecordingBindingErrorKind {
        self.kind
    }

    #[must_use]
    pub fn metadata_id(&self) -> Option<&str> {
        self.metadata_id.as_deref()
    }
}

impl fmt::Display for RecordingBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(metadata_id) = &self.metadata_id {
            write!(formatter, "recording metadata {metadata_id}: ")?;
        }
        match self.kind {
            RecordingBindingErrorKind::CountMismatch { metadata, assets } => write!(
                formatter,
                "{metadata} metadata resources cannot describe {assets} recording assets"
            ),
            RecordingBindingErrorKind::ResourceIdMismatch => {
                formatter.write_str("document ID does not match its resource descriptor")
            }
            RecordingBindingErrorKind::IdentityCollision => {
                formatter.write_str("document ID collides with a nested playback identity")
            }
            RecordingBindingErrorKind::ProjectIdMismatch => {
                formatter.write_str("document belongs to another project")
            }
            RecordingBindingErrorKind::DuplicateAssetReference => {
                formatter.write_str("recording asset is described more than once")
            }
            RecordingBindingErrorKind::AssetNotRecording => {
                formatter.write_str("asset reference does not identify a recording")
            }
            RecordingBindingErrorKind::TrackNotAudio => {
                formatter.write_str("track reference does not identify an audio track")
            }
            RecordingBindingErrorKind::InputUnavailable { index, available } => write!(
                formatter,
                "input {index} is unavailable when the project declares {available} inputs"
            ),
            RecordingBindingErrorKind::SampleRateMismatch { metadata, project } => write!(
                formatter,
                "sample rate {metadata} Hz does not match project rate {project} Hz"
            ),
            RecordingBindingErrorKind::ChannelMismatch { metadata, audio } => write!(
                formatter,
                "{metadata} metadata channels do not match {audio} decoded channels"
            ),
            RecordingBindingErrorKind::FrameCountMismatch { metadata, audio } => write!(
                formatter,
                "{metadata} metadata frames do not match {audio} decoded frames"
            ),
        }
    }
}

impl Error for RecordingBindingError {}

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
    Storage(StorageError),
    RecordingMetadata {
        resource_id: String,
        source: RecordingMetadataParseError,
    },
    ResourceJson {
        resource_id: String,
        source: StrictJsonError,
    },
    ResourceIdMismatch {
        descriptor_id: String,
        document_id: Option<String>,
    },
    RecordingBinding(RecordingBindingError),
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
            Self::Storage(source) => write!(formatter, "package verification failed: {source}"),
            Self::RecordingMetadata {
                resource_id,
                source,
            } => write!(
                formatter,
                "recording metadata resource {resource_id} is invalid: {source}"
            ),
            Self::ResourceJson {
                resource_id,
                source,
            } => write!(
                formatter,
                "playback resource {resource_id} has invalid strict JSON: {source}"
            ),
            Self::ResourceIdMismatch {
                descriptor_id,
                document_id,
            } => match document_id {
                Some(document_id) => write!(
                    formatter,
                    "playback resource descriptor {descriptor_id} does not match document ID {document_id}"
                ),
                None => write!(
                    formatter,
                    "playback resource descriptor {descriptor_id} has no string document ID"
                ),
            },
            Self::RecordingBinding(source) => source.fmt(formatter),
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
            Self::RecordingMetadata { source, .. } => Some(source),
            Self::ResourceJson { source, .. } => Some(source),
            Self::RecordingBinding(source) => Some(source),
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
            | Self::ResourceIdMismatch { .. }
            | Self::UnsupportedResourceRole(_) => None,
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
/// Recording metadata is bound to verified recording assets, decoded media
/// facts, project input capabilities, and loaded audio tracks. Control
/// mappings remain explicitly rejected because they are not playback inputs.
/// No resource role is silently ignored.
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
    let verification_limits = ProjectPackageVerificationLimits {
        max_total_resource_bytes: u64::try_from(limits.max_total_resource_bytes)
            .unwrap_or(u64::MAX),
        max_assets: cache_limits.max_assets,
        max_asset_file_bytes: limits.audio.max_file_bytes(),
    };
    let package = verify_project_package_with_limits(root, verification_limits)
        .map_err(playback_storage_error)?;
    let inventory = ResourceInventory::new(&package, limits.max_total_resource_bytes)?;
    let captured = inventory.read(&package)?;
    validate_recording_inventory(
        &package,
        &captured.recording_metadata,
        &captured.nested_playback_ids,
    )?;

    let decoded = decode_project_assets(&package, limits)?;

    let session = assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: captured.playback,
        decoded_assets: decoded.assets,
        limits: limits.session,
    })
    .map_err(ProjectPlaybackError::Assembly)?;
    validate_recording_media(&session, &captured.recording_metadata, &decoded.facts)?;
    package.confirm_unchanged()?;
    Ok(PreparedProjectPlayback {
        package,
        session,
        recording_metadata: captured
            .recording_metadata
            .into_iter()
            .map(|record| record.document)
            .collect(),
    })
}

fn playback_storage_error(error: StorageError) -> ProjectPlaybackError {
    match error.limit() {
        Some(ProjectPackageLimit::TotalResourceBytes { actual, maximum }) => {
            ProjectPlaybackError::ResourceBytesLimit {
                actual: *actual,
                maximum: usize::try_from(*maximum).unwrap_or(usize::MAX),
            }
        }
        Some(ProjectPackageLimit::AssetCount { actual, maximum }) => {
            ProjectPlaybackError::AssetCountLimit {
                actual: *actual,
                maximum: *maximum,
            }
        }
        Some(ProjectPackageLimit::AssetFileBytes {
            asset_id,
            actual,
            maximum,
        }) => ProjectPlaybackError::Audio {
            asset_id: asset_id.clone(),
            source: DecodeError::FileTooLarge {
                actual: *actual,
                maximum: *maximum,
            },
        },
        None => ProjectPlaybackError::Storage(error),
    }
}

struct DecodedProjectAssets<'project> {
    assets: Vec<DecodedPlaybackAsset>,
    facts: Vec<DecodedAssetFact<'project>>,
}

#[allow(
    clippy::too_many_lines,
    reason = "decode authority, preallocation limits, and post-decode accounting form one fail-closed transaction"
)]
fn decode_project_assets(
    package: &StructurallyVerifiedProject,
    limits: ProjectPlaybackLimits,
) -> Result<DecodedProjectAssets<'_>, ProjectPlaybackError> {
    let cache_limits = limits.session.media_cache;
    let asset_count = package.manifest().assets().len();
    if asset_count > cache_limits.max_assets {
        return Err(ProjectPlaybackError::AssetCountLimit {
            actual: asset_count,
            maximum: cache_limits.max_assets,
        });
    }
    let mut assets = Vec::new();
    assets
        .try_reserve_exact(asset_count)
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    let mut facts = Vec::new();
    facts
        .try_reserve_exact(asset_count)
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    let mut decoded_bytes = 0_usize;
    for descriptor in package.manifest().assets() {
        let remaining_decoded_bytes = cache_limits
            .max_decoded_sample_bytes
            .checked_sub(decoded_bytes)
            .ok_or(ProjectPlaybackError::DecodedBytesLimit {
                actual: decoded_bytes,
                maximum: cache_limits.max_decoded_sample_bytes,
            })?;
        if remaining_decoded_bytes == 0 {
            return Err(ProjectPlaybackError::DecodedBytesLimit {
                actual: decoded_bytes.saturating_add(1),
                maximum: cache_limits.max_decoded_sample_bytes,
            });
        }
        let cache_frame_limit =
            u64::try_from(cache_limits.max_frames_per_asset).unwrap_or(u64::MAX);
        let decode_limits = DecodeLimits::new(
            limits.audio.max_file_bytes(),
            limits.audio.max_frames().min(cache_frame_limit),
            limits
                .audio
                .max_decoded_sample_bytes()
                .min(remaining_decoded_bytes),
        )
        .map_err(|source| ProjectPlaybackError::Audio {
            asset_id: descriptor.id().to_owned(),
            source,
        })?;
        let bytes = package.read_asset(descriptor.id(), limits.audio.max_file_bytes())?;
        let audio = decode_bytes(
            bytes,
            audio_container(descriptor.media_type()),
            package.manifest().sample_rate_hz(),
            decode_limits,
        )
        .map_err(|source| match source {
            DecodeError::FrameLimitExceeded { declared, .. } if declared > cache_frame_limit => {
                ProjectPlaybackError::AssetFrameLimit {
                    asset_id: descriptor.id().to_owned(),
                    actual: usize::try_from(declared).unwrap_or(usize::MAX),
                    maximum: cache_limits.max_frames_per_asset,
                }
            }
            DecodeError::DecodedSampleByteLimitExceeded { declared, .. }
                if declared > remaining_decoded_bytes =>
            {
                ProjectPlaybackError::DecodedBytesLimit {
                    actual: decoded_bytes.saturating_add(declared),
                    maximum: cache_limits.max_decoded_sample_bytes,
                }
            }
            source => ProjectPlaybackError::Audio {
                asset_id: descriptor.id().to_owned(),
                source,
            },
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
        facts.push(DecodedAssetFact {
            id: descriptor.id(),
            channels: audio.channels(),
            frames: audio.frame_count(),
        });
        assets.push(DecodedPlaybackAsset::new(descriptor.id().to_owned(), audio));
    }
    Ok(DecodedProjectAssets { assets, facts })
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
    recording_metadata: Vec<&'manifest ResourceDescriptor>,
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
        let mut recording_metadata = Vec::new();
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
                ResourceRole::Mapping => {
                    return Err(ProjectPlaybackError::UnsupportedResourceRole(
                        descriptor.role(),
                    ));
                }
                ResourceRole::RecordingMetadata => {
                    push_descriptor(&mut recording_metadata, descriptor)?;
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
            recording_metadata,
        })
    }

    fn read(
        &self,
        package: &StructurallyVerifiedProject,
    ) -> Result<CapturedPackage, ProjectPlaybackError> {
        let mut nested_playback_ids = BTreeSet::new();
        Ok(CapturedPackage {
            playback: PlaybackDocumentBytes {
                project_id: package.manifest().project_id().to_owned(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: read_playback_document(package, self.timeline, &mut nested_playback_ids)?,
                tracks: read_documents(package, &self.tracks, &mut nested_playback_ids)?,
                clips: read_documents(package, &self.clips, &mut nested_playback_ids)?,
                scenes: read_documents(package, &self.scenes, &mut nested_playback_ids)?,
                arrangement: self
                    .arrangement
                    .map(|descriptor| {
                        read_playback_document(package, descriptor, &mut nested_playback_ids)
                    })
                    .transpose()?,
                mixer: read_playback_document(package, self.mixer, &mut nested_playback_ids)?,
                automation_takes: read_documents(
                    package,
                    &self.automation_takes,
                    &mut nested_playback_ids,
                )?,
            },
            recording_metadata: read_recording_metadata(package, &self.recording_metadata)?,
            nested_playback_ids,
        })
    }
}

struct CapturedPackage {
    playback: PlaybackDocumentBytes,
    recording_metadata: Vec<CapturedRecordingMetadata>,
    nested_playback_ids: BTreeSet<String>,
}

struct CapturedRecordingMetadata {
    descriptor_id: String,
    document: RecordingMetadataDocument,
}

struct DecodedAssetFact<'asset> {
    id: &'asset str,
    channels: usize,
    frames: usize,
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
    nested_ids: &mut BTreeSet<String>,
) -> Result<Vec<Vec<u8>>, ProjectPlaybackError> {
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(descriptors.len())
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    for descriptor in descriptors {
        documents.push(read_playback_document(package, descriptor, nested_ids)?);
    }
    Ok(documents)
}

fn read_playback_document(
    package: &StructurallyVerifiedProject,
    descriptor: &ResourceDescriptor,
    nested_ids: &mut BTreeSet<String>,
) -> Result<Vec<u8>, ProjectPlaybackError> {
    let bytes = package.read_resource(descriptor.id())?;
    let document: Value =
        hyper_strict_json::parse(&bytes, bytes.len().max(1)).map_err(|source| {
            ProjectPlaybackError::ResourceJson {
                resource_id: descriptor.id().to_owned(),
                source,
            }
        })?;
    let document_id = document
        .as_object()
        .and_then(|object| object.get("id"))
        .and_then(Value::as_str);
    if document_id != Some(descriptor.id()) {
        return Err(ProjectPlaybackError::ResourceIdMismatch {
            descriptor_id: descriptor.id().to_owned(),
            document_id: document_id.map(str::to_owned),
        });
    }
    if let Some(object) = document.as_object() {
        for (key, value) in object {
            if key != "id" && key != "extensions" {
                collect_nested_ids(value, nested_ids);
            }
        }
    }
    Ok(bytes)
}

fn collect_nested_ids(value: &Value, identities: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_nested_ids(value, identities);
            }
        }
        Value::Object(object) => {
            if let Some(id) = object.get("id").and_then(Value::as_str) {
                identities.insert(id.to_owned());
            }
            for (key, value) in object {
                if key != "extensions" {
                    collect_nested_ids(value, identities);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn read_recording_metadata(
    package: &StructurallyVerifiedProject,
    descriptors: &[&ResourceDescriptor],
) -> Result<Vec<CapturedRecordingMetadata>, ProjectPlaybackError> {
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(descriptors.len())
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    for descriptor in descriptors {
        let bytes = package.read_resource(descriptor.id())?;
        let document = RecordingMetadataDocument::parse(&bytes).map_err(|source| {
            ProjectPlaybackError::RecordingMetadata {
                resource_id: descriptor.id().to_owned(),
                source,
            }
        })?;
        documents.push(CapturedRecordingMetadata {
            descriptor_id: descriptor.id().to_owned(),
            document,
        });
    }
    Ok(documents)
}

fn validate_recording_inventory(
    package: &StructurallyVerifiedProject,
    metadata: &[CapturedRecordingMetadata],
    nested_playback_ids: &BTreeSet<String>,
) -> Result<(), ProjectPlaybackError> {
    let recording_assets = package
        .manifest()
        .assets()
        .iter()
        .filter(|asset| asset.role() == AssetRole::Recording)
        .count();
    if metadata.len() != recording_assets {
        return Err(recording_binding(
            None,
            RecordingBindingErrorKind::CountMismatch {
                metadata: metadata.len(),
                assets: recording_assets,
            },
        ));
    }

    let mut described_assets = BTreeSet::new();
    for captured in metadata {
        let document = &captured.document;
        if captured.descriptor_id != document.id() {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::ResourceIdMismatch,
            ));
        }
        if nested_playback_ids.contains(document.id()) {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::IdentityCollision,
            ));
        }
        if document.project_id() != package.manifest().project_id() {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::ProjectIdMismatch,
            ));
        }
        if !described_assets.insert(document.asset_id()) {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::DuplicateAssetReference,
            ));
        }
        let asset = package
            .manifest()
            .assets()
            .iter()
            .find(|asset| asset.id() == document.asset_id());
        if asset.is_none_or(|asset| asset.role() != AssetRole::Recording) {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::AssetNotRecording,
            ));
        }
        let project_rate = package.manifest().sample_rate_hz();
        if document.sample_rate_hz() != project_rate {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::SampleRateMismatch {
                    metadata: document.sample_rate_hz(),
                    project: project_rate,
                },
            ));
        }
        let available = package.manifest().required_capabilities().audio_inputs();
        for &index in document.source_input_indices() {
            if index >= available {
                return Err(recording_binding(
                    Some(document.id()),
                    RecordingBindingErrorKind::InputUnavailable { index, available },
                ));
            }
        }
    }
    Ok(())
}

fn validate_recording_media(
    session: &PreparedPlaybackSession,
    metadata: &[CapturedRecordingMetadata],
    assets: &[DecodedAssetFact<'_>],
) -> Result<(), ProjectPlaybackError> {
    let project = session.parts().0;
    for captured in metadata {
        let document = &captured.document;
        if !project.is_audio_track(document.track_id()) {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::TrackNotAudio,
            ));
        }
        let Some(asset) = assets.iter().find(|asset| asset.id == document.asset_id()) else {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::AssetNotRecording,
            ));
        };
        if usize::from(document.channels()) != asset.channels {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::ChannelMismatch {
                    metadata: document.channels(),
                    audio: asset.channels,
                },
            ));
        }
        if usize::try_from(document.frame_count()) != Ok(asset.frames) {
            return Err(recording_binding(
                Some(document.id()),
                RecordingBindingErrorKind::FrameCountMismatch {
                    metadata: document.frame_count(),
                    audio: asset.frames,
                },
            ));
        }
    }
    Ok(())
}

fn recording_binding(
    metadata_id: Option<&str>,
    kind: RecordingBindingErrorKind,
) -> ProjectPlaybackError {
    ProjectPlaybackError::RecordingBinding(RecordingBindingError {
        metadata_id: metadata_id.map(str::to_owned),
        kind,
    })
}
