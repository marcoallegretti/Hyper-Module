#![doc = "Provenance-preserving project-package loading for native playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::path::Path;

use hyper_audio_source::{AudioContainer, DecodeError, DecodeLimits, decode_bytes};
use hyper_control_mappings::{
    ControlMappingBindingError, ControlMappingDocument, ControlMappingParseError,
    ControlMappingProject,
};
use hyper_mapping_compiler::{
    CompiledControlMappingTable, ControlMappingCompileError, ControlMappingResolver,
    MixerSendAddress, compile_control_mapping,
};
use hyper_native_device::{DeviceChain, DeviceChainError};
use hyper_playback_session::{
    DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyError,
    PlaybackSessionAssemblyInput, PlaybackSessionAssemblyLimits, PreparedPlaybackSession,
    assemble_playback_session,
};
use hyper_project_model::{
    AssetDescriptor, AssetRole, AudioMediaType, ResourceDescriptor, ResourceRole,
};
use hyper_recording_metadata::{RecordingMetadataDocument, RecordingMetadataParseError};
use hyper_storage_engine::{
    ProjectPackageLimit, ProjectPackageVerificationLimits, StorageError,
    StructurallyVerifiedProject, verify_project_package_with_limits,
};
use hyper_strict_json::StrictJsonError;
use hyper_track_freeze::{TrackFreezeState, TrackFreezeStateError};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectPlaybackLimits {
    pub max_total_resource_bytes: usize,
    pub audio: DecodeLimits,
    pub session: PlaybackSessionAssemblyLimits,
}

impl ProjectPlaybackLimits {
    /// Derives the structural package bounds used before playback capture.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectPlaybackError::InvalidLimits`] when any required
    /// playback capacity is zero.
    pub fn package_verification_limits(
        self,
    ) -> Result<ProjectPackageVerificationLimits, ProjectPlaybackError> {
        let cache_limits = self.session.media_cache;
        if self.max_total_resource_bytes == 0
            || cache_limits.max_assets == 0
            || cache_limits.max_frames_per_asset == 0
            || cache_limits.max_decoded_sample_bytes == 0
        {
            return Err(ProjectPlaybackError::InvalidLimits);
        }
        Ok(ProjectPackageVerificationLimits {
            max_total_resource_bytes: u64::try_from(self.max_total_resource_bytes)
                .unwrap_or(u64::MAX),
            max_assets: cache_limits.max_assets,
            max_asset_file_bytes: self.audio.max_file_bytes(),
        })
    }
}

#[derive(Debug)]
pub struct PreparedProjectPlayback {
    package: StructurallyVerifiedProject,
    session: PreparedPlaybackSession,
    control_mapping: Option<ControlMappingDocument>,
    compiled_control_mapping: Option<CompiledControlMappingTable>,
    recording_metadata: Vec<RecordingMetadataDocument>,
    device_chains: Vec<DeviceChain>,
    freeze_states: Vec<TrackFreezeState>,
    nested_playback_ids: BTreeSet<String>,
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

