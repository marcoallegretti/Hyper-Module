use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{ProjectManifest, ResourceDescriptor, ResourceRole, is_valid_entity_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{
    AUDIO_TRACK_TRANSACTION_PREFIX, PROJECT_RESOURCE_TRANSACTION_PREFIX as TRANSACTION_PREFIX,
    ProjectLock, ProjectTransactionFamily, find_unhandled_project_transaction,
};
use super::stable_file::read_open_stable;
use super::{
    MANIFEST_NAME, MAX_JSON_BYTES, MAX_JSON_FILE_BYTES, StructurallyVerifiedProject,
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

#[derive(Clone, Copy, Debug)]
pub struct AudioTrackRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub track_id: &'a str,
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackRemovalOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

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
    extensions: serde_json::Map<String, Value>,
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
        skip_serializing_if = "AudioTrackTransactionOperation::is_publication"
    )]
    operation: AudioTrackTransactionOperation,
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
    Resource,
    Manifest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProjectResourcePublicationMode {
    Replacement,
    ArrangementAddition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioTrackPublicationPhase {
    Transaction,
    Track,
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
    validate_audio_track_documents(request)?;
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
    reject_audio_track_collisions(&verified, request, &track_path)?;
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
    validate_audio_track_document_identity(
        request,
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
        candidate_audio_track_manifest(&active_project, request, &track_path, next_revision)?;
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
                Err(recovery_error) => Err(ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio-track publication and recovery both failed".to_owned(),
                    published: false,
                    source: Some(Box::new(recovery_error)),
                }),
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
                Err(recovery_error) => Err(ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio-track removal and recovery both failed".to_owned(),
                    published: false,
                    source: Some(Box::new(recovery_error)),
                }),
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
    validate_canonical_resource(request.resource_bytes)?;
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
    if mode == ProjectResourcePublicationMode::ArrangementAddition {
        validate_arrangement_addition_request(&verified, request)?;
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
            let created_parent_directory =
                inspect_arrangement_destination(&verified.root_dir, &path)?;
            let candidate =
                candidate_arrangement_manifest(&active_project, request, &path, next_revision)?;
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
    let result = run_publication(
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
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_transaction(&backups, transaction, &transaction_name).map_err(
                |error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "resource was published but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                },
            )?;
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
                        message: "resource publication failed and its durable transaction could not be read"
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
                    let outcome = outcome_from_transaction(&tx, true)?;
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)
                        .map_err(|error| ProjectResourcePublicationError {
                            kind: ProjectResourcePublicationErrorKind::Recovery,
                            message: "resource was published but transaction cleanup is incomplete"
                                .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "resource publication and recovery both failed".to_owned(),
                    published: false,
                    source: Some(Box::new(recovery_error)),
                }),
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

#[allow(clippy::too_many_arguments)]
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
        verified
            .root_dir
            .create_dir("arrangement")
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Io,
                    "create arrangement resource directory",
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
    reason = "the durable write order and every crash observation point remain visible in one coordinator"
)]
fn run_audio_track_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: AudioTrackPublicationRequest<'_>,
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
    request: AudioTrackRemovalRequest<'_>,
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

fn validate_audio_track_documents(
    request: AudioTrackPublicationRequest<'_>,
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

fn inspect_arrangement_destination(
    root: &Dir,
    resource_path: &str,
) -> Result<bool, ProjectResourcePublicationError> {
    let directory = match root.symlink_metadata("arrangement") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect arrangement resource directory",
                error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "arrangement resource directory is not a real directory",
            ));
        }
        Ok(_) => root.open_dir_nofollow("arrangement").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open arrangement resource directory",
                error,
            )
        })?,
    };
    let name = file_name(resource_path)?;
    match directory.symlink_metadata(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "arrangement resource path already exists",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect arrangement resource destination",
            error,
        )),
    }
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
    request: AudioTrackRemovalRequest<'_>,
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
    if track["format"].as_str() != Some("hyper-track")
        || track["id"].as_str() != Some(request.track_id)
        || track["kind"].as_str() != Some("audio")
        || track["clip_ids"]
            .as_array()
            .is_none_or(|clips| !clips.is_empty())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track removal requires the matching empty audio-track resource",
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
    if restored != previous {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track removal must preserve every unrelated mixer field",
        ));
    }
    Ok(())
}

fn reject_audio_track_collisions(
    verified: &StructurallyVerifiedProject,
    request: AudioTrackPublicationRequest<'_>,
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

fn candidate_audio_track_manifest(
    active: &[u8],
    request: AudioTrackPublicationRequest<'_>,
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
    request: AudioTrackRemovalRequest<'_>,
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

fn candidate_arrangement_manifest(
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
        "role": "arrangement",
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
            "candidate arrangement project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
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
    validate_staged_audio_track_transaction(transaction, tx)?;
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
        if tx.operation == AudioTrackTransactionOperation::Removal {
            ensure_audio_track_absent(root, tx)?;
        }
        cleanup_audio_track_temporaries(root, backups, transaction, tx)?;
        verify_committed(root_path, root, after)?;
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
            candidate_audio_track_manifest(
                &previous_project,
                AudioTrackPublicationRequest {
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
                AudioTrackRemovalRequest {
                    expected_project_sha256: digest(&previous_project),
                    track_id: &tx.track_id,
                    mixer_id: &tx.mixer_id,
                    mixer_bytes: &mixer,
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
        AudioTrackTransactionOperation::Publication => validate_audio_track_document_identity(
            AudioTrackPublicationRequest {
                expected_project_sha256: digest(&previous_project),
                track_id: &tx.track_id,
                track_bytes: &track,
                mixer_id: &tx.mixer_id,
                mixer_bytes: &mixer,
            },
            previous_manifest.project_id(),
            &previous_mixer,
        ),
        AudioTrackTransactionOperation::Removal => validate_audio_track_removal_documents(
            AudioTrackRemovalRequest {
                expected_project_sha256: digest(&previous_project),
                track_id: &tx.track_id,
                mixer_id: &tx.mixer_id,
                mixer_bytes: &mixer,
            },
            &track,
            &previous_mixer,
        ),
    }
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
        cleanup_publication_temporaries(root, backups, transaction, tx)?;
        verify_committed(root_path, root, after)?;
        return Ok(ProjectResourcePublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource recovery found an unrelated project identity",
        ));
    }
    rollback_resource(root, transaction, tx)?;
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
    match root.remove_dir("arrangement") {
        Ok(()) => sync_dir(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove transaction-created arrangement directory",
            error,
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
        || tx.previous_resource.is_none()
            && (tx.resource_role != "arrangement" || !tx.resource.path.starts_with("arrangement/"))
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334468";
    const NEW_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334469";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334470";

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
    }

    impl Drop for AudioTrackFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
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
