use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_native_device::DeviceChain;
use hyper_playback_json::MixerStripReorderDocuments;
use hyper_project_model::{ProjectManifest, ResourceDescriptor, ResourceRole, is_valid_entity_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

mod audio_clip_publication;
mod audio_track_content_removal;
mod audio_track_duplication;
mod sample_cleanup;

pub use audio_clip_publication::{
    AudioClipPublicationOutcome, AudioClipPublicationRecovery,
    AudioClipPublicationRecoveryDisposition, AudioClipPublicationRequest, AudioClipRemovalOutcome,
    AudioClipRemovalRequest, ImportedAudioClipPublicationRequest, publish_audio_clip,
    publish_imported_audio_clip, recover_audio_clip_publications, remove_audio_clip,
};
pub use audio_track_content_removal::{
    AudioTrackContentRemovalPublicationOutcome, AudioTrackContentRemovalPublicationRecovery,
    AudioTrackContentRemovalPublicationRecoveryDisposition,
    AudioTrackContentRemovalPublicationRequest, publish_audio_track_content_removal,
    recover_audio_track_content_removals,
};
pub use audio_track_duplication::{
    AudioTrackDuplicationPublicationOutcome, AudioTrackDuplicationPublicationRecovery,
    AudioTrackDuplicationPublicationRecoveryDisposition, AudioTrackDuplicationPublicationRequest,
    RoutingBusDuplicationPublicationRequest, publish_audio_track_duplication,
    publish_routing_bus_duplication, recover_audio_track_duplications,
};
pub use sample_cleanup::{
    SampleCleanupOutcome, SampleCleanupRecovery, SampleCleanupRecoveryDisposition,
    SampleCleanupRequest, clean_unused_samples, recover_sample_cleanups,
};

use super::project_lock::{
    AUDIO_TRACK_TRANSACTION_PREFIX, PROJECT_RESOURCE_TRANSACTION_PREFIX as TRANSACTION_PREFIX,
    ProjectLock, ProjectTransactionFamily, TRACK_ORDER_TRANSACTION_PREFIX,
    find_unhandled_project_transaction,
};
use super::stable_file::read_open_stable;
use super::{
    MANIFEST_NAME, MAX_ENTRIES, MAX_JSON_BYTES, MAX_JSON_FILE_BYTES, StructurallyVerifiedProject,
    open_stable_project_root, same_directory, verify_project_candidate, verify_project_package,
    verify_retained_project_candidate,
};

const INTENT_NAME: &str = "intent.json";
const INTENT_STAGING_NAME: &str = ".intent.json.tmp";
const RESOURCE_STAGING_NAME: &str = "resource.json";
const PREVIOUS_RESOURCE_NAME: &str = "previous-resource.json";
const PROJECT_STAGING_NAME: &str = "project.json";
const PREVIOUS_PROJECT_NAME: &str = "previous-project.json";
const TRANSACTION_NAME: &str = "transaction.json";
const AUDIO_TRACK_STAGING_NAME: &str = "track.json";
const AUDIO_TRACK_MIXER_STAGING_NAME: &str = "mixer.json";
const AUDIO_TRACK_PREVIOUS_MIXER_NAME: &str = "previous-mixer.json";
const TRACK_ORDER_MIXER_STAGING_NAME: &str = "mixer.json";
const TRACK_ORDER_PREVIOUS_MIXER_NAME: &str = "previous-mixer.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub struct ProjectResourcePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub resource_id: &'a str,
    pub expected_role: ResourceRole,
    pub resource_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct ArrangementPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub arrangement_id: &'a str,
    pub arrangement_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct ScenePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub scene_id: &'a str,
    pub scene_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct SceneRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub scene_id: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct AutomationTakePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub take_id: &'a str,
    pub take_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct AutomationTakeRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub take_id: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceChainPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub chain_id: &'a str,
    pub chain_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceChainRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub chain_id: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectResourcePublicationErrorKind {
    InvalidResource,
    ProjectChanged,
    ResourceConflict,
    TransactionUnavailable,
    Integrity,
    Io,
    Recovery,
}

#[derive(Debug)]
pub struct ProjectResourcePublicationError {
    kind: ProjectResourcePublicationErrorKind,
    message: String,
    published: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectResourcePublicationError {
    #[must_use]
    pub const fn kind(&self) -> ProjectResourcePublicationErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn published(&self) -> bool {
        self.published
    }
}

impl fmt::Display for ProjectResourcePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProjectResourcePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectResourcePublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    resource_id: String,
    resource_path: String,
    changed: bool,
    previous_resource: Option<Vec<u8>>,
}

impl ProjectResourcePublicationOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn resource_id(&self) -> &str {
        &self.resource_id
    }

    #[must_use]
    pub fn resource_path(&self) -> &str {
        &self.resource_path
    }

    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }

    /// The complete canonical bytes this publication replaced, present only
    /// when a replacement actually changed content. Republishing them through
    /// the same lifecycle restores the previous state exactly.
    #[must_use]
    pub fn previous_resource(&self) -> Option<&[u8]> {
        self.previous_resource.as_deref()
    }

    #[must_use]
    pub fn into_previous_resource(self) -> Option<Vec<u8>> {
        self.previous_resource
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectResourcePublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectResourcePublicationRecovery {
    transaction_id: String,
    disposition: ProjectResourcePublicationRecoveryDisposition,
}

#[derive(Clone, Copy, Debug)]
pub struct AudioTrackPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub track_id: &'a str,
    pub track_bytes: &'a [u8],
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
}

/// The routing effect a new bus must prove in its candidate mixer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingBusPublicationKind {
    Group,
    Return,
}

/// One canonical routing-bus resource and the complete mixer rewrite it
/// authorizes. Selected sources and return-send identities are retained in the
/// durable transaction record for recovery verification.
#[derive(Clone, Copy, Debug)]
pub struct RoutingBusPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub track_id: &'a str,
    pub track_bytes: &'a [u8],
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
    pub kind: RoutingBusPublicationKind,
    pub source_track_ids: &'a [&'a str],
    pub send_ids: &'a [&'a str],
}

#[derive(Clone, Copy, Debug)]
struct TrackAdditionPublicationRequest<'a> {
    expected_project_sha256: [u8; 32],
    track_id: &'a str,
    track_bytes: &'a [u8],
    mixer_id: &'a str,
    mixer_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
enum TrackAdditionPublicationContract<'a> {
    Audio,
    Routing {
        kind: RoutingBusPublicationKind,
        source_track_ids: &'a [String],
        send_ids: &'a [String],
    },
}

impl<'a> From<AudioTrackPublicationRequest<'a>> for TrackAdditionPublicationRequest<'a> {
    fn from(request: AudioTrackPublicationRequest<'a>) -> Self {
        Self {
            expected_project_sha256: request.expected_project_sha256,
            track_id: request.track_id,
            track_bytes: request.track_bytes,
            mixer_id: request.mixer_id,
            mixer_bytes: request.mixer_bytes,
        }
    }
}

impl<'a> From<RoutingBusPublicationRequest<'a>> for TrackAdditionPublicationRequest<'a> {
    fn from(request: RoutingBusPublicationRequest<'a>) -> Self {
        Self {
            expected_project_sha256: request.expected_project_sha256,
            track_id: request.track_id,
            track_bytes: request.track_bytes,
            mixer_id: request.mixer_id,
            mixer_bytes: request.mixer_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AudioTrackRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub track_id: &'a str,
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
}

/// The exact routing-bus kind addressed by a removal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingBusRemovalKind {
    Group,
    Return,
}

/// Removes one verified, unreferenced Group or Return track and its matching
/// mixer state through the structural-track removal transaction.
#[derive(Clone, Copy, Debug)]
pub struct RoutingBusRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub track_id: &'a str,
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
    pub kind: RoutingBusRemovalKind,
}

#[derive(Clone, Debug)]
struct StructuralTrackRemovalRequest<'a> {
    expected_project_sha256: [u8; 32],
    track_id: &'a str,
    mixer_id: &'a str,
    mixer_bytes: &'a [u8],
    contract: AudioTrackTransactionContract,
}

impl<'a> StructuralTrackRemovalRequest<'a> {
    fn audio(request: AudioTrackRemovalRequest<'a>) -> Self {
        Self {
            expected_project_sha256: request.expected_project_sha256,
            track_id: request.track_id,
            mixer_id: request.mixer_id,
            mixer_bytes: request.mixer_bytes,
            contract: AudioTrackTransactionContract::Audio,
        }
    }

    fn routing_bus(request: RoutingBusRemovalRequest<'a>) -> Self {
        Self {
            expected_project_sha256: request.expected_project_sha256,
            track_id: request.track_id,
            mixer_id: request.mixer_id,
            mixer_bytes: request.mixer_bytes,
            contract: match request.kind {
                RoutingBusRemovalKind::Group => AudioTrackTransactionContract::RoutingGroup {
                    source_track_ids: Vec::new(),
                },
                RoutingBusRemovalKind::Return => AudioTrackTransactionContract::RoutingReturn {
                    source_track_ids: Vec::new(),
                    send_ids: Vec::new(),
                },
            },
        }
    }
}

/// Replaces every structural track document and the matching mixer through one
/// manifest-last transaction.  The request is intentionally complete: a track
/// order is a project-wide permutation, never a sequence of independently
/// publishable edits.
#[derive(Clone, Copy, Debug)]
pub struct TrackOrderPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub mixer_id: &'a str,
    /// The opaque canonical output of the playback-document reorder boundary.
    /// It cannot be assembled from arbitrary bytes by a storage caller.
    pub documents: &'a MixerStripReorderDocuments,
}

#[derive(Clone, Copy, Debug)]
struct TrackOrderPublicationTrack<'a> {
    track_id: &'a str,
    track_bytes: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackOrderPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
}

impl TrackOrderPublicationOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackOrderPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackOrderPublicationRecovery {
    transaction_id: String,
    disposition: TrackOrderPublicationRecoveryDisposition,
}

impl TrackOrderPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> TrackOrderPublicationRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackRemovalOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

/// The durable outcome of one routing-bus removal.
pub type RoutingBusRemovalOutcome = AudioTrackRemovalOutcome;

impl AudioTrackRemovalOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
    track_path: String,
}

impl AudioTrackPublicationOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub fn track_path(&self) -> &str {
        &self.track_path
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTrackPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackPublicationRecovery {
    transaction_id: String,
    disposition: AudioTrackPublicationRecoveryDisposition,
}

impl AudioTrackPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> AudioTrackPublicationRecoveryDisposition {
        self.disposition
    }
}

impl ProjectResourcePublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> ProjectResourcePublicationRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionIntent {
    format: String,
    format_version: u8,
    id: String,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionFile {
    path: String,
    byte_length: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    resource_id: String,
    resource_role: String,
    resource: TransactionFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_resource: Option<TransactionFile>,
    #[serde(default, skip_serializing_if = "is_false")]
    created_parent_directory: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    removed_parent_directory: bool,
    #[serde(default, skip_serializing_if = "TransactionOperation::is_publication")]
    operation: TransactionOperation,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum TransactionOperation {
    #[default]
    Publication,
    Removal,
}

impl TransactionOperation {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a shared-reference predicate"
    )]
    const fn is_publication(&self) -> bool {
        matches!(self, Self::Publication)
    }
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if requires a shared-reference predicate"
)]
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AudioTrackTransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    track_id: String,
    mixer_id: String,
    track: TransactionFile,
    mixer: TransactionFile,
    previous_mixer: TransactionFile,
    #[serde(
        default,
        skip_serializing_if = "AudioTrackTransactionContract::is_audio"
    )]
    contract: AudioTrackTransactionContract,
    #[serde(
        default,
        skip_serializing_if = "AudioTrackTransactionOperation::is_publication"
    )]
    operation: AudioTrackTransactionOperation,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum AudioTrackTransactionContract {
    #[default]
    Audio,
    RoutingGroup {
        source_track_ids: Vec<String>,
    },
    RoutingReturn {
        source_track_ids: Vec<String>,
        send_ids: Vec<String>,
    },
}

impl AudioTrackTransactionContract {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a shared-reference predicate"
    )]
    const fn is_audio(&self) -> bool {
        matches!(self, Self::Audio)
    }

    fn publication_contract(&self) -> TrackAdditionPublicationContract<'_> {
        match self {
            Self::Audio => TrackAdditionPublicationContract::Audio,
            Self::RoutingGroup { source_track_ids } => TrackAdditionPublicationContract::Routing {
                kind: RoutingBusPublicationKind::Group,
                source_track_ids,
                send_ids: &[],
            },
            Self::RoutingReturn {
                source_track_ids,
                send_ids,
            } => TrackAdditionPublicationContract::Routing {
                kind: RoutingBusPublicationKind::Return,
                source_track_ids,
                send_ids,
            },
        }
    }

    fn is_valid(&self, operation: AudioTrackTransactionOperation) -> bool {
        match self {
            Self::Audio => true,
            Self::RoutingGroup { source_track_ids } => {
                (operation == AudioTrackTransactionOperation::Publication
                    && source_track_ids_are_valid(source_track_ids))
                    || (operation == AudioTrackTransactionOperation::Removal
                        && source_track_ids.is_empty())
            }
            Self::RoutingReturn {
                source_track_ids,
                send_ids,
            } => {
                (operation == AudioTrackTransactionOperation::Publication
                    && source_track_ids_are_valid(source_track_ids)
                    && send_ids.len() == source_track_ids.len()
                    && send_ids_are_valid(send_ids))
                    || (operation == AudioTrackTransactionOperation::Removal
                        && source_track_ids.is_empty()
                        && send_ids.is_empty())
            }
        }
    }
}

fn source_track_ids_are_valid(source_track_ids: &[String]) -> bool {
    !source_track_ids.is_empty()
        && source_track_ids.len() <= MAX_ENTRIES
        && source_track_ids.iter().all(|id| is_valid_entity_id(id))
        && source_track_ids.iter().collect::<BTreeSet<_>>().len() == source_track_ids.len()
}

fn send_ids_are_valid(send_ids: &[String]) -> bool {
    send_ids.len() <= MAX_ENTRIES
        && send_ids.iter().all(|id| is_valid_entity_id(id))
        && send_ids.iter().collect::<BTreeSet<_>>().len() == send_ids.len()
}

fn transaction_contract(
    contract: TrackAdditionPublicationContract<'_>,
) -> AudioTrackTransactionContract {
    match contract {
        TrackAdditionPublicationContract::Audio => AudioTrackTransactionContract::Audio,
        TrackAdditionPublicationContract::Routing {
            kind: RoutingBusPublicationKind::Group,
            source_track_ids,
            ..
        } => AudioTrackTransactionContract::RoutingGroup {
            source_track_ids: source_track_ids.to_vec(),
        },
        TrackAdditionPublicationContract::Routing {
            kind: RoutingBusPublicationKind::Return,
            source_track_ids,
            send_ids,
        } => AudioTrackTransactionContract::RoutingReturn {
            source_track_ids: source_track_ids.to_vec(),
            send_ids: send_ids.to_vec(),
        },
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrackOrderTransactionTrack {
    track_id: String,
    track: TransactionFile,
    previous_track: TransactionFile,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrackOrderTransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    mixer_id: String,
    mixer: TransactionFile,
    previous_mixer: TransactionFile,
    tracks: Vec<TrackOrderTransactionTrack>,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrackOrderTransactionIntent {
    format: String,
    format_version: u8,
    id: String,
    track_count: usize,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AudioTrackTransactionOperation {
    #[default]
    Publication,
    Removal,
}

impl AudioTrackTransactionOperation {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a shared-reference predicate"
    )]
    const fn is_publication(&self) -> bool {
        matches!(self, Self::Publication)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    ResourceFile,
    Resource,
    Manifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectResourcePublicationMode {
    Replacement,
    ArrangementAddition,
    SceneAddition,
    SceneRemoval,
    AutomationTakeAddition,
    AutomationTakeRemoval,
    DeviceChainAddition,
    DeviceChainRemoval,
}

impl ProjectResourcePublicationMode {
    const fn is_removal(self) -> bool {
        matches!(
            self,
            Self::SceneRemoval | Self::AutomationTakeRemoval | Self::DeviceChainRemoval
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioTrackPublicationPhase {
    Transaction,
    Track,
    Mixer,
    Manifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackOrderPublicationPhase {
    Transaction,
    Track(usize),
    Mixer,
    Manifest,
}

/// Replaces one existing canonical JSON resource and its descriptor through a
/// serialized, recoverable manifest-last transaction.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. [`ProjectResourcePublicationError::published`] is true only when
/// recovery proved that the requested resource and manifest became active.
pub fn publish_project_resource(
    project_path: impl AsRef<Path>,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        request,
        ProjectResourcePublicationMode::Replacement,
        |_, _| Ok(()),
    )
}

/// Publishes the first arrangement resource in a project through the existing
/// serialized, recoverable manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Publication is rejected if any arrangement is already declared or
/// if the new stable identity or managed path collides with project content.
pub fn publish_arrangement(
    project_path: impl AsRef<Path>,
    request: ArrangementPublicationRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.arrangement_id,
            expected_role: ResourceRole::Arrangement,
            resource_bytes: request.arrangement_bytes,
        },
        ProjectResourcePublicationMode::ArrangementAddition,
        |_, _| Ok(()),
    )
}

/// Publishes one new canonical scene resource through the existing serialized,
/// recoverable manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Publication is rejected if the stable identity or managed path
/// collides with active project content.
pub fn publish_scene(
    project_path: impl AsRef<Path>,
    request: ScenePublicationRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.scene_id,
            expected_role: ResourceRole::Scene,
            resource_bytes: request.scene_bytes,
        },
        ProjectResourcePublicationMode::SceneAddition,
        |_, _| Ok(()),
    )
}

/// Removes one scene through the serialized, recoverable manifest-last
/// resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Only an existing scene resource can be removed.
pub fn remove_scene(
    project_path: impl AsRef<Path>,
    request: SceneRemovalRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.scene_id,
            expected_role: ResourceRole::Scene,
            resource_bytes: &[],
        },
        ProjectResourcePublicationMode::SceneRemoval,
        |_, _| Ok(()),
    )
}

/// Publishes one new canonical automation take through the existing
/// serialized, recoverable manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Publication is rejected if the stable identity or managed path
/// collides with active project content.
pub fn publish_automation_take(
    project_path: impl AsRef<Path>,
    request: AutomationTakePublicationRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.take_id,
            expected_role: ResourceRole::AutomationTake,
            resource_bytes: request.take_bytes,
        },
        ProjectResourcePublicationMode::AutomationTakeAddition,
        |_, _| Ok(()),
    )
}

/// Removes one automation take through the serialized, recoverable
/// manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Only an existing automation-take resource can be removed.
pub fn remove_automation_take(
    project_path: impl AsRef<Path>,
    request: AutomationTakeRemovalRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.take_id,
            expected_role: ResourceRole::AutomationTake,
            resource_bytes: &[],
        },
        ProjectResourcePublicationMode::AutomationTakeRemoval,
        |_, _| Ok(()),
    )
}

/// Publishes one new native-device chain through the serialized, recoverable
/// manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Publication is rejected if the stable identity or managed path
/// collides with active project content.
pub fn publish_device_chain(
    project_path: impl AsRef<Path>,
    request: DeviceChainPublicationRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.chain_id,
            expected_role: ResourceRole::DeviceChain,
            resource_bytes: request.chain_bytes,
        },
        ProjectResourcePublicationMode::DeviceChainAddition,
        |_, _| Ok(()),
    )
}

/// Removes one native-device chain through the serialized, recoverable
/// manifest-last resource transaction family.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. Only an existing native-device-chain resource can be removed.
pub fn remove_device_chain(
    project_path: impl AsRef<Path>,
    request: DeviceChainRemovalRequest<'_>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    publish_project_resource_observed(
        project_path.as_ref(),
        ProjectResourcePublicationRequest {
            expected_project_sha256: request.expected_project_sha256,
            resource_id: request.chain_id,
            expected_role: ResourceRole::DeviceChain,
            resource_bytes: &[],
        },
        ProjectResourcePublicationMode::DeviceChainRemoval,
        |_, _| Ok(()),
    )
}

/// Recovers every durable existing-resource publication in one project.
///
/// # Errors
///
/// Returns an error rather than overwriting active content whose identity is
/// unrelated to either recorded side of a transaction.
pub fn recover_project_resource_publications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<ProjectResourcePublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for resource recovery",
            error,
        )
    })?;
    let Some(backups) = open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    recover_transactions_locked(&root_path, &root, &backups)
}

/// Publishes one new canonical audio-track resource and its matching mixer
/// replacement through a serialized, recoverable manifest-last transaction.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. No unrelated active file is overwritten during publication or
/// recovery.
pub fn publish_audio_track(
    project_path: impl AsRef<Path>,
    request: AudioTrackPublicationRequest<'_>,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    publish_audio_track_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Publishes one usable group or return bus and its complete mixer routing
/// rewrite through the same serialized, recoverable manifest-last transaction
/// family as audio-track creation.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. The request is rejected unless its candidate mixer contains exactly
/// the routing transformation described by `kind`, `source_track_ids`, and
/// `send_ids`.
pub fn publish_routing_bus(
    project_path: impl AsRef<Path>,
    request: RoutingBusPublicationRequest<'_>,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    let source_track_ids = request
        .source_track_ids
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    let send_ids = request
        .send_ids
        .iter()
        .map(|id| (*id).to_owned())
        .collect::<Vec<_>>();
    let contract = TrackAdditionPublicationContract::Routing {
        kind: request.kind,
        source_track_ids: &source_track_ids,
        send_ids: &send_ids,
    };
    publish_track_addition_observed(project_path.as_ref(), request.into(), contract, |_, _| {
        Ok(())
    })
}

/// Removes one empty audio-track resource and its mixer channel through the
/// same serialized, recoverable manifest-last transaction family used for
/// publication.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. The request is rejected unless it removes exactly one empty audio
/// track and preserves every unrelated mixer field.
pub fn remove_audio_track(
    project_path: impl AsRef<Path>,
    request: AudioTrackRemovalRequest<'_>,
) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
    remove_audio_track_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Removes one unreferenced Group or Return bus through the same durable
/// structural-track transaction used by Audio-track removal.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. The request is rejected unless its exact routing-bus kind and mixer
/// reduction agree with the verified project package.
pub fn remove_routing_bus(
    project_path: impl AsRef<Path>,
    request: RoutingBusRemovalRequest<'_>,
) -> Result<RoutingBusRemovalOutcome, ProjectResourcePublicationError> {
    remove_routing_bus_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Publishes a complete audio-track order permutation and its matching mixer
/// through one serialized, recoverable manifest-last transaction.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. [`ProjectResourcePublicationError::published`] is true only when
/// recovery proved the complete replacement set and manifest became active.
pub fn publish_track_order(
    project_path: impl AsRef<Path>,
    request: TrackOrderPublicationRequest<'_>,
) -> Result<TrackOrderPublicationOutcome, ProjectResourcePublicationError> {
    publish_track_order_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable audio-track publication in one project.
///
/// # Errors
///
/// Returns an error rather than discarding or overwriting content that does
/// not match either recorded side of a transaction.
pub fn recover_audio_track_publications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackPublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for audio-track recovery",
            error,
        )
    })?;
    let Some(backups) = open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    recover_audio_track_transactions_locked(&root_path, &root, &backups)
}

/// Recovers every durable track-order transaction in one project.
///
/// # Errors
///
/// Returns an error rather than discarding or overwriting content that does
/// not match either recorded side of a transaction.
pub fn recover_track_order_publications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<TrackOrderPublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for track-order recovery",
            error,
        )
    })?;
    let Some(backups) = open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    recover_track_order_transactions_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn publish_audio_track_observed(
    project_path: &Path,
    request: AudioTrackPublicationRequest<'_>,
    mut observer: impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    publish_track_addition_observed(
        project_path,
        request.into(),
        TrackAdditionPublicationContract::Audio,
        &mut observer,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn publish_track_addition_observed(
    project_path: &Path,
    request: TrackAdditionPublicationRequest<'_>,
    contract: TrackAdditionPublicationContract<'_>,
    mut observer: impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    validate_track_addition_documents(request)?;
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before audio-track publication",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the audio-track edit began",
        ));
    }
    let track_path = format!("tracks/{}.json", request.track_id);
    reject_track_addition_collisions(&verified, request, &track_path)?;
    let mixer_descriptor = unique_resource(&verified, request.mixer_id, ResourceRole::Mixer)?;
    let previous_mixer = verified.read_resource(request.mixer_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current mixer resource",
            error,
        )
    })?;
    if previous_mixer == request.mixer_bytes {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track publication does not add a mixer channel",
        ));
    }
    validate_track_addition_document_identity(
        request,
        contract,
        Some(&verified),
        verified.manifest().project_id(),
        &previous_mixer,
    )?;

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    reject_unhandled_resource_transactions(&backups)?;
    let recovered_resources =
        recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered_resources.is_empty() || !recovered_tracks.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before audio-track publication",
            error,
        )
    })?;
    reject_published_track_path(&verified.root_dir, &track_path)?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before audio-track staging",
        ));
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let candidate_project =
        candidate_track_addition_manifest(&active_project, request, &track_path, next_revision)?;
    let transaction_name = format!("{AUDIO_TRACK_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create audio-track transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-track transaction",
                error,
            )
        })?;
    let result = run_audio_track_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        contract,
        mixer_descriptor.path(),
        &track_path,
        &previous_mixer,
        &active_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_audio_track_transaction(&backups, transaction, &transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio track was published but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_audio_track_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_audio_track_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio-track publication failed and its durable transaction could not be read"
                            .to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_audio_track_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(AudioTrackPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    Err(operation_error)
                }
                Ok(AudioTrackPublicationRecoveryDisposition::Committed) => {
                    let outcome = audio_track_outcome(&tx)?;
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )
                    .map_err(|error| ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio track was published but transaction cleanup is incomplete"
                            .to_owned(),
                        published: true,
                        source: Some(Box::new(error)),
                    })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(publication_recovery_failure(
                    "audio-track publication and recovery both failed",
                    "audio track was published but verification or transaction recovery is incomplete",
                    recovery_error,
                )),
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn remove_audio_track_observed(
    project_path: &Path,
    request: AudioTrackRemovalRequest<'_>,
    observer: impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
    remove_structural_track_observed(
        project_path,
        &StructuralTrackRemovalRequest::audio(request),
        observer,
    )
}

fn remove_routing_bus_observed(
    project_path: &Path,
    request: RoutingBusRemovalRequest<'_>,
    observer: impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<RoutingBusRemovalOutcome, ProjectResourcePublicationError> {
    remove_structural_track_observed(
        project_path,
        &StructuralTrackRemovalRequest::routing_bus(request),
        observer,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn remove_structural_track_observed(
    project_path: &Path,
    request: &StructuralTrackRemovalRequest<'_>,
    mut observer: impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
    validate_canonical_resource(request.mixer_bytes)?;
    if request.track_id.is_empty() || request.mixer_id.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track and mixer identities must be present",
        ));
    }
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before audio-track removal",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the audio-track removal began",
        ));
    }
    let track_descriptor = unique_resource(&verified, request.track_id, ResourceRole::Track)?;
    let mixer_descriptor = unique_resource(&verified, request.mixer_id, ResourceRole::Mixer)?;
    let track = verified.read_resource(request.track_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current audio-track resource",
            error,
        )
    })?;
    let previous_mixer = verified.read_resource(request.mixer_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current mixer resource",
            error,
        )
    })?;
    validate_audio_track_removal_documents(request, &track, &previous_mixer)?;

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered_resources =
        recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered_resources.is_empty() || !recovered_tracks.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before audio-track removal",
            error,
        )
    })?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before audio-track removal staging",
        ));
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let candidate_project = candidate_audio_track_removal_manifest(
        &active_project,
        request,
        track_descriptor.path(),
        next_revision,
    )?;
    let transaction_name = format!("{AUDIO_TRACK_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create audio-track removal transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-track removal transaction",
                error,
            )
        })?;
    let result = run_audio_track_removal(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        mixer_descriptor.path(),
        track_descriptor.path(),
        &track,
        &previous_mixer,
        &active_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_audio_track_transaction(&backups, transaction, &transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio track was removed but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_audio_track_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_audio_track_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio-track removal failed and its durable transaction could not be read"
                            .to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_audio_track_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(AudioTrackPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    Err(operation_error)
                }
                Ok(AudioTrackPublicationRecoveryDisposition::Committed) => {
                    let outcome = audio_track_removal_outcome(&tx)?;
                    clear_and_remove_audio_track_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )
                    .map_err(|error| ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio track was removed but transaction cleanup is incomplete"
                            .to_owned(),
                        published: true,
                        source: Some(Box::new(error)),
                    })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(publication_recovery_failure(
                    "audio-track removal and recovery both failed",
                    "audio track was removed but verification or transaction recovery is incomplete",
                    recovery_error,
                )),
            }
        }
    }
}