    /// Borrows the same resolver that compiled this project's control
    /// mappings, so editor previews resolve identities identically.
    #[must_use]
    pub const fn mapping_project(&self) -> LoadedMappingProject<'_> {
        LoadedMappingProject {
            package: &self.package,
            session: &self.session,
            nested_playback_ids: &self.nested_playback_ids,
        }
    }

    #[must_use]
    pub const fn control_mapping(&self) -> Option<&ControlMappingDocument> {
        self.control_mapping.as_ref()
    }

    #[must_use]
    pub const fn compiled_control_mapping(&self) -> Option<&CompiledControlMappingTable> {
        self.compiled_control_mapping.as_ref()
    }

    #[must_use]
    pub fn recording_metadata(&self) -> &[RecordingMetadataDocument] {
        &self.recording_metadata
    }

    #[must_use]
    pub fn device_chains(&self) -> &[DeviceChain] {
        &self.device_chains
    }

    #[must_use]
    pub fn freeze_states(&self) -> &[TrackFreezeState] {
        &self.freeze_states
    }

    /// Returns verified project-owned sample assets that no reusable audio
    /// clip references. Recording assets are deliberately outside this
    /// cleanup domain because they carry independent provenance metadata.
    pub fn unused_sample_assets(&self) -> impl Iterator<Item = &AssetDescriptor> {
        let loaded = self.session.parts().0;
        self.package
            .manifest()
            .assets()
            .iter()
            .filter(move |asset| {
                asset.role() == AssetRole::Sample
                    && !(0..loaded.clip_count())
                        .any(|clip_index| loaded.clip_asset_id_at(clip_index) == Some(asset.id()))
            })
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        StructurallyVerifiedProject,
        PreparedPlaybackSession,
        Option<ControlMappingDocument>,
        Option<CompiledControlMappingTable>,
        Vec<RecordingMetadataDocument>,
        Vec<DeviceChain>,
    ) {
        (
            self.package,
            self.session,
            self.control_mapping,
            self.compiled_control_mapping,
            self.recording_metadata,
            self.device_chains,
        )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceChainBindingErrorKind {
    ResourceIdMismatch,
    ProjectIdMismatch,
    TrackUnavailable,
    DuplicateTrackChain,
    IdentityCollision,
}

#[derive(Debug)]
pub struct DeviceChainBindingError {
    chain_id: String,
    kind: DeviceChainBindingErrorKind,
}

impl DeviceChainBindingError {
    #[must_use]
    pub const fn kind(&self) -> DeviceChainBindingErrorKind {
        self.kind
    }

    #[must_use]
    pub fn chain_id(&self) -> &str {
        &self.chain_id
    }
}

impl fmt::Display for DeviceChainBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "device chain {}: ", self.chain_id)?;
        formatter.write_str(match self.kind {
            DeviceChainBindingErrorKind::ResourceIdMismatch => {
                "document ID does not match its resource descriptor"
            }
            DeviceChainBindingErrorKind::ProjectIdMismatch => "document belongs to another project",
            DeviceChainBindingErrorKind::TrackUnavailable => {
                "owning track is not a compiled mixer route"
            }
            DeviceChainBindingErrorKind::DuplicateTrackChain => {
                "owning track has more than one device chain"
            }
            DeviceChainBindingErrorKind::IdentityCollision => {
                "chain or device identity collides with another project identity"
            }
        })
    }
}