#[derive(Debug)]
struct TrackOrderActiveTrack {
    path: String,
    previous: Vec<u8>,
}

#[allow(
    clippy::too_many_lines,
    reason = "the complete batch authority, staging, publication, and recovery ordering must remain auditable together"
)]
fn publish_track_order_observed(
    project_path: &Path,
    request: TrackOrderPublicationRequest<'_>,
    mut observer: impl FnMut(
        TrackOrderPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<TrackOrderPublicationOutcome, ProjectResourcePublicationError> {
    validate_track_order_request(request)?;
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before track-order publication",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the track reorder began",
        ));
    }
    let mixer_descriptor = unique_resource(&verified, request.mixer_id, ResourceRole::Mixer)?;
    let previous_mixer = verified.read_resource(request.mixer_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current mixer resource",
            error,
        )
    })?;
    let tracks = resolve_track_order_tracks(&verified, request)?;
    validate_track_order_document_identity(&verified, request, &tracks, &previous_mixer)?;
    if previous_mixer == request.documents.mixer()
        && tracks
            .iter()
            .zip(request.documents.tracks())
            .all(|(active, replacement)| active.previous == replacement.bytes())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track reorder does not change the active project order",
        ));
    }

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered_resources =
        recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_orders =
        recover_track_order_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered_resources.is_empty()
        || !recovered_tracks.is_empty()
        || !recovered_orders.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    reject_unhandled_resource_transactions(&backups)?;
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before track-order publication",
            error,
        )
    })?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before track-order staging",
        ));
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let replacement_tracks: Vec<_> = request
        .documents
        .tracks()
        .iter()
        .map(|track| TrackOrderPublicationTrack {
            track_id: track.track_id(),
            track_bytes: track.bytes(),
        })
        .collect();
    let candidate_project = candidate_track_order_manifest(
        &active_project,
        request.mixer_id,
        request.documents.mixer(),
        &replacement_tracks,
        next_revision,
    )?;
    let transaction_name = format!("{TRACK_ORDER_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create track-order transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open track-order transaction",
                error,
            )
        })?;
    let result = run_track_order_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        &tracks,
        mixer_descriptor.path(),
        &previous_mixer,
        &active_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_track_order_transaction(&backups, transaction, &transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "track order was published but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_track_order_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_track_order_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_track_order_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "track-order publication failed and its durable transaction could not be read".to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_track_order_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(TrackOrderPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_track_order_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    Err(operation_error)
                }
                Ok(TrackOrderPublicationRecoveryDisposition::Committed) => {
                    let outcome = track_order_outcome(&tx)?;
                    clear_and_remove_track_order_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )
                    .map_err(|error| ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "track order was published but transaction cleanup is incomplete"
                            .to_owned(),
                        published: true,
                        source: Some(Box::new(error)),
                    })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(publication_recovery_failure(
                    "track-order publication and recovery both failed",
                    "track order was published but verification or transaction recovery is incomplete",
                    recovery_error,
                )),
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the publication coordinator keeps lock, recovery, staging, and disposition ordering visible in one fail-closed sequence"
)]
fn publish_project_resource_observed(
    project_path: &Path,
    request: ProjectResourcePublicationRequest<'_>,
    mode: ProjectResourcePublicationMode,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    if !mode.is_removal() {
        validate_canonical_resource(request.resource_bytes)?;
    }
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before resource publication",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the resource edit began",
        ));
    }
    match mode {
        ProjectResourcePublicationMode::Replacement => {
            if request.expected_role == ResourceRole::DeviceChain {
                validate_device_chain_document(&verified, request)?;
            }
        }
        ProjectResourcePublicationMode::ArrangementAddition => {
            validate_arrangement_addition_request(&verified, request)?;
        }
        ProjectResourcePublicationMode::SceneAddition => {
            validate_scene_addition_request(&verified, request)?;
        }
        ProjectResourcePublicationMode::SceneRemoval => {
            unique_resource(&verified, request.resource_id, ResourceRole::Scene)?;
        }
        ProjectResourcePublicationMode::AutomationTakeAddition => {
            validate_automation_take_addition_request(&verified, request)?;
        }
        ProjectResourcePublicationMode::AutomationTakeRemoval => {
            unique_resource(&verified, request.resource_id, ResourceRole::AutomationTake)?;
        }
        ProjectResourcePublicationMode::DeviceChainAddition => {
            validate_device_chain_addition_request(&verified, request)?;
        }
        ProjectResourcePublicationMode::DeviceChainRemoval => {
            unique_resource(&verified, request.resource_id, ResourceRole::DeviceChain)?;
        }
    }
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    reject_unhandled_resource_transactions(&backups)?;
    let recovered = recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() || !recovered_tracks.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before resource publication",
            error,
        )
    })?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before resource staging",
        ));
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let (resource_path, previous, created_parent_directory, candidate_project) = match mode {
        ProjectResourcePublicationMode::Replacement => {
            let descriptor =
                unique_resource(&verified, request.resource_id, request.expected_role)?;
            let previous = verified
                .read_resource(request.resource_id)
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "read current project resource",
                        error,
                    )
                })?;
            if previous == request.resource_bytes {
                return Ok(ProjectResourcePublicationOutcome {
                    project_identity: request.expected_project_sha256,
                    project_revision: verified.manifest().revision(),
                    resource_id: request.resource_id.to_owned(),
                    resource_path: descriptor.path().to_owned(),
                    changed: false,
                    previous_resource: None,
                });
            }
            let candidate = candidate_manifest(
                &active_project,
                request.resource_id,
                request.resource_bytes,
                next_revision,
            )?;
            (
                descriptor.path().to_owned(),
                Some(previous),
                false,
                candidate,
            )
        }
        ProjectResourcePublicationMode::ArrangementAddition => {
            let path = format!("arrangement/{}.json", request.resource_id);
            let created_parent_directory = inspect_addition_destination(
                &verified.root_dir,
                "arrangement",
                &path,
                "arrangement",
            )?;
            let candidate =
                candidate_addition_manifest(&active_project, request, &path, next_revision)?;
            (path, None, created_parent_directory, candidate)
        }
        ProjectResourcePublicationMode::SceneRemoval
        | ProjectResourcePublicationMode::AutomationTakeRemoval
        | ProjectResourcePublicationMode::DeviceChainRemoval => {
            let descriptor =
                unique_resource(&verified, request.resource_id, request.expected_role)?;
            let previous = verified
                .read_resource(request.resource_id)
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "read current removable project resource",
                        error,
                    )
                })?;
            let candidate =
                candidate_removal_manifest(&active_project, request.resource_id, next_revision)?;
            let removed_parent_directory = verified
                .manifest()
                .resources()
                .iter()
                .filter(|resource| resource.role() == request.expected_role)
                .count()
                == 1;
            (
                descriptor.path().to_owned(),
                Some(previous),
                removed_parent_directory,
                candidate,
            )
        }
        ProjectResourcePublicationMode::SceneAddition => {
            let path = format!("scenes/{}.json", request.resource_id);
            let created_parent_directory =
                inspect_addition_destination(&verified.root_dir, "scenes", &path, "scene")?;
            let candidate =
                candidate_addition_manifest(&active_project, request, &path, next_revision)?;
            (path, None, created_parent_directory, candidate)
        }
        ProjectResourcePublicationMode::AutomationTakeAddition => {
            let path = format!("takes/{}.json", request.resource_id);
            let created_parent_directory = inspect_addition_destination(
                &verified.root_dir,
                "takes",
                &path,
                "automation take",
            )?;
            let candidate =
                candidate_addition_manifest(&active_project, request, &path, next_revision)?;
            (path, None, created_parent_directory, candidate)
        }
        ProjectResourcePublicationMode::DeviceChainAddition => {
            let path = format!("devices/{}.json", request.resource_id);
            let created_parent_directory =
                inspect_addition_destination(&verified.root_dir, "devices", &path, "device chain")?;
            let candidate =
                candidate_addition_manifest(&active_project, request, &path, next_revision)?;
            (path, None, created_parent_directory, candidate)
        }
    };
    let transaction_name = format!("{TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create resource transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open resource transaction",
                error,
            )
        })?;
    let result = if mode.is_removal() {
        run_resource_removal(
            &verified,
            &backups,
            &transaction,
            &transaction_name,
            request,
            &resource_path,
            previous.as_deref().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "resource removal lost its previous resource evidence",
                )
            })?,
            created_parent_directory,
            &active_project,
            &candidate_project,
            next_revision,
            &mut observer,
        )
    } else {
        run_publication(
            &verified,
            &backups,
            &transaction,
            &transaction_name,
            request,
            &resource_path,
            previous.as_deref(),
            created_parent_directory,
            &active_project,
            &candidate_project,
            next_revision,
            &mut observer,
        )
    };
    match result {
        Ok(mut outcome) => {
            clear_and_remove_transaction(&backups, transaction, &transaction_name).map_err(
                |error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "resource change committed but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                },
            )?;
            outcome.previous_resource = previous;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message:
                            "resource change failed and its durable transaction could not be read"
                                .to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(ProjectResourcePublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)?;
                    Err(operation_error)
                }
                Ok(ProjectResourcePublicationRecoveryDisposition::Committed) => {
                    let mut outcome = outcome_from_transaction(&tx, true)?;
                    outcome.previous_resource = previous;
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)
                        .map_err(|error| ProjectResourcePublicationError {
                            kind: ProjectResourcePublicationErrorKind::Recovery,
                            message:
                                "resource change committed but transaction cleanup is incomplete"
                                    .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(publication_recovery_failure(
                    "resource change and recovery both failed",
                    "resource change committed but verification or transaction recovery is incomplete",
                    recovery_error,
                )),
            }
        }
    }
}

fn reject_unhandled_resource_transactions(
    backups: &Dir,
) -> Result<(), ProjectResourcePublicationError> {
    let unhandled = find_unhandled_project_transaction(
        backups,
        &[
            ProjectTransactionFamily::ProjectResource,
            ProjectTransactionFamily::AudioTrack,
        ],
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "inspect project transactions before resource publication",
            error,
        )
    })?;
    if unhandled.is_some() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable write order and every crash observation point remain visible in one coordinator"
)]
fn run_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: ProjectResourcePublicationRequest<'_>,
    resource_path: &str,
    previous_resource: Option<&[u8]>,
    created_parent_directory: bool,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, RESOURCE_STAGING_NAME, request.resource_bytes)?;
    if let Some(previous_resource) = previous_resource {
        write_exclusive_synced(transaction, PREVIOUS_RESOURCE_NAME, previous_resource)?;
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = TransactionRecord {
        format: "hyper-project-resource-publication-transaction".to_owned(),
        format_version: 1,
        id: transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        resource_id: request.resource_id.to_owned(),
        resource_role: role_name(request.expected_role).to_owned(),
        resource: TransactionFile {
            path: resource_path.to_owned(),
            byte_length: request.resource_bytes.len() as u64,
            sha256: hex_digest(digest(request.resource_bytes)),
        },
        previous_resource: previous_resource.map(|bytes| TransactionFile {
            path: resource_path.to_owned(),
            byte_length: bytes.len() as u64,
            sha256: hex_digest(digest(bytes)),
        }),
        created_parent_directory,
        removed_parent_directory: false,
        operation: TransactionOperation::Publication,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize resource transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;

    if created_parent_directory {
        let directory = addition_directory(request.expected_role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "resource addition directory is inconsistent",
            )
        })?;
        verified.root_dir.create_dir(directory).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "create resource directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
    }
    let parent = open_relative_parent(&verified.root_dir, resource_path)?;
    publish_staged_file(
        transaction,
        RESOURCE_STAGING_NAME,
        &parent,
        &parent,
        file_name(resource_path)?,
        &format!(".resource-{}.json", tx.id),
        "project resource",
    )?;
    observer(PublicationPhase::Resource, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "resource publication candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "resource publication candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".project-{}.json", tx.id),
        "project manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    outcome_from_transaction(&tx, true)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable removal order and every crash observation point remain explicit in one coordinator"
)]
fn run_resource_removal(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: ProjectResourcePublicationRequest<'_>,
    resource_path: &str,
    previous_resource: &[u8],
    removed_parent_directory: bool,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, RESOURCE_STAGING_NAME, previous_resource)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = TransactionRecord {
        format: "hyper-project-resource-publication-transaction".to_owned(),
        format_version: 1,
        id: transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        resource_id: request.resource_id.to_owned(),
        resource_role: role_name(request.expected_role).to_owned(),
        resource: TransactionFile {
            path: resource_path.to_owned(),
            byte_length: previous_resource.len() as u64,
            sha256: hex_digest(digest(previous_resource)),
        },
        previous_resource: None,
        created_parent_directory: false,
        removed_parent_directory,
        operation: TransactionOperation::Removal,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize resource-removal transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;

    let parent = open_relative_parent(&verified.root_dir, resource_path)?;
    let active = read_bounded(&parent, file_name(resource_path)?, MAX_JSON_BYTES)?;
    if digest(&active) != digest(previous_resource) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project resource changed before removal",
        ));
    }
    parent
        .remove_file(file_name(resource_path)?)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "remove project resource",
                error,
            )
        })?;
    sync_dir(&parent)?;
    observer(PublicationPhase::ResourceFile, verified.root())?;
    if removed_parent_directory {
        drop(parent);
        let directory = addition_directory(request.expected_role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "removed project resource has no managed directory",
            )
        })?;
        verified.root_dir.remove_dir(directory).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "remove empty project-resource directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
    }
    observer(PublicationPhase::Resource, verified.root())?;

    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "resource-removal candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "resource-removal candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".project-{}.json", tx.id),
        "project manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    finish_removed_resource(&verified.root_dir, &tx)?;
    outcome_from_transaction(&tx, true)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable write order and every crash observation point remain visible in one coordinator"
)]
fn run_audio_track_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: TrackAdditionPublicationRequest<'_>,
    contract: TrackAdditionPublicationContract<'_>,
    mixer_path: &str,
    track_path: &str,
    previous_mixer: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    write_audio_track_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, AUDIO_TRACK_STAGING_NAME, request.track_bytes)?;
    write_exclusive_synced(
        transaction,
        AUDIO_TRACK_MIXER_STAGING_NAME,
        request.mixer_bytes,
    )?;
    write_exclusive_synced(transaction, AUDIO_TRACK_PREVIOUS_MIXER_NAME, previous_mixer)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = AudioTrackTransactionRecord {
        format: "hyper-audio-track-publication-transaction".to_owned(),
        format_version: 1,
        id: audio_track_transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        track_id: request.track_id.to_owned(),
        mixer_id: request.mixer_id.to_owned(),
        track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: request.track_bytes.len() as u64,
            sha256: hex_digest(digest(request.track_bytes)),
        },
        mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: request.mixer_bytes.len() as u64,
            sha256: hex_digest(digest(request.mixer_bytes)),
        },
        previous_mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: previous_mixer.len() as u64,
            sha256: hex_digest(digest(previous_mixer)),
        },
        contract: transaction_contract(contract),
        operation: AudioTrackTransactionOperation::Publication,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-track transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(AudioTrackPublicationPhase::Transaction, verified.root())?;

    let track_parent = open_relative_parent(&verified.root_dir, track_path)?;
    transaction
        .hard_link(
            AUDIO_TRACK_STAGING_NAME,
            &track_parent,
            file_name(track_path)?,
        )
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "publish new audio-track resource without overwriting",
                error,
            )
        })?;
    sync_dir(&track_parent)?;
    observer(AudioTrackPublicationPhase::Track, verified.root())?;

    let mixer_parent = open_relative_parent(&verified.root_dir, mixer_path)?;
    publish_staged_file(
        transaction,
        AUDIO_TRACK_MIXER_STAGING_NAME,
        &mixer_parent,
        &mixer_parent,
        file_name(mixer_path)?,
        &format!(".audio-track-mixer-{}.json", tx.id),
        "audio-track mixer",
    )?;
    observer(AudioTrackPublicationPhase::Mixer, verified.root())?;

    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-track publication candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-track publication candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".audio-track-project-{}.json", tx.id),
        "audio-track project manifest",
    )?;
    observer(AudioTrackPublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    audio_track_outcome(&tx)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable write order and every crash observation point remain visible in one coordinator"
)]
fn run_audio_track_removal(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: &StructuralTrackRemovalRequest<'_>,
    mixer_path: &str,
    track_path: &str,
    track: &[u8],
    previous_mixer: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        AudioTrackPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
    write_audio_track_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, AUDIO_TRACK_STAGING_NAME, track)?;
    write_exclusive_synced(
        transaction,
        AUDIO_TRACK_MIXER_STAGING_NAME,
        request.mixer_bytes,
    )?;
    write_exclusive_synced(transaction, AUDIO_TRACK_PREVIOUS_MIXER_NAME, previous_mixer)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = AudioTrackTransactionRecord {
        format: "hyper-audio-track-removal-transaction".to_owned(),
        format_version: 1,
        id: audio_track_transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        track_id: request.track_id.to_owned(),
        mixer_id: request.mixer_id.to_owned(),
        track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: track.len() as u64,
            sha256: hex_digest(digest(track)),
        },
        mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: request.mixer_bytes.len() as u64,
            sha256: hex_digest(digest(request.mixer_bytes)),
        },
        previous_mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: previous_mixer.len() as u64,
            sha256: hex_digest(digest(previous_mixer)),
        },
        contract: request.contract.clone(),
        operation: AudioTrackTransactionOperation::Removal,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-track removal transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(AudioTrackPublicationPhase::Transaction, verified.root())?;

    let mixer_parent = open_relative_parent(&verified.root_dir, mixer_path)?;
    publish_staged_file(
        transaction,
        AUDIO_TRACK_MIXER_STAGING_NAME,
        &mixer_parent,
        &mixer_parent,
        file_name(mixer_path)?,
        &format!(".audio-track-mixer-{}.json", tx.id),
        "audio-track removal mixer",
    )?;
    observer(AudioTrackPublicationPhase::Mixer, verified.root())?;

    remove_published_audio_track(&verified.root_dir, &tx)?;
    observer(AudioTrackPublicationPhase::Track, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-track removal candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-track removal candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".audio-track-project-{}.json", tx.id),
        "audio-track removal project manifest",
    )?;
    observer(AudioTrackPublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    audio_track_removal_outcome(&tx)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable batch write order and every crash observation point remain visible in one coordinator"
)]
fn run_track_order_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: TrackOrderPublicationRequest<'_>,
    active_tracks: &[TrackOrderActiveTrack],
    mixer_path: &str,
    previous_mixer: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        TrackOrderPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<TrackOrderPublicationOutcome, ProjectResourcePublicationError> {
    write_track_order_intent(
        transaction,
        transaction_name,
        request.documents.tracks().len(),
    )?;
    for (index, (active, replacement)) in active_tracks
        .iter()
        .zip(request.documents.tracks())
        .enumerate()
    {
        write_exclusive_synced(
            transaction,
            &track_order_staging_name(index),
            replacement.bytes(),
        )?;
        write_exclusive_synced(
            transaction,
            &track_order_previous_staging_name(index),
            &active.previous,
        )?;
    }
    write_exclusive_synced(
        transaction,
        TRACK_ORDER_MIXER_STAGING_NAME,
        request.documents.mixer(),
    )?;
    write_exclusive_synced(transaction, TRACK_ORDER_PREVIOUS_MIXER_NAME, previous_mixer)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(request.documents.tracks().len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate track-order transaction records",
            )
        })?;
    for (active, replacement) in active_tracks.iter().zip(request.documents.tracks()) {
        tracks.push(TrackOrderTransactionTrack {
            track_id: replacement.track_id().to_owned(),
            track: TransactionFile {
                path: active.path.clone(),
                byte_length: replacement.bytes().len() as u64,
                sha256: hex_digest(digest(replacement.bytes())),
            },
            previous_track: TransactionFile {
                path: active.path.clone(),
                byte_length: active.previous.len() as u64,
                sha256: hex_digest(digest(&active.previous)),
            },
        });
    }
    let tx = TrackOrderTransactionRecord {
        format: "hyper-track-order-publication-transaction".to_owned(),
        format_version: 1,
        id: track_order_transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        mixer_id: request.mixer_id.to_owned(),
        mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: request.documents.mixer().len() as u64,
            sha256: hex_digest(digest(request.documents.mixer())),
        },
        previous_mixer: TransactionFile {
            path: mixer_path.to_owned(),
            byte_length: previous_mixer.len() as u64,
            sha256: hex_digest(digest(previous_mixer)),
        },
        tracks,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-order transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(TrackOrderPublicationPhase::Transaction, verified.root())?;

    for (index, track) in tx.tracks.iter().enumerate() {
        let parent = open_relative_parent(&verified.root_dir, &track.track.path)?;
        publish_staged_file(
            transaction,
            &track_order_staging_name(index),
            &parent,
            &parent,
            file_name(&track.track.path)?,
            &format!(".track-order-track-{index}-{}.json", tx.id),
            "track-order track",
        )?;
        observer(TrackOrderPublicationPhase::Track(index), verified.root())?;
    }
    let mixer_parent = open_relative_parent(&verified.root_dir, mixer_path)?;
    publish_staged_file(
        transaction,
        TRACK_ORDER_MIXER_STAGING_NAME,
        &mixer_parent,
        &mixer_parent,
        file_name(mixer_path)?,
        &format!(".track-order-mixer-{}.json", tx.id),
        "track-order mixer",
    )?;
    observer(TrackOrderPublicationPhase::Mixer, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-order publication candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-order publication candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".track-order-project-{}.json", tx.id),
        "track-order project manifest",
    )?;
    observer(TrackOrderPublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    track_order_outcome(&tx)
}

fn validate_track_addition_documents(
    request: TrackAdditionPublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    validate_canonical_resource(request.track_bytes)?;
    validate_canonical_resource(request.mixer_bytes)?;
    if request.track_id.is_empty() || request.mixer_id.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track and mixer identities must be present",
        ));
    }
    Ok(())
}

fn validate_arrangement_addition_request(
    verified: &StructurallyVerifiedProject,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    if request.expected_role != ResourceRole::Arrangement
        || !is_valid_entity_id(request.resource_id)
        || request.resource_id == verified.manifest().project_id()
        || verified.manifest().resources().iter().any(|descriptor| {
            descriptor.role() == ResourceRole::Arrangement || descriptor.id() == request.resource_id
        })
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "arrangement identity conflicts with the active project",
        ));
    }
    let arrangement: Value = serde_json::from_slice(request.resource_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new arrangement resource",
            error,
        )
    })?;
    if arrangement["format"].as_str() != Some("hyper-arrangement")
        || arrangement["format_version"].as_u64() != Some(1)
        || arrangement["id"].as_str() != Some(request.resource_id)
        || arrangement["project_id"].as_str() != Some(verified.manifest().project_id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "arrangement resource does not match the active project",
        ));
    }
    Ok(())
}

fn validate_scene_addition_request(
    verified: &StructurallyVerifiedProject,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    if request.expected_role != ResourceRole::Scene
        || !is_valid_entity_id(request.resource_id)
        || request.resource_id == verified.manifest().project_id()
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "scene identity conflicts with the active project",
        ));
    }
    let scene: Value = serde_json::from_slice(request.resource_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new scene resource",
            error,
        )
    })?;
    if scene["format"].as_str() != Some("hyper-scene")
        || scene["format_version"].as_u64() != Some(1)
        || scene["id"].as_str() != Some(request.resource_id)
        || scene["project_id"].as_str() != Some(verified.manifest().project_id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "scene resource does not match the active project",
        ));
    }
    Ok(())
}

fn validate_automation_take_addition_request(
    verified: &StructurallyVerifiedProject,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    if request.expected_role != ResourceRole::AutomationTake
        || !is_valid_entity_id(request.resource_id)
        || request.resource_id == verified.manifest().project_id()
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "automation take identity conflicts with the active project",
        ));
    }
    let take: Value = serde_json::from_slice(request.resource_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new automation take resource",
            error,
        )
    })?;
    if take["format"].as_str() != Some("hyper-automation-take")
        || take["format_version"].as_u64() != Some(1)
        || take["id"].as_str() != Some(request.resource_id)
        || take["project_id"].as_str() != Some(verified.manifest().project_id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "automation take resource does not match the active project",
        ));
    }
    Ok(())
}

fn validate_device_chain_addition_request(
    verified: &StructurallyVerifiedProject,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    if request.expected_role != ResourceRole::DeviceChain
        || !is_valid_entity_id(request.resource_id)
        || request.resource_id == verified.manifest().project_id()
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|descriptor| descriptor.id() == request.resource_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "device-chain identity conflicts with the active project",
        ));
    }
    validate_device_chain_document(verified, request)
}

fn validate_device_chain_document(
    verified: &StructurallyVerifiedProject,
    request: ProjectResourcePublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    let chain = DeviceChain::parse(request.resource_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new device-chain resource",
            error,
        )
    })?;
    if chain.id() != request.resource_id || chain.project_id() != verified.manifest().project_id() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "device-chain resource does not match the active project",
        ));
    }
    unique_resource(verified, chain.track_id(), ResourceRole::Track).map_err(|error| {
        if error.kind() == ProjectResourcePublicationErrorKind::ResourceConflict {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "device-chain track ownership does not match the active project",
            )
        } else {
            error
        }
    })?;
    validate_device_chain_inventory(verified, request.resource_id, &chain)?;
    Ok(())
}

fn validate_device_chain_inventory(
    verified: &StructurallyVerifiedProject,
    replacing_resource_id: &str,
    candidate: &DeviceChain,
) -> Result<(), ProjectResourcePublicationError> {
    let manifest_ids = verified
        .manifest()
        .resources()
        .iter()
        .map(|resource| resource.id().to_owned())
        .chain(
            verified
                .manifest()
                .assets()
                .iter()
                .map(|asset| asset.id().to_owned()),
        )
        .collect::<BTreeSet<_>>();
    let mut device_ids = BTreeSet::new();
    for descriptor in verified
        .manifest()
        .resources()
        .iter()
        .filter(|descriptor| descriptor.role() == ResourceRole::DeviceChain)
        .filter(|descriptor| descriptor.id() != replacing_resource_id)
    {
        let bytes = verified.read_resource(descriptor.id()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "read active device-chain resource",
                error,
            )
        })?;
        let chain = DeviceChain::parse(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "parse active device-chain resource",
                error,
            )
        })?;
        if chain.track_id() == candidate.track_id() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "device-chain track already has an active chain",
            ));
        }
        device_ids.extend(chain.devices().iter().map(|device| device.id().to_owned()));
    }
    if candidate.devices().iter().any(|device| {
        manifest_ids.contains(device.id()) || !device_ids.insert(device.id().to_owned())
    }) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "device-chain identity collides with active project content",
        ));
    }
    Ok(())
}

fn inspect_addition_destination(
    root: &Dir,
    directory_name: &str,
    resource_path: &str,
    resource_label: &str,
) -> Result<bool, ProjectResourcePublicationError> {
    let directory = match root.symlink_metadata(directory_name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect resource directory",
                error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                format!("{resource_label} resource directory is not a real directory"),
            ));
        }
        Ok(_) => root.open_dir_nofollow(directory_name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open resource directory",
                error,
            )
        })?,
    };
    let name = file_name(resource_path)?;
    match directory.symlink_metadata(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            format!("{resource_label} resource path already exists"),
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect arrangement resource destination",
            error,
        )),
    }
}

fn validate_track_addition_document_identity(
    request: TrackAdditionPublicationRequest<'_>,
    contract: TrackAdditionPublicationContract<'_>,
    verified: Option<&StructurallyVerifiedProject>,
    project_id: &str,
    previous_mixer: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    match contract {
        TrackAdditionPublicationContract::Audio => validate_audio_track_document_identity(
            AudioTrackPublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                track_id: request.track_id,
                track_bytes: request.track_bytes,
                mixer_id: request.mixer_id,
                mixer_bytes: request.mixer_bytes,
            },
            project_id,
            previous_mixer,
        ),
        TrackAdditionPublicationContract::Routing {
            kind,
            source_track_ids,
            send_ids,
        } => validate_routing_bus_document_identity(
            request,
            kind,
            source_track_ids,
            send_ids,
            verified,
            project_id,
            previous_mixer,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the persistence boundary proves the exact source-to-bus graph before it stages any filesystem changes"
)]
fn validate_routing_bus_document_identity(
    request: TrackAdditionPublicationRequest<'_>,
    kind: RoutingBusPublicationKind,
    source_track_ids: &[String],
    send_ids: &[String],
    verified: Option<&StructurallyVerifiedProject>,
    project_id: &str,
    previous_mixer: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let track: Value = serde_json::from_slice(request.track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new routing-bus resource",
            error,
        )
    })?;
    let mixer: Value = serde_json::from_slice(request.mixer_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse candidate routing-bus mixer",
            error,
        )
    })?;
    let previous: Value = serde_json::from_slice(previous_mixer).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current mixer resource",
            error,
        )
    })?;
    let track_kind = match kind {
        RoutingBusPublicationKind::Group => "group",
        RoutingBusPublicationKind::Return => "return",
    };
    if track["format"].as_str() != Some("hyper-track")
        || track["format_version"].as_u64() != Some(1)
        || track["id"].as_str() != Some(request.track_id)
        || track["project_id"].as_str() != Some(project_id)
        || track["kind"].as_str() != Some(track_kind)
        || track["active"].as_bool() != Some(true)
        || track["clip_ids"]
            .as_array()
            .is_none_or(|clips| !clips.is_empty())
        || mixer["format"].as_str() != Some("hyper-mixer")
        || mixer["format_version"].as_u64() != Some(1)
        || mixer["id"].as_str() != Some(request.mixer_id)
        || mixer["project_id"].as_str() != Some(project_id)
        || previous["format"].as_str() != Some("hyper-mixer")
        || previous["format_version"].as_u64() != Some(1)
        || previous["id"].as_str() != Some(request.mixer_id)
        || previous["project_id"].as_str() != Some(project_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "routing-bus resources do not match the active project",
        ));
    }
    let mut sources = BTreeSet::new();
    if source_track_ids.is_empty()
        || source_track_ids
            .iter()
            .any(|id| !is_valid_entity_id(id) || !sources.insert(id.as_str()))
        || (kind == RoutingBusPublicationKind::Group && !send_ids.is_empty())
        || (kind == RoutingBusPublicationKind::Return
            && (send_ids.len() != source_track_ids.len()
                || send_ids.iter().any(|id| !is_valid_entity_id(id))))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "routing-bus source or send identities are invalid",
        ));
    }
    if let Some(verified) = verified {
        if kind == RoutingBusPublicationKind::Return
            && send_ids.iter().any(|send_id| {
                send_id == project_id
                    || verified
                        .manifest()
                        .resources()
                        .iter()
                        .any(|resource| resource.id() == send_id)
                    || verified
                        .manifest()
                        .assets()
                        .iter()
                        .any(|asset| asset.id() == send_id)
            })
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "routing-bus send identity conflicts with active project content",
            ));
        }
        for source_track_id in &sources {
            let descriptor = unique_resource(verified, source_track_id, ResourceRole::Track)?;
            let source = verified.read_resource(descriptor.id()).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "read routing-bus source track",
                    error,
                )
            })?;
            let source: Value = serde_json::from_slice(&source).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "parse routing-bus source track",
                    error,
                )
            })?;
            if source["kind"].as_str() != Some("audio") || source["active"].as_bool() != Some(true)
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "routing-bus sources must be active audio tracks",
                ));
            }
        }
    }
    let previous_channels = previous["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current mixer channel collection is invalid",
        )
    })?;
    let mut expected = previous.clone();
    if previous_channels.len().checked_add(1)
        != Some(mixer["channels"].as_array().map_or(usize::MAX, Vec::len))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "routing-bus candidate must append exactly one matching channel",
        ));
    }

    match kind {
        RoutingBusPublicationKind::Group => {
            let expected_channels = expected["channels"].as_array_mut().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "current mixer channel collection changed during validation",
                )
            })?;
            let mut matched = BTreeSet::new();
            for channel in expected_channels.iter_mut().filter(|channel| {
                channel["track_id"]
                    .as_str()
                    .is_some_and(|track_id| sources.contains(track_id))
            }) {
                matched.insert(channel["track_id"].as_str().unwrap_or_default().to_owned());
                channel["destination"] =
                    serde_json::json!({"kind": "track", "track_id": request.track_id});
            }
            if sources.iter().any(|source| !matched.contains(*source)) {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "routing-bus source channel is missing",
                ));
            }
        }
        RoutingBusPublicationKind::Return => {
            let return_bus = {
                let return_buses = expected["return_buses"].as_array().ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "current mixer return buses are invalid",
                    )
                })?;
                let used_buses = return_buses
                    .iter()
                    .map(|entry| entry["bus"].as_str())
                    .collect::<Option<BTreeSet<_>>>()
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Integrity,
                            "current mixer return bus identity is invalid",
                        )
                    })?;
                ["a", "b"]
                    .into_iter()
                    .find(|bus| !used_buses.contains(bus))
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::InvalidResource,
                            "routing-bus candidate has no available return bus",
                        )
                    })?
            };
            let expected_channels = expected["channels"].as_array_mut().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "current mixer channel collection changed during validation",
                )
            })?;
            let mut all_send_ids = BTreeSet::new();
            for channel in expected_channels.iter() {
                for send in channel["sends"].as_array().ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "current mixer sends are invalid",
                    )
                })? {
                    let id = send["id"].as_str().ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Integrity,
                            "current mixer send identity is invalid",
                        )
                    })?;
                    all_send_ids.insert(id);
                }
            }
            if send_ids
                .iter()
                .any(|id| id == request.track_id || id == project_id || !all_send_ids.insert(id))
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "routing-bus send identity conflicts with the active mixer",
                ));
            }
            for (source_track_id, send_id) in source_track_ids.iter().zip(send_ids) {
                let channel = expected_channels
                    .iter_mut()
                    .find(|channel| channel["track_id"].as_str() == Some(source_track_id))
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::InvalidResource,
                            "routing-bus source channel is missing",
                        )
                    })?;
                let sends = channel["sends"].as_array_mut().ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "current mixer sends are invalid",
                    )
                })?;
                if sends.len() >= 2 {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::InvalidResource,
                        "routing-bus source has no remaining send capacity",
                    ));
                }
                sends.push(serde_json::json!({
                    "id": send_id,
                    "bus": return_bus,
                    "tap": "post-fader",
                    "gain_millionths": 1_000_000,
                    "pan_millionths": 0,
                    "enabled": true
                }));
                sends.sort_by_key(|entry| entry["bus"].as_str().unwrap_or_default().to_owned());
            }
            let return_buses = expected["return_buses"].as_array_mut().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "current mixer return buses changed during validation",
                )
            })?;
            return_buses.push(serde_json::json!({"bus": return_bus, "track_id": request.track_id}));
            return_buses.sort_by_key(|entry| entry["bus"].as_str().unwrap_or_default().to_owned());
        }
    }
    expected["channels"]
        .as_array_mut()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current mixer channel collection changed during validation",
            )
        })?
        .push(serde_json::json!({
            "track_id": request.track_id,
            "gain_millionths": 1_000_000,
            "pan_millionths": 0,
            "mute": false,
            "solo": false,
            "destination": {"kind": "main"},
            "sends": []
        }));
    if expected != mixer {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "routing-bus candidate contains changes outside its declared routing operation",
        ));
    }
    Ok(())
}

fn validate_audio_track_document_identity(
    request: AudioTrackPublicationRequest<'_>,
    project_id: &str,
    previous_mixer: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let track: Value = serde_json::from_slice(request.track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new audio-track resource",
            error,
        )
    })?;
    let mixer: Value = serde_json::from_slice(request.mixer_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse candidate mixer resource",
            error,
        )
    })?;
    let previous: Value = serde_json::from_slice(previous_mixer).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current mixer resource",
            error,
        )
    })?;
    if track["format"].as_str() != Some("hyper-track")
        || track["format_version"].as_u64() != Some(1)
        || track["id"].as_str() != Some(request.track_id)
        || track["project_id"].as_str() != Some(project_id)
        || track["kind"].as_str() != Some("audio")
        || track["active"].as_bool() != Some(true)
        || track["clip_ids"]
            .as_array()
            .is_none_or(|clips| !clips.is_empty())
        || mixer["format"].as_str() != Some("hyper-mixer")
        || mixer["format_version"].as_u64() != Some(1)
        || mixer["id"].as_str() != Some(request.mixer_id)
        || mixer["project_id"].as_str() != Some(project_id)
        || previous["format"].as_str() != Some("hyper-mixer")
        || previous["id"].as_str() != Some(request.mixer_id)
        || previous["project_id"].as_str() != Some(project_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track resources do not match the active project",
        ));
    }
    let previous_channels = previous["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current mixer channel collection is invalid",
        )
    })?;
    let channels = mixer["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate mixer channel collection is invalid",
        )
    })?;
    if previous_channels.len().checked_add(1) != Some(channels.len())
        || channels
            .last()
            .and_then(|channel| channel["track_id"].as_str())
            != Some(request.track_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate mixer must append exactly one matching audio-track channel",
        ));
    }
    let mut preserved = mixer.clone();
    preserved["channels"]
        .as_array_mut()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "candidate mixer channel collection changed during validation",
            )
        })?
        .pop();
    if preserved != previous
        || channels.last()
            != Some(&serde_json::json!({
                "track_id": request.track_id,
                "gain_millionths": 1_000_000,
                "pan_millionths": 0,
                "mute": false,
                "solo": false,
                "destination": {"kind": "main"},
                "sends": []
            }))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track publication must preserve the mixer and append one default main channel",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the exact track and mixer removal shape is validated together before any transaction staging"
)]
fn validate_audio_track_removal_documents(
    request: &StructuralTrackRemovalRequest<'_>,
    track_bytes: &[u8],
    previous_mixer_bytes: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    validate_canonical_resource(track_bytes)?;
    let track: Value = serde_json::from_slice(track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse removed audio-track resource",
            error,
        )
    })?;
    let previous: Value = serde_json::from_slice(previous_mixer_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current mixer resource",
            error,
        )
    })?;
    let candidate: Value = serde_json::from_slice(request.mixer_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse audio-track removal mixer",
            error,
        )
    })?;
    let (expected_kind, removes_return_mapping, requires_empty_owned_sends) =
        match &request.contract {
            AudioTrackTransactionContract::Audio => ("audio", false, false),
            AudioTrackTransactionContract::RoutingGroup { source_track_ids }
                if source_track_ids.is_empty() =>
            {
                ("group", false, true)
            }
            AudioTrackTransactionContract::RoutingReturn {
                source_track_ids,
                send_ids,
            } if source_track_ids.is_empty() && send_ids.is_empty() => ("return", true, false),
            _ => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "structural track-removal contract is invalid",
                ));
            }
        };
    if track["format"].as_str() != Some("hyper-track")
        || track["id"].as_str() != Some(request.track_id)
        || track["kind"].as_str() != Some(expected_kind)
        || track["clip_ids"]
            .as_array()
            .is_none_or(|clips| !clips.is_empty())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "structural track removal requires the matching empty track resource",
        ));
    }
    if previous["format"].as_str() != Some("hyper-mixer")
        || candidate["format"].as_str() != Some("hyper-mixer")
        || previous["id"].as_str() != Some(request.mixer_id)
        || candidate["id"].as_str() != Some(request.mixer_id)
        || previous["project_id"] != candidate["project_id"]
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track removal mixer identity changed",
        ));
    }
    let previous_channels = previous["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current mixer channel collection is invalid",
        )
    })?;
    let candidate_channels = candidate["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate mixer channel collection is invalid",
        )
    })?;
    let mut matching = previous_channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel["track_id"].as_str() == Some(request.track_id));
    let Some((removed_index, _)) = matching.next() else {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "current mixer has no channel for the removed audio track",
        ));
    };
    if matching.next().is_some()
        || candidate_channels.len().checked_add(1) != Some(previous_channels.len())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track removal must remove exactly one mixer channel",
        ));
    }
    if previous_channels.iter().any(|channel| {
        channel["track_id"].as_str() != Some(request.track_id)
            && channel["destination"]["kind"].as_str() == Some("track")
            && channel["destination"]["track_id"].as_str() == Some(request.track_id)
    }) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio track is still a mixer routing destination",
        ));
    }
    if requires_empty_owned_sends
        && previous_channels[removed_index]["sends"]
            .as_array()
            .is_none_or(|sends| !sends.is_empty())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "group routing bus still owns mixer sends",
        ));
    }
    let mut restored = candidate.clone();
    restored["channels"]
        .as_array_mut()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "candidate mixer channel collection changed during validation",
            )
        })?
        .insert(removed_index, previous_channels[removed_index].clone());
    if removes_return_mapping {
        let previous_return_buses = previous["return_buses"].as_array().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current mixer return-bus collection is invalid",
            )
        })?;
        let candidate_return_buses = candidate["return_buses"].as_array().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "candidate mixer return-bus collection is invalid",
            )
        })?;
        let mut matching = previous_return_buses
            .iter()
            .enumerate()
            .filter(|(_, mapping)| mapping["track_id"].as_str() == Some(request.track_id));
        let Some((removed_return_index, removed_return)) = matching.next() else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "current mixer has no return-bus mapping for the removed track",
            ));
        };
        if matching.next().is_some()
            || candidate_return_buses.len().checked_add(1) != Some(previous_return_buses.len())
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "return routing-bus removal must remove exactly one return-bus mapping",
            ));
        }
        let removed_bus = removed_return["bus"].as_str().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current return-bus mapping is invalid",
            )
        })?;
        if previous_channels.iter().any(|channel| {
            channel["sends"].as_array().is_some_and(|sends| {
                sends
                    .iter()
                    .any(|send| send["bus"].as_str() == Some(removed_bus))
            })
        }) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "return routing bus is still a mixer send target",
            ));
        }
        restored["return_buses"]
            .as_array_mut()
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "candidate mixer return-bus collection changed during validation",
                )
            })?
            .insert(removed_return_index, removed_return.clone());
    }
    if restored != previous {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "structural track removal must preserve every unrelated mixer field",
        ));
    }
    Ok(())
}

fn reject_track_addition_collisions(
    verified: &StructurallyVerifiedProject,
    request: TrackAdditionPublicationRequest<'_>,
    track_path: &str,
) -> Result<(), ProjectResourcePublicationError> {
    if verified
        .manifest()
        .resources()
        .iter()
        .any(|descriptor| descriptor.id() == request.track_id || descriptor.path() == track_path)
        || verified.manifest().assets().iter().any(|descriptor| {
            descriptor.id() == request.track_id || descriptor.path() == track_path
        })
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-track identity or path is already declared",
        ));
    }
    Ok(())
}

fn reject_published_track_path(
    root: &Dir,
    track_path: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, track_path)?;
    match parent.symlink_metadata(file_name(track_path)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-track path already exists",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect audio-track destination",
            error,
        )),
    }
}

fn candidate_track_addition_manifest(
    active: &[u8],
    request: TrackAdditionPublicationRequest<'_>,
    track_path: &str,
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let mixer = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(request.mixer_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current mixer descriptor disappeared",
            )
        })?;
    mixer["byte_length"] = request.mixer_bytes.len().into();
    mixer["sha256"] = hex_digest(digest(request.mixer_bytes)).into();
    resources.push(serde_json::json!({
        "id": request.track_id,
        "role": "track",
        "path": track_path,
        "media_type": "application/json",
        "byte_length": request.track_bytes.len(),
        "sha256": hex_digest(digest(request.track_bytes)),
        "extensions": {}
    }));
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize audio-track project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate audio-track project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn candidate_audio_track_removal_manifest(
    active: &[u8],
    request: &StructuralTrackRemovalRequest<'_>,
    track_path: &str,
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let track_index = resources
        .iter()
        .position(|resource| {
            resource["id"].as_str() == Some(request.track_id)
                && resource["role"].as_str() == Some("track")
                && resource["path"].as_str() == Some(track_path)
        })
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "removed audio-track descriptor disappeared",
            )
        })?;
    resources.remove(track_index);
    let mixer = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(request.mixer_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current mixer descriptor disappeared",
            )
        })?;
    mixer["byte_length"] = request.mixer_bytes.len().into();
    mixer["sha256"] = hex_digest(digest(request.mixer_bytes)).into();
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize audio-track removal manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate audio-track removal manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn validate_track_order_request(
    request: TrackOrderPublicationRequest<'_>,
) -> Result<(), ProjectResourcePublicationError> {
    validate_canonical_resource(request.documents.mixer())?;
    if request.mixer_id.is_empty()
        || request.documents.tracks().is_empty()
        || request.documents.tracks().len() > MAX_ENTRIES
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order request must contain a mixer and at least one track",
        ));
    }
    let mut ids = BTreeSet::new();
    for track in request.documents.tracks() {
        if track.track_id().is_empty() || !ids.insert(track.track_id()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order request contains an invalid or duplicate track identity",
            ));
        }
        validate_canonical_resource(track.bytes())?;
    }
    Ok(())
}

fn resolve_track_order_tracks(
    verified: &StructurallyVerifiedProject,
    request: TrackOrderPublicationRequest<'_>,
) -> Result<Vec<TrackOrderActiveTrack>, ProjectResourcePublicationError> {
    let declared = verified
        .manifest()
        .resources()
        .iter()
        .filter(|descriptor| descriptor.role() == ResourceRole::Track)
        .count();
    if declared != request.documents.tracks().len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order request does not cover every active track",
        ));
    }
    let mut tracks = Vec::new();
    tracks
        .try_reserve_exact(request.documents.tracks().len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate track-order active-resource state",
            )
        })?;
    for replacement in request.documents.tracks() {
        let descriptor = unique_resource(verified, replacement.track_id(), ResourceRole::Track)?;
        let expected_path = format!("tracks/{}.json", replacement.track_id());
        if descriptor.path() != expected_path {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order publication requires canonical track resource paths",
            ));
        }
        let previous = verified
            .read_resource(replacement.track_id())
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "read current track-order resource",
                    error,
                )
            })?;
        tracks.push(TrackOrderActiveTrack {
            path: descriptor.path().to_owned(),
            previous,
        });
    }
    Ok(tracks)
}

#[allow(
    clippy::too_many_lines,
    reason = "the complete coupled track and mixer identity proof must stay together before any transaction staging"
)]
fn validate_track_order_document_identity(
    verified: &StructurallyVerifiedProject,
    request: TrackOrderPublicationRequest<'_>,
    active_tracks: &[TrackOrderActiveTrack],
    previous_mixer: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    if active_tracks.len() != request.documents.tracks().len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "resolved track-order resources do not match the request",
        ));
    }
    let mut previous_orders = BTreeSet::new();
    let mut candidate_orders = BTreeSet::new();
    let mut ordered_mixable_tracks = Vec::new();
    ordered_mixable_tracks
        .try_reserve_exact(active_tracks.len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate track-order mixer sequence",
            )
        })?;
    for (active, replacement) in active_tracks.iter().zip(request.documents.tracks()) {
        let previous: Value = serde_json::from_slice(&active.previous).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "parse current track-order track",
                error,
            )
        })?;
        let mut candidate: Value =
            serde_json::from_slice(replacement.bytes()).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "parse candidate track-order track",
                    error,
                )
            })?;
        if previous["format"].as_str() != Some("hyper-track")
            || previous["format_version"].as_u64() != Some(1)
            || previous["id"].as_str() != Some(replacement.track_id())
            || previous["project_id"].as_str() != Some(verified.manifest().project_id())
            || candidate["format"].as_str() != Some("hyper-track")
            || candidate["format_version"].as_u64() != Some(1)
            || candidate["id"].as_str() != Some(replacement.track_id())
            || candidate["project_id"].as_str() != Some(verified.manifest().project_id())
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order track identity does not match the active project",
            ));
        }
        let previous_order = previous["order"].as_u64().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current track-order track order is invalid",
            )
        })?;
        let candidate_order = candidate["order"].as_u64().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order candidate order is invalid",
            )
        })?;
        if !previous_orders.insert(previous_order) || !candidate_orders.insert(candidate_order) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order values must remain a unique permutation",
            ));
        }
        candidate["order"] = previous_order.into();
        if candidate != previous {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order replacement changes fields other than track order",
            ));
        }
        if matches!(
            previous["kind"].as_str(),
            Some("audio" | "external-input" | "group" | "return")
        ) {
            ordered_mixable_tracks.push((candidate_order, replacement.track_id()));
        }
    }
    if previous_orders != candidate_orders {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order values must preserve the active order slots",
        ));
    }
    ordered_mixable_tracks.sort_unstable_by_key(|(order, _)| *order);

    let previous: Value = serde_json::from_slice(previous_mixer).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current track-order mixer",
            error,
        )
    })?;
    let mut candidate: Value =
        serde_json::from_slice(request.documents.mixer()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "parse candidate track-order mixer",
                error,
            )
        })?;
    if previous["format"].as_str() != Some("hyper-mixer")
        || previous["format_version"].as_u64() != Some(1)
        || previous["id"].as_str() != Some(request.mixer_id)
        || previous["project_id"].as_str() != Some(verified.manifest().project_id())
        || candidate["format"].as_str() != Some("hyper-mixer")
        || candidate["format_version"].as_u64() != Some(1)
        || candidate["id"].as_str() != Some(request.mixer_id)
        || candidate["project_id"].as_str() != Some(verified.manifest().project_id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order mixer identity does not match the active project",
        ));
    }
    let previous_channels = previous["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current track-order mixer channels are invalid",
        )
    })?;
    let candidate_channels = candidate["channels"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate track-order mixer channels are invalid",
        )
    })?;
    if previous_channels.len() != candidate_channels.len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order mixer channel count changed",
        ));
    }
    if candidate_channels
        .iter()
        .map(|channel| channel["track_id"].as_str())
        .ne(ordered_mixable_tracks.iter().map(|(_, id)| Some(*id)))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order mixer channels do not follow the candidate track order",
        ));
    }
    let mut consumed = vec![false; previous_channels.len()];
    for candidate_channel in candidate_channels {
        let id = candidate_channel["track_id"].as_str().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "candidate track-order mixer channel identity is invalid",
            )
        })?;
        let Some((index, previous_channel)) = previous_channels
            .iter()
            .enumerate()
            .find(|(index, channel)| !consumed[*index] && channel["track_id"].as_str() == Some(id))
        else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order mixer must preserve exactly the active channels",
            ));
        };
        let mut normalized_channel = candidate_channel.clone();
        restore_track_order_channel_base_state(&mut normalized_channel, previous_channel)?;
        if normalized_channel != *previous_channel {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order replacement changes mixer channel fields outside the live base state",
            ));
        }
        consumed[index] = true;
    }
    candidate["channels"] = previous["channels"].clone();
    restore_track_order_main_base_state(&mut candidate, &previous)?;
    if candidate != previous {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order replacement changes mixer fields outside the live base state and channel order",
        ));
    }
    Ok(())
}

/// Replaces the mixer fields a fenced live session may persist before its
/// structural track-order transaction. Every identity, route, extension, and
/// immutable document field remains compared exactly against the active mixer.
fn restore_track_order_channel_base_state(
    candidate: &mut Value,
    previous: &Value,
) -> Result<(), ProjectResourcePublicationError> {
    let candidate = candidate.as_object_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order candidate channel is not an object",
        )
    })?;
    let previous = previous.as_object().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "active track-order channel is not an object",
        )
    })?;
    restore_track_order_base_fields(
        candidate,
        previous,
        &["gain_millionths", "pan_millionths", "mute", "solo"],
        "channel",
    )?;
    let candidate_sends = candidate
        .get_mut("sends")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order candidate channel sends are invalid",
            )
        })?;
    let previous_sends = previous
        .get("sends")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "active track-order channel sends are invalid",
            )
        })?;
    if candidate_sends.len() != previous_sends.len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-order replacement changes mixer send count",
        ));
    }
    for (candidate_send, previous_send) in candidate_sends.iter_mut().zip(previous_sends) {
        let candidate_send = candidate_send.as_object_mut().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order candidate mixer send is not an object",
            )
        })?;
        let previous_send = previous_send.as_object().ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "active track-order mixer send is not an object",
            )
        })?;
        if candidate_send.get("id") != previous_send.get("id") {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order replacement changes a mixer send identity",
            ));
        }
        restore_track_order_base_fields(
            candidate_send,
            previous_send,
            &["gain_millionths", "pan_millionths", "enabled"],
            "send",
        )?;
    }
    Ok(())
}

fn restore_track_order_main_base_state(
    candidate: &mut Value,
    previous: &Value,
) -> Result<(), ProjectResourcePublicationError> {
    let candidate = candidate
        .get_mut("main")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-order candidate main mixer state is invalid",
            )
        })?;
    let previous = previous
        .get("main")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "active track-order main mixer state is invalid",
            )
        })?;
    restore_track_order_base_fields(candidate, previous, &["gain_millionths", "mute"], "main")
}

fn restore_track_order_base_fields(
    candidate: &mut serde_json::Map<String, Value>,
    previous: &serde_json::Map<String, Value>,
    fields: &[&str],
    label: &str,
) -> Result<(), ProjectResourcePublicationError> {
    for field in fields {
        let previous_value = previous.get(*field).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                format!("active track-order mixer {label} is missing {field}"),
            )
        })?;
        let candidate_value = candidate.get_mut(*field).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                format!("track-order candidate mixer {label} is missing {field}"),
            )
        })?;
        *candidate_value = previous_value.clone();
    }
    Ok(())
}

fn candidate_track_order_manifest(
    active: &[u8],
    mixer_id: &str,
    mixer_bytes: &[u8],
    tracks: &[TrackOrderPublicationTrack<'_>],
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let mixer = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(mixer_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current mixer descriptor disappeared",
            )
        })?;
    if mixer["role"].as_str() != Some("mixer") {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-order mixer descriptor has the wrong role",
        ));
    }
    mixer["byte_length"] = mixer_bytes.len().into();
    mixer["sha256"] = hex_digest(digest(mixer_bytes)).into();
    for replacement in tracks {
        let descriptor = resources
            .iter_mut()
            .find(|resource| resource["id"].as_str() == Some(replacement.track_id))
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "current track descriptor disappeared",
                )
            })?;
        if descriptor["role"].as_str() != Some("track") {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-order descriptor has the wrong role",
            ));
        }
        descriptor["byte_length"] = replacement.track_bytes.len().into();
        descriptor["sha256"] = hex_digest(digest(replacement.track_bytes)).into();
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize track-order project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate track-order project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn track_order_staging_name(index: usize) -> String {
    format!("track-{index}.json")
}