impl Error for DeviceChainBindingError {}

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
    MultipleControlMappings,
    Storage(StorageError),
    ControlMapping {
        resource_id: String,
        source: ControlMappingParseError,
    },
    ControlMappingBinding(ControlMappingBindingError),
    ControlMappingCompile(ControlMappingCompileError),
    RecordingMetadata {
        resource_id: String,
        source: RecordingMetadataParseError,
    },
    DeviceChain {
        resource_id: String,
        source: DeviceChainError,
    },
    DeviceChainBinding(DeviceChainBindingError),
    FreezeState {
        resource_id: String,
        source: TrackFreezeStateError,
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
            Self::MultipleControlMappings => {
                formatter.write_str("playback package has multiple control-mapping resources")
            }
            Self::Storage(source) => write!(formatter, "package verification failed: {source}"),
            Self::ControlMapping {
                resource_id,
                source,
            } => write!(
                formatter,
                "control-mapping resource {resource_id} is invalid: {source}"
            ),
            Self::ControlMappingBinding(source) => source.fmt(formatter),
            Self::ControlMappingCompile(source) => {
                write!(formatter, "control mapping could not be compiled: {source}")
            }
            Self::RecordingMetadata {
                resource_id,
                source,
            } => write!(
                formatter,
                "recording metadata resource {resource_id} is invalid: {source}"
            ),
            Self::DeviceChain {
                resource_id,
                source,
            } => write!(
                formatter,
                "device-chain resource {resource_id} is invalid: {source}"
            ),
            Self::DeviceChainBinding(source) => source.fmt(formatter),
            Self::FreezeState {
                resource_id,
                source,
            } => write!(
                formatter,
                "freeze-state resource {resource_id} is invalid: {source}"
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
            Self::ControlMapping { source, .. } => Some(source),
            Self::ControlMappingBinding(source) => Some(source),
            Self::ControlMappingCompile(source) => Some(source),
            Self::RecordingMetadata { source, .. } => Some(source),
            Self::DeviceChain { source, .. } => Some(source),
            Self::DeviceChainBinding(source) => Some(source),
            Self::FreezeState { source, .. } => Some(source),
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
            | Self::MultipleControlMappings
            | Self::ResourceIdMismatch { .. } => None,
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
/// mappings are parsed and bound to verified capabilities, identities, scopes,
/// mixer targets, scenes, and exact integer transforms. No resource role is
/// silently ignored.
///
/// # Errors
///
/// Returns a fail-closed rejection for structural, capacity, role, media, or
/// cross-resource semantic failure. No partial session is published.
pub fn prepare_project_playback(
    root: impl AsRef<Path>,
    limits: ProjectPlaybackLimits,
) -> Result<PreparedProjectPlayback, ProjectPlaybackError> {
    let verification_limits = limits.package_verification_limits()?;
    let package = verify_project_package_with_limits(root, verification_limits)
        .map_err(playback_storage_error)?;
    prepare_verified_project_playback_inner(package, limits)
}

/// Semantically resolves and decodes an already structurally verified project.
///
/// This is the loading seam for a stronger outer artifact authority such as a
/// runtime bundle. The project authority remains owned by the returned session
/// and is reconfirmed after all resources and assets have been captured.
///
/// # Errors
///
/// Returns a fail-closed rejection for invalid limits, capacity, role, media,
/// cross-resource semantic, or concurrent package failure.
pub fn prepare_verified_project_playback(
    package: StructurallyVerifiedProject,
    limits: ProjectPlaybackLimits,
) -> Result<PreparedProjectPlayback, ProjectPlaybackError> {
    limits.package_verification_limits()?;
    prepare_verified_project_playback_inner(package, limits)
}

fn prepare_verified_project_playback_inner(
    package: StructurallyVerifiedProject,
    limits: ProjectPlaybackLimits,
) -> Result<PreparedProjectPlayback, ProjectPlaybackError> {
    let inventory = ResourceInventory::new(&package, limits.max_total_resource_bytes)?;
    let mut captured = inventory.read(&package)?;
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
    validate_device_inventory(
        &package,
        &captured.device_chains,
        &session,
        &captured.nested_playback_ids,
    )?;
    validate_freeze_state_inventory(&package, &captured.freeze_states, &session)?;
    for captured_chain in &captured.device_chains {
        for device in captured_chain.document.devices() {
            captured.nested_playback_ids.insert(device.id().to_owned());
        }
    }
    validate_recording_media(&session, &captured.recording_metadata, &decoded.facts)?;
    let compiled_control_mapping = captured
        .control_mapping
        .as_ref()
        .map(|mapping| {
            compile_control_mapping(
                &mapping.document,
                &mapping.descriptor_id,
                &LoadedMappingProject {
                    package: &package,
                    session: &session,
                    nested_playback_ids: &captured.nested_playback_ids,
                },
            )
            .map_err(|source| match source {
                ControlMappingCompileError::Binding(source) => {
                    ProjectPlaybackError::ControlMappingBinding(source)
                }
                source => ProjectPlaybackError::ControlMappingCompile(source),
            })
        })
        .transpose()?;
    let bypassed_device_chain_ids: BTreeSet<&str> = captured
        .freeze_states
        .iter()
        .filter_map(|state| state.document.device_chain_id())
        .collect();
    package.confirm_unchanged()?;
    Ok(PreparedProjectPlayback {
        package,
        session,
        control_mapping: captured.control_mapping.map(|mapping| mapping.document),
        compiled_control_mapping,
        recording_metadata: captured
            .recording_metadata
            .into_iter()
            .map(|record| record.document)
            .collect(),
        device_chains: captured
            .device_chains
            .into_iter()
            .filter(|chain| !bypassed_device_chain_ids.contains(chain.descriptor_id.as_str()))
            .map(|chain| chain.document)
            .collect(),
        freeze_states: captured
            .freeze_states
            .into_iter()
            .map(|state| state.document)
            .collect(),
        nested_playback_ids: captured.nested_playback_ids,
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
    control_mapping: Option<&'manifest ResourceDescriptor>,
    automation_takes: Vec<&'manifest ResourceDescriptor>,
    recording_metadata: Vec<&'manifest ResourceDescriptor>,
    device_chains: Vec<&'manifest ResourceDescriptor>,
    freeze_states: Vec<&'manifest ResourceDescriptor>,
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
        let mut control_mapping = None;
        let mut automation_takes = Vec::new();
        let mut recording_metadata = Vec::new();
        let mut device_chains = Vec::new();
        let mut freeze_states = Vec::new();
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
                    if control_mapping.replace(descriptor).is_some() {
                        return Err(ProjectPlaybackError::MultipleControlMappings);
                    }
                }
                ResourceRole::RecordingMetadata => {
                    push_descriptor(&mut recording_metadata, descriptor)?;
                }
                ResourceRole::DeviceChain => push_descriptor(&mut device_chains, descriptor)?,
                ResourceRole::FreezeState => push_descriptor(&mut freeze_states, descriptor)?,
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
            control_mapping,
            automation_takes,
            recording_metadata,
            device_chains,
            freeze_states,
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
            control_mapping: self
                .control_mapping
                .map(|descriptor| read_control_mapping(package, descriptor))
                .transpose()?,
            recording_metadata: read_recording_metadata(package, &self.recording_metadata)?,
            device_chains: read_device_chains(package, &self.device_chains)?,
            freeze_states: read_freeze_states(package, &self.freeze_states)?,
            nested_playback_ids,
        })
    }
}