fn track_order_previous_staging_name(index: usize) -> String {
    format!("previous-track-{index}.json")
}

fn write_track_order_intent(
    transaction: &Dir,
    directory_name: &str,
    track_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = TrackOrderTransactionIntent {
        format: "hyper-track-order-publication-intent".to_owned(),
        format_version: 1,
        id: track_order_transaction_id(directory_name)?,
        track_count,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-order publication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish track-order transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn write_audio_track_intent(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = TransactionIntent {
        format: "hyper-audio-track-publication-intent".to_owned(),
        format_version: 1,
        id: audio_track_transaction_id(directory_name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-track publication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish audio-track transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn validate_canonical_resource(bytes: &[u8]) -> Result<(), ProjectResourcePublicationError> {
    if bytes.is_empty() || bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "project resource size is invalid",
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse project resource",
            error,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "canonicalize project resource",
            error,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "project resource is not canonical RFC 8785 JSON",
        ));
    }
    Ok(())
}

fn unique_resource<'a>(
    verified: &'a StructurallyVerifiedProject,
    resource_id: &str,
    role: ResourceRole,
) -> Result<&'a ResourceDescriptor, ProjectResourcePublicationError> {
    let mut matching = verified
        .manifest()
        .resources()
        .iter()
        .filter(|descriptor| descriptor.id() == resource_id);
    let descriptor = matching.next().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "project resource does not exist",
        )
    })?;
    if matching.next().is_some() || descriptor.role() != role {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "project resource identity or role is inconsistent",
        ));
    }
    Ok(descriptor)
}

fn candidate_manifest(
    active: &[u8],
    resource_id: &str,
    resource_bytes: &[u8],
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let descriptor = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(resource_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current resource descriptor disappeared",
            )
        })?;
    descriptor["byte_length"] = resource_bytes.len().into();
    descriptor["sha256"] = hex_digest(digest(resource_bytes)).into();
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize candidate project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn candidate_addition_manifest(
    active: &[u8],
    request: ProjectResourcePublicationRequest<'_>,
    resource_path: &str,
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    resources.push(serde_json::json!({
        "id": request.resource_id,
        "role": role_name(request.expected_role),
        "path": resource_path,
        "media_type": "application/json",
        "byte_length": request.resource_bytes.len(),
        "sha256": hex_digest(digest(request.resource_bytes)),
        "extensions": {}
    }));
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize arrangement project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate resource-addition manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn candidate_removal_manifest(
    active: &[u8],
    resource_id: &str,
    revision: u64,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let Some(index) = resources
        .iter()
        .position(|resource| resource["id"].as_str() == Some(resource_id))
    else {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "removed resource descriptor disappeared",
        ));
    };
    resources.remove(index);
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize resource-removal project manifest",
            error,
        )
    })?;
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate resource-removal manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

const fn addition_directory(role: ResourceRole) -> Option<&'static str> {
    match role {
        ResourceRole::Arrangement => Some("arrangement"),
        ResourceRole::Scene => Some("scenes"),
        ResourceRole::AutomationTake => Some("takes"),
        ResourceRole::DeviceChain => Some("devices"),
        _ => None,
    }
}

fn role_name(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Timeline => "timeline",
        ResourceRole::Track => "track",
        ResourceRole::Clip => "clip",
        ResourceRole::Scene => "scene",
        ResourceRole::Arrangement => "arrangement",
        ResourceRole::Mixer => "mixer",
        ResourceRole::Mapping => "mapping",
        ResourceRole::AutomationTake => "automation-take",
        ResourceRole::RecordingMetadata => "recording-metadata",
        ResourceRole::DeviceChain => "device-chain",
    }
}

fn write_intent(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = TransactionIntent {
        format: "hyper-project-resource-publication-intent".to_owned(),
        format_version: 1,
        id: transaction_id(directory_name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize resource publication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish resource transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn recover_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<ProjectResourcePublicationRecovery>, ProjectResourcePublicationError> {
    let mut names = Vec::new();
    for entry in backups.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "read project backups",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read project backup entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(TRANSACTION_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    let mut outcomes = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open resource recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_prepublication_residue(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove_transaction(backups, transaction, &name)?;
                outcomes.push(ProjectResourcePublicationRecovery {
                    transaction_id: id,
                    disposition: ProjectResourcePublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition = recover_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_transaction(backups, transaction, &name)?;
        outcomes.push(ProjectResourcePublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn recover_audio_track_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<AudioTrackPublicationRecovery>, ProjectResourcePublicationError> {
    let mut names = Vec::new();
    for entry in backups.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "read project backups",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read project backup entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(AUDIO_TRACK_TRANSACTION_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    let mut outcomes = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-track recovery transaction",
                error,
            )
        })?;
        let tx = match read_audio_track_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_audio_track_prepublication_residue(&transaction, &name)?;
                let id = audio_track_transaction_id(&name)?;
                clear_and_remove_audio_track_transaction(backups, transaction, &name)?;
                outcomes.push(AudioTrackPublicationRecovery {
                    transaction_id: id,
                    disposition: AudioTrackPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition =
            recover_audio_track_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_audio_track_transaction(backups, transaction, &name)?;
        outcomes.push(AudioTrackPublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn recover_audio_track_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<AudioTrackPublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_audio_track_transaction(root, transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = match read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES) {
        Ok(bytes) => digest(&bytes),
        Err(error) if error_is_not_found(&error) => {
            restore_previous_manifest(root, backups, transaction, &tx.id, before)?;
            before
        }
        Err(error) => return Err(error),
    };
    if active == after {
        (|| {
            if tx.operation == AudioTrackTransactionOperation::Removal {
                ensure_audio_track_absent(root, tx)?;
            }
            cleanup_audio_track_temporaries(root, backups, transaction, tx)?;
            verify_committed(root_path, root, after)
        })()
        .map_err(|source| ProjectResourcePublicationError {
            kind: ProjectResourcePublicationErrorKind::Recovery,
            message: "committed audio-track transaction could not be verified or fully recovered"
                .to_owned(),
            published: true,
            source: Some(Box::new(source)),
        })?;
        return Ok(AudioTrackPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track recovery found an unrelated project identity",
        ));
    }
    rollback_audio_track_mixer(root, transaction, tx)?;
    match tx.operation {
        AudioTrackTransactionOperation::Publication => remove_published_audio_track(root, tx)?,
        AudioTrackTransactionOperation::Removal => {
            restore_removed_audio_track(root, transaction, tx)?;
        }
    }
    cleanup_audio_track_temporaries(root, backups, transaction, tx)?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    verify_retained_project_candidate(root, &previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "rolled-back audio-track project does not verify",
            error,
        )
    })?;
    Ok(AudioTrackPublicationRecoveryDisposition::RolledBack)
}

fn recover_track_order_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<TrackOrderPublicationRecovery>, ProjectResourcePublicationError> {
    let mut names = Vec::new();
    for entry in backups.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "read project backups",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read project backup entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(TRACK_ORDER_TRANSACTION_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    let mut outcomes = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open track-order recovery transaction",
                error,
            )
        })?;
        let tx = match read_track_order_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_track_order_prepublication_residue(&transaction, &name)?;
                let id = track_order_transaction_id(&name)?;
                clear_and_remove_track_order_transaction(backups, transaction, &name)?;
                outcomes.push(TrackOrderPublicationRecovery {
                    transaction_id: id,
                    disposition: TrackOrderPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition =
            recover_track_order_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_track_order_transaction(backups, transaction, &name)?;
        outcomes.push(TrackOrderPublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn recover_track_order_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TrackOrderTransactionRecord,
) -> Result<TrackOrderPublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_track_order_transaction(transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = match read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES) {
        Ok(bytes) => digest(&bytes),
        Err(error) if error_is_not_found(&error) => {
            restore_previous_manifest(root, backups, transaction, &tx.id, before)?;
            before
        }
        Err(error) => return Err(error),
    };
    if active == after {
        cleanup_track_order_temporaries(root, backups, transaction, tx)
            .and_then(|()| verify_committed(root_path, root, after))
            .map_err(|source| ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message:
                    "committed track-order transaction could not be verified or fully recovered"
                        .to_owned(),
                published: true,
                source: Some(Box::new(source)),
            })?;
        return Ok(TrackOrderPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order recovery found an unrelated project identity",
        ));
    }
    validate_track_order_rollback_authority(root, tx)?;
    rollback_track_order_mixer(root, transaction, tx)?;
    for (index, track) in tx.tracks.iter().enumerate() {
        rollback_track_order_track(root, transaction, tx, index, track)?;
    }
    cleanup_track_order_temporaries(root, backups, transaction, tx)?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    verify_retained_project_candidate(root, &previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "rolled-back track-order project does not verify",
            error,
        )
    })?;
    Ok(TrackOrderPublicationRecoveryDisposition::RolledBack)
}

fn validate_track_order_rollback_authority(
    root: &Dir,
    tx: &TrackOrderTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    validate_track_order_active_file(root, &tx.mixer.path, &tx.previous_mixer, &tx.mixer, "mixer")?;
    for track in &tx.tracks {
        validate_track_order_active_file(
            root,
            &track.track.path,
            &track.previous_track,
            &track.track,
            "track",
        )?;
    }
    Ok(())
}

fn validate_track_order_active_file(
    root: &Dir,
    path: &str,
    before: &TransactionFile,
    after: &TransactionFile,
    label: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, path)?;
    let current = read_optional_file(&parent, file_name(path)?)?.ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            format!("track-order rollback is missing its active {label}"),
        )
    })?;
    let found = digest(&current);
    if found != decode_digest(&before.sha256)? && found != decode_digest(&after.sha256)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            format!("track-order rollback refuses unrelated active {label} content"),
        ));
    }
    Ok(())
}

fn rollback_track_order_mixer(
    root: &Dir,
    transaction: &Dir,
    tx: &TrackOrderTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.mixer.path)?;
    let current = read_optional_file(&parent, file_name(&tx.mixer.path)?)?;
    let previous = decode_digest(&tx.previous_mixer.sha256)?;
    if current
        .as_deref()
        .is_some_and(|bytes| digest(bytes) == previous)
    {
        return Ok(());
    }
    publish_staged_file(
        transaction,
        TRACK_ORDER_PREVIOUS_MIXER_NAME,
        &parent,
        &parent,
        file_name(&tx.previous_mixer.path)?,
        &format!(".track-order-mixer-rollback-{}.json", tx.id),
        "previous track-order mixer",
    )
}

fn rollback_track_order_track(
    root: &Dir,
    transaction: &Dir,
    tx: &TrackOrderTransactionRecord,
    index: usize,
    track: &TrackOrderTransactionTrack,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &track.track.path)?;
    let current = read_optional_file(&parent, file_name(&track.track.path)?)?;
    let previous = decode_digest(&track.previous_track.sha256)?;
    if current
        .as_deref()
        .is_some_and(|bytes| digest(bytes) == previous)
    {
        return Ok(());
    }
    publish_staged_file(
        transaction,
        &track_order_previous_staging_name(index),
        &parent,
        &parent,
        file_name(&track.previous_track.path)?,
        &format!(".track-order-track-{index}-rollback-{}.json", tx.id),
        "previous track-order track",
    )
}

fn cleanup_track_order_temporaries(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TrackOrderTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if let Ok(parent) = open_relative_parent(root, &tx.mixer.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".track-order-mixer-{}.json", tx.id),
            decode_digest(&tx.mixer.sha256)?,
        )?;
        remove_optional_matching_file(
            &parent,
            &format!(".track-order-mixer-rollback-{}.json", tx.id),
            decode_digest(&tx.previous_mixer.sha256)?,
        )?;
    }
    for (index, track) in tx.tracks.iter().enumerate() {
        if let Ok(parent) = open_relative_parent(root, &track.track.path) {
            remove_optional_matching_file(
                &parent,
                &format!(".track-order-track-{index}-{}.json", tx.id),
                decode_digest(&track.track.sha256)?,
            )?;
            remove_optional_matching_file(
                &parent,
                &format!(".track-order-track-{index}-rollback-{}.json", tx.id),
                decode_digest(&track.previous_track.sha256)?,
            )?;
        }
    }
    remove_optional_matching_file(
        backups,
        &format!(".track-order-project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    remove_optional_matching_file(
        backups,
        &format!(".project-rollback-{}.json", tx.id),
        digest(&previous),
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "recovery validates every staged document and the complete candidate manifest before any filesystem mutation"
)]
fn validate_staged_track_order_transaction(
    transaction: &Dir,
    tx: &TrackOrderTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let mixer = read_bounded(transaction, TRACK_ORDER_MIXER_STAGING_NAME, MAX_JSON_BYTES)?;
    let previous_mixer =
        read_bounded(transaction, TRACK_ORDER_PREVIOUS_MIXER_NAME, MAX_JSON_BYTES)?;
    validate_canonical_resource(&mixer).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged track-order mixer is not canonical",
            error,
        )
    })?;
    validate_canonical_resource(&previous_mixer).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous track-order mixer is not canonical",
            error,
        )
    })?;
    if mixer.len() as u64 != tx.mixer.byte_length
        || digest(&mixer) != decode_digest(&tx.mixer.sha256)?
        || previous_mixer.len() as u64 != tx.previous_mixer.byte_length
        || digest(&previous_mixer) != decode_digest(&tx.previous_mixer.sha256)?
        || tx.mixer.path != tx.previous_mixer.path
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged track-order mixer bytes do not match the transaction record",
        ));
    }
    let mut replacements = Vec::new();
    replacements
        .try_reserve_exact(tx.tracks.len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "allocate staged track-order resources",
            )
        })?;
    for (index, track) in tx.tracks.iter().enumerate() {
        let bytes = read_bounded(
            transaction,
            &track_order_staging_name(index),
            MAX_JSON_BYTES,
        )?;
        let previous = read_bounded(
            transaction,
            &track_order_previous_staging_name(index),
            MAX_JSON_BYTES,
        )?;
        validate_canonical_resource(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged track-order track is not canonical",
                error,
            )
        })?;
        validate_canonical_resource(&previous).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged previous track-order track is not canonical",
                error,
            )
        })?;
        if bytes.len() as u64 != track.track.byte_length
            || digest(&bytes) != decode_digest(&track.track.sha256)?
            || previous.len() as u64 != track.previous_track.byte_length
            || digest(&previous) != decode_digest(&track.previous_track.sha256)?
            || track.track.path != track.previous_track.path
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged track-order track bytes do not match the transaction record",
            ));
        }
        replacements.push((bytes, previous));
    }
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged track-order project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous project manifest is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate project manifest is invalid",
            error,
        )
    })?;
    if previous_manifest.project_id() != candidate_manifest.project_id()
        || previous_manifest.revision() != tx.before_project_revision
        || candidate_manifest.revision() != tx.after_project_revision
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order transaction project revisions are inconsistent",
        ));
    }
    validate_track_order_manifest_descriptor(
        &previous_manifest,
        &tx.mixer_id,
        ResourceRole::Mixer,
        &tx.previous_mixer,
    )?;
    validate_track_order_manifest_descriptor(
        &candidate_manifest,
        &tx.mixer_id,
        ResourceRole::Mixer,
        &tx.mixer,
    )?;
    let mut request_tracks = Vec::new();
    request_tracks
        .try_reserve_exact(tx.tracks.len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "allocate track-order candidate request",
            )
        })?;
    for (entry, (bytes, _)) in tx.tracks.iter().zip(&replacements) {
        validate_track_order_manifest_descriptor(
            &previous_manifest,
            &entry.track_id,
            ResourceRole::Track,
            &entry.previous_track,
        )?;
        validate_track_order_manifest_descriptor(
            &candidate_manifest,
            &entry.track_id,
            ResourceRole::Track,
            &entry.track,
        )?;
        request_tracks.push(TrackOrderPublicationTrack {
            track_id: &entry.track_id,
            track_bytes: bytes,
        });
    }
    let expected = candidate_track_order_manifest(
        &previous_project,
        &tx.mixer_id,
        &mixer,
        &request_tracks,
        tx.after_project_revision,
    )?;
    if expected != candidate_project {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order candidate manifest contains unrelated changes",
        ));
    }
    Ok(())
}

fn validate_track_order_manifest_descriptor(
    manifest: &ProjectManifest,
    id: &str,
    role: ResourceRole,
    file: &TransactionFile,
) -> Result<(), ProjectResourcePublicationError> {
    let descriptor = manifest_resource(manifest, id)?;
    if descriptor.role() != role
        || descriptor.path() != file.path
        || descriptor.byte_length() != file.byte_length
        || descriptor.sha256() != file.sha256
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order descriptor does not match the transaction",
        ));
    }
    Ok(())
}

fn ensure_audio_track_absent(
    root: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.track.path)?;
    if read_optional_file(&parent, file_name(&tx.track.path)?)?.is_some() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "committed audio-track removal still has an active track file",
        ));
    }
    Ok(())
}

fn restore_removed_audio_track(
    root: &Dir,
    transaction: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.track.path)?;
    let name = file_name(&tx.track.path)?;
    let expected = decode_digest(&tx.track.sha256)?;
    match read_optional_file(&parent, name)? {
        Some(bytes) if digest(&bytes) == expected => return Ok(()),
        Some(_) => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track rollback refuses unrelated track content",
            ));
        }
        None => {}
    }
    publish_staged_file(
        transaction,
        AUDIO_TRACK_STAGING_NAME,
        &parent,
        &parent,
        name,
        &format!(".audio-track-rollback-{}.json", tx.id),
        "removed audio track",
    )
}

fn rollback_audio_track_mixer(
    root: &Dir,
    transaction: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.mixer.path)?;
    let current = read_optional_file(&parent, file_name(&tx.mixer.path)?)?;
    let current_digest = current.as_deref().map(digest);
    let before = decode_digest(&tx.previous_mixer.sha256)?;
    let after = decode_digest(&tx.mixer.sha256)?;
    match current_digest {
        Some(found) if found == before => return Ok(()),
        Some(found) if found != after => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track rollback refuses unrelated mixer content",
            ));
        }
        Some(_) | None => {}
    }
    publish_staged_file(
        transaction,
        AUDIO_TRACK_PREVIOUS_MIXER_NAME,
        &parent,
        &parent,
        file_name(&tx.previous_mixer.path)?,
        &format!(".audio-track-mixer-rollback-{}.json", tx.id),
        "previous audio-track mixer",
    )
}

fn remove_published_audio_track(
    root: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.track.path)?;
    let name = file_name(&tx.track.path)?;
    let Some(bytes) = read_optional_file(&parent, name)? else {
        return Ok(());
    };
    if digest(&bytes) != decode_digest(&tx.track.sha256)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track rollback refuses unrelated track content",
        ));
    }
    parent.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove rolled-back audio-track resource",
            error,
        )
    })?;
    sync_dir(&parent)
}

#[allow(
    clippy::too_many_lines,
    reason = "recovery validates the complete transaction graph before permitting any filesystem mutation"
)]
fn validate_staged_audio_track_transaction(
    root: &Dir,
    transaction: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let track = read_bounded(transaction, AUDIO_TRACK_STAGING_NAME, MAX_JSON_BYTES)?;
    let mixer = read_bounded(transaction, AUDIO_TRACK_MIXER_STAGING_NAME, MAX_JSON_BYTES)?;
    let previous_mixer =
        read_bounded(transaction, AUDIO_TRACK_PREVIOUS_MIXER_NAME, MAX_JSON_BYTES)?;
    for bytes in [&track, &mixer, &previous_mixer] {
        validate_canonical_resource(bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged audio-track resource is not canonical",
                error,
            )
        })?;
    }
    if track.len() as u64 != tx.track.byte_length
        || digest(&track) != decode_digest(&tx.track.sha256)?
        || mixer.len() as u64 != tx.mixer.byte_length
        || digest(&mixer) != decode_digest(&tx.mixer.sha256)?
        || previous_mixer.len() as u64 != tx.previous_mixer.byte_length
        || digest(&previous_mixer) != decode_digest(&tx.previous_mixer.sha256)?
        || tx.mixer.path != tx.previous_mixer.path
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged audio-track bytes do not match the transaction record",
        ));
    }
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged audio-track project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous project manifest is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate project manifest is invalid",
            error,
        )
    })?;
    if previous_manifest.project_id() != candidate_manifest.project_id()
        || previous_manifest.revision() != tx.before_project_revision
        || candidate_manifest.revision() != tx.after_project_revision
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track transaction project revisions are inconsistent",
        ));
    }
    validate_recovered_routing_bus_sources(root, &previous_manifest, tx)?;
    let previous_mixer_descriptor = manifest_resource(&previous_manifest, &tx.mixer_id)?;
    let candidate_mixer_descriptor = manifest_resource(&candidate_manifest, &tx.mixer_id)?;
    if previous_mixer_descriptor.role() != ResourceRole::Mixer
        || candidate_mixer_descriptor.role() != ResourceRole::Mixer
        || previous_mixer_descriptor.path() != tx.previous_mixer.path
        || previous_mixer_descriptor.byte_length() != previous_mixer.len() as u64
        || previous_mixer_descriptor.sha256() != hex_digest(digest(&previous_mixer))
        || candidate_mixer_descriptor.path() != tx.mixer.path
        || candidate_mixer_descriptor.byte_length() != mixer.len() as u64
        || candidate_mixer_descriptor.sha256() != hex_digest(digest(&mixer))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track descriptors do not match the transaction",
        ));
    }
    let expected = match tx.operation {
        AudioTrackTransactionOperation::Publication => {
            if previous_manifest.resources().iter().any(|descriptor| {
                descriptor.id() == tx.track_id || descriptor.path() == tx.track.path
            }) {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "previous project already declares the transaction audio track",
                ));
            }
            let candidate_track_descriptor = manifest_resource(&candidate_manifest, &tx.track_id)?;
            if candidate_track_descriptor.role() != ResourceRole::Track
                || candidate_track_descriptor.path() != tx.track.path
                || candidate_track_descriptor.byte_length() != track.len() as u64
                || candidate_track_descriptor.sha256() != hex_digest(digest(&track))
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "published audio-track descriptor does not match the transaction",
                ));
            }
            candidate_track_addition_manifest(
                &previous_project,
                TrackAdditionPublicationRequest {
                    expected_project_sha256: digest(&previous_project),
                    track_id: &tx.track_id,
                    track_bytes: &track,
                    mixer_id: &tx.mixer_id,
                    mixer_bytes: &mixer,
                },
                &tx.track.path,
                tx.after_project_revision,
            )?
        }
        AudioTrackTransactionOperation::Removal => {
            let previous_track_descriptor = manifest_resource(&previous_manifest, &tx.track_id)?;
            if previous_track_descriptor.role() != ResourceRole::Track
                || previous_track_descriptor.path() != tx.track.path
                || previous_track_descriptor.byte_length() != track.len() as u64
                || previous_track_descriptor.sha256() != hex_digest(digest(&track))
                || candidate_manifest
                    .resources()
                    .iter()
                    .any(|descriptor| descriptor.id() == tx.track_id)
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "removed audio-track descriptor does not match the transaction",
                ));
            }
            candidate_audio_track_removal_manifest(
                &previous_project,
                &StructuralTrackRemovalRequest {
                    expected_project_sha256: digest(&previous_project),
                    track_id: &tx.track_id,
                    mixer_id: &tx.mixer_id,
                    mixer_bytes: &mixer,
                    contract: tx.contract.clone(),
                },
                &tx.track.path,
                tx.after_project_revision,
            )?
        }
    };
    if expected != candidate_project {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track candidate manifest contains unrelated changes",
        ));
    }
    match tx.operation {
        AudioTrackTransactionOperation::Publication => validate_track_addition_document_identity(
            TrackAdditionPublicationRequest {
                expected_project_sha256: digest(&previous_project),
                track_id: &tx.track_id,
                track_bytes: &track,
                mixer_id: &tx.mixer_id,
                mixer_bytes: &mixer,
            },
            tx.contract.publication_contract(),
            None,
            previous_manifest.project_id(),
            &previous_mixer,
        ),
        AudioTrackTransactionOperation::Removal => validate_audio_track_removal_documents(
            &StructuralTrackRemovalRequest {
                expected_project_sha256: digest(&previous_project),
                track_id: &tx.track_id,
                mixer_id: &tx.mixer_id,
                mixer_bytes: &mixer,
                contract: tx.contract.clone(),
            },
            &track,
            &previous_mixer,
        ),
    }
}

fn validate_recovered_routing_bus_sources(
    root: &Dir,
    previous_manifest: &ProjectManifest,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let source_track_ids = match &tx.contract {
        AudioTrackTransactionContract::Audio => return Ok(()),
        AudioTrackTransactionContract::RoutingGroup { source_track_ids }
        | AudioTrackTransactionContract::RoutingReturn {
            source_track_ids, ..
        } => source_track_ids,
    };
    if let AudioTrackTransactionContract::RoutingReturn { send_ids, .. } = &tx.contract
        && send_ids.iter().any(|send_id| {
            send_id == previous_manifest.project_id()
                || previous_manifest
                    .resources()
                    .iter()
                    .any(|resource| resource.id() == send_id)
                || previous_manifest
                    .assets()
                    .iter()
                    .any(|asset| asset.id() == send_id)
        })
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "routing-bus transaction send identity conflicts with the previous project",
        ));
    }
    for source_track_id in source_track_ids {
        let descriptor = manifest_resource(previous_manifest, source_track_id)?;
        if descriptor.role() != ResourceRole::Track {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "routing-bus transaction source is not a track",
            ));
        }
        let parent = open_relative_parent(root, descriptor.path())?;
        let source = read_bounded(&parent, file_name(descriptor.path())?, MAX_JSON_BYTES)?;
        if source.len() as u64 != descriptor.byte_length()
            || hex_digest(digest(&source)) != descriptor.sha256()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "routing-bus transaction source track changed",
            ));
        }
        let source: Value = serde_json::from_slice(&source).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse routing-bus transaction source track",
                error,
            )
        })?;
        if source["kind"].as_str() != Some("audio") || source["active"].as_bool() != Some(true) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "routing-bus transaction source is not an active audio track",
            ));
        }
    }
    Ok(())
}