struct CapturedPackage {
    playback: PlaybackDocumentBytes,
    control_mapping: Option<CapturedControlMapping>,
    recording_metadata: Vec<CapturedRecordingMetadata>,
    device_chains: Vec<CapturedDeviceChain>,
    freeze_states: Vec<CapturedFreezeState>,
    nested_playback_ids: BTreeSet<String>,
}

struct CapturedControlMapping {
    descriptor_id: String,
    document: ControlMappingDocument,
}

struct CapturedRecordingMetadata {
    descriptor_id: String,
    document: RecordingMetadataDocument,
}

struct CapturedDeviceChain {
    descriptor_id: String,
    document: DeviceChain,
}

struct CapturedFreezeState {
    descriptor_id: String,
    document: TrackFreezeState,
}

/// Resolves control-mapping identities against one verified prepared project.
pub struct LoadedMappingProject<'project> {
    package: &'project StructurallyVerifiedProject,
    session: &'project PreparedPlaybackSession,
    nested_playback_ids: &'project BTreeSet<String>,
}

impl ControlMappingProject for LoadedMappingProject<'_> {
    fn project_id(&self) -> &str {
        self.package.manifest().project_id()
    }

    fn cv_inputs(&self) -> u8 {
        self.package.manifest().required_capabilities().cv_inputs()
    }

    fn has_clock_input(&self) -> bool {
        self.package
            .manifest()
            .required_capabilities()
            .has_clock_input()
    }

    fn has_reset_input(&self) -> bool {
        self.package
            .manifest()
            .required_capabilities()
            .has_reset_input()
    }

    fn has_mixer(&self) -> bool {
        true
    }

    fn contains_nested_entity(&self, id: &str) -> bool {
        self.nested_playback_ids.contains(id)
    }

    fn contains_entity(&self, id: &str) -> bool {
        self.package
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == id)
            || self
                .package
                .manifest()
                .assets()
                .iter()
                .any(|descriptor| descriptor.id() == id)
            || self.nested_playback_ids.contains(id)
    }

    fn contains_track(&self, id: &str) -> bool {
        self.package
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.role() == ResourceRole::Track && descriptor.id() == id)
    }

    fn contains_scene(&self, id: &str) -> bool {
        self.package
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.role() == ResourceRole::Scene && descriptor.id() == id)
    }

    fn contains_mixer_channel(&self, track_id: &str) -> bool {
        self.session
            .parts()
            .1
            .slots()
            .iter()
            .any(|slot| slot.track_id() == track_id)
    }

    fn contains_mixer_send(&self, send_id: &str) -> bool {
        self.session
            .parts()
            .1
            .slots()
            .iter()
            .any(|slot| slot.sends().iter().any(|send| send.id() == send_id))
    }

    fn is_solo_source_track(&self, track_id: &str) -> bool {
        self.session.parts().0.is_audio_track(track_id)
    }
}