fn read_audio_track_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<AudioTrackTransactionRecord, ProjectResourcePublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "audio-track transaction intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-track transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "audio-track transaction")?;
    let tx: AudioTrackTransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-track transaction",
            error,
        )
    })?;
    let expected_id = audio_track_transaction_id(directory_name)?;
    if intent.format != "hyper-audio-track-publication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || match tx.operation {
            AudioTrackTransactionOperation::Publication => {
                tx.format != "hyper-audio-track-publication-transaction"
            }
            AudioTrackTransactionOperation::Removal => {
                tx.format != "hyper-audio-track-removal-transaction"
            }
        }
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || !tx.contract.is_valid(tx.operation)
        || tx.id != expected_id
        || tx.track_id.is_empty()
        || tx.mixer_id.is_empty()
        || tx.track.path != format!("tracks/{}.json", tx.track_id)
        || tx.mixer.path != tx.previous_mixer.path
        || [&tx.track, &tx.mixer, &tx.previous_mixer]
            .iter()
            .any(|file| {
                file.byte_length == 0
                    || file.byte_length > MAX_JSON_FILE_BYTES
                    || decode_digest(&file.sha256).is_err()
            })
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track publication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn read_track_order_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<TrackOrderTransactionRecord, ProjectResourcePublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "track-order transaction intent")?;
    let intent: TrackOrderTransactionIntent =
        serde_json::from_slice(&intent_bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse track-order transaction intent",
                error,
            )
        })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "track-order transaction")?;
    let tx: TrackOrderTransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-order transaction",
            error,
        )
    })?;
    let expected_id = track_order_transaction_id(directory_name)?;
    let mut track_ids = BTreeSet::new();
    let records_valid = tx.tracks.iter().all(|track| {
        track_ids.insert(track.track_id.as_str())
            && is_valid_entity_id(&track.track_id)
            && track.track.path == format!("tracks/{}.json", track.track_id)
            && track.track.path == track.previous_track.path
            && ["current", "previous"]
                .iter()
                .zip([&track.track, &track.previous_track])
                .all(|(_, file)| {
                    file.byte_length > 0
                        && file.byte_length <= MAX_JSON_FILE_BYTES
                        && decode_digest(&file.sha256).is_ok()
                })
    });
    if intent.format != "hyper-track-order-publication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || intent.track_count != tx.tracks.len()
        || tx.format != "hyper-track-order-publication-transaction"
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || tx.id != expected_id
        || !is_valid_entity_id(&tx.mixer_id)
        || tx.tracks.is_empty()
        || tx.tracks.len() > MAX_ENTRIES
        || !records_valid
        || tx.mixer.path != tx.previous_mixer.path
        || [&tx.mixer, &tx.previous_mixer].iter().any(|file| {
            file.byte_length == 0
                || file.byte_length > MAX_JSON_FILE_BYTES
                || decode_digest(&file.sha256).is_err()
        })
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order publication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn validate_track_order_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let id = track_order_transaction_id(directory_name)?;
    let mut track_count = None;
    if !missing_file(transaction, INTENT_NAME)? {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "track-order transaction intent")?;
        let intent: TrackOrderTransactionIntent =
            serde_json::from_slice(&bytes).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "parse incomplete track-order transaction intent",
                    error,
                )
            })?;
        if intent.format != "hyper-track-order-publication-intent"
            || intent.format_version != 1
            || intent.id != id
            || intent.track_count == 0
            || intent.track_count > MAX_ENTRIES
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-order transaction intent is invalid",
            ));
        }
        track_count = Some(intent.track_count);
    }
    let count = track_count.unwrap_or(0);
    let mut expected = BTreeSet::new();
    expected.insert(INTENT_STAGING_NAME.to_owned());
    expected.insert(INTENT_NAME.to_owned());
    expected.insert(TRACK_ORDER_MIXER_STAGING_NAME.to_owned());
    expected.insert(TRACK_ORDER_PREVIOUS_MIXER_NAME.to_owned());
    expected.insert(PROJECT_STAGING_NAME.to_owned());
    expected.insert(PREVIOUS_PROJECT_NAME.to_owned());
    for index in 0..count {
        expected.insert(track_order_staging_name(index));
        expected.insert(track_order_previous_staging_name(index));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete track-order transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete track-order transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !expected.contains(&name) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-order transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn validate_audio_track_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let id = audio_track_transaction_id(directory_name)?;
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete audio-track transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete audio-track transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            INTENT_STAGING_NAME
                | INTENT_NAME
                | AUDIO_TRACK_STAGING_NAME
                | AUDIO_TRACK_MIXER_STAGING_NAME
                | AUDIO_TRACK_PREVIOUS_MIXER_NAME
                | PROJECT_STAGING_NAME
                | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-track transaction contains an unknown entry",
            ));
        }
    }
    if !missing_file(transaction, INTENT_NAME)? {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "audio-track transaction intent")?;
        let intent: TransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse audio-track transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-audio-track-publication-intent"
            || intent.format_version != 1
            || intent.id != id
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-track transaction intent is invalid",
            ));
        }
    }
    Ok(())
}

fn audio_track_outcome(
    tx: &AudioTrackTransactionRecord,
) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
    Ok(AudioTrackPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.track_id.clone(),
        track_path: tx.track.path.clone(),
    })
}

fn audio_track_removal_outcome(
    tx: &AudioTrackTransactionRecord,
) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
    if tx.operation != AudioTrackTransactionOperation::Removal {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track transaction is not a removal",
        ));
    }
    Ok(AudioTrackRemovalOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.track_id.clone(),
    })
}

fn track_order_outcome(
    tx: &TrackOrderTransactionRecord,
) -> Result<TrackOrderPublicationOutcome, ProjectResourcePublicationError> {
    Ok(TrackOrderPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
    })
}

fn cleanup_audio_track_temporaries(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &AudioTrackTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if let Ok(parent) = open_relative_parent(root, &tx.mixer.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".audio-track-mixer-{}.json", tx.id),
            decode_digest(&tx.mixer.sha256)?,
        )?;
        remove_optional_matching_file(
            &parent,
            &format!(".audio-track-mixer-rollback-{}.json", tx.id),
            decode_digest(&tx.previous_mixer.sha256)?,
        )?;
    }
    if let Ok(parent) = open_relative_parent(root, &tx.track.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".audio-track-rollback-{}.json", tx.id),
            decode_digest(&tx.track.sha256)?,
        )?;
    }
    remove_optional_matching_file(
        backups,
        &format!(".audio-track-project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    remove_optional_matching_file(
        backups,
        &format!(".project-rollback-{}.json", tx.id),
        digest(&previous),
    )
}

fn recover_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<ProjectResourcePublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_transaction(transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = match read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES) {
        Ok(bytes) => digest(&bytes),
        Err(error) if error_is_not_found(&error) => {
            restore_previous_manifest(root, backups, transaction, &tx.id, before)?;
            before
        }
        Err(error) => return Err(error),
    };
    if active == after {
        if tx.operation == TransactionOperation::Removal {
            finish_removed_resource(root, tx)?;
        }
        cleanup_publication_temporaries(root, backups, transaction, tx)
            .and_then(|()| verify_committed(root_path, root, after))
            .map_err(|source| ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message: "committed resource publication could not be verified or fully recovered"
                    .to_owned(),
                published: true,
                source: Some(Box::new(source)),
            })?;
        return Ok(ProjectResourcePublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource recovery found an unrelated project identity",
        ));
    }
    if tx.operation == TransactionOperation::Removal {
        rollback_removed_resource(root, transaction, tx)?;
    } else {
        rollback_resource(root, transaction, tx)?;
    }
    cleanup_publication_temporaries(root, backups, transaction, tx)?;
    remove_created_resource_parent(root, tx)?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    verify_retained_project_candidate(root, &previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "rolled-back resource project does not verify",
            error,
        )
    })?;
    Ok(ProjectResourcePublicationRecoveryDisposition::RolledBack)
}

fn finish_removed_resource(
    root: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = match open_relative_parent(root, &tx.resource.path) {
        Ok(parent) => Some(parent),
        Err(error) if tx.removed_parent_directory && error_is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    let Some(parent) = parent else {
        return Ok(());
    };
    let current = read_optional_file(&parent, file_name(&tx.resource.path)?)?;
    if let Some(bytes) = current {
        if digest(&bytes) != decode_digest(&tx.resource.sha256)? {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "committed resource removal refuses unrelated active content",
            ));
        }
        parent
            .remove_file(file_name(&tx.resource.path)?)
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "finish committed resource removal",
                    error,
                )
            })?;
        sync_dir(&parent)?;
    }
    if !tx.removed_parent_directory {
        return Ok(());
    }
    drop(parent);
    let directory = removal_directory(tx)?;
    match root.remove_dir(directory) {
        Ok(()) => sync_dir(root),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove empty project-resource directory",
            error,
        )),
    }
}

fn rollback_removed_resource(
    root: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if tx.removed_parent_directory {
        let directory = removal_directory(tx)?;
        match root.create_dir(directory) {
            Ok(()) => sync_dir(root)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "restore removed project-resource directory",
                    error,
                ));
            }
        }
    }
    let parent = open_relative_parent(root, &tx.resource.path)?;
    let current = read_optional_file(&parent, file_name(&tx.resource.path)?)?;
    let before = decode_digest(&tx.resource.sha256)?;
    match current.as_deref().map(digest) {
        Some(found) if found == before => Ok(()),
        Some(_) => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource-removal rollback refuses unrelated active content",
        )),
        None => publish_staged_file(
            transaction,
            RESOURCE_STAGING_NAME,
            &parent,
            &parent,
            file_name(&tx.resource.path)?,
            &format!(".resource-rollback-{}.json", tx.id),
            "removed project resource",
        ),
    }
}

fn rollback_resource(
    root: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = match open_relative_parent(root, &tx.resource.path) {
        Ok(parent) => parent,
        Err(error) if tx.previous_resource.is_none() && error_is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let current = read_optional_file(&parent, file_name(&tx.resource.path)?)?;
    let current_digest = current.as_deref().map(digest);
    let after = decode_digest(&tx.resource.sha256)?;
    let Some(previous_resource) = &tx.previous_resource else {
        return match current_digest {
            None => Ok(()),
            Some(found) if found != after => Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "resource rollback refuses unrelated published content",
            )),
            Some(_) => {
                parent
                    .remove_file(file_name(&tx.resource.path)?)
                    .map_err(|error| {
                        caused(
                            ProjectResourcePublicationErrorKind::Recovery,
                            "remove transaction-created project resource",
                            error,
                        )
                    })?;
                sync_dir(&parent)
            }
        };
    };
    let before = decode_digest(&previous_resource.sha256)?;
    match current_digest {
        Some(found) if found == before => return Ok(()),
        Some(found) if found != after => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "resource rollback refuses unrelated published content",
            ));
        }
        Some(_) | None => {}
    }
    publish_staged_file(
        transaction,
        PREVIOUS_RESOURCE_NAME,
        &parent,
        &parent,
        file_name(&previous_resource.path)?,
        &format!(".resource-rollback-{}.json", tx.id),
        "previous project resource",
    )
}

fn remove_created_resource_parent(
    root: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if !tx.created_parent_directory {
        return Ok(());
    }
    let directory = transaction_addition_directory(tx)?;
    match root.remove_dir(directory) {
        Ok(()) => sync_dir(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove transaction-created resource directory",
            error,
        )),
    }
}

fn transaction_addition_directory(
    tx: &TransactionRecord,
) -> Result<&'static str, ProjectResourcePublicationError> {
    match tx.resource_role.as_str() {
        "arrangement" if addition_path_matches(tx, "arrangement") => Ok("arrangement"),
        "scene" if addition_path_matches(tx, "scenes") => Ok("scenes"),
        "automation-take" if addition_path_matches(tx, "takes") => Ok("takes"),
        "device-chain" if addition_path_matches(tx, "devices") => Ok("devices"),
        _ => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource addition transaction directory is invalid",
        )),
    }
}

fn validate_staged_transaction(
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let resource = read_bounded(transaction, RESOURCE_STAGING_NAME, MAX_JSON_BYTES)?;
    validate_canonical_resource(&resource).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged project resource is not canonical",
            error,
        )
    })?;
    let previous_resource = tx
        .previous_resource
        .as_ref()
        .map(|previous| {
            let bytes = read_bounded(transaction, PREVIOUS_RESOURCE_NAME, MAX_JSON_BYTES)?;
            validate_canonical_resource(&bytes).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "staged previous project resource is not canonical",
                    error,
                )
            })?;
            if bytes.len() as u64 != previous.byte_length
                || digest(&bytes) != decode_digest(&previous.sha256)?
                || tx.resource.path != previous.path
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "staged previous resource bytes do not match the transaction record",
                ));
            }
            Ok(bytes)
        })
        .transpose()?;
    if tx.previous_resource.is_none() && !missing_file(transaction, PREVIOUS_RESOURCE_NAME)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource addition transaction contains unexpected previous-resource evidence",
        ));
    }
    if resource.len() as u64 != tx.resource.byte_length
        || digest(&resource) != decode_digest(&tx.resource.sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged resource bytes do not match the transaction record",
        ));
    }
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged project manifest identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous project manifest is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate project manifest is invalid",
            error,
        )
    })?;
    if previous_manifest.project_id() != candidate_manifest.project_id()
        || previous_manifest.revision() != tx.before_project_revision
        || candidate_manifest.revision() != tx.after_project_revision
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource transaction project revisions are inconsistent",
        ));
    }
    validate_transaction_descriptors(
        &previous_manifest,
        &candidate_manifest,
        previous_resource.as_deref(),
        &resource,
        tx,
    )
}

fn validate_transaction_descriptors(
    previous_manifest: &ProjectManifest,
    candidate_manifest: &ProjectManifest,
    previous_bytes: Option<&[u8]>,
    candidate_bytes: &[u8],
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if tx.operation == TransactionOperation::Removal {
        let previous = manifest_resource(previous_manifest, &tx.resource_id)?;
        if role_name(previous.role()) != tx.resource_role
            || previous.path() != tx.resource.path
            || previous.byte_length() != candidate_bytes.len() as u64
            || previous.sha256() != hex_digest(digest(candidate_bytes))
            || candidate_manifest
                .resources()
                .iter()
                .any(|descriptor| descriptor.id() == tx.resource_id)
            || previous_manifest.resources().len()
                != candidate_manifest.resources().len().saturating_add(1)
            || !candidate_manifest.resources().iter().eq(previous_manifest
                .resources()
                .iter()
                .filter(|descriptor| descriptor.id() != tx.resource_id))
            || previous_bytes.is_some()
            || tx.previous_resource.is_some()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "removed resource descriptor does not match the transaction",
            ));
        }
        return Ok(());
    }
    let candidate = manifest_resource(candidate_manifest, &tx.resource_id)?;
    if candidate.path() != tx.resource.path
        || candidate.byte_length() != candidate_bytes.len() as u64
        || candidate.sha256() != hex_digest(digest(candidate_bytes))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource descriptors do not match the transaction",
        ));
    }
    if let (Some(previous_bytes), Some(previous_file)) =
        (previous_bytes, tx.previous_resource.as_ref())
    {
        let previous = manifest_resource(previous_manifest, &tx.resource_id)?;
        if role_name(previous.role()) != tx.resource_role
            || candidate.role() != previous.role()
            || previous.path() != previous_file.path
            || previous.byte_length() != previous_bytes.len() as u64
            || previous.sha256() != hex_digest(digest(previous_bytes))
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "previous resource descriptor does not match the transaction",
            ));
        }
    } else if previous_bytes.is_some() || tx.previous_resource.is_some() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource transaction previous-state evidence is inconsistent",
        ));
    } else if role_name(candidate.role()) != tx.resource_role
        || previous_manifest
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == tx.resource_id)
        || previous_manifest.resources().len().checked_add(1)
            != Some(candidate_manifest.resources().len())
        || previous_manifest.resources()
            != &candidate_manifest.resources()[..previous_manifest.resources().len()]
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "added resource descriptor does not match the transaction",
        ));
    }
    Ok(())
}

fn manifest_resource<'a>(
    manifest: &'a ProjectManifest,
    resource_id: &str,
) -> Result<&'a ResourceDescriptor, ProjectResourcePublicationError> {
    let mut matching = manifest
        .resources()
        .iter()
        .filter(|descriptor| descriptor.id() == resource_id);
    let descriptor = matching.next().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "transaction resource descriptor is missing",
        )
    })?;
    if matching.next().is_some() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "transaction resource descriptor is ambiguous",
        ));
    }
    Ok(descriptor)
}

fn read_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<TransactionRecord, ProjectResourcePublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "resource transaction intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse resource transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "resource transaction")?;
    let tx: TransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse resource transaction",
            error,
        )
    })?;
    let expected_id = transaction_id(directory_name)?;
    if intent.format != "hyper-project-resource-publication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || tx.format != "hyper-project-resource-publication-transaction"
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || tx.id != expected_id
        || tx.resource_id.is_empty()
        || tx.resource.byte_length == 0
        || tx.resource.byte_length > MAX_JSON_FILE_BYTES
        || decode_digest(&tx.resource.sha256).is_err()
        || tx.previous_resource.as_ref().is_some_and(|previous| {
            tx.resource.path != previous.path
                || previous.byte_length == 0
                || previous.byte_length > MAX_JSON_FILE_BYTES
                || decode_digest(&previous.sha256).is_err()
        })
        || tx.created_parent_directory && tx.previous_resource.is_some()
        || tx.operation == TransactionOperation::Publication
            && (tx.removed_parent_directory
                || tx.previous_resource.is_none() && !is_valid_addition_destination(&tx))
        || tx.operation == TransactionOperation::Removal
            && (tx.previous_resource.is_some()
                || tx.created_parent_directory
                || removal_directory(&tx).is_err())
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource publication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn removal_directory(
    tx: &TransactionRecord,
) -> Result<&'static str, ProjectResourcePublicationError> {
    match tx.resource_role.as_str() {
        "scene" if addition_path_matches(tx, "scenes") => Ok("scenes"),
        "automation-take" if addition_path_matches(tx, "takes") => Ok("takes"),
        "device-chain" if addition_path_matches(tx, "devices") => Ok("devices"),
        _ => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource removal transaction directory is invalid",
        )),
    }
}

fn is_valid_addition_destination(tx: &TransactionRecord) -> bool {
    matches!(tx.resource_role.as_str(), "arrangement") && addition_path_matches(tx, "arrangement")
        || matches!(tx.resource_role.as_str(), "scene") && addition_path_matches(tx, "scenes")
        || matches!(tx.resource_role.as_str(), "automation-take")
            && addition_path_matches(tx, "takes")
        || matches!(tx.resource_role.as_str(), "device-chain")
            && addition_path_matches(tx, "devices")
}

fn addition_path_matches(tx: &TransactionRecord, directory: &str) -> bool {
    tx.resource.path == format!("{directory}/{}.json", tx.resource_id)
}

fn validate_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let id = transaction_id(directory_name)?;
    let mut files = BTreeSet::new();
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete resource transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete resource transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            INTENT_STAGING_NAME
                | INTENT_NAME
                | RESOURCE_STAGING_NAME
                | PREVIOUS_RESOURCE_NAME
                | PROJECT_STAGING_NAME
                | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete resource transaction contains an unknown entry",
            ));
        }
        files.insert(name);
    }
    if files.contains(INTENT_NAME) {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "resource transaction intent")?;
        let intent: TransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse resource transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-project-resource-publication-intent"
            || intent.format_version != 1
            || intent.id != id
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete resource transaction intent is invalid",
            ));
        }
    }
    Ok(())
}

fn outcome_from_transaction(
    tx: &TransactionRecord,
    changed: bool,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    Ok(ProjectResourcePublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        resource_id: tx.resource_id.clone(),
        resource_path: tx.resource.path.clone(),
        changed,
        previous_resource: None,
    })
}

fn verify_committed(
    root_path: &Path,
    retained_root: &Dir,
    expected: [u8; 32],
) -> Result<(), ProjectResourcePublicationError> {
    let committed = verify_project_package(root_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "committed resource project does not verify",
            error,
        )
    })?;
    if !same_directory(retained_root, &committed.root_dir).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "compare committed resource project root",
            error,
        )
    })? || *committed.manifest_sha256() != expected
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "committed resource project identity changed",
        ));
    }
    Ok(())
}

fn restore_previous_manifest(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    id: &str,
    expected: [u8; 32],
) -> Result<(), ProjectResourcePublicationError> {
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&previous, "previous project manifest")?;
    ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "previous project manifest is invalid",
            error,
        )
    })?;
    if digest(&previous) != expected {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    publish_staged_file(
        transaction,
        PREVIOUS_PROJECT_NAME,
        backups,
        root,
        MANIFEST_NAME,
        &format!(".project-rollback-{id}.json"),
        "previous project manifest",
    )?;
    sync_dir(backups)
}

fn publish_staged_file(
    transaction: &Dir,
    staged_name: &str,
    temporary_directory: &Dir,
    destination: &Dir,
    destination_name: &str,
    temporary_name: &str,
    label: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, staged_name, MAX_JSON_BYTES)?;
    write_exclusive_synced(temporary_directory, temporary_name, &bytes)?;
    sync_dir(temporary_directory)?;
    temporary_directory
        .rename(temporary_name, destination, destination_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                format!("publish {label}"),
                error,
            )
        })?;
    sync_dir(destination)
}

fn cleanup_publication_temporaries(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if let Ok(parent) = open_relative_parent(root, &tx.resource.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".resource-{}.json", tx.id),
            decode_digest(&tx.resource.sha256)?,
        )?;
        if let Some(previous) = &tx.previous_resource {
            remove_optional_matching_file(
                &parent,
                &format!(".resource-rollback-{}.json", tx.id),
                decode_digest(&previous.sha256)?,
            )?;
        } else if tx.operation == TransactionOperation::Removal {
            remove_optional_matching_file(
                &parent,
                &format!(".resource-rollback-{}.json", tx.id),
                decode_digest(&tx.resource.sha256)?,
            )?;
        }
    }
    remove_optional_matching_file(
        backups,
        &format!(".project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    remove_optional_matching_file(
        backups,
        &format!(".project-rollback-{}.json", tx.id),
        digest(&previous),
    )
}

fn remove_optional_matching_file(
    directory: &Dir,
    name: &str,
    expected: [u8; 32],
) -> Result<(), ProjectResourcePublicationError> {
    let Some(bytes) = read_optional_file(directory, name)? else {
        return Ok(());
    };
    if digest(&bytes) != expected {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource recovery refuses an unrelated temporary file",
        ));
    }
    directory.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove resource publication temporary file",
            error,
        )
    })?;
    sync_dir(directory)
}

fn open_relative_parent(
    root: &Dir,
    relative: &str,
) -> Result<Dir, ProjectResourcePublicationError> {
    let mut segments = relative.split('/').peekable();
    let mut directory = root.try_clone().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "retain project root",
            error,
        )
    })?;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            return Ok(directory);
        }
        let metadata = directory.symlink_metadata(segment).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect resource path ancestor",
                error,
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "resource path ancestor is not a real directory",
            ));
        }
        directory = directory.open_dir_nofollow(segment).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open resource path ancestor",
                error,
            )
        })?;
    }
    Err(failure(
        ProjectResourcePublicationErrorKind::Integrity,
        "resource path has no file component",
    ))
}

fn file_name(relative: &str) -> Result<&str, ProjectResourcePublicationError> {
    relative
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "resource path has no file name",
            )
        })
}

fn read_optional_file(
    directory: &Dir,
    name: &str,
) -> Result<Option<Vec<u8>>, ProjectResourcePublicationError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect project resource",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project resource is not a regular non-link file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project resource",
            error,
        )
    })?;
    read_open_stable(file.into_std(), name, MAX_JSON_FILE_BYTES)
        .map(Some)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "read stable project resource",
                error,
            )
        })
}

fn read_bounded(
    directory: &Dir,
    name: &str,
    maximum: usize,
) -> Result<Vec<u8>, ProjectResourcePublicationError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("open {name}"),
            error,
        )
    })?;
    read_open_stable(file.into_std(), name, maximum as u64).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            format!("read stable {name}"),
            error,
        )
    })
}

fn write_exclusive_synced(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("create {name}"),
            error,
        )
    })?;
    file.write_all(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("write {name}"),
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("sync {name}"),
            error,
        )
    })
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir, ProjectResourcePublicationError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                format!("create {name} directory"),
                error,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("open {name} directory"),
            error,
        )
    })
}

fn open_optional_dir(
    parent: &Dir,
    name: &str,
) -> Result<Option<Dir>, ProjectResourcePublicationError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("open {name} directory"),
            error,
        )),
    }
}

fn sync_dir(directory: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open directory for synchronization",
                error,
            )
        })?
        .sync_all()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "sync directory",
                error,
            )
        })
}

fn transaction_id(directory_name: &str) -> Result<String, ProjectResourcePublicationError> {
    let id = directory_name
        .strip_prefix(TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "resource transaction directory name is invalid",
            )
        })?;
    Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource transaction identifier is invalid",
            error,
        )
    })?;
    Ok(id.to_owned())
}

fn audio_track_transaction_id(
    directory_name: &str,
) -> Result<String, ProjectResourcePublicationError> {
    let id = directory_name
        .strip_prefix(AUDIO_TRACK_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track transaction identifier is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track transaction identifier is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn track_order_transaction_id(
    directory_name: &str,
) -> Result<String, ProjectResourcePublicationError> {
    let id = directory_name
        .strip_prefix(TRACK_ORDER_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-order transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order transaction identity is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order transaction identity is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn require_canonical(bytes: &[u8], label: &str) -> Result<(), ProjectResourcePublicationError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            format!("parse {label}"),
            error,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            format!("canonicalize {label}"),
            error,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
}

fn missing_file(directory: &Dir, name: &str) -> Result<bool, ProjectResourcePublicationError> {
    match directory.symlink_metadata(name) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            format!("inspect {name}"),
            error,
        )),
    }
}

fn error_is_not_found(error: &ProjectResourcePublicationError) -> bool {
    error.source().is_some_and(|source| {
        source
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
            || source
                .downcast_ref::<super::StorageError>()
                .and_then(Error::source)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
    })
}

fn clear_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    for name in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        RESOURCE_STAGING_NAME,
        PREVIOUS_RESOURCE_NAME,
        PROJECT_STAGING_NAME,
        PREVIOUS_PROJECT_NAME,
        TRANSACTION_NAME,
    ] {
        match transaction.remove_file(name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    format!("remove resource transaction file {name}"),
                    error,
                ));
            }
        }
    }
    sync_dir(transaction)
}

fn clear_audio_track_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    for name in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        AUDIO_TRACK_STAGING_NAME,
        AUDIO_TRACK_MIXER_STAGING_NAME,
        AUDIO_TRACK_PREVIOUS_MIXER_NAME,
        PROJECT_STAGING_NAME,
        PREVIOUS_PROJECT_NAME,
        TRANSACTION_NAME,
    ] {
        match transaction.remove_file(name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    format!("remove audio-track transaction file {name}"),
                    error,
                ));
            }
        }
    }
    sync_dir(transaction)
}

fn clear_track_order_transaction(
    transaction: &Dir,
    track_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let mut names = vec![
        INTENT_STAGING_NAME.to_owned(),
        INTENT_NAME.to_owned(),
        TRACK_ORDER_MIXER_STAGING_NAME.to_owned(),
        TRACK_ORDER_PREVIOUS_MIXER_NAME.to_owned(),
        PROJECT_STAGING_NAME.to_owned(),
        PREVIOUS_PROJECT_NAME.to_owned(),
        TRANSACTION_NAME.to_owned(),
    ];
    names
        .try_reserve(track_count.saturating_mul(2))
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "allocate track-order transaction cleanup names",
            )
        })?;
    for index in 0..track_count {
        names.push(track_order_staging_name(index));
        names.push(track_order_previous_staging_name(index));
    }
    for name in names {
        match transaction.remove_file(&name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    format!("remove track-order transaction file {name}"),
                    error,
                ));
            }
        }
    }
    sync_dir(transaction)
}

fn track_order_transaction_count(
    transaction: &Dir,
) -> Result<usize, ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "track-order transaction intent")?;
    let intent: TrackOrderTransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-order transaction intent for cleanup",
            error,
        )
    })?;
    if intent.format != "hyper-track-order-publication-intent"
        || intent.format_version != 1
        || intent.track_count == 0
        || intent.track_count > MAX_ENTRIES
        || !intent.extensions.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order transaction intent is invalid during cleanup",
        ));
    }
    Ok(intent.track_count)
}

#[cfg(not(windows))]
fn clear_and_remove_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".project-resource-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine resource transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined resource transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined resource transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove resource transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(not(windows))]
fn clear_and_remove_audio_track_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".audio-track-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine audio-track transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined audio-track transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined audio-track transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_audio_track_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-track transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(not(windows))]
fn clear_and_remove_track_order_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".track-order-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine track-order transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined track-order transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined track-order transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-order transaction changed before cleanup",
        ));
    }
    drop(moved);
    let count = track_order_transaction_count(&transaction)?;
    clear_track_order_transaction(&transaction, count)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove track-order transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove resource transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_audio_track_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    clear_audio_track_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-track transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_track_order_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let count = track_order_transaction_count(&transaction)?;
    clear_track_order_transaction(&transaction, count)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove track-order transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex_digest(value: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn decode_digest(value: &str) -> Result<[u8; 32], ProjectResourcePublicationError> {
    if value.len() != 64 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "SHA-256 digest is invalid",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "SHA-256 digest is invalid",
                error,
            )
        })?;
        output[index] = u8::from_str_radix(text, 16).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "SHA-256 digest is invalid",
                error,
            )
        })?;
    }
    Ok(output)
}

fn publication_recovery_failure(
    unpublished_message: &str,
    published_message: &str,
    recovery_error: ProjectResourcePublicationError,
) -> ProjectResourcePublicationError {
    let published = recovery_error.published();
    ProjectResourcePublicationError {
        kind: ProjectResourcePublicationErrorKind::Recovery,
        message: if published {
            published_message
        } else {
            unpublished_message
        }
        .to_owned(),
        published,
        source: Some(Box::new(recovery_error)),
    }
}

fn failure(
    kind: ProjectResourcePublicationErrorKind,
    message: impl Into<String>,
) -> ProjectResourcePublicationError {
    ProjectResourcePublicationError {
        kind,
        message: message.into(),
        published: false,
        source: None,
    }
}

fn caused(
    kind: ProjectResourcePublicationErrorKind,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> ProjectResourcePublicationError {
    ProjectResourcePublicationError {
        kind,
        message: message.into(),
        published: false,
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334468";
    const NEW_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334469";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334470";
    const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334471";
    const SECOND_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334472";
    const AUTOMATION_TAKE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334479";
    const AUTOMATION_LANE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334481";
    const DEVICE_CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334482";
    const DEVICE_INSTANCE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334483";
    const GROUP_BUS_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334484";
    const RETURN_BUS_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334485";
    const RETURN_SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334486";

    struct Fixture {
        root: PathBuf,
        original: Vec<u8>,
        replacement: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-project-resource-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("mixer")).unwrap();
            fs::create_dir_all(root.join("timeline")).unwrap();
            let timeline = serde_json_canonicalizer::to_vec(&json!({
                "format":"fixture-timeline"
            }))
            .unwrap();
            fs::write(root.join("timeline/timeline.json"), &timeline).unwrap();
            let original = serde_json_canonicalizer::to_vec(&json!({
                "format":"fixture-mixer", "gain_millionths":1_000_000
            }))
            .unwrap();
            let replacement = serde_json_canonicalizer::to_vec(&json!({
                "format":"fixture-mixer", "gain_millionths":750_000
            }))
            .unwrap();
            fs::write(root.join("mixer/mixer.json"), &original).unwrap();
            let manifest = json!({
                "format":"hyper-project", "format_version":1,
                "project_id":PROJECT_ID, "revision":1,
                "name":"Resource publication fixture", "sample_rate_hz":48000,
                "required_capabilities":{
                    "format":"hyper-hardware-capabilities", "format_version":1,
                    "audio_inputs":0, "audio_outputs":0, "cv_inputs":0,
                    "cv_outputs":0, "gate_outputs":0, "has_clock_input":false,
                    "has_reset_input":false, "has_clock_output":false,
                    "usb_mode":"none", "has_ethernet":false, "extensions":{}
                },
                "resources":[
                    {
                        "id":TIMELINE_ID, "role":"timeline",
                        "path":"timeline/timeline.json", "media_type":"application/json",
                        "byte_length":timeline.len(), "sha256":hex_digest(digest(&timeline)),
                        "extensions":{}
                    },
                    {
                        "id":MIXER_ID, "role":"mixer", "path":"mixer/mixer.json",
                        "media_type":"application/json", "byte_length":original.len(),
                        "sha256":hex_digest(digest(&original)), "extensions":{}
                    }
                ],
                "assets":[], "extensions":{}
            });
            fs::write(
                root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            Self {
                root,
                original,
                replacement,
            }
        }

        fn identity(&self) -> [u8; 32] {
            *verify_project_package(&self.root)
                .unwrap()
                .manifest_sha256()
        }

        fn request(&self) -> ProjectResourcePublicationRequest<'_> {
            ProjectResourcePublicationRequest {
                expected_project_sha256: self.identity(),
                resource_id: MIXER_ID,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &self.replacement,
            }
        }

        fn transaction_count(&self) -> usize {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .count()
        }

        fn transaction_path(&self) -> PathBuf {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .unwrap()
                .path()
        }

        fn transaction_id(&self) -> String {
            self.transaction_path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .strip_prefix(TRANSACTION_PREFIX)
                .unwrap()
                .to_owned()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct AudioTrackFixture {
        parent: PathBuf,
        root: PathBuf,
        track: Vec<u8>,
        mixer: Vec<u8>,
        original_mixer: Vec<u8>,
        mixer_id: String,
    }

    impl AudioTrackFixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-audio-track-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Session",
                    project_name: "Session",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let package = verify_project_package(&root).unwrap();
            let mixer_descriptor = package
                .manifest()
                .resources()
                .iter()
                .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
                .unwrap();
            let original_mixer = package.read_resource(mixer_descriptor.id()).unwrap();
            let mixer_id = mixer_descriptor.id().to_owned();
            let mut mixer: Value = serde_json::from_slice(&original_mixer).unwrap();
            mixer["channels"].as_array_mut().unwrap().push(json!({
                "track_id":NEW_TRACK_ID,
                "gain_millionths":1_000_000,
                "pan_millionths":0,
                "mute":false,
                "solo":false,
                "destination":{"kind":"main"},
                "sends":[]
            }));
            let mixer = serde_json_canonicalizer::to_vec(&mixer).unwrap();
            let track = serde_json_canonicalizer::to_vec(&json!({
                "format":"hyper-track",
                "format_version":1,
                "id":NEW_TRACK_ID,
                "project_id":package.manifest().project_id(),
                "name":"Vocal",
                "kind":"audio",
                "order":1,
                "active":true,
                "clip_ids":[],
                "extensions":{}
            }))
            .unwrap();
            Self {
                parent,
                root,
                track,
                mixer,
                original_mixer,
                mixer_id,
            }
        }

        fn request(&self) -> AudioTrackPublicationRequest<'_> {
            let package = verify_project_package(&self.root).unwrap();
            AudioTrackPublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                track_id: NEW_TRACK_ID,
                track_bytes: &self.track,
                mixer_id: &self.mixer_id,
                mixer_bytes: &self.mixer,
            }
        }

        fn removal_request(&self) -> AudioTrackRemovalRequest<'_> {
            let package = verify_project_package(&self.root).unwrap();
            AudioTrackRemovalRequest {
                expected_project_sha256: *package.manifest_sha256(),
                track_id: NEW_TRACK_ID,
                mixer_id: &self.mixer_id,
                mixer_bytes: &self.original_mixer,
            }
        }

        fn transaction_count(&self) -> usize {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(AUDIO_TRACK_TRANSACTION_PREFIX)
                })
                .count()
        }

        fn transaction_path(&self) -> PathBuf {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(AUDIO_TRACK_TRANSACTION_PREFIX)
                })
                .unwrap()
                .path()
        }

        fn transaction_id(&self) -> String {
            self.transaction_path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .strip_prefix(AUDIO_TRACK_TRANSACTION_PREFIX)
                .unwrap()
                .to_owned()
        }
    }

    impl Drop for AudioTrackFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    fn prepared_track_order_documents(
        fixture: &AudioTrackFixture,
    ) -> ([u8; 32], hyper_playback_json::MixerStripReorderDocuments) {
        let package = verify_project_package(&fixture.root).unwrap();
        let mixer = package.read_resource(&fixture.mixer_id).unwrap();
        prepared_track_order_documents_with_mixer(fixture, &mixer)
    }

    fn prepared_track_order_documents_with_mixer(
        fixture: &AudioTrackFixture,
        mixer: &[u8],
    ) -> ([u8; 32], hyper_playback_json::MixerStripReorderDocuments) {
        use hyper_playback_json::{
            LoaderLimits, PlaybackJsonInput, load as load_playback, serialize_mixer_strip_reorder,
        };

        let package = verify_project_package(&fixture.root).unwrap();
        let identity = *package.manifest_sha256();
        let timeline = package
            .manifest()
            .resources()
            .iter()
            .find(|resource| resource.role() == ResourceRole::Timeline)
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .unwrap();
        let track_descriptors: Vec<_> = package
            .manifest()
            .resources()
            .iter()
            .filter(|resource| resource.role() == ResourceRole::Track)
            .collect();
        let track_ids: Vec<_> = track_descriptors
            .iter()
            .map(|resource| resource.id().to_owned())
            .collect();
        let tracks: Vec<_> = track_descriptors
            .iter()
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .collect();
        let track_slices: Vec<_> = tracks.iter().map(Vec::as_slice).collect();
        let project = load_playback(
            PlaybackJsonInput {
                project_id: package.manifest().project_id(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: &timeline,
                tracks: &track_slices,
                clips: &[],
                scenes: &[],
                arrangement: None,
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let source_tracks: Vec<_> = track_ids
            .iter()
            .zip(&tracks)
            .map(|(id, bytes)| (id.as_str(), bytes.as_slice()))
            .collect();
        let documents = serialize_mixer_strip_reorder(
            &project,
            &[],
            &source_tracks,
            mixer,
            &fixture.mixer_id,
            NEW_TRACK_ID,
            0,
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        (identity, documents)
    }

    fn prepared_routing_bus_documents(
        fixture: &AudioTrackFixture,
        kind: hyper_playback_json::RoutingBusKind,
        track_id: &str,
        send_id: Option<&str>,
    ) -> (
        [u8; 32],
        String,
        hyper_playback_json::RoutingBusCreationDocuments,
    ) {
        use hyper_playback_json::{
            LoaderLimits, PlaybackJsonInput, load as load_playback, serialize_routing_bus_creation,
        };

        let package = verify_project_package(&fixture.root).unwrap();
        let identity = *package.manifest_sha256();
        let timeline = package
            .manifest()
            .resources()
            .iter()
            .find(|resource| resource.role() == ResourceRole::Timeline)
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .unwrap();
        let track_descriptors: Vec<_> = package
            .manifest()
            .resources()
            .iter()
            .filter(|resource| resource.role() == ResourceRole::Track)
            .collect();
        let track_ids: Vec<_> = track_descriptors
            .iter()
            .map(|resource| resource.id().to_owned())
            .collect();
        let tracks: Vec<_> = track_descriptors
            .iter()
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .collect();
        let track_slices: Vec<_> = tracks.iter().map(Vec::as_slice).collect();
        let project = load_playback(
            PlaybackJsonInput {
                project_id: package.manifest().project_id(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: &timeline,
                tracks: &track_slices,
                clips: &[],
                scenes: &[],
                arrangement: None,
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let source_track_id = track_ids[0].clone();
        let source_track_ids = [source_track_id.as_str()];
        let no_send_ids: [&str; 0] = [];
        let return_send_ids = [send_id.unwrap_or_default()];
        let mixer = package.read_resource(&fixture.mixer_id).unwrap();
        let documents = serialize_routing_bus_creation(
            &project,
            &[],
            &mixer,
            &fixture.mixer_id,
            kind,
            track_id,
            if kind == hyper_playback_json::RoutingBusKind::Group {
                "Group"
            } else {
                "Return"
            },
            &source_track_ids,
            if kind == hyper_playback_json::RoutingBusKind::Group {
                &no_send_ids
            } else {
                &return_send_ids
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        (identity, source_track_id, documents)
    }

    fn assert_active_mixer_loads(root: &Path, mixer_id: &str) {
        use hyper_playback_json::{LoaderLimits, PlaybackJsonInput, load as load_playback};

        let package = verify_project_package(root).unwrap();
        let timeline = package
            .manifest()
            .resources()
            .iter()
            .find(|resource| resource.role() == ResourceRole::Timeline)
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .unwrap();
        let tracks: Vec<_> = package
            .manifest()
            .resources()
            .iter()
            .filter(|resource| resource.role() == ResourceRole::Track)
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .collect();
        let track_slices: Vec<_> = tracks.iter().map(Vec::as_slice).collect();
        let project = load_playback(
            PlaybackJsonInput {
                project_id: package.manifest().project_id(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: &timeline,
                tracks: &track_slices,
                clips: &[],
                scenes: &[],
                arrangement: None,
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = package.read_resource(mixer_id).unwrap();
        project
            .load_mixer(&mixer, &[], LoaderLimits::VERSION_1)
            .unwrap();
    }

    struct ArrangementFixture {
        parent: PathBuf,
        root: PathBuf,
        arrangement: Vec<u8>,
    }

    impl ArrangementFixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-arrangement-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Session",
                    project_name: "Session",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let project_id = created.project().manifest().project_id();
            let arrangement = serde_json_canonicalizer::to_vec(&json!({
                "format": "hyper-arrangement",
                "format_version": 1,
                "id": ARRANGEMENT_ID,
                "project_id": project_id,
                "name": "Main",
                "enabled": true,
                "placements": [],
                "scene_launches": [],
                "extensions": {}
            }))
            .unwrap();
            Self {
                parent,
                root,
                arrangement,
            }
        }

        fn request(&self) -> ArrangementPublicationRequest<'_> {
            ArrangementPublicationRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                arrangement_id: ARRANGEMENT_ID,
                arrangement_bytes: &self.arrangement,
            }
        }

        fn resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.arrangement_id,
                expected_role: ResourceRole::Arrangement,
                resource_bytes: request.arrangement_bytes,
            }
        }

        fn transaction_count(&self) -> usize {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .count()
        }
    }

    impl Drop for ArrangementFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    struct SceneFixture {
        parent: PathBuf,
        root: PathBuf,
        scene: Vec<u8>,
    }

    impl SceneFixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-scene-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Session",
                    project_name: "Session",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let project_id = created.project().manifest().project_id();
            let scene = serde_json_canonicalizer::to_vec(&json!({
                "format": "hyper-scene",
                "format_version": 1,
                "id": SCENE_ID,
                "project_id": project_id,
                "name": "Intro",
                "order": 0,
                "enabled": true,
                "launch_quantization": {"mode": "immediate"},
                "slots": [],
                "extensions": {}
            }))
            .unwrap();
            Self {
                parent,
                root,
                scene,
            }
        }

        fn request(&self) -> ScenePublicationRequest<'_> {
            ScenePublicationRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                scene_id: SCENE_ID,
                scene_bytes: &self.scene,
            }
        }

        fn resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.scene_id,
                expected_role: ResourceRole::Scene,
                resource_bytes: request.scene_bytes,
            }
        }

        fn removal_request(&self) -> SceneRemovalRequest<'_> {
            SceneRemovalRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                scene_id: SCENE_ID,
            }
        }

        fn removal_resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.removal_request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.scene_id,
                expected_role: ResourceRole::Scene,
                resource_bytes: &[],
            }
        }

        fn transaction_count(&self) -> usize {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .count()
        }
    }

    impl Drop for SceneFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    struct AutomationTakeFixture {
        parent: PathBuf,
        root: PathBuf,
        take: Vec<u8>,
    }

    impl AutomationTakeFixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-automation-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Session",
                    project_name: "Session",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let project_id = created.project().manifest().project_id();
            let take = serde_json_canonicalizer::to_vec(&json!({
                "format": "hyper-automation-take",
                "format_version": 1,
                "id": AUTOMATION_TAKE_ID,
                "project_id": project_id,
                "name": "Fade out",
                "enabled": true,
                "captured_at_utc": "2026-08-16T09:30:00.000Z",
                "start_tick": 0,
                "end_tick": 8,
                "lanes": [{
                    "id": AUTOMATION_LANE_ID,
                    "target": {"kind": "mixer-main", "parameter": "gain"},
                    "interpolation": "linear",
                    "points": [
                        {"tick": 0, "value": 1_000_000},
                        {"tick": 4, "value": 0}
                    ]
                }],
                "extensions": {}
            }))
            .unwrap();
            Self { parent, root, take }
        }

        fn request(&self) -> AutomationTakePublicationRequest<'_> {
            AutomationTakePublicationRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                take_id: AUTOMATION_TAKE_ID,
                take_bytes: &self.take,
            }
        }

        fn resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.take_id,
                expected_role: ResourceRole::AutomationTake,
                resource_bytes: request.take_bytes,
            }
        }

        fn removal_request(&self) -> AutomationTakeRemovalRequest<'_> {
            AutomationTakeRemovalRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                take_id: AUTOMATION_TAKE_ID,
            }
        }

        fn removal_resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.removal_request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.take_id,
                expected_role: ResourceRole::AutomationTake,
                resource_bytes: &[],
            }
        }

        fn transaction_count(&self) -> usize {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .count()
        }

        fn transaction_path(&self) -> PathBuf {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .unwrap()
                .path()
        }

        fn transaction_id(&self) -> String {
            self.transaction_path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .strip_prefix(TRANSACTION_PREFIX)
                .unwrap()
                .to_owned()
        }
    }

    impl Drop for AutomationTakeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    struct DeviceChainFixture {
        parent: PathBuf,
        root: PathBuf,
        chain: Vec<u8>,
    }

    impl DeviceChainFixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-device-chain-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Session",
                    project_name: "Session",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let project_id = created.project().manifest().project_id();
            let track_id = created
                .project()
                .manifest()
                .resources()
                .iter()
                .find(|resource| resource.role() == ResourceRole::Track)
                .unwrap()
                .id();
            let chain = serde_json_canonicalizer::to_vec(&json!({
                "format": "hyper-device-chain",
                "format_version": 1,
                "id": DEVICE_CHAIN_ID,
                "project_id": project_id,
                "track_id": track_id,
                "devices": [{
                    "id": DEVICE_INSTANCE_ID,
                    "device_id": "org.hyper-module.native-filter",
                    "state_version": 1,
                    "dsp_version": 1,
                    "enabled": true,
                    "state": {
                        "mode": "low-pass",
                        "cutoff_millihertz": 1_000_000,
                        "resonance_millionths": 707_107
                    },
                    "extensions": {}
                }],
                "extensions": {}
            }))
            .unwrap();
            Self {
                parent,
                root,
                chain,
            }
        }

        fn request(&self) -> DeviceChainPublicationRequest<'_> {
            DeviceChainPublicationRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                chain_id: DEVICE_CHAIN_ID,
                chain_bytes: &self.chain,
            }
        }

        fn removal_request(&self) -> DeviceChainRemovalRequest<'_> {
            DeviceChainRemovalRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                chain_id: DEVICE_CHAIN_ID,
            }
        }

        fn removal_resource_request(&self) -> ProjectResourcePublicationRequest<'_> {
            let request = self.removal_request();
            ProjectResourcePublicationRequest {
                expected_project_sha256: request.expected_project_sha256,
                resource_id: request.chain_id,
                expected_role: ResourceRole::DeviceChain,
                resource_bytes: &[],
            }
        }
    }

    impl Drop for DeviceChainFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn publishes_a_new_device_chain_manifest_last() {
        let fixture = DeviceChainFixture::new();
        assert!(!fixture.root.join("devices").exists());

        let published = publish_device_chain(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.resource_id(), DEVICE_CHAIN_ID);
        assert_eq!(
            published.resource_path(),
            format!("devices/{DEVICE_CHAIN_ID}.json")
        );
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            fixture.chain
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        let descriptor = verified
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == DEVICE_CHAIN_ID)
            .unwrap();
        assert_eq!(descriptor.role(), ResourceRole::DeviceChain);
        assert!(
            recover_project_resource_publications(&fixture.root)
                .unwrap()
                .is_empty()
        );
    }

    fn version_two_compressor_chain(fixture: &DeviceChainFixture) -> Vec<u8> {
        let source: Value = serde_json::from_slice(&fixture.chain).unwrap();
        serde_json_canonicalizer::to_vec(&json!({
            "format": "hyper-device-chain",
            "format_version": 2,
            "id": DEVICE_CHAIN_ID,
            "project_id": source["project_id"],
            "track_id": source["track_id"],
            "devices": [{
                "id": DEVICE_INSTANCE_ID,
                "device_id": "org.hyper-module.native-compressor",
                "state_version": 1,
                "dsp_version": 1,
                "enabled": true,
                "state": {
                    "kind": "compressor",
                    "parameters": {
                        "threshold_millidecibels": -18_000,
                        "ratio_millionths": 4_000_000,
                        "attack_microseconds": 10_000,
                        "release_microseconds": 100_000,
                        "makeup_millidecibels": 0
                    }
                },
                "extensions": {}
            }],
            "extensions": {}
        }))
        .unwrap()
    }

    #[test]
    fn publishes_a_version_two_compressor_device_chain_manifest_last() {
        let fixture = DeviceChainFixture::new();
        let chain = version_two_compressor_chain(&fixture);
        let request = DeviceChainPublicationRequest {
            expected_project_sha256: *verify_project_package(&fixture.root)
                .unwrap()
                .manifest_sha256(),
            chain_id: DEVICE_CHAIN_ID,
            chain_bytes: &chain,
        };

        let published = publish_device_chain(&fixture.root, request).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            chain
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .resources()
                .iter()
                .filter(|resource| resource.role() == ResourceRole::DeviceChain)
                .count(),
            1
        );
    }

    #[test]
    fn device_chain_publication_validates_v2_state_and_replacements_before_commit() {
        let fixture = DeviceChainFixture::new();
        let initial_manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let valid = version_two_compressor_chain(&fixture);
        let source: Value = serde_json::from_slice(&valid).unwrap();
        let mut out_of_range = source.clone();
        out_of_range["devices"][0]["state"]["parameters"]["threshold_millidecibels"] =
            json!(-80_001);
        let mut cross_kind = source.clone();
        cross_kind["devices"][0]["device_id"] = json!("org.hyper-module.native-filter");
        let mut unknown_track = source;
        unknown_track["track_id"] = json!("018f47a2-6b2d-7c11-8a44-001122334499");

        for malformed in [out_of_range, cross_kind, unknown_track] {
            let bytes = serde_json_canonicalizer::to_vec(&malformed).unwrap();
            let error = publish_device_chain(
                &fixture.root,
                DeviceChainPublicationRequest {
                    expected_project_sha256: *verify_project_package(&fixture.root)
                        .unwrap()
                        .manifest_sha256(),
                    chain_id: DEVICE_CHAIN_ID,
                    chain_bytes: &bytes,
                },
            )
            .unwrap_err();
            assert_eq!(
                error.kind(),
                ProjectResourcePublicationErrorKind::InvalidResource
            );
            assert_eq!(
                fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
                initial_manifest
            );
            assert!(!fixture.root.join("devices").exists());
        }

        publish_device_chain(
            &fixture.root,
            DeviceChainPublicationRequest {
                expected_project_sha256: *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256(),
                chain_id: DEVICE_CHAIN_ID,
                chain_bytes: &valid,
            },
        )
        .unwrap();
        let manifest_after_addition = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let mut duplicate_track: Value = serde_json::from_slice(&valid).unwrap();
        duplicate_track["id"] = json!("018f47a2-6b2d-7c11-8a44-001122334498");
        duplicate_track["devices"][0]["id"] = json!("018f47a2-6b2d-7c11-8a44-001122334497");
        let duplicate_track = serde_json_canonicalizer::to_vec(&duplicate_track).unwrap();
        let error = publish_device_chain(
            &fixture.root,
            DeviceChainPublicationRequest {
                expected_project_sha256: *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256(),
                chain_id: "018f47a2-6b2d-7c11-8a44-001122334498",
                chain_bytes: &duplicate_track,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            manifest_after_addition
        );
        let mut replacement: Value = serde_json::from_slice(&valid).unwrap();
        replacement["devices"][0]["state"]["parameters"]["threshold_millidecibels"] =
            json!(-17_000);
        let replacement = serde_json_canonicalizer::to_vec(&replacement).unwrap();
        let outcome = publish_project_resource(
            &fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256(),
                resource_id: DEVICE_CHAIN_ID,
                expected_role: ResourceRole::DeviceChain,
                resource_bytes: &replacement,
            },
        )
        .unwrap();
        assert_eq!(outcome.project_revision(), 3);
        assert_eq!(
            DeviceChain::parse(&fs::read(fixture.root.join(outcome.resource_path())).unwrap())
                .unwrap()
                .devices()[0]
                .compressor_state()
                .unwrap()
                .threshold_millidecibels(),
            -17_000
        );
        verify_project_package(&fixture.root).unwrap();
    }

    #[test]
    fn removes_the_last_device_chain_without_leaving_a_resource_directory() {
        let fixture = DeviceChainFixture::new();
        publish_device_chain(&fixture.root, fixture.request()).unwrap();

        let removed = remove_device_chain(&fixture.root, fixture.removal_request()).unwrap();

        assert!(removed.changed());
        assert_eq!(removed.project_revision(), 3);
        assert_eq!(removed.resource_id(), DEVICE_CHAIN_ID);
        assert_eq!(removed.previous_resource(), Some(fixture.chain.as_slice()));
        assert!(!fixture.root.join(removed.resource_path()).exists());
        assert!(!fixture.root.join("devices").exists());
        let verified = verify_project_package(&fixture.root).unwrap();
        assert!(
            verified
                .manifest()
                .resources()
                .iter()
                .all(|resource| resource.id() != DEVICE_CHAIN_ID)
        );
    }

    #[test]
    fn device_chain_removal_recovers_every_durable_phase() {
        for crash_phase in [
            PublicationPhase::Transaction,
            PublicationPhase::ResourceFile,
            PublicationPhase::Resource,
            PublicationPhase::Manifest,
        ] {
            let fixture = DeviceChainFixture::new();
            publish_device_chain(&fixture.root, fixture.request()).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.removal_resource_request(),
                    ProjectResourcePublicationMode::DeviceChainRemoval,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            let committed = crash_phase == PublicationPhase::Manifest;
            assert_eq!(
                recovered[0].disposition(),
                if committed {
                    ProjectResourcePublicationRecoveryDisposition::Committed
                } else {
                    ProjectResourcePublicationRecoveryDisposition::RolledBack
                }
            );
            let verified = verify_project_package(&fixture.root).unwrap();
            assert_eq!(
                verified.manifest().revision(),
                if committed { 3 } else { 2 }
            );
            assert_eq!(
                fixture
                    .root
                    .join("devices")
                    .join(format!("{DEVICE_CHAIN_ID}.json"))
                    .exists(),
                !committed
            );
            assert_eq!(fixture.root.join("devices").exists(), !committed);
        }
    }

    #[test]
    fn publishes_a_new_automation_take_manifest_last() {
        let fixture = AutomationTakeFixture::new();
        assert!(!fixture.root.join("takes").exists());

        let published = publish_automation_take(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.resource_id(), AUTOMATION_TAKE_ID);
        assert_eq!(
            published.resource_path(),
            format!("takes/{AUTOMATION_TAKE_ID}.json")
        );
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            fixture.take
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        let descriptor = verified
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == AUTOMATION_TAKE_ID)
            .unwrap();
        assert_eq!(descriptor.role(), ResourceRole::AutomationTake);
        assert!(
            recover_project_resource_publications(&fixture.root)
                .unwrap()
                .is_empty()
        );

        let error = publish_automation_take(
            &fixture.root,
            AutomationTakePublicationRequest {
                expected_project_sha256: *published.project_identity(),
                ..fixture.request()
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ResourceConflict
        );
    }

    #[test]
    fn removes_the_last_automation_take_without_leaving_a_resource_directory() {
        let fixture = AutomationTakeFixture::new();
        publish_automation_take(&fixture.root, fixture.request()).unwrap();

        let removed = remove_automation_take(&fixture.root, fixture.removal_request()).unwrap();

        assert!(removed.changed());
        assert_eq!(removed.project_revision(), 3);
        assert_eq!(removed.resource_id(), AUTOMATION_TAKE_ID);
        assert_eq!(removed.previous_resource(), Some(fixture.take.as_slice()));
        assert!(!fixture.root.join(removed.resource_path()).exists());
        assert!(!fixture.root.join("takes").exists());
        let verified = verify_project_package(&fixture.root).unwrap();
        assert!(
            verified
                .manifest()
                .resources()
                .iter()
                .all(|resource| resource.id() != AUTOMATION_TAKE_ID)
        );
    }

    #[test]
    fn automation_take_removal_recovers_every_durable_phase() {
        for crash_phase in [
            PublicationPhase::Transaction,
            PublicationPhase::ResourceFile,
            PublicationPhase::Resource,
            PublicationPhase::Manifest,
        ] {
            let fixture = AutomationTakeFixture::new();
            publish_automation_take(&fixture.root, fixture.request()).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.removal_resource_request(),
                    ProjectResourcePublicationMode::AutomationTakeRemoval,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            let committed = crash_phase == PublicationPhase::Manifest;
            assert_eq!(
                recovered[0].disposition(),
                if committed {
                    ProjectResourcePublicationRecoveryDisposition::Committed
                } else {
                    ProjectResourcePublicationRecoveryDisposition::RolledBack
                }
            );
            let verified = verify_project_package(&fixture.root).unwrap();
            assert_eq!(
                verified.manifest().revision(),
                if committed { 3 } else { 2 }
            );
            assert_eq!(
                fixture
                    .root
                    .join("takes")
                    .join(format!("{AUTOMATION_TAKE_ID}.json"))
                    .exists(),
                !committed
            );
            assert_eq!(fixture.root.join("takes").exists(), !committed);
        }
    }

    #[test]
    fn automation_take_removal_recovery_rejects_tampered_evidence_without_mutation() {
        for tamper in ["bytes", "identity", "role", "path"] {
            let fixture = AutomationTakeFixture::new();
            publish_automation_take(&fixture.root, fixture.request()).unwrap();
            let active_path = fixture
                .root
                .join("takes")
                .join(format!("{AUTOMATION_TAKE_ID}.json"));
            let manifest_before = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.removal_resource_request(),
                    ProjectResourcePublicationMode::AutomationTakeRemoval,
                    |phase, _| {
                        assert_ne!(
                            phase,
                            PublicationPhase::Transaction,
                            "simulated process crash"
                        );
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let transaction_path = fixture.transaction_path();
            if tamper == "bytes" {
                fs::write(transaction_path.join(RESOURCE_STAGING_NAME), b"tampered").unwrap();
            } else {
                let record_path = transaction_path.join(TRANSACTION_NAME);
                let mut record: Value =
                    serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
                match tamper {
                    "identity" => record["resource_id"] = SCENE_ID.into(),
                    "role" => record["resource_role"] = "device-chain".into(),
                    "path" => record["resource"]["path"] = "takes/other.json".into(),
                    _ => unreachable!(),
                }
                fs::write(
                    record_path,
                    serde_json_canonicalizer::to_vec(&record).unwrap(),
                )
                .unwrap();
            }

            let error = recover_project_resource_publications(&fixture.root).unwrap_err();
            assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
            assert_eq!(fs::read(&active_path).unwrap(), fixture.take);
            assert_eq!(
                fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
                manifest_before
            );
            assert_eq!(fixture.transaction_count(), 1);
        }
    }

    #[test]
    fn committed_automation_take_removal_warning_uses_neutral_change_wording() {
        let fixture = AutomationTakeFixture::new();
        publish_automation_take(&fixture.root, fixture.request()).unwrap();
        let error = publish_project_resource_observed(
            &fixture.root,
            fixture.removal_resource_request(),
            ProjectResourcePublicationMode::AutomationTakeRemoval,
            |phase, _| {
                if phase != PublicationPhase::Manifest {
                    return Ok(());
                }
                fs::write(
                    fixture
                        .root
                        .join("backups")
                        .join(format!(".project-{}.json", fixture.transaction_id())),
                    b"unrelated cleanup residue",
                )
                .unwrap();
                Err(failure(
                    ProjectResourcePublicationErrorKind::Io,
                    "simulated post-manifest failure",
                ))
            },
        )
        .unwrap_err();

        assert!(error.published());
        assert!(error.to_string().contains("resource change committed"));
        assert!(!error.to_string().contains("published"));
        assert!(!fixture.root.join("takes").exists());
    }

    #[test]
    fn automation_take_publication_rejects_stale_and_mismatched_documents() {
        let fixture = AutomationTakeFixture::new();
        let mut stale = fixture.request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            publish_automation_take(&fixture.root, stale)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );

        let mut wrong: Value = serde_json::from_slice(&fixture.take).unwrap();
        wrong["id"] = SCENE_ID.into();
        let wrong = serde_json_canonicalizer::to_vec(&wrong).unwrap();
        let mut request = fixture.request();
        request.take_bytes = &wrong;
        assert_eq!(
            publish_automation_take(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );

        let pretty =
            serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&fixture.take).unwrap())
                .unwrap();
        let mut request = fixture.request();
        request.take_bytes = &pretty;
        assert_eq!(
            publish_automation_take(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(!fixture.root.join("takes").exists());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn automation_take_crashes_before_manifest_roll_back_the_created_directory() {
        for crash_phase in [PublicationPhase::Transaction, PublicationPhase::Resource] {
            let fixture = AutomationTakeFixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.resource_request(),
                    ProjectResourcePublicationMode::AutomationTakeAddition,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                ProjectResourcePublicationRecoveryDisposition::RolledBack
            );
            assert!(!fixture.root.join("takes").exists());
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                1
            );
        }
    }

    #[test]
    fn replaces_existing_resource_and_identical_save_is_a_no_op() {
        let fixture = Fixture::new();

        let published = publish_project_resource(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.resource_id(), MIXER_ID);
        assert_eq!(published.resource_path(), "mixer/mixer.json");
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            fixture.replacement
        );
        assert_eq!(
            published.previous_resource(),
            Some(fixture.original.as_slice()),
            "a changed replacement carries the bytes it replaced"
        );
        let unchanged = publish_project_resource(
            &fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *published.project_identity(),
                resource_id: MIXER_ID,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &fixture.replacement,
            },
        )
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(
            unchanged.previous_resource(),
            None,
            "an identical republication replaces nothing"
        );
        assert_eq!(unchanged.project_revision(), 2);
        assert!(
            recover_project_resource_publications(&fixture.root)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn publishes_the_first_arrangement_resource() {
        let fixture = ArrangementFixture::new();
        assert!(!fixture.root.join("arrangement").exists());

        let published = publish_arrangement(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.resource_id(), ARRANGEMENT_ID);
        assert_eq!(
            published.resource_path(),
            format!("arrangement/{ARRANGEMENT_ID}.json")
        );
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            fixture.arrangement
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        let arrangements: Vec<_> = verified
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .collect();
        assert_eq!(arrangements.len(), 1);
        assert_eq!(arrangements[0].id(), ARRANGEMENT_ID);
        assert_eq!(fixture.transaction_count(), 0);

        let error = publish_arrangement(
            &fixture.root,
            ArrangementPublicationRequest {
                expected_project_sha256: *published.project_identity(),
                arrangement_id: ARRANGEMENT_ID,
                arrangement_bytes: &fixture.arrangement,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ResourceConflict
        );
    }

    #[test]
    fn publishes_a_new_scene_resource_manifest_last() {
        let fixture = SceneFixture::new();
        assert!(!fixture.root.join("scenes").exists());

        let published = publish_scene(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.resource_id(), SCENE_ID);
        assert_eq!(published.resource_path(), format!("scenes/{SCENE_ID}.json"));
        assert_eq!(
            fs::read(fixture.root.join(published.resource_path())).unwrap(),
            fixture.scene
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        let descriptor = verified
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == SCENE_ID)
            .unwrap();
        assert_eq!(descriptor.role(), ResourceRole::Scene);
        assert_eq!(fixture.transaction_count(), 0);

        let error = publish_scene(
            &fixture.root,
            ScenePublicationRequest {
                expected_project_sha256: *published.project_identity(),
                ..fixture.request()
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ResourceConflict
        );
    }

    #[test]
    fn removes_the_last_scene_without_leaving_a_resource_directory() {
        let fixture = SceneFixture::new();
        publish_scene(&fixture.root, fixture.request()).unwrap();

        let removed = remove_scene(&fixture.root, fixture.removal_request()).unwrap();

        assert!(removed.changed());
        assert_eq!(removed.project_revision(), 3);
        assert_eq!(removed.resource_id(), SCENE_ID);
        assert_eq!(removed.previous_resource(), Some(fixture.scene.as_slice()));
        assert!(!fixture.root.join(removed.resource_path()).exists());
        assert!(!fixture.root.join("scenes").exists());
        let verified = verify_project_package(&fixture.root).unwrap();
        assert!(
            verified
                .manifest()
                .resources()
                .iter()
                .all(|resource| resource.id() != SCENE_ID)
        );
    }

    #[test]
    fn scene_removal_recovers_every_durable_phase() {
        for crash_phase in [
            PublicationPhase::Transaction,
            PublicationPhase::ResourceFile,
            PublicationPhase::Resource,
            PublicationPhase::Manifest,
        ] {
            let fixture = SceneFixture::new();
            publish_scene(&fixture.root, fixture.request()).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.removal_resource_request(),
                    ProjectResourcePublicationMode::SceneRemoval,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            let committed = crash_phase == PublicationPhase::Manifest;
            assert_eq!(
                recovered[0].disposition(),
                if committed {
                    ProjectResourcePublicationRecoveryDisposition::Committed
                } else {
                    ProjectResourcePublicationRecoveryDisposition::RolledBack
                }
            );
            let verified = verify_project_package(&fixture.root).unwrap();
            assert_eq!(
                verified.manifest().revision(),
                if committed { 3 } else { 2 }
            );
            assert_eq!(
                fixture
                    .root
                    .join("scenes")
                    .join(format!("{SCENE_ID}.json"))
                    .exists(),
                !committed
            );
            assert_eq!(fixture.root.join("scenes").exists(), !committed);
        }
    }

    #[test]
    fn scene_publication_rejects_stale_noncanonical_and_mismatched_documents() {
        let fixture = SceneFixture::new();
        let mut stale = fixture.request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            publish_scene(&fixture.root, stale).unwrap_err().kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );

        let mut wrong: Value = serde_json::from_slice(&fixture.scene).unwrap();
        wrong["id"] = ARRANGEMENT_ID.into();
        let wrong = serde_json_canonicalizer::to_vec(&wrong).unwrap();
        let mut request = fixture.request();
        request.scene_bytes = &wrong;
        assert_eq!(
            publish_scene(&fixture.root, request).unwrap_err().kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );

        let pretty =
            serde_json::to_vec_pretty(&serde_json::from_slice::<Value>(&fixture.scene).unwrap())
                .unwrap();
        let mut request = fixture.request();
        request.scene_bytes = &pretty;
        assert_eq!(
            publish_scene(&fixture.root, request).unwrap_err().kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(!fixture.root.join("scenes").exists());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn scene_crashes_before_manifest_roll_back_resource_and_created_directory() {
        for crash_phase in [PublicationPhase::Transaction, PublicationPhase::Resource] {
            let fixture = SceneFixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.resource_request(),
                    ProjectResourcePublicationMode::SceneAddition,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                ProjectResourcePublicationRecoveryDisposition::RolledBack
            );
            assert!(!fixture.root.join("scenes").exists());
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                1
            );
        }
    }

    #[test]
    fn automation_take_crash_after_manifest_recovers_as_committed() {
        let fixture = AutomationTakeFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.resource_request(),
                ProjectResourcePublicationMode::AutomationTakeAddition,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_project_resource_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("takes/{AUTOMATION_TAKE_ID}.json"))
            )
            .unwrap(),
            fixture.take
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
    }

    #[test]
    fn scene_crash_after_manifest_recovers_as_committed() {
        let fixture = SceneFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.resource_request(),
                ProjectResourcePublicationMode::SceneAddition,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_project_resource_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("scenes/{SCENE_ID}.json"))).unwrap(),
            fixture.scene
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
    }

    #[test]
    fn scene_rollback_preserves_a_preexisting_directory_and_scene() {
        let fixture = SceneFixture::new();
        let first = publish_scene(&fixture.root, fixture.request()).unwrap();
        let mut second: Value = serde_json::from_slice(&fixture.scene).unwrap();
        second["id"] = SECOND_SCENE_ID.into();
        second["name"] = "Verse".into();
        second["order"] = 1.into();
        let second = serde_json_canonicalizer::to_vec(&second).unwrap();
        let request = ProjectResourcePublicationRequest {
            expected_project_sha256: *first.project_identity(),
            resource_id: SECOND_SCENE_ID,
            expected_role: ResourceRole::Scene,
            resource_bytes: &second,
        };
        let scenes = fixture.root.join("scenes");
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                request,
                ProjectResourcePublicationMode::SceneAddition,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Resource, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_project_resource_publications(&fixture.root).unwrap();

        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::RolledBack
        );
        assert!(scenes.is_dir());
        assert_eq!(
            fs::read(scenes.join(format!("{SCENE_ID}.json"))).unwrap(),
            fixture.scene
        );
        assert!(!scenes.join(format!("{SECOND_SCENE_ID}.json")).exists());
    }

    #[test]
    fn scene_publication_refuses_an_unmanaged_destination_without_overwrite() {
        let fixture = SceneFixture::new();
        let request = fixture.request();
        let scenes = fixture.root.join("scenes");
        fs::create_dir(&scenes).unwrap();
        let destination = scenes.join(format!("{SCENE_ID}.json"));
        fs::write(&destination, b"unrelated").unwrap();

        let error = publish_scene(&fixture.root, request).unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Integrity);
        assert_eq!(fs::read(destination).unwrap(), b"unrelated");
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn arrangement_publication_rejects_stale_and_mismatched_documents() {
        let fixture = ArrangementFixture::new();
        let mut stale = fixture.request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            publish_arrangement(&fixture.root, stale)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );

        let mut wrong: Value = serde_json::from_slice(&fixture.arrangement).unwrap();
        wrong["project_id"] = PROJECT_ID.into();
        let wrong = serde_json_canonicalizer::to_vec(&wrong).unwrap();
        let mut request = fixture.request();
        request.arrangement_bytes = &wrong;
        assert_eq!(
            publish_arrangement(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );

        let pretty = serde_json::to_vec_pretty(
            &serde_json::from_slice::<Value>(&fixture.arrangement).unwrap(),
        )
        .unwrap();
        let mut request = fixture.request();
        request.arrangement_bytes = &pretty;
        assert_eq!(
            publish_arrangement(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(!fixture.root.join("arrangement").exists());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn arrangement_crashes_before_manifest_roll_back_the_resource_and_directory() {
        for crash_phase in [PublicationPhase::Transaction, PublicationPhase::Resource] {
            let fixture = ArrangementFixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.resource_request(),
                    ProjectResourcePublicationMode::ArrangementAddition,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                ProjectResourcePublicationRecoveryDisposition::RolledBack
            );
            assert!(!fixture.root.join("arrangement").exists());
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                1
            );
        }
    }

    #[test]
    fn arrangement_crash_after_manifest_recovers_as_committed() {
        let fixture = ArrangementFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.resource_request(),
                ProjectResourcePublicationMode::ArrangementAddition,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_project_resource_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("arrangement/{ARRANGEMENT_ID}.json"))
            )
            .unwrap(),
            fixture.arrangement
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
    }

    #[test]
    fn arrangement_recovery_refuses_unrelated_published_content() {
        let fixture = ArrangementFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.resource_request(),
                ProjectResourcePublicationMode::ArrangementAddition,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Resource, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());
        let path = fixture
            .root
            .join(format!("arrangement/{ARRANGEMENT_ID}.json"));
        let unrelated = serde_json_canonicalizer::to_vec(&json!({"unrelated": true})).unwrap();
        fs::write(&path, &unrelated).unwrap();

        let error = recover_project_resource_publications(&fixture.root).unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(fs::read(path).unwrap(), unrelated);
        assert_eq!(fixture.transaction_count(), 1);
    }

    #[test]
    fn resource_publication_refuses_a_foreign_transaction_before_staging() {
        let fixture = Fixture::new();
        let foreign_name = format!(
            "{}fixture",
            crate::project_lock::CONTROL_MAPPING_TRANSACTION_PREFIX
        );
        let foreign = fixture.root.join("backups").join(&foreign_name);
        fs::create_dir_all(&foreign).unwrap();

        let error = publish_project_resource(&fixture.root, fixture.request()).unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
            fixture.original
        );
        assert!(foreign.exists());
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn rejects_stale_role_and_noncanonical_requests_without_mutation() {
        let fixture = Fixture::new();
        let mut stale = fixture.request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            publish_project_resource(&fixture.root, stale)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        let mut wrong_role = fixture.request();
        wrong_role.expected_role = ResourceRole::Timeline;
        assert_eq!(
            publish_project_resource(&fixture.root, wrong_role)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::ResourceConflict
        );
        let pretty = b"{\n  \"format\": \"fixture-mixer\"\n}";
        let mut noncanonical = fixture.request();
        noncanonical.resource_bytes = pretty;
        assert_eq!(
            publish_project_resource(&fixture.root, noncanonical)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
            fixture.original
        );
    }

    #[test]
    fn crash_before_manifest_publication_rolls_back_the_resource() {
        for crash_phase in [PublicationPhase::Transaction, PublicationPhase::Resource] {
            let fixture = Fixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_project_resource_observed(
                    &fixture.root,
                    fixture.request(),
                    ProjectResourcePublicationMode::Replacement,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let transaction: Value = serde_json::from_slice(
                &fs::read(fixture.transaction_path().join(TRANSACTION_NAME)).unwrap(),
            )
            .unwrap();
            assert!(transaction["previous_resource"].is_object());
            assert!(transaction.get("created_parent_directory").is_none());

            let recovered = recover_project_resource_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                ProjectResourcePublicationRecoveryDisposition::RolledBack
            );
            assert_eq!(
                fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
                fixture.original
            );
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                1
            );
        }
    }

    #[test]
    fn identical_save_recovers_prepublication_residue_before_returning() {
        let fixture = Fixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.request(),
                ProjectResourcePublicationMode::Replacement,
                |phase, _| {
                    assert_ne!(
                        phase,
                        PublicationPhase::Transaction,
                        "simulated process crash"
                    );
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());
        assert_eq!(fixture.transaction_count(), 1);

        let error = publish_project_resource(
            &fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: fixture.identity(),
                resource_id: MIXER_ID,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &fixture.original,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        assert_eq!(fixture.transaction_count(), 0);
        assert_eq!(
            fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
            fixture.original
        );
    }

    #[test]
    fn failed_publication_retains_a_present_but_unreadable_transaction() {
        let fixture = Fixture::new();

        let error = publish_project_resource_observed(
            &fixture.root,
            fixture.request(),
            ProjectResourcePublicationMode::Replacement,
            |phase, _| {
                if phase == PublicationPhase::Resource {
                    fs::write(fixture.transaction_path().join(TRANSACTION_NAME), b"{}").unwrap();
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Io,
                        "simulated publication failure",
                    ));
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(fixture.transaction_count(), 1);
        assert!(fixture.transaction_path().join(TRANSACTION_NAME).exists());
        assert!(verify_project_package(&fixture.root).is_err());
    }

    #[test]
    fn crash_after_manifest_publication_recovers_as_committed() {
        let fixture = Fixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_project_resource_observed(
                &fixture.root,
                fixture.request(),
                ProjectResourcePublicationMode::Replacement,
                |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_project_resource_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
            fixture.replacement
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
    }

    /// A publication that fails after the manifest is committed recovers
    /// forward. The replaced bytes must survive that path too, or an undo
    /// captured through recovery would have nothing to restore.
    #[test]
    fn a_publication_recovered_as_committed_still_reports_what_it_replaced() {
        let fixture = Fixture::new();
        let outcome = publish_project_resource_observed(
            &fixture.root,
            fixture.request(),
            ProjectResourcePublicationMode::Replacement,
            |phase, _| {
                if phase == PublicationPhase::Manifest {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Io,
                        "simulated post-manifest failure",
                    ));
                }
                Ok(())
            },
        )
        .unwrap();

        assert!(outcome.changed());
        assert_eq!(
            outcome.previous_resource(),
            Some(fixture.original.as_slice()),
            "the recovered publication carries the bytes it replaced"
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/mixer.json")).unwrap(),
            fixture.replacement
        );
    }

    #[test]
    fn committed_resource_manifest_retains_publication_authority_during_recovery_failure() {
        let fixture = Fixture::new();
        let error = publish_project_resource_observed(
            &fixture.root,
            fixture.request(),
            ProjectResourcePublicationMode::Replacement,
            |phase, _| {
                if phase != PublicationPhase::Manifest {
                    return Ok(());
                }
                fs::write(
                    fixture
                        .root
                        .join("backups")
                        .join(format!(".project-{}.json", fixture.transaction_id())),
                    b"unrelated cleanup residue",
                )
                .unwrap();
                Err(failure(
                    ProjectResourcePublicationErrorKind::Io,
                    "simulated post-manifest failure",
                ))
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert!(error.published());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
        fs::remove_file(
            fixture
                .root
                .join("backups")
                .join(format!(".project-{}.json", fixture.transaction_id())),
        )
        .unwrap();
        let recovered = recover_project_resource_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            ProjectResourcePublicationRecoveryDisposition::Committed
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn publishes_a_new_audio_track_and_matching_mixer_atomically() {
        let fixture = AudioTrackFixture::new();

        let outcome = publish_audio_track(&fixture.root, fixture.request()).unwrap();

        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.track_id(), NEW_TRACK_ID);
        assert_eq!(outcome.track_path(), format!("tracks/{NEW_TRACK_ID}.json"));
        assert_eq!(
            fs::read(fixture.root.join(outcome.track_path())).unwrap(),
            fixture.track
        );
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(package.manifest_sha256(), outcome.project_identity());
        let track = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == NEW_TRACK_ID)
            .unwrap();
        assert_eq!(track.role(), ResourceRole::Track);
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn publishes_complete_group_and_return_routing_buses_atomically() {
        let group_fixture = AudioTrackFixture::new();
        let (identity, source_track_id, documents) = prepared_routing_bus_documents(
            &group_fixture,
            hyper_playback_json::RoutingBusKind::Group,
            GROUP_BUS_ID,
            None,
        );
        let source_track_ids = [source_track_id.as_str()];
        let no_send_ids: [&str; 0] = [];
        let group = publish_routing_bus(
            &group_fixture.root,
            RoutingBusPublicationRequest {
                expected_project_sha256: identity,
                track_id: GROUP_BUS_ID,
                track_bytes: documents.track(),
                mixer_id: &group_fixture.mixer_id,
                mixer_bytes: documents.mixer(),
                kind: RoutingBusPublicationKind::Group,
                source_track_ids: &source_track_ids,
                send_ids: &no_send_ids,
            },
        )
        .unwrap();
        assert_eq!(group.project_revision(), 2);
        let group_mixer: Value = serde_json::from_slice(
            &verify_project_package(&group_fixture.root)
                .unwrap()
                .read_resource(&group_fixture.mixer_id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            group_mixer["channels"][0]["destination"],
            json!({"kind":"track", "track_id":GROUP_BUS_ID})
        );
        assert_active_mixer_loads(&group_fixture.root, &group_fixture.mixer_id);

        let return_fixture = AudioTrackFixture::new();
        let (identity, source_track_id, documents) = prepared_routing_bus_documents(
            &return_fixture,
            hyper_playback_json::RoutingBusKind::Return,
            RETURN_BUS_ID,
            Some(RETURN_SEND_ID),
        );
        let source_track_ids = [source_track_id.as_str()];
        let send_ids = [RETURN_SEND_ID];
        let returned = publish_routing_bus(
            &return_fixture.root,
            RoutingBusPublicationRequest {
                expected_project_sha256: identity,
                track_id: RETURN_BUS_ID,
                track_bytes: documents.track(),
                mixer_id: &return_fixture.mixer_id,
                mixer_bytes: documents.mixer(),
                kind: RoutingBusPublicationKind::Return,
                source_track_ids: &source_track_ids,
                send_ids: &send_ids,
            },
        )
        .unwrap();
        assert_eq!(returned.project_revision(), 2);
        let return_mixer: Value = serde_json::from_slice(
            &verify_project_package(&return_fixture.root)
                .unwrap()
                .read_resource(&return_fixture.mixer_id)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            return_mixer["return_buses"],
            json!([{"bus":"a", "track_id":RETURN_BUS_ID}])
        );
        assert_eq!(
            return_mixer["channels"][0]["sends"][0]["id"],
            RETURN_SEND_ID
        );
        assert_active_mixer_loads(&return_fixture.root, &return_fixture.mixer_id);
    }

    #[test]
    fn group_routing_bus_crash_recovery_preserves_one_complete_project_identity() {
        for crash_phase in [
            AudioTrackPublicationPhase::Transaction,
            AudioTrackPublicationPhase::Track,
            AudioTrackPublicationPhase::Mixer,
            AudioTrackPublicationPhase::Manifest,
        ] {
            let fixture = AudioTrackFixture::new();
            let (identity, source_track_id, documents) = prepared_routing_bus_documents(
                &fixture,
                hyper_playback_json::RoutingBusKind::Group,
                GROUP_BUS_ID,
                None,
            );
            let source_track_ids = vec![source_track_id];
            let no_send_ids = Vec::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_track_addition_observed(
                    &fixture.root,
                    TrackAdditionPublicationRequest {
                        expected_project_sha256: identity,
                        track_id: GROUP_BUS_ID,
                        track_bytes: documents.track(),
                        mixer_id: &fixture.mixer_id,
                        mixer_bytes: documents.mixer(),
                    },
                    TrackAdditionPublicationContract::Routing {
                        kind: RoutingBusPublicationKind::Group,
                        source_track_ids: &source_track_ids,
                        send_ids: &no_send_ids,
                    },
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_audio_track_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                if crash_phase == AudioTrackPublicationPhase::Manifest {
                    AudioTrackPublicationRecoveryDisposition::Committed
                } else {
                    AudioTrackPublicationRecoveryDisposition::RolledBack
                }
            );
            let package = verify_project_package(&fixture.root).unwrap();
            if crash_phase == AudioTrackPublicationPhase::Manifest {
                assert_active_mixer_loads(&fixture.root, &fixture.mixer_id);
                assert!(
                    package
                        .manifest()
                        .resources()
                        .iter()
                        .any(|resource| resource.id() == GROUP_BUS_ID)
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
            }
        }
    }

    #[test]
    fn routing_bus_rejects_mixer_changes_outside_its_declared_graph() {
        let fixture = AudioTrackFixture::new();
        let (identity, source_track_id, documents) = prepared_routing_bus_documents(
            &fixture,
            hyper_playback_json::RoutingBusKind::Group,
            GROUP_BUS_ID,
            None,
        );
        let mut mixer: Value = serde_json::from_slice(documents.mixer()).unwrap();
        mixer["main"]["mute"] = true.into();
        let mixer = serde_json_canonicalizer::to_vec(&mixer).unwrap();
        let source_track_ids = [source_track_id.as_str()];
        let no_send_ids: [&str; 0] = [];

        let error = publish_routing_bus(
            &fixture.root,
            RoutingBusPublicationRequest {
                expected_project_sha256: identity,
                track_id: GROUP_BUS_ID,
                track_bytes: documents.track(),
                mixer_id: &fixture.mixer_id,
                mixer_bytes: &mixer,
                kind: RoutingBusPublicationKind::Group,
                source_track_ids: &source_track_ids,
                send_ids: &no_send_ids,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(fixture.transaction_count(), 0);
        assert_eq!(
            *verify_project_package(&fixture.root)
                .unwrap()
                .manifest_sha256(),
            identity
        );
    }

    #[test]
    fn removes_an_empty_audio_track_and_matching_mixer_channel_atomically() {
        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();

        let outcome = remove_audio_track(&fixture.root, fixture.removal_request()).unwrap();

        assert_eq!(outcome.project_revision(), 3);
        assert_eq!(outcome.track_id(), NEW_TRACK_ID);
        assert!(
            !fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.original_mixer
        );
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 3);
        assert_eq!(package.manifest_sha256(), outcome.project_identity());
        assert!(
            package
                .manifest()
                .resources()
                .iter()
                .all(|descriptor| descriptor.id() != NEW_TRACK_ID)
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one paired Group and Return lifecycle proves both removal contracts stay executable"
    )]
    fn removes_unreferenced_group_and_return_buses_atomically() {
        let group_fixture = AudioTrackFixture::new();
        let (identity, source_track_id, documents) = prepared_routing_bus_documents(
            &group_fixture,
            hyper_playback_json::RoutingBusKind::Group,
            GROUP_BUS_ID,
            None,
        );
        let source_track_ids = [source_track_id.as_str()];
        let no_send_ids: [&str; 0] = [];
        publish_routing_bus(
            &group_fixture.root,
            RoutingBusPublicationRequest {
                expected_project_sha256: identity,
                track_id: GROUP_BUS_ID,
                track_bytes: documents.track(),
                mixer_id: &group_fixture.mixer_id,
                mixer_bytes: documents.mixer(),
                kind: RoutingBusPublicationKind::Group,
                source_track_ids: &source_track_ids,
                send_ids: &no_send_ids,
            },
        )
        .unwrap();
        let mut group_mixer: Value = serde_json::from_slice(documents.mixer()).unwrap();
        let channels = group_mixer["channels"].as_array_mut().unwrap();
        channels
            .iter_mut()
            .find(|channel| channel["track_id"].as_str() == Some(source_track_id.as_str()))
            .unwrap()["destination"] = json!({"kind":"main"});
        let group_mixer = serde_json_canonicalizer::to_vec(&group_mixer).unwrap();
        let group_identity = *verify_project_package(&group_fixture.root)
            .unwrap()
            .manifest_sha256();
        publish_project_resource(
            &group_fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: group_identity,
                resource_id: &group_fixture.mixer_id,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &group_mixer,
            },
        )
        .unwrap();
        let mut group_removal: Value = serde_json::from_slice(&group_mixer).unwrap();
        let channels = group_removal["channels"].as_array_mut().unwrap();
        channels.remove(
            channels
                .iter()
                .position(|channel| channel["track_id"].as_str() == Some(GROUP_BUS_ID))
                .unwrap(),
        );
        let group_removal = serde_json_canonicalizer::to_vec(&group_removal).unwrap();
        let group_identity = *verify_project_package(&group_fixture.root)
            .unwrap()
            .manifest_sha256();
        let group = remove_routing_bus(
            &group_fixture.root,
            RoutingBusRemovalRequest {
                expected_project_sha256: group_identity,
                track_id: GROUP_BUS_ID,
                mixer_id: &group_fixture.mixer_id,
                mixer_bytes: &group_removal,
                kind: RoutingBusRemovalKind::Group,
            },
        )
        .unwrap();
        assert_eq!(group.project_revision(), 4);
        assert!(
            !group_fixture
                .root
                .join(format!("tracks/{GROUP_BUS_ID}.json"))
                .exists()
        );
        assert_active_mixer_loads(&group_fixture.root, &group_fixture.mixer_id);

        let return_fixture = AudioTrackFixture::new();
        let (identity, source_track_id, documents) = prepared_routing_bus_documents(
            &return_fixture,
            hyper_playback_json::RoutingBusKind::Return,
            RETURN_BUS_ID,
            Some(RETURN_SEND_ID),
        );
        let source_track_ids = [source_track_id.as_str()];
        let send_ids = [RETURN_SEND_ID];
        publish_routing_bus(
            &return_fixture.root,
            RoutingBusPublicationRequest {
                expected_project_sha256: identity,
                track_id: RETURN_BUS_ID,
                track_bytes: documents.track(),
                mixer_id: &return_fixture.mixer_id,
                mixer_bytes: documents.mixer(),
                kind: RoutingBusPublicationKind::Return,
                source_track_ids: &source_track_ids,
                send_ids: &send_ids,
            },
        )
        .unwrap();
        let mut return_mixer: Value = serde_json::from_slice(documents.mixer()).unwrap();
        let channels = return_mixer["channels"].as_array_mut().unwrap();
        channels
            .iter_mut()
            .find(|channel| channel["track_id"].as_str() == Some(source_track_id.as_str()))
            .unwrap()["sends"] = json!([]);
        let return_mixer = serde_json_canonicalizer::to_vec(&return_mixer).unwrap();
        let return_identity = *verify_project_package(&return_fixture.root)
            .unwrap()
            .manifest_sha256();
        publish_project_resource(
            &return_fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: return_identity,
                resource_id: &return_fixture.mixer_id,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &return_mixer,
            },
        )
        .unwrap();
        let mut return_removal: Value = serde_json::from_slice(&return_mixer).unwrap();
        let channels = return_removal["channels"].as_array_mut().unwrap();
        channels.remove(
            channels
                .iter()
                .position(|channel| channel["track_id"].as_str() == Some(RETURN_BUS_ID))
                .unwrap(),
        );
        return_removal["return_buses"] = json!([]);
        let return_removal = serde_json_canonicalizer::to_vec(&return_removal).unwrap();
        let return_identity = *verify_project_package(&return_fixture.root)
            .unwrap()
            .manifest_sha256();
        let returned = remove_routing_bus(
            &return_fixture.root,
            RoutingBusRemovalRequest {
                expected_project_sha256: return_identity,
                track_id: RETURN_BUS_ID,
                mixer_id: &return_fixture.mixer_id,
                mixer_bytes: &return_removal,
                kind: RoutingBusRemovalKind::Return,
            },
        )
        .unwrap();
        assert_eq!(returned.project_revision(), 4);
        assert!(
            !return_fixture
                .root
                .join(format!("tracks/{RETURN_BUS_ID}.json"))
                .exists()
        );
        assert_active_mixer_loads(&return_fixture.root, &return_fixture.mixer_id);
    }

    #[test]
    fn group_removal_rejects_a_forged_reduction_that_discards_owned_sends() {
        let fixture = AudioTrackFixture::new();
        let (_, source_track_id, documents) = prepared_routing_bus_documents(
            &fixture,
            hyper_playback_json::RoutingBusKind::Group,
            GROUP_BUS_ID,
            None,
        );
        let mut previous: Value = serde_json::from_slice(documents.mixer()).unwrap();
        let channels = previous["channels"].as_array_mut().unwrap();
        channels
            .iter_mut()
            .find(|channel| channel["track_id"].as_str() == Some(source_track_id.as_str()))
            .unwrap()["destination"] = json!({"kind":"main"});
        channels
            .iter_mut()
            .find(|channel| channel["track_id"].as_str() == Some(GROUP_BUS_ID))
            .unwrap()["sends"] = json!([{"id":"018f47a2-6b2d-7c11-8a44-00112233446f"}]);
        let previous = serde_json_canonicalizer::to_vec(&previous).unwrap();
        let mut candidate: Value = serde_json::from_slice(&previous).unwrap();
        let channels = candidate["channels"].as_array_mut().unwrap();
        channels.remove(
            channels
                .iter()
                .position(|channel| channel["track_id"].as_str() == Some(GROUP_BUS_ID))
                .unwrap(),
        );
        let candidate = serde_json_canonicalizer::to_vec(&candidate).unwrap();
        let request = StructuralTrackRemovalRequest {
            expected_project_sha256: [0; 32],
            track_id: GROUP_BUS_ID,
            mixer_id: &fixture.mixer_id,
            mixer_bytes: &candidate,
            contract: AudioTrackTransactionContract::RoutingGroup {
                source_track_ids: Vec::new(),
            },
        };

        let error = validate_audio_track_removal_documents(&request, documents.track(), &previous)
            .expect_err("storage must refuse a candidate that drops a Group-owned send");
        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ResourceConflict
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one crash matrix verifies every durable phase for both routing-bus contracts"
    )]
    fn routing_bus_removal_crashes_recover_to_one_complete_project_identity() {
        for (playback_kind, removal_kind, track_id, send_id) in [
            (
                hyper_playback_json::RoutingBusKind::Group,
                RoutingBusRemovalKind::Group,
                GROUP_BUS_ID,
                None,
            ),
            (
                hyper_playback_json::RoutingBusKind::Return,
                RoutingBusRemovalKind::Return,
                RETURN_BUS_ID,
                Some(RETURN_SEND_ID),
            ),
        ] {
            for crash_phase in [
                AudioTrackPublicationPhase::Transaction,
                AudioTrackPublicationPhase::Track,
                AudioTrackPublicationPhase::Mixer,
                AudioTrackPublicationPhase::Manifest,
            ] {
                let fixture = AudioTrackFixture::new();
                let (identity, source_track_id, documents) =
                    prepared_routing_bus_documents(&fixture, playback_kind, track_id, send_id);
                let source_track_ids = [source_track_id.as_str()];
                let send_ids = send_id.map_or_else(Vec::new, |id| vec![id]);
                publish_routing_bus(
                    &fixture.root,
                    RoutingBusPublicationRequest {
                        expected_project_sha256: identity,
                        track_id,
                        track_bytes: documents.track(),
                        mixer_id: &fixture.mixer_id,
                        mixer_bytes: documents.mixer(),
                        kind: match removal_kind {
                            RoutingBusRemovalKind::Group => RoutingBusPublicationKind::Group,
                            RoutingBusRemovalKind::Return => RoutingBusPublicationKind::Return,
                        },
                        source_track_ids: &source_track_ids,
                        send_ids: &send_ids,
                    },
                )
                .unwrap();
                let mut cleaned: Value = serde_json::from_slice(documents.mixer()).unwrap();
                let channels = cleaned["channels"].as_array_mut().unwrap();
                let source = channels
                    .iter_mut()
                    .find(|channel| channel["track_id"].as_str() == Some(source_track_id.as_str()))
                    .unwrap();
                match removal_kind {
                    RoutingBusRemovalKind::Group => {
                        source["destination"] = json!({"kind":"main"});
                    }
                    RoutingBusRemovalKind::Return => source["sends"] = json!([]),
                }
                let cleaned = serde_json_canonicalizer::to_vec(&cleaned).unwrap();
                let cleanup_identity = *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256();
                publish_project_resource(
                    &fixture.root,
                    ProjectResourcePublicationRequest {
                        expected_project_sha256: cleanup_identity,
                        resource_id: &fixture.mixer_id,
                        expected_role: ResourceRole::Mixer,
                        resource_bytes: &cleaned,
                    },
                )
                .unwrap();
                let mut removal: Value = serde_json::from_slice(&cleaned).unwrap();
                let channels = removal["channels"].as_array_mut().unwrap();
                channels.remove(
                    channels
                        .iter()
                        .position(|channel| channel["track_id"].as_str() == Some(track_id))
                        .unwrap(),
                );
                if removal_kind == RoutingBusRemovalKind::Return {
                    removal["return_buses"] = json!([]);
                }
                let removal = serde_json_canonicalizer::to_vec(&removal).unwrap();
                let removal_identity = *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256();
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let _ = remove_routing_bus_observed(
                        &fixture.root,
                        RoutingBusRemovalRequest {
                            expected_project_sha256: removal_identity,
                            track_id,
                            mixer_id: &fixture.mixer_id,
                            mixer_bytes: &removal,
                            kind: removal_kind,
                        },
                        |phase, _| {
                            assert_ne!(phase, crash_phase, "simulated process crash");
                            Ok(())
                        },
                    );
                }));
                assert!(result.is_err());
                let recovered = recover_audio_track_publications(&fixture.root).unwrap();
                assert_eq!(recovered.len(), 1);
                assert_eq!(
                    recovered[0].disposition(),
                    if crash_phase == AudioTrackPublicationPhase::Manifest {
                        AudioTrackPublicationRecoveryDisposition::Committed
                    } else {
                        AudioTrackPublicationRecoveryDisposition::RolledBack
                    }
                );
                let package = verify_project_package(&fixture.root).unwrap();
                if crash_phase == AudioTrackPublicationPhase::Manifest {
                    assert!(
                        package
                            .manifest()
                            .resources()
                            .iter()
                            .all(|resource| resource.id() != track_id)
                    );
                    assert_active_mixer_loads(&fixture.root, &fixture.mixer_id);
                } else {
                    assert!(
                        package
                            .manifest()
                            .resources()
                            .iter()
                            .any(|resource| resource.id() == track_id)
                    );
                }
            }
        }
    }

    #[test]
    fn reorders_every_track_and_the_matching_mixer_in_one_publication() {
        use hyper_playback_json::{
            LoaderLimits, PlaybackJsonInput, load as load_playback, serialize_mixer_strip_reorder,
        };

        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        let timeline = package
            .manifest()
            .resources()
            .iter()
            .find(|resource| resource.role() == ResourceRole::Timeline)
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .unwrap();
        let track_descriptors: Vec<_> = package
            .manifest()
            .resources()
            .iter()
            .filter(|resource| resource.role() == ResourceRole::Track)
            .collect();
        let track_ids: Vec<_> = track_descriptors
            .iter()
            .map(|resource| resource.id().to_owned())
            .collect();
        let tracks: Vec<_> = track_descriptors
            .iter()
            .map(|resource| package.read_resource(resource.id()).unwrap())
            .collect();
        let track_slices: Vec<_> = tracks.iter().map(Vec::as_slice).collect();
        let project = load_playback(
            PlaybackJsonInput {
                project_id: package.manifest().project_id(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: &timeline,
                tracks: &track_slices,
                clips: &[],
                scenes: &[],
                arrangement: None,
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let mixer = package.read_resource(&fixture.mixer_id).unwrap();
        let source_tracks: Vec<_> = track_ids
            .iter()
            .zip(&tracks)
            .map(|(id, bytes)| (id.as_str(), bytes.as_slice()))
            .collect();
        let documents = serialize_mixer_strip_reorder(
            &project,
            &[],
            &source_tracks,
            &mixer,
            &fixture.mixer_id,
            NEW_TRACK_ID,
            0,
            LoaderLimits::VERSION_1,
        )
        .unwrap();

        let outcome = publish_track_order(
            &fixture.root,
            TrackOrderPublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                mixer_id: &fixture.mixer_id,
                documents: &documents,
            },
        )
        .unwrap();

        assert_eq!(outcome.project_revision(), 3);
        let reopened = verify_project_package(&fixture.root).unwrap();
        assert_eq!(*reopened.manifest_sha256(), *outcome.project_identity());
        let mixer: Value =
            serde_json::from_slice(&reopened.read_resource(&fixture.mixer_id).unwrap()).unwrap();
        assert_eq!(
            mixer["channels"]
                .as_array()
                .unwrap()
                .iter()
                .map(|channel| channel["track_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![NEW_TRACK_ID, track_ids[0].as_str()]
        );
        let ordered_tracks: Vec<_> = documents
            .tracks()
            .iter()
            .map(hyper_playback_json::MixerStripReorderDocument::track_id)
            .collect();
        assert_eq!(ordered_tracks, vec![NEW_TRACK_ID, track_ids[0].as_str()]);
    }

    #[test]
    fn track_order_refuses_mixer_changes_outside_the_fenced_live_base_state() {
        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        let identity = *package.manifest_sha256();
        let previous_manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let previous_mixer = package.read_resource(&fixture.mixer_id).unwrap();
        let mut unrelated_mixer: Value = serde_json::from_slice(&previous_mixer).unwrap();
        unrelated_mixer["cue"]["mute"] = true.into();
        let unrelated_mixer = serde_json_canonicalizer::to_vec(&unrelated_mixer).unwrap();
        let (_, documents) = prepared_track_order_documents_with_mixer(&fixture, &unrelated_mixer);

        let error = publish_track_order(
            &fixture.root,
            TrackOrderPublicationRequest {
                expected_project_sha256: identity,
                mixer_id: &fixture.mixer_id,
                documents: &documents,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            previous_manifest
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            previous_mixer
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn track_order_crashes_recover_to_one_complete_project_identity() {
        for crash_phase in [
            TrackOrderPublicationPhase::Transaction,
            TrackOrderPublicationPhase::Track(0),
            TrackOrderPublicationPhase::Track(1),
            TrackOrderPublicationPhase::Mixer,
            TrackOrderPublicationPhase::Manifest,
        ] {
            let fixture = AudioTrackFixture::new();
            publish_audio_track(&fixture.root, fixture.request()).unwrap();
            let (before, documents) = prepared_track_order_documents(&fixture);
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_track_order_observed(
                    &fixture.root,
                    TrackOrderPublicationRequest {
                        expected_project_sha256: before,
                        mixer_id: &fixture.mixer_id,
                        documents: &documents,
                    },
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = crate::recover_project_transactions(&fixture.root).unwrap();
            assert_eq!(recovered.track_orders().len(), 1);
            assert_eq!(
                recovered.track_orders()[0].disposition(),
                if crash_phase == TrackOrderPublicationPhase::Manifest {
                    TrackOrderPublicationRecoveryDisposition::Committed
                } else {
                    TrackOrderPublicationRecoveryDisposition::RolledBack
                }
            );
            let package = verify_project_package(&fixture.root).unwrap();
            if crash_phase == TrackOrderPublicationPhase::Manifest {
                let mixer: Value =
                    serde_json::from_slice(&package.read_resource(&fixture.mixer_id).unwrap())
                        .unwrap();
                assert_eq!(
                    mixer["channels"][0]["track_id"].as_str(),
                    Some(NEW_TRACK_ID)
                );
            } else {
                assert_eq!(*package.manifest_sha256(), before);
            }
        }
    }

    #[test]
    fn audio_track_removal_rejects_content_and_unrelated_mixer_changes() {
        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();
        let track_path = fixture.root.join(format!("tracks/{NEW_TRACK_ID}.json"));
        let mut populated: Value = serde_json::from_slice(&fs::read(&track_path).unwrap()).unwrap();
        populated["clip_ids"] = json!(["018f47a2-6b2d-7c11-8a44-00112233446a"]);
        let populated = serde_json_canonicalizer::to_vec(&populated).unwrap();
        fs::write(&track_path, &populated).unwrap();
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap()).unwrap();
        let descriptor = manifest["resources"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|descriptor| descriptor["id"].as_str() == Some(NEW_TRACK_ID))
            .unwrap();
        descriptor["byte_length"] = populated.len().into();
        descriptor["sha256"] = hex_digest(digest(&populated)).into();
        fs::write(
            fixture.root.join(MANIFEST_NAME),
            serde_json_canonicalizer::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let error = remove_audio_track(&fixture.root, fixture.removal_request()).unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(track_path.exists());
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.mixer
        );
        assert_eq!(fixture.transaction_count(), 0);

        let mixer_fixture = AudioTrackFixture::new();
        publish_audio_track(&mixer_fixture.root, mixer_fixture.request()).unwrap();
        let mut unrelated_mixer: Value =
            serde_json::from_slice(&mixer_fixture.original_mixer).unwrap();
        unrelated_mixer["main"]["gain_millionths"] = 750_000.into();
        let unrelated_mixer = serde_json_canonicalizer::to_vec(&unrelated_mixer).unwrap();
        let identity = *verify_project_package(&mixer_fixture.root)
            .unwrap()
            .manifest_sha256();

        let error = remove_audio_track(
            &mixer_fixture.root,
            AudioTrackRemovalRequest {
                expected_project_sha256: identity,
                track_id: NEW_TRACK_ID,
                mixer_id: &mixer_fixture.mixer_id,
                mixer_bytes: &unrelated_mixer,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(
            mixer_fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(mixer_fixture.root.join("mixer/main.json")).unwrap(),
            mixer_fixture.mixer
        );
        assert_eq!(mixer_fixture.transaction_count(), 0);
    }

    #[test]
    fn audio_track_removal_crashes_before_manifest_roll_back_both_resources() {
        for crash_phase in [
            AudioTrackPublicationPhase::Transaction,
            AudioTrackPublicationPhase::Mixer,
            AudioTrackPublicationPhase::Track,
        ] {
            let fixture = AudioTrackFixture::new();
            publish_audio_track(&fixture.root, fixture.request()).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = remove_audio_track_observed(
                    &fixture.root,
                    fixture.removal_request(),
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_audio_track_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                AudioTrackPublicationRecoveryDisposition::RolledBack
            );
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{NEW_TRACK_ID}.json"))).unwrap(),
                fixture.track
            );
            assert_eq!(
                fs::read(fixture.root.join("mixer/main.json")).unwrap(),
                fixture.mixer
            );
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                2
            );
        }
    }

    #[test]
    fn audio_track_removal_crash_after_manifest_recovers_as_committed() {
        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = remove_audio_track_observed(
                &fixture.root,
                fixture.removal_request(),
                |phase, _| {
                    assert_ne!(
                        phase,
                        AudioTrackPublicationPhase::Manifest,
                        "simulated process crash"
                    );
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());

        let recovered = recover_audio_track_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioTrackPublicationRecoveryDisposition::Committed
        );
        assert!(
            !fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.original_mixer
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            3
        );
    }

    #[test]
    fn committed_audio_track_removal_retains_publication_authority_during_recovery_failure() {
        let fixture = AudioTrackFixture::new();
        publish_audio_track(&fixture.root, fixture.request()).unwrap();
        let error =
            remove_audio_track_observed(&fixture.root, fixture.removal_request(), |phase, _| {
                if phase != AudioTrackPublicationPhase::Manifest {
                    return Ok(());
                }
                fs::write(
                    fixture.root.join("backups").join(format!(
                        ".audio-track-project-{}.json",
                        fixture.transaction_id()
                    )),
                    b"unrelated cleanup residue",
                )
                .unwrap();
                Err(failure(
                    ProjectResourcePublicationErrorKind::Io,
                    "simulated post-manifest failure",
                ))
            })
            .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert!(error.published());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            3
        );
        fs::remove_file(fixture.root.join("backups").join(format!(
            ".audio-track-project-{}.json",
            fixture.transaction_id()
        )))
        .unwrap();
        let recovered = recover_audio_track_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioTrackPublicationRecoveryDisposition::Committed
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn audio_track_publication_refuses_a_foreign_transaction_before_staging() {
        let fixture = AudioTrackFixture::new();
        let foreign_name = format!(
            "{}fixture",
            crate::project_lock::CONTROL_MAPPING_TRANSACTION_PREFIX
        );
        let foreign = fixture.root.join("backups").join(&foreign_name);
        fs::create_dir(&foreign).unwrap();

        let error = publish_audio_track(&fixture.root, fixture.request()).unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        assert!(
            !fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.original_mixer
        );
        assert!(foreign.exists());
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn audio_track_publication_refuses_unrelated_mixer_edits() {
        let fixture = AudioTrackFixture::new();
        let mut changed: Value = serde_json::from_slice(&fixture.mixer).unwrap();
        changed["main"]["gain_millionths"] = 500_000.into();
        let changed = serde_json_canonicalizer::to_vec(&changed).unwrap();
        let package = verify_project_package(&fixture.root).unwrap();

        let error = publish_audio_track(
            &fixture.root,
            AudioTrackPublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                track_id: NEW_TRACK_ID,
                track_bytes: &fixture.track,
                mixer_id: &fixture.mixer_id,
                mixer_bytes: &changed,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.original_mixer
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn audio_track_crashes_before_manifest_roll_back_both_resources() {
        for crash_phase in [
            AudioTrackPublicationPhase::Transaction,
            AudioTrackPublicationPhase::Track,
            AudioTrackPublicationPhase::Mixer,
        ] {
            let fixture = AudioTrackFixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ =
                    publish_audio_track_observed(&fixture.root, fixture.request(), |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    });
            }));
            assert!(result.is_err());

            let recovered = recover_audio_track_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                AudioTrackPublicationRecoveryDisposition::RolledBack
            );
            assert!(
                !fixture
                    .root
                    .join(format!("tracks/{NEW_TRACK_ID}.json"))
                    .exists()
            );
            assert_eq!(
                fs::read(fixture.root.join("mixer/main.json")).unwrap(),
                fixture.original_mixer
            );
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                1
            );
        }
    }

    #[test]
    fn staged_audio_track_blocks_mapping_until_global_recovery() {
        let fixture = AudioTrackFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_audio_track_observed(&fixture.root, fixture.request(), |phase, _| {
                assert_ne!(
                    phase,
                    AudioTrackPublicationPhase::Transaction,
                    "simulated process crash"
                );
                Ok(())
            });
        }));
        assert!(result.is_err());
        assert_eq!(fixture.transaction_count(), 1);
        let package = verify_project_package(&fixture.root).unwrap();
        let mapping = serde_json_canonicalizer::to_vec(&json!({
            "format":"hyper-control-mappings",
            "format_version":1,
            "id":"018f47a2-6b2d-7c11-8a44-00112233446a",
            "project_id":package.manifest().project_id(),
            "bindings":[],
            "extensions":{}
        }))
        .unwrap();

        let error = crate::publish_control_mapping(
            &fixture.root,
            crate::ControlMappingPublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                mapping_bytes: &mapping,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            crate::ControlMappingPublicationErrorKind::ProjectChanged
        );
        assert_eq!(fixture.transaction_count(), 1);
        let recovered = crate::recover_project_transactions(&fixture.root).unwrap();
        assert_eq!(recovered.audio_tracks().len(), 1);
        assert_eq!(
            recovered.audio_tracks()[0].disposition(),
            AudioTrackPublicationRecoveryDisposition::RolledBack
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
        assert!(
            !fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            fixture.original_mixer
        );
    }

    #[test]
    fn audio_track_crash_after_manifest_recovers_as_committed() {
        let fixture = AudioTrackFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_audio_track_observed(&fixture.root, fixture.request(), |phase, _| {
                assert_ne!(
                    phase,
                    AudioTrackPublicationPhase::Manifest,
                    "simulated process crash"
                );
                Ok(())
            });
        }));
        assert!(result.is_err());

        let recovered = recover_audio_track_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioTrackPublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{NEW_TRACK_ID}.json"))).unwrap(),
            fixture.track
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
    }

    #[test]
    fn committed_audio_track_manifest_retains_publication_authority_during_recovery_failure() {
        let fixture = AudioTrackFixture::new();
        let error = publish_audio_track_observed(&fixture.root, fixture.request(), |phase, _| {
            if phase != AudioTrackPublicationPhase::Manifest {
                return Ok(());
            }
            fs::write(
                fixture.root.join("backups").join(format!(
                    ".audio-track-project-{}.json",
                    fixture.transaction_id()
                )),
                b"unrelated cleanup residue",
            )
            .unwrap();
            Err(failure(
                ProjectResourcePublicationErrorKind::Io,
                "simulated post-manifest failure",
            ))
        })
        .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert!(error.published());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
        fs::remove_file(fixture.root.join("backups").join(format!(
            ".audio-track-project-{}.json",
            fixture.transaction_id()
        )))
        .unwrap();
        let recovered = recover_audio_track_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioTrackPublicationRecoveryDisposition::Committed
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn audio_track_publication_refuses_existing_and_unrelated_content() {
        let fixture = AudioTrackFixture::new();
        let request = fixture.request();
        let existing_path = fixture.root.join(format!("tracks/{NEW_TRACK_ID}.json"));
        fs::write(&existing_path, b"unrelated").unwrap();

        let error = publish_audio_track(&fixture.root, request).unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Integrity);
        assert_eq!(fs::read(&existing_path).unwrap(), b"unrelated");
    }

    #[test]
    fn audio_track_recovery_refuses_to_overwrite_an_unrelated_mixer() {
        let fixture = AudioTrackFixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_audio_track_observed(&fixture.root, fixture.request(), |phase, _| {
                assert_ne!(
                    phase,
                    AudioTrackPublicationPhase::Mixer,
                    "simulated process crash"
                );
                Ok(())
            });
        }));
        assert!(result.is_err());
        let unrelated = serde_json_canonicalizer::to_vec(&json!({
            "format":"unrelated-mixer"
        }))
        .unwrap();
        fs::write(fixture.root.join("mixer/main.json"), &unrelated).unwrap();

        let error = recover_audio_track_publications(&fixture.root).unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            unrelated
        );
        assert!(
            fixture
                .root
                .join(format!("tracks/{NEW_TRACK_ID}.json"))
                .exists()
        );
        assert_eq!(fixture.transaction_count(), 1);
    }
}