impl ControlMappingResolver for LoadedMappingProject<'_> {
    fn track_index(&self, track_id: &str) -> Option<usize> {
        self.session.parts().0.track_index(track_id)
    }

    fn scene_index(&self, scene_id: &str) -> Option<usize> {
        self.session.parts().0.scene_index(scene_id)
    }

    fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
        self.session
            .parts()
            .1
            .slots()
            .iter()
            .position(|slot| slot.track_id() == track_id)
    }

    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress> {
        self.session
            .parts()
            .1
            .slots()
            .iter()
            .enumerate()
            .find_map(|(channel_index, slot)| {
                slot.sends()
                    .iter()
                    .position(|send| send.id() == send_id)
                    .map(|send_index| MixerSendAddress::new(channel_index, send_index))
            })
    }
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

fn read_control_mapping(
    package: &StructurallyVerifiedProject,
    descriptor: &ResourceDescriptor,
) -> Result<CapturedControlMapping, ProjectPlaybackError> {
    let bytes = package.read_resource(descriptor.id())?;
    let document = ControlMappingDocument::parse(&bytes).map_err(|source| {
        ProjectPlaybackError::ControlMapping {
            resource_id: descriptor.id().to_owned(),
            source,
        }
    })?;
    Ok(CapturedControlMapping {
        descriptor_id: descriptor.id().to_owned(),
        document,
    })
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

fn read_device_chains(
    package: &StructurallyVerifiedProject,
    descriptors: &[&ResourceDescriptor],
) -> Result<Vec<CapturedDeviceChain>, ProjectPlaybackError> {
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(descriptors.len())
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    for descriptor in descriptors {
        let bytes = package.read_resource(descriptor.id())?;
        let document =
            DeviceChain::parse(&bytes).map_err(|source| ProjectPlaybackError::DeviceChain {
                resource_id: descriptor.id().to_owned(),
                source,
            })?;
        documents.push(CapturedDeviceChain {
            descriptor_id: descriptor.id().to_owned(),
            document,
        });
    }
    Ok(documents)
}

fn read_freeze_states(
    package: &StructurallyVerifiedProject,
    descriptors: &[&ResourceDescriptor],
) -> Result<Vec<CapturedFreezeState>, ProjectPlaybackError> {
    let mut documents = Vec::new();
    documents
        .try_reserve_exact(descriptors.len())
        .map_err(|_| ProjectPlaybackError::ResourceCapacityUnavailable)?;
    for descriptor in descriptors {
        let bytes = package.read_resource(descriptor.id())?;
        let document = TrackFreezeState::parse(&bytes).map_err(|source| {
            ProjectPlaybackError::FreezeState {
                resource_id: descriptor.id().to_owned(),
                source,
            }
        })?;
        documents.push(CapturedFreezeState {
            descriptor_id: descriptor.id().to_owned(),
            document,
        });
    }
    Ok(documents)
}

fn validate_freeze_state_inventory(
    package: &StructurallyVerifiedProject,
    states: &[CapturedFreezeState],
    session: &PreparedPlaybackSession,
) -> Result<(), ProjectPlaybackError> {
    let mut tracks = BTreeSet::new();
    let descriptors = package.manifest().resources();
    for state in states {
        let document = &state.document;
        let fail = |message| ProjectPlaybackError::FreezeState {
            resource_id: state.descriptor_id.clone(),
            source: TrackFreezeStateError::InvalidDomain(message),
        };
        if state.descriptor_id != document.id()
            || document.project_id() != package.manifest().project_id()
            || !tracks.insert(document.track_id())
        {
            return Err(fail("freeze-state resource identity is inconsistent"));
        }
        if !session.parts().0.is_audio_track(document.track_id()) {
            return Err(fail("freeze-state track is not an audio track"));
        }
        let loaded = session.parts().0;
        let clip_belongs_to_track = |clip_id: &str| {
            (0..loaded.clip_count()).any(|index| {
                loaded.clip_id_at(index) == Some(clip_id)
                    && loaded.clip_track_id_at(index) == Some(document.track_id())
            })
        };
        if !clip_belongs_to_track(document.frozen_clip_id()) {
            return Err(fail("freeze-state clip does not belong to its track"));
        }
        for (id, role) in [
            (document.frozen_clip_id(), ResourceRole::Clip),
            (document.arrangement_id(), ResourceRole::Arrangement),
        ] {
            if !descriptors
                .iter()
                .any(|descriptor| descriptor.id() == id && descriptor.role() == role)
            {
                return Err(fail("freeze-state references a missing resource"));
            }
        }
        let frozen_clip = package.read_resource(document.frozen_clip_id())?;
        let retained_frozen_clip =
            serde_json_canonicalizer::to_vec(document.frozen_clip().document()).map_err(|_| {
                ProjectPlaybackError::FreezeState {
                    resource_id: state.descriptor_id.clone(),
                    source: TrackFreezeStateError::InvalidDomain(
                        "freeze-state rendered clip cannot be canonicalized",
                    ),
                }
            })?;
        if frozen_clip != retained_frozen_clip {
            return Err(fail(
                "freeze-state rendered clip authority differs from the active clip",
            ));
        }
        if !package.manifest().assets().iter().any(|asset| {
            asset.id() == document.frozen_asset_id() && asset.role() == AssetRole::Sample
        }) {
            return Err(fail("freeze-state references a missing rendered sample"));
        }
        if document.original_clips().iter().any(|clip| {
            clip.track_id() != document.track_id()
                || descriptors.iter().any(|descriptor| {
                    descriptor.id() == clip.id() && descriptor.role() == ResourceRole::Clip
                })
                || !package
                    .manifest()
                    .assets()
                    .iter()
                    .any(|asset| asset.id() == clip.asset_id() && asset.role() == AssetRole::Sample)
        }) {
            return Err(fail(
                "freeze-state has inconsistent original clip authority",
            ));
        }
        if let Some(chain_id) = document.device_chain_id()
            && !descriptors.iter().any(|descriptor| {
                descriptor.id() == chain_id && descriptor.role() == ResourceRole::DeviceChain
            })
        {
            return Err(fail("freeze-state references a missing device chain"));
        }
    }
    Ok(())
}

fn validate_device_inventory(
    package: &StructurallyVerifiedProject,
    chains: &[CapturedDeviceChain],
    session: &PreparedPlaybackSession,
    nested_playback_ids: &BTreeSet<String>,
) -> Result<(), ProjectPlaybackError> {
    let manifest_ids: BTreeSet<&str> = package
        .manifest()
        .resources()
        .iter()
        .map(ResourceDescriptor::id)
        .chain(package.manifest().assets().iter().map(AssetDescriptor::id))
        .collect();
    let mixer = session.parts().1;
    let mut owned_tracks = BTreeSet::new();
    let mut device_ids = BTreeSet::new();
    for captured in chains {
        let chain = &captured.document;
        let fail = |kind| {
            ProjectPlaybackError::DeviceChainBinding(DeviceChainBindingError {
                chain_id: chain.id().to_owned(),
                kind,
            })
        };
        if captured.descriptor_id != chain.id() {
            return Err(fail(DeviceChainBindingErrorKind::ResourceIdMismatch));
        }
        if chain.project_id() != package.manifest().project_id() {
            return Err(fail(DeviceChainBindingErrorKind::ProjectIdMismatch));
        }
        if !mixer
            .slots()
            .iter()
            .any(|slot| slot.track_id() == chain.track_id())
        {
            return Err(fail(DeviceChainBindingErrorKind::TrackUnavailable));
        }
        if !owned_tracks.insert(chain.track_id()) {
            return Err(fail(DeviceChainBindingErrorKind::DuplicateTrackChain));
        }
        if nested_playback_ids.contains(chain.id()) {
            return Err(fail(DeviceChainBindingErrorKind::IdentityCollision));
        }
        for device in chain.devices() {
            if manifest_ids.contains(device.id())
                || nested_playback_ids.contains(device.id())
                || !device_ids.insert(device.id())
            {
                return Err(fail(DeviceChainBindingErrorKind::IdentityCollision));
            }
        }
    }
    Ok(())
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
