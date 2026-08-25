use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_JSON_BYTES, MAX_JSON_FILE_BYTES,
    MAX_SAFE_INTEGER, PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME, ProjectResourcePublicationError,
    ProjectResourcePublicationErrorKind, StructurallyVerifiedProject, TRANSACTION_NAME,
    TransactionFile, TransactionIntent, caused, decode_digest, digest, error_is_not_found, failure,
    file_name, hex_digest, manifest_resource, missing_file, open_optional_dir, open_or_create_dir,
    open_relative_parent, open_stable_project_root, publish_staged_file, read_bounded,
    read_optional_file, recover_audio_track_transactions_locked, recover_transactions_locked,
    remove_optional_matching_file, require_canonical, restore_previous_manifest, sync_dir,
    unique_resource, validate_canonical_resource, verify_committed, verify_project_candidate,
    verify_project_package, verify_retained_project_candidate, write_exclusive_synced,
};
use crate::project_lock::{
    AUDIO_CLIP_TRANSACTION_PREFIX, ProjectLock, ProjectTransactionFamily,
    find_unhandled_project_transaction,
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use hyper_audio_source::{AudioContainer, DecodeLimits, decode_bytes};
use hyper_project_model::{
    AssetRole, AudioMediaType, ProjectManifest, ResourceRole, is_valid_entity_id,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use std::path::Path;
use uuid::Uuid;

#[cfg(not(windows))]
use super::same_directory;

const CLIP_STAGING_NAME: &str = "clip.json";
const SAMPLE_STAGING_NAME: &str = "sample.wav";
const TRACK_STAGING_NAME: &str = "track.json";
const PREVIOUS_TRACK_NAME: &str = "previous-track.json";

#[derive(Clone, Copy, Debug)]
pub struct AudioClipPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub clip_id: &'a str,
    pub clip_bytes: &'a [u8],
    pub track_id: &'a str,
    pub track_bytes: &'a [u8],
    pub asset_id: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct ImportedAudioClipPublicationRequest<'a> {
    pub clip: AudioClipPublicationRequest<'a>,
    pub sample_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug)]
pub struct AudioClipRemovalRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub clip_id: &'a str,
    pub track_id: &'a str,
    pub track_bytes: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioClipPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    clip_id: String,
    clip_path: String,
    track_id: String,
}

impl AudioClipPublicationOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn clip_id(&self) -> &str {
        &self.clip_id
    }

    #[must_use]
    pub fn clip_path(&self) -> &str {
        &self.clip_path
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioClipRemovalOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    clip_id: String,
    track_id: String,
}

impl AudioClipRemovalOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn clip_id(&self) -> &str {
        &self.clip_id
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioClipPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioClipPublicationRecovery {
    transaction_id: String,
    disposition: AudioClipPublicationRecoveryDisposition,
}

impl AudioClipPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> AudioClipPublicationRecoveryDisposition {
        self.disposition
    }
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "independent directory effects are persisted as backward-compatible journal evidence"
)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AudioClipTransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    clip_id: String,
    track_id: String,
    asset_id: String,
    clip: TransactionFile,
    track: TransactionFile,
    previous_track: TransactionFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sample: Option<TransactionFile>,
    created_clip_directory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    created_sample_directory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    created_asset_directory: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    removed_clip_directory: bool,
    #[serde(
        default,
        skip_serializing_if = "AudioClipTransactionOperation::is_publication"
    )]
    operation: AudioClipTransactionOperation,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum AudioClipTransactionOperation {
    #[default]
    Publication,
    Removal,
}

impl AudioClipTransactionOperation {
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a shared-reference predicate"
    )]
    const fn is_publication(&self) -> bool {
        matches!(self, Self::Publication)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SampleDestinationPlan {
    created_asset_directory: bool,
    created_sample_directory: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioClipPublicationPhase {
    Transaction,
    Sample,
    Clip,
    Track,
    Manifest,
}

/// Publishes one new reusable audio clip and the owning track reference as a
/// single recoverable, manifest-last project-package mutation.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. No unrelated active file is overwritten during publication or
/// recovery.
pub fn publish_audio_clip(
    project_path: impl AsRef<Path>,
    request: AudioClipPublicationRequest<'_>,
) -> Result<AudioClipPublicationOutcome, ProjectResourcePublicationError> {
    publish_audio_clip_observed(project_path.as_ref(), request, None, |_, _| Ok(()))
}

/// Publishes one project-owned WAV sample and a full-range reusable clip as one
/// recoverable, manifest-last mutation.
///
/// # Errors
///
/// Returns the same classified errors as [`publish_audio_clip`], and rejects an
/// empty or conflicting sample payload before publication.
pub fn publish_imported_audio_clip(
    project_path: impl AsRef<Path>,
    request: ImportedAudioClipPublicationRequest<'_>,
) -> Result<AudioClipPublicationOutcome, ProjectResourcePublicationError> {
    publish_audio_clip_observed(
        project_path.as_ref(),
        request.clip,
        Some(request.sample_bytes),
        |_, _| Ok(()),
    )
}

/// Removes one reusable audio clip and its owning-track reference through the
/// audio-clip transaction family. The source asset is deliberately retained.
///
/// # Errors
///
/// Returns a classified validation, conflict, integrity, I/O, or recovery
/// error. The request must remove exactly the named clip reference while
/// preserving every unrelated owning-track field.
pub fn remove_audio_clip(
    project_path: impl AsRef<Path>,
    request: AudioClipRemovalRequest<'_>,
) -> Result<AudioClipRemovalOutcome, ProjectResourcePublicationError> {
    remove_audio_clip_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable audio-clip publication in one project.
///
/// # Errors
///
/// Returns an error rather than discarding or overwriting content that does
/// not match either recorded side of a transaction.
pub fn recover_audio_clip_publications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioClipPublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for audio-clip recovery",
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
    recover_audio_clip_transactions_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn publish_audio_clip_observed(
    project_path: &Path,
    request: AudioClipPublicationRequest<'_>,
    sample_bytes: Option<&[u8]>,
    mut observer: impl FnMut(
        AudioClipPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioClipPublicationOutcome, ProjectResourcePublicationError> {
    validate_canonical_resource(request.clip_bytes)?;
    validate_canonical_resource(request.track_bytes)?;
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before audio-clip publication",
            error,
        )
    })?;
    let imported_frames = sample_bytes
        .map(|bytes| validate_imported_sample(bytes, verified.manifest().sample_rate_hz()))
        .transpose()?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the audio-clip edit began",
        ));
    }
    let clip_path = format!("clips/{}.json", request.clip_id);
    let sample_path = sample_bytes.map(|_| format!("assets/samples/{}.wav", request.asset_id));
    validate_audio_clip_request(&verified, request, &clip_path, sample_path.as_deref())?;
    let track_descriptor = unique_resource(&verified, request.track_id, ResourceRole::Track)?;
    let previous_track = verified.read_resource(request.track_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current audio-clip owning track",
            error,
        )
    })?;
    validate_audio_clip_documents(request, verified.manifest().project_id(), &previous_track)?;
    if let Some(frames) = imported_frames {
        validate_imported_clip_range(request.clip_bytes, frames)?;
    }
    inspect_clip_destination(&verified.root_dir, &clip_path)?;

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    reject_unhandled_audio_clip_transactions(&backups)?;
    let recovered_resources =
        recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_clips =
        recover_audio_clip_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered_resources.is_empty()
        || !recovered_tracks.is_empty()
        || !recovered_clips.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before audio-clip publication",
            error,
        )
    })?;
    let created_clip_directory = inspect_clip_destination(&verified.root_dir, &clip_path)?;
    let sample_destination = if let Some(path) = sample_path.as_deref() {
        inspect_sample_destination(&verified.root_dir, path)?
    } else {
        SampleDestinationPlan {
            created_asset_directory: false,
            created_sample_directory: false,
        }
    };
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before audio-clip staging",
        ));
    }
    let active_track = read_bounded(
        &open_relative_parent(&verified.root_dir, track_descriptor.path())?,
        file_name(track_descriptor.path())?,
        MAX_JSON_BYTES,
    )?;
    if active_track != previous_track {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "owning track changed before audio-clip staging",
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
    let candidate_project = candidate_audio_clip_manifest(
        &active_project,
        request,
        &clip_path,
        sample_path.as_deref(),
        sample_bytes,
        next_revision,
    )?;
    let transaction_name = format!("{AUDIO_CLIP_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create audio-clip transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-clip transaction",
                error,
            )
        })?;
    let result = run_audio_clip_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        track_descriptor.path(),
        &clip_path,
        sample_path.as_deref(),
        sample_bytes,
        &previous_track,
        created_clip_directory,
        sample_destination,
        &active_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_audio_clip_transaction(&backups, transaction, &transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio clip was published but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_audio_clip_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_audio_clip_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio-clip publication failed and its durable transaction could not be read".to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_audio_clip_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(AudioClipPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    Err(operation_error)
                }
                Ok(AudioClipPublicationRecoveryDisposition::Committed) => {
                    let outcome = audio_clip_outcome(&tx)?;
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )
                    .map_err(|error| ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio clip was published but transaction cleanup is incomplete"
                            .to_owned(),
                        published: true,
                        source: Some(Box::new(error)),
                    })?;
                    Ok(outcome)
                }
                Err(recovery_error) => {
                    let published = recovery_error.published();
                    Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: if published {
                            "audio clip was published but verification or transaction recovery is incomplete"
                        } else {
                            "audio-clip publication and recovery both failed"
                        }
                        .to_owned(),
                        published,
                        source: Some(Box::new(recovery_error)),
                    })
                }
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the coordinator keeps lock, recovery, staging, and disposition ordering visible as one fail-closed operation"
)]
fn remove_audio_clip_observed(
    project_path: &Path,
    request: AudioClipRemovalRequest<'_>,
    mut observer: impl FnMut(
        AudioClipPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioClipRemovalOutcome, ProjectResourcePublicationError> {
    validate_canonical_resource(request.track_bytes)?;
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before audio-clip removal",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after the audio-clip removal began",
        ));
    }
    if !is_valid_entity_id(request.clip_id) || !is_valid_entity_id(request.track_id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-clip and owning-track identities must be valid",
        ));
    }
    let clip_descriptor = unique_resource(&verified, request.clip_id, ResourceRole::Clip)?;
    let track_descriptor = unique_resource(&verified, request.track_id, ResourceRole::Track)?;
    let clip = verified.read_resource(request.clip_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current audio-clip resource",
            error,
        )
    })?;
    let previous_track = verified.read_resource(request.track_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read current audio-clip owning track",
            error,
        )
    })?;
    let asset_id = validate_audio_clip_removal_documents(
        request,
        verified.manifest().project_id(),
        &clip,
        &previous_track,
    )?;

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    reject_unhandled_audio_clip_transactions(&backups)?;
    let recovered_resources =
        recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_tracks =
        recover_audio_track_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    let recovered_clips =
        recover_audio_clip_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered_resources.is_empty()
        || !recovered_tracks.is_empty()
        || !recovered_clips.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior project transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before audio-clip removal",
            error,
        )
    })?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before audio-clip removal staging",
        ));
    }
    let active_track = read_bounded(
        &open_relative_parent(&verified.root_dir, track_descriptor.path())?,
        file_name(track_descriptor.path())?,
        MAX_JSON_BYTES,
    )?;
    let active_clip = read_bounded(
        &open_relative_parent(&verified.root_dir, clip_descriptor.path())?,
        file_name(clip_descriptor.path())?,
        MAX_JSON_BYTES,
    )?;
    if active_track != previous_track || active_clip != clip {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "audio-clip resources changed before removal staging",
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
    let candidate_project = candidate_audio_clip_removal_manifest(
        &active_project,
        request,
        clip_descriptor.path(),
        next_revision,
    )?;
    let removed_clip_directory = verified
        .manifest()
        .resources()
        .iter()
        .filter(|descriptor| descriptor.role() == ResourceRole::Clip)
        .count()
        == 1;
    let transaction_name = format!("{AUDIO_CLIP_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create audio-clip removal transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-clip removal transaction",
                error,
            )
        })?;
    let result = run_audio_clip_removal(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        track_descriptor.path(),
        clip_descriptor.path(),
        &asset_id,
        &clip,
        &previous_track,
        removed_clip_directory,
        &active_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_audio_clip_transaction(&backups, transaction, &transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio clip was removed but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(operation_error) => {
            let tx = match read_audio_clip_transaction(&transaction, &transaction_name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_audio_clip_prepublication_residue(&transaction, &transaction_name)?;
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    return Err(operation_error);
                }
                Err(source) => {
                    return Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio-clip removal failed and its durable transaction could not be read".to_owned(),
                        published: false,
                        source: Some(Box::new(source)),
                    });
                }
            };
            match recover_audio_clip_transaction(
                verified.root(),
                &verified.root_dir,
                &backups,
                &transaction,
                &tx,
            ) {
                Ok(AudioClipPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )?;
                    Err(operation_error)
                }
                Ok(AudioClipPublicationRecoveryDisposition::Committed) => {
                    let outcome = audio_clip_removal_outcome(&tx)?;
                    clear_and_remove_audio_clip_transaction(
                        &backups,
                        transaction,
                        &transaction_name,
                    )
                    .map_err(|error| ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "audio clip was removed but transaction cleanup is incomplete"
                            .to_owned(),
                        published: true,
                        source: Some(Box::new(error)),
                    })?;
                    Ok(outcome)
                }
                Err(recovery_error) => {
                    let published = recovery_error.published();
                    Err(ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: if published {
                            "audio clip was removed but verification or transaction recovery is incomplete"
                        } else {
                            "audio-clip removal and recovery both failed"
                        }
                        .to_owned(),
                        published,
                        source: Some(Box::new(recovery_error)),
                    })
                }
            }
        }
    }
}

fn reject_unhandled_audio_clip_transactions(
    backups: &Dir,
) -> Result<(), ProjectResourcePublicationError> {
    let unhandled = find_unhandled_project_transaction(
        backups,
        &[
            ProjectTransactionFamily::ProjectResource,
            ProjectTransactionFamily::AudioTrack,
            ProjectTransactionFamily::AudioClip,
        ],
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "inspect project transactions before audio-clip publication",
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
fn run_audio_clip_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: AudioClipPublicationRequest<'_>,
    track_path: &str,
    clip_path: &str,
    sample_path: Option<&str>,
    sample_bytes: Option<&[u8]>,
    previous_track: &[u8],
    created_clip_directory: bool,
    sample_destination: SampleDestinationPlan,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        AudioClipPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioClipPublicationOutcome, ProjectResourcePublicationError> {
    write_audio_clip_intent(transaction, transaction_name)?;
    if let Some(bytes) = sample_bytes {
        write_exclusive_synced(transaction, SAMPLE_STAGING_NAME, bytes)?;
    }
    write_exclusive_synced(transaction, CLIP_STAGING_NAME, request.clip_bytes)?;
    write_exclusive_synced(transaction, TRACK_STAGING_NAME, request.track_bytes)?;
    write_exclusive_synced(transaction, PREVIOUS_TRACK_NAME, previous_track)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = AudioClipTransactionRecord {
        format: "hyper-audio-clip-publication-transaction".to_owned(),
        format_version: if sample_bytes.is_some() { 3 } else { 1 },
        id: audio_clip_transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        clip_id: request.clip_id.to_owned(),
        track_id: request.track_id.to_owned(),
        asset_id: request.asset_id.to_owned(),
        clip: TransactionFile {
            path: clip_path.to_owned(),
            byte_length: request.clip_bytes.len() as u64,
            sha256: hex_digest(digest(request.clip_bytes)),
        },
        track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: request.track_bytes.len() as u64,
            sha256: hex_digest(digest(request.track_bytes)),
        },
        previous_track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: previous_track.len() as u64,
            sha256: hex_digest(digest(previous_track)),
        },
        sample: sample_path
            .zip(sample_bytes)
            .map(|(path, bytes)| TransactionFile {
                path: path.to_owned(),
                byte_length: bytes.len() as u64,
                sha256: hex_digest(digest(bytes)),
            }),
        created_clip_directory,
        created_sample_directory: sample_destination.created_sample_directory,
        created_asset_directory: sample_destination.created_asset_directory,
        removed_clip_directory: false,
        operation: AudioClipTransactionOperation::Publication,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-clip transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(AudioClipPublicationPhase::Transaction, verified.root())?;

    if sample_destination.created_sample_directory {
        let assets = open_or_create_dir(&verified.root_dir, "assets")?;
        assets.create_dir("samples").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "create imported-sample directory",
                error,
            )
        })?;
        sync_dir(&assets)?;
    }
    if let Some(path) = sample_path {
        let sample_parent = open_relative_parent(&verified.root_dir, path)?;
        transaction
            .hard_link(SAMPLE_STAGING_NAME, &sample_parent, file_name(path)?)
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::ResourceConflict,
                    "publish imported sample without overwriting",
                    error,
                )
            })?;
        sync_dir(&sample_parent)?;
        observer(AudioClipPublicationPhase::Sample, verified.root())?;
    }

    if created_clip_directory {
        verified.root_dir.create_dir("clips").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "create audio-clip resource directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
    }
    let clip_parent = open_relative_parent(&verified.root_dir, clip_path)?;
    transaction
        .hard_link(CLIP_STAGING_NAME, &clip_parent, file_name(clip_path)?)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "publish new audio-clip resource without overwriting",
                error,
            )
        })?;
    sync_dir(&clip_parent)?;
    observer(AudioClipPublicationPhase::Clip, verified.root())?;

    let track_parent = open_relative_parent(&verified.root_dir, track_path)?;
    publish_staged_file(
        transaction,
        TRACK_STAGING_NAME,
        &track_parent,
        &track_parent,
        file_name(track_path)?,
        &format!(".audio-clip-track-{}.json", tx.id),
        "audio-clip owning track",
    )?;
    observer(AudioClipPublicationPhase::Track, verified.root())?;

    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-clip publication candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-clip publication candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".audio-clip-project-{}.json", tx.id),
        "audio-clip project manifest",
    )?;
    observer(AudioClipPublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    audio_clip_outcome(&tx)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable removal order and every crash observation point remain visible in one coordinator"
)]
fn run_audio_clip_removal(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: AudioClipRemovalRequest<'_>,
    track_path: &str,
    clip_path: &str,
    asset_id: &str,
    clip: &[u8],
    previous_track: &[u8],
    removed_clip_directory: bool,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        AudioClipPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioClipRemovalOutcome, ProjectResourcePublicationError> {
    write_audio_clip_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, CLIP_STAGING_NAME, clip)?;
    write_exclusive_synced(transaction, TRACK_STAGING_NAME, request.track_bytes)?;
    write_exclusive_synced(transaction, PREVIOUS_TRACK_NAME, previous_track)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = AudioClipTransactionRecord {
        format: "hyper-audio-clip-removal-transaction".to_owned(),
        format_version: 4,
        id: audio_clip_transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        clip_id: request.clip_id.to_owned(),
        track_id: request.track_id.to_owned(),
        asset_id: asset_id.to_owned(),
        clip: TransactionFile {
            path: clip_path.to_owned(),
            byte_length: clip.len() as u64,
            sha256: hex_digest(digest(clip)),
        },
        track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: request.track_bytes.len() as u64,
            sha256: hex_digest(digest(request.track_bytes)),
        },
        previous_track: TransactionFile {
            path: track_path.to_owned(),
            byte_length: previous_track.len() as u64,
            sha256: hex_digest(digest(previous_track)),
        },
        sample: None,
        created_clip_directory: false,
        created_sample_directory: false,
        created_asset_directory: false,
        removed_clip_directory,
        operation: AudioClipTransactionOperation::Removal,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-clip removal transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(AudioClipPublicationPhase::Transaction, verified.root())?;

    let track_parent = open_relative_parent(&verified.root_dir, track_path)?;
    publish_staged_file(
        transaction,
        TRACK_STAGING_NAME,
        &track_parent,
        &track_parent,
        file_name(track_path)?,
        &format!(".audio-clip-track-{}.json", tx.id),
        "audio-clip removal owning track",
    )?;
    observer(AudioClipPublicationPhase::Track, verified.root())?;

    remove_existing_audio_clip(&verified.root_dir, &tx)?;
    if removed_clip_directory {
        verified.root_dir.remove_dir("clips").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty audio-clip directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
    }
    observer(AudioClipPublicationPhase::Clip, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-clip removal candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-clip removal candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".audio-clip-project-{}.json", tx.id),
        "audio-clip removal project manifest",
    )?;
    observer(AudioClipPublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    audio_clip_removal_outcome(&tx)
}

fn validate_audio_clip_request(
    verified: &StructurallyVerifiedProject,
    request: AudioClipPublicationRequest<'_>,
    clip_path: &str,
    sample_path: Option<&str>,
) -> Result<(), ProjectResourcePublicationError> {
    if !is_valid_entity_id(request.clip_id)
        || request.clip_id == verified.manifest().project_id()
        || !is_valid_entity_id(request.track_id)
        || !is_valid_entity_id(request.asset_id)
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == request.clip_id || descriptor.path() == clip_path)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|descriptor| descriptor.id() == request.clip_id || descriptor.path() == clip_path)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-clip identity or path conflicts with the active project",
        ));
    }
    let matching_assets = verified
        .manifest()
        .assets()
        .iter()
        .filter(|descriptor| descriptor.id() == request.asset_id)
        .count();
    if (sample_path.is_none() && matching_assets != 1)
        || (sample_path.is_some()
            && (matching_assets != 0
                || verified.manifest().assets().iter().any(|descriptor| {
                    descriptor.path() == sample_path.expect("checked sample path")
                })))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-clip source asset does not exist uniquely",
        ));
    }
    Ok(())
}

fn validate_imported_sample(
    bytes: &[u8],
    sample_rate_hz: u32,
) -> Result<usize, ProjectResourcePublicationError> {
    let byte_limit = u64::try_from(bytes.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "imported sample is not addressable",
        )
    })?;
    if byte_limit == 0 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "imported sample is empty",
        ));
    }
    let limits = DecodeLimits::new(byte_limit, byte_limit, bytes.len()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "construct imported-sample verification limits",
            error,
        )
    })?;
    let audio = decode_bytes(bytes.to_vec(), AudioContainer::Wav, sample_rate_hz, limits).map_err(
        |error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "verify imported sample WAV",
                error,
            )
        },
    )?;
    Ok(audio.frame_count())
}

fn validate_imported_clip_range(
    clip_bytes: &[u8],
    frame_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let clip: Value = serde_json::from_slice(clip_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse imported audio clip",
            error,
        )
    })?;
    if clip["source"]["start_frame"].as_u64() != Some(0)
        || clip["source"]["frame_count"].as_u64() != u64::try_from(frame_count).ok()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "imported audio clip must cover the complete sample",
        ));
    }
    Ok(())
}

fn inspect_sample_destination(
    root: &Dir,
    sample_path: &str,
) -> Result<SampleDestinationPlan, ProjectResourcePublicationError> {
    let assets = match root.symlink_metadata("assets") {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "asset directory is not a real directory",
            ));
        }
        Ok(_) => root.open_dir_nofollow("assets").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open asset directory",
                error,
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SampleDestinationPlan {
                created_asset_directory: true,
                created_sample_directory: true,
            });
        }
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect asset directory",
                error,
            ));
        }
    };
    let samples = match assets.symlink_metadata("samples") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(SampleDestinationPlan {
                created_asset_directory: false,
                created_sample_directory: true,
            });
        }
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect imported-sample directory",
                error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "imported-sample directory is not a real directory",
            ));
        }
        Ok(_) => assets.open_dir_nofollow("samples").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open imported-sample directory",
                error,
            )
        })?,
    };
    match samples.symlink_metadata(file_name(sample_path)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SampleDestinationPlan {
            created_asset_directory: false,
            created_sample_directory: false,
        }),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "imported-sample path already exists",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect imported-sample destination",
            error,
        )),
    }
}

fn validate_audio_clip_documents(
    request: AudioClipPublicationRequest<'_>,
    project_id: &str,
    previous_track_bytes: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let clip: Value = serde_json::from_slice(request.clip_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse new audio-clip resource",
            error,
        )
    })?;
    let previous: Value = serde_json::from_slice(previous_track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current audio-clip owning track",
            error,
        )
    })?;
    let candidate: Value = serde_json::from_slice(request.track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse candidate audio-clip owning track",
            error,
        )
    })?;
    if clip["format"].as_str() != Some("hyper-audio-clip")
        || clip["format_version"].as_u64() != Some(1)
        || clip["id"].as_str() != Some(request.clip_id)
        || clip["project_id"].as_str() != Some(project_id)
        || clip["track_id"].as_str() != Some(request.track_id)
        || clip["source"]["asset_id"].as_str() != Some(request.asset_id)
        || previous["format"].as_str() != Some("hyper-track")
        || previous["format_version"].as_u64() != Some(1)
        || previous["id"].as_str() != Some(request.track_id)
        || previous["project_id"].as_str() != Some(project_id)
        || previous["kind"].as_str() != Some("audio")
        || candidate["format"].as_str() != Some("hyper-track")
        || candidate["id"].as_str() != Some(request.track_id)
        || candidate["project_id"].as_str() != Some(project_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-clip resources do not match the active project",
        ));
    }
    let candidate_ids = candidate["clip_ids"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate audio-track clip collection is invalid",
        )
    })?;
    if candidate_ids.last().and_then(Value::as_str) != Some(request.clip_id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "candidate audio track must append the matching clip reference",
        ));
    }
    let mut preserved = candidate;
    preserved["clip_ids"]
        .as_array_mut()
        .expect("candidate clip_ids was validated")
        .pop();
    if preserved != previous {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-clip publication must preserve the owning track and append one clip reference",
        ));
    }
    Ok(())
}

fn validate_audio_clip_removal_documents(
    request: AudioClipRemovalRequest<'_>,
    project_id: &str,
    clip_bytes: &[u8],
    previous_track_bytes: &[u8],
) -> Result<String, ProjectResourcePublicationError> {
    validate_canonical_resource(clip_bytes)?;
    let clip: Value = serde_json::from_slice(clip_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current audio-clip resource",
            error,
        )
    })?;
    let previous: Value = serde_json::from_slice(previous_track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse current audio-clip owning track",
            error,
        )
    })?;
    let candidate: Value = serde_json::from_slice(request.track_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse audio-clip removal owning track",
            error,
        )
    })?;
    let asset_id = clip["source"]["asset_id"].as_str().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current audio clip has no source asset identity",
        )
    })?;
    if clip["format"].as_str() != Some("hyper-audio-clip")
        || clip["format_version"].as_u64() != Some(1)
        || clip["id"].as_str() != Some(request.clip_id)
        || clip["project_id"].as_str() != Some(project_id)
        || clip["track_id"].as_str() != Some(request.track_id)
        || !is_valid_entity_id(asset_id)
        || previous["format"].as_str() != Some("hyper-track")
        || previous["format_version"].as_u64() != Some(1)
        || previous["id"].as_str() != Some(request.track_id)
        || previous["project_id"].as_str() != Some(project_id)
        || previous["kind"].as_str() != Some("audio")
        || candidate["format"].as_str() != Some("hyper-track")
        || candidate["id"].as_str() != Some(request.track_id)
        || candidate["project_id"].as_str() != Some(project_id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-clip removal resources do not match the active project",
        ));
    }
    let previous_ids = previous["clip_ids"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "current audio-track clip collection is invalid",
        )
    })?;
    if previous_ids
        .iter()
        .filter(|value| value.as_str() == Some(request.clip_id))
        .count()
        != 1
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "current audio track must contain exactly one matching clip reference",
        ));
    }
    let expected_ids = previous_ids
        .iter()
        .filter(|value| value.as_str() != Some(request.clip_id))
        .cloned()
        .collect();
    let mut expected = previous;
    expected["clip_ids"] = Value::Array(expected_ids);
    if candidate != expected {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-clip removal must preserve the owning track and remove one clip reference",
        ));
    }
    Ok(asset_id.to_owned())
}

fn inspect_clip_destination(
    root: &Dir,
    resource_path: &str,
) -> Result<bool, ProjectResourcePublicationError> {
    let directory = match root.symlink_metadata("clips") {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect audio-clip resource directory",
                error,
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "audio-clip resource directory is not a real directory",
            ));
        }
        Ok(_) => root.open_dir_nofollow("clips").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-clip resource directory",
                error,
            )
        })?,
    };
    match directory.symlink_metadata(file_name(resource_path)?) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-clip resource path already exists",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect audio-clip resource destination",
            error,
        )),
    }
}

fn candidate_audio_clip_manifest(
    active: &[u8],
    request: AudioClipPublicationRequest<'_>,
    clip_path: &str,
    sample_path: Option<&str>,
    sample_bytes: Option<&[u8]>,
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
    let track = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(request.track_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current audio-clip track descriptor disappeared",
            )
        })?;
    track["byte_length"] = request.track_bytes.len().into();
    track["sha256"] = hex_digest(digest(request.track_bytes)).into();
    resources.push(serde_json::json!({
        "id": request.clip_id,
        "role": "clip",
        "path": clip_path,
        "media_type": "application/json",
        "byte_length": request.clip_bytes.len(),
        "sha256": hex_digest(digest(request.clip_bytes)),
        "extensions": {}
    }));
    if let Some((path, bytes)) = sample_path.zip(sample_bytes) {
        value["assets"]
            .as_array_mut()
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "project asset declarations are invalid",
                )
            })?
            .push(serde_json::json!({
                "id": request.asset_id,
                "role": "sample",
                "path": path,
                "media_type": "audio/wav",
                "byte_length": bytes.len(),
                "sha256": hex_digest(digest(bytes)),
                "extensions": {}
            }));
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize audio-clip project manifest",
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
            "candidate audio-clip project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn candidate_audio_clip_removal_manifest(
    active: &[u8],
    request: AudioClipRemovalRequest<'_>,
    clip_path: &str,
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
    let track = resources
        .iter_mut()
        .find(|resource| resource["id"].as_str() == Some(request.track_id))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "current audio-clip track descriptor disappeared",
            )
        })?;
    track["byte_length"] = request.track_bytes.len().into();
    track["sha256"] = hex_digest(digest(request.track_bytes)).into();
    let before = resources.len();
    resources.retain(|resource| {
        resource["id"].as_str() != Some(request.clip_id)
            && resource["path"].as_str() != Some(clip_path)
    });
    if resources.len().checked_add(1) != Some(before) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-clip removal manifest must remove exactly one descriptor",
        ));
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize audio-clip removal project manifest",
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
            "candidate audio-clip removal manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn write_audio_clip_intent(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = TransactionIntent {
        format: "hyper-audio-clip-publication-intent".to_owned(),
        format_version: 1,
        id: audio_clip_transaction_id(directory_name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-clip publication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish audio-clip transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn recover_audio_clip_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<AudioClipPublicationRecovery>, ProjectResourcePublicationError> {
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
        if name.starts_with(AUDIO_CLIP_TRANSACTION_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    let mut outcomes = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-clip recovery transaction",
                error,
            )
        })?;
        let tx = match read_audio_clip_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_audio_clip_prepublication_residue(&transaction, &name)?;
                let id = audio_clip_transaction_id(&name)?;
                clear_and_remove_audio_clip_transaction(backups, transaction, &name)?;
                outcomes.push(AudioClipPublicationRecovery {
                    transaction_id: id,
                    disposition: AudioClipPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition =
            recover_audio_clip_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_audio_clip_transaction(backups, transaction, &name)?;
        outcomes.push(AudioClipPublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn recover_audio_clip_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<AudioClipPublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_audio_clip_transaction(transaction, tx)?;
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
        cleanup_audio_clip_temporaries(root, backups, transaction, tx)
            .and_then(|()| verify_committed(root_path, root, after))
            .map_err(|source| ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message:
                    "committed audio-clip publication could not be verified or fully recovered"
                        .to_owned(),
                published: true,
                source: Some(Box::new(source)),
            })?;
        return Ok(AudioClipPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip recovery found an unrelated project identity",
        ));
    }
    rollback_audio_clip_track(root, transaction, tx)?;
    if tx.operation == AudioClipTransactionOperation::Removal {
        restore_removed_clip_directory(root, tx)?;
        restore_removed_audio_clip(root, transaction, tx)?;
    } else {
        remove_published_audio_clip(root, tx)?;
        remove_published_imported_sample(root, tx)?;
    }
    cleanup_audio_clip_temporaries(root, backups, transaction, tx)?;
    remove_created_clip_directory(root, tx)?;
    remove_created_sample_directory(root, tx)?;
    remove_created_asset_directory(root, tx)?;
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
            "rolled-back audio-clip project does not verify",
            error,
        )
    })?;
    Ok(AudioClipPublicationRecoveryDisposition::RolledBack)
}

fn rollback_audio_clip_track(
    root: &Dir,
    transaction: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.track.path)?;
    let current = read_optional_file(&parent, file_name(&tx.track.path)?)?;
    let current_digest = current.as_deref().map(digest);
    let before = decode_digest(&tx.previous_track.sha256)?;
    let after = decode_digest(&tx.track.sha256)?;
    match current_digest {
        Some(found) if found == before => return Ok(()),
        Some(found) if found != after => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-clip rollback refuses unrelated track content",
            ));
        }
        Some(_) | None => {}
    }
    publish_staged_file(
        transaction,
        PREVIOUS_TRACK_NAME,
        &parent,
        &parent,
        file_name(&tx.previous_track.path)?,
        &format!(".audio-clip-track-rollback-{}.json", tx.id),
        "previous audio-clip owning track",
    )
}

fn remove_published_audio_clip(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = match open_relative_parent(root, &tx.clip.path) {
        Ok(parent) => parent,
        Err(error) if error_is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = file_name(&tx.clip.path)?;
    let Some(bytes) = read_optional_file(&parent, name)? else {
        return Ok(());
    };
    if digest(&bytes) != decode_digest(&tx.clip.sha256)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip rollback refuses unrelated clip content",
        ));
    }
    parent.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove rolled-back audio-clip resource",
            error,
        )
    })?;
    sync_dir(&parent)
}

fn remove_existing_audio_clip(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.clip.path)?;
    let name = file_name(&tx.clip.path)?;
    let bytes = read_bounded(&parent, name, MAX_JSON_BYTES)?;
    if bytes.len() as u64 != tx.clip.byte_length
        || digest(&bytes) != decode_digest(&tx.clip.sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "audio-clip resource changed before removal",
        ));
    }
    parent.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "remove audio-clip resource",
            error,
        )
    })?;
    sync_dir(&parent)
}

fn restore_removed_audio_clip(
    root: &Dir,
    transaction: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &tx.clip.path)?;
    let name = file_name(&tx.clip.path)?;
    let current = read_optional_file(&parent, name)?;
    let before = decode_digest(&tx.clip.sha256)?;
    match current.as_deref().map(digest) {
        Some(found) if found == before => return Ok(()),
        Some(_) => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-clip removal rollback refuses unrelated clip content",
            ));
        }
        None => {}
    }
    transaction
        .hard_link(CLIP_STAGING_NAME, &parent, name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "restore removed audio-clip resource",
                error,
            )
        })?;
    sync_dir(&parent)
}

fn restore_removed_clip_directory(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if !tx.removed_clip_directory {
        return Ok(());
    }
    match root.create_dir("clips") {
        Ok(()) => sync_dir(root),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = root.symlink_metadata("clips").map_err(|source| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "inspect restored audio-clip directory",
                    source,
                )
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "audio-clip rollback directory is not a real directory",
                ));
            }
            Ok(())
        }
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "restore removed audio-clip directory",
            error,
        )),
    }
}

fn remove_published_imported_sample(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let Some(sample) = &tx.sample else {
        return Ok(());
    };
    let parent = match open_relative_parent(root, &sample.path) {
        Ok(parent) => parent,
        Err(error) if error_is_not_found(&error) => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = file_name(&sample.path)?;
    let Some(bytes) = read_optional_file(&parent, name)? else {
        return Ok(());
    };
    if digest(&bytes) != decode_digest(&sample.sha256)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "imported-sample rollback refuses unrelated content",
        ));
    }
    parent.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove rolled-back imported sample",
            error,
        )
    })?;
    sync_dir(&parent)
}

fn remove_created_clip_directory(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if !tx.created_clip_directory {
        return Ok(());
    }
    match root.remove_dir("clips") {
        Ok(()) => sync_dir(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove transaction-created audio-clip directory",
            error,
        )),
    }
}

fn remove_created_sample_directory(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if !tx.created_sample_directory {
        return Ok(());
    }
    let assets = root.open_dir_nofollow("assets").map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open asset directory for imported-sample rollback",
            error,
        )
    })?;
    match assets.remove_dir("samples") {
        Ok(()) => sync_dir(&assets),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove transaction-created imported-sample directory",
            error,
        )),
    }
}

fn remove_created_asset_directory(
    root: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if !tx.created_asset_directory {
        return Ok(());
    }
    match root.remove_dir("assets") {
        Ok(()) => sync_dir(root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove transaction-created asset directory",
            error,
        )),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "recovery validates the complete transaction graph before permitting any filesystem mutation"
)]
fn validate_staged_audio_clip_transaction(
    transaction: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let clip = read_bounded(transaction, CLIP_STAGING_NAME, MAX_JSON_BYTES)?;
    let track = read_bounded(transaction, TRACK_STAGING_NAME, MAX_JSON_BYTES)?;
    let previous_track = read_bounded(transaction, PREVIOUS_TRACK_NAME, MAX_JSON_BYTES)?;
    for bytes in [&clip, &track, &previous_track] {
        validate_canonical_resource(bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged audio-clip resource is not canonical",
                error,
            )
        })?;
    }
    if clip.len() as u64 != tx.clip.byte_length
        || digest(&clip) != decode_digest(&tx.clip.sha256)?
        || track.len() as u64 != tx.track.byte_length
        || digest(&track) != decode_digest(&tx.track.sha256)?
        || previous_track.len() as u64 != tx.previous_track.byte_length
        || digest(&previous_track) != decode_digest(&tx.previous_track.sha256)?
        || tx.track.path != tx.previous_track.path
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged audio-clip bytes do not match the transaction record",
        ));
    }
    let sample_bytes = if let Some(sample) = &tx.sample {
        let maximum = usize::try_from(sample.byte_length).map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged imported sample length is not addressable",
            )
        })?;
        let bytes = read_bounded(transaction, SAMPLE_STAGING_NAME, maximum)?;
        if bytes.len() as u64 != sample.byte_length
            || digest(&bytes) != decode_digest(&sample.sha256)?
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged imported sample does not match the transaction record",
            ));
        }
        Some(bytes)
    } else {
        None
    };
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged audio-clip project identity changed",
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
            "audio-clip transaction project revisions are inconsistent",
        ));
    }
    if tx.operation == AudioClipTransactionOperation::Removal {
        return validate_staged_audio_clip_removal(
            tx,
            &clip,
            &track,
            &previous_track,
            &previous_project,
            &candidate_project,
            &previous_manifest,
            &candidate_manifest,
        );
    }
    let previous_track_descriptor = manifest_resource(&previous_manifest, &tx.track_id)?;
    let candidate_track_descriptor = manifest_resource(&candidate_manifest, &tx.track_id)?;
    let candidate_clip_descriptor = manifest_resource(&candidate_manifest, &tx.clip_id)?;
    let candidate_sample_descriptor = tx.sample.as_ref().map(|_| {
        candidate_manifest
            .assets()
            .iter()
            .find(|descriptor| descriptor.id() == tx.asset_id)
    });
    if previous_track_descriptor.role() != ResourceRole::Track
        || candidate_track_descriptor.role() != ResourceRole::Track
        || previous_track_descriptor.path() != tx.previous_track.path
        || previous_track_descriptor.byte_length() != previous_track.len() as u64
        || previous_track_descriptor.sha256() != hex_digest(digest(&previous_track))
        || candidate_track_descriptor.path() != tx.track.path
        || candidate_track_descriptor.byte_length() != track.len() as u64
        || candidate_track_descriptor.sha256() != hex_digest(digest(&track))
        || candidate_clip_descriptor.role() != ResourceRole::Clip
        || candidate_clip_descriptor.path() != tx.clip.path
        || candidate_clip_descriptor.byte_length() != clip.len() as u64
        || candidate_clip_descriptor.sha256() != hex_digest(digest(&clip))
        || previous_manifest
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == tx.clip_id || descriptor.path() == tx.clip.path)
        || match (&tx.sample, candidate_sample_descriptor) {
            (None, None) => {
                previous_manifest
                    .assets()
                    .iter()
                    .filter(|descriptor| descriptor.id() == tx.asset_id)
                    .count()
                    != 1
            }
            (Some(sample), Some(Some(descriptor))) => {
                previous_manifest
                    .assets()
                    .iter()
                    .any(|previous| previous.id() == tx.asset_id || previous.path() == sample.path)
                    || descriptor.path() != sample.path
                    || descriptor.role() != AssetRole::Sample
                    || descriptor.media_type() != AudioMediaType::AudioWav
                    || descriptor.byte_length() != sample.byte_length
                    || descriptor.sha256() != sample.sha256
            }
            _ => true,
        }
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip descriptors do not match the transaction",
        ));
    }
    let request = AudioClipPublicationRequest {
        expected_project_sha256: digest(&previous_project),
        clip_id: &tx.clip_id,
        clip_bytes: &clip,
        track_id: &tx.track_id,
        track_bytes: &track,
        asset_id: &tx.asset_id,
    };
    validate_audio_clip_documents(request, previous_manifest.project_id(), &previous_track)?;
    if let Some(bytes) = sample_bytes.as_deref() {
        let frames = validate_imported_sample(bytes, previous_manifest.sample_rate_hz())?;
        validate_imported_clip_range(&clip, frames)?;
    }
    let expected = candidate_audio_clip_manifest(
        &previous_project,
        request,
        &tx.clip.path,
        tx.sample.as_ref().map(|sample| sample.path.as_str()),
        sample_bytes.as_deref(),
        tx.after_project_revision,
    )?;
    if expected != candidate_project {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip candidate manifest contains unrelated changes",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "recovery validates both staged manifests and all three staged resources as one graph"
)]
fn validate_staged_audio_clip_removal(
    tx: &AudioClipTransactionRecord,
    clip: &[u8],
    track: &[u8],
    previous_track: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    previous_manifest: &ProjectManifest,
    candidate_manifest: &ProjectManifest,
) -> Result<(), ProjectResourcePublicationError> {
    if tx.sample.is_some()
        || tx.created_clip_directory
        || tx.created_sample_directory
        || tx.created_asset_directory
        || tx.removed_clip_directory
            != (previous_manifest
                .resources()
                .iter()
                .filter(|descriptor| descriptor.role() == ResourceRole::Clip)
                .count()
                == 1)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip removal transaction contains publication state",
        ));
    }
    let previous_track_descriptor = manifest_resource(previous_manifest, &tx.track_id)?;
    let previous_clip_descriptor = manifest_resource(previous_manifest, &tx.clip_id)?;
    let candidate_track_descriptor = manifest_resource(candidate_manifest, &tx.track_id)?;
    if previous_track_descriptor.role() != ResourceRole::Track
        || candidate_track_descriptor.role() != ResourceRole::Track
        || previous_clip_descriptor.role() != ResourceRole::Clip
        || previous_track_descriptor.path() != tx.previous_track.path
        || previous_track_descriptor.byte_length() != previous_track.len() as u64
        || previous_track_descriptor.sha256() != hex_digest(digest(previous_track))
        || candidate_track_descriptor.path() != tx.track.path
        || candidate_track_descriptor.byte_length() != track.len() as u64
        || candidate_track_descriptor.sha256() != hex_digest(digest(track))
        || previous_clip_descriptor.path() != tx.clip.path
        || previous_clip_descriptor.byte_length() != clip.len() as u64
        || previous_clip_descriptor.sha256() != hex_digest(digest(clip))
        || candidate_manifest
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == tx.clip_id || descriptor.path() == tx.clip.path)
        || previous_manifest
            .assets()
            .iter()
            .filter(|descriptor| descriptor.id() == tx.asset_id)
            .count()
            != 1
        || candidate_manifest.assets() != previous_manifest.assets()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip removal descriptors do not match the transaction",
        ));
    }
    let request = AudioClipRemovalRequest {
        expected_project_sha256: digest(previous_project),
        clip_id: &tx.clip_id,
        track_id: &tx.track_id,
        track_bytes: track,
    };
    let asset_id = validate_audio_clip_removal_documents(
        request,
        previous_manifest.project_id(),
        clip,
        previous_track,
    )?;
    if asset_id != tx.asset_id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip removal source asset identity changed",
        ));
    }
    let expected = candidate_audio_clip_removal_manifest(
        previous_project,
        request,
        &tx.clip.path,
        tx.after_project_revision,
    )?;
    if expected != candidate_project {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip removal candidate manifest contains unrelated changes",
        ));
    }
    Ok(())
}

fn read_audio_clip_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<AudioClipTransactionRecord, ProjectResourcePublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "audio-clip transaction intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-clip transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "audio-clip transaction")?;
    let tx: AudioClipTransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-clip transaction",
            error,
        )
    })?;
    let expected_id = audio_clip_transaction_id(directory_name)?;
    if intent.format != "hyper-audio-clip-publication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || !matches!(
            (tx.operation, tx.format.as_str(), tx.format_version),
            (
                AudioClipTransactionOperation::Publication,
                "hyper-audio-clip-publication-transaction",
                1..=3
            ) | (
                AudioClipTransactionOperation::Removal,
                "hyper-audio-clip-removal-transaction",
                4
            )
        )
        || (tx.format_version == 1
            && (tx.sample.is_some() || tx.created_sample_directory || tx.created_asset_directory))
        || (tx.format_version == 2 && (tx.sample.is_none() || tx.created_asset_directory))
        || (tx.format_version == 3
            && (tx.sample.is_none()
                || (tx.created_asset_directory && !tx.created_sample_directory)))
        || (tx.operation == AudioClipTransactionOperation::Removal
            && (tx.sample.is_some()
                || tx.created_clip_directory
                || tx.created_sample_directory
                || tx.created_asset_directory))
        || (tx.operation == AudioClipTransactionOperation::Publication && tx.removed_clip_directory)
        || !tx.extensions.is_empty()
        || tx.id != expected_id
        || !is_valid_entity_id(&tx.clip_id)
        || !is_valid_entity_id(&tx.track_id)
        || !is_valid_entity_id(&tx.asset_id)
        || tx.clip.path != format!("clips/{}.json", tx.clip_id)
        || tx.track.path != tx.previous_track.path
        || [&tx.clip, &tx.track, &tx.previous_track]
            .iter()
            .any(|file| {
                file.byte_length == 0
                    || file.byte_length > MAX_JSON_FILE_BYTES
                    || decode_digest(&file.sha256).is_err()
            })
        || tx.sample.as_ref().is_some_and(|sample| {
            sample.path != format!("assets/samples/{}.wav", tx.asset_id)
                || sample.byte_length == 0
                || decode_digest(&sample.sha256).is_err()
        })
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip publication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn validate_audio_clip_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let id = audio_clip_transaction_id(directory_name)?;
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete audio-clip transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete audio-clip transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            INTENT_STAGING_NAME
                | INTENT_NAME
                | SAMPLE_STAGING_NAME
                | CLIP_STAGING_NAME
                | TRACK_STAGING_NAME
                | PREVIOUS_TRACK_NAME
                | PROJECT_STAGING_NAME
                | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-clip transaction contains an unknown entry",
            ));
        }
    }
    if !missing_file(transaction, INTENT_NAME)? {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "audio-clip transaction intent")?;
        let intent: TransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse audio-clip transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-audio-clip-publication-intent"
            || intent.format_version != 1
            || intent.id != id
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-clip transaction intent is invalid",
            ));
        }
    }
    Ok(())
}

fn audio_clip_outcome(
    tx: &AudioClipTransactionRecord,
) -> Result<AudioClipPublicationOutcome, ProjectResourcePublicationError> {
    if tx.operation != AudioClipTransactionOperation::Publication {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip transaction is not a publication",
        ));
    }
    Ok(AudioClipPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        clip_id: tx.clip_id.clone(),
        clip_path: tx.clip.path.clone(),
        track_id: tx.track_id.clone(),
    })
}

fn audio_clip_removal_outcome(
    tx: &AudioClipTransactionRecord,
) -> Result<AudioClipRemovalOutcome, ProjectResourcePublicationError> {
    if tx.operation != AudioClipTransactionOperation::Removal {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip transaction is not a removal",
        ));
    }
    Ok(AudioClipRemovalOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        clip_id: tx.clip_id.clone(),
        track_id: tx.track_id.clone(),
    })
}

fn cleanup_audio_clip_temporaries(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &AudioClipTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    if let Ok(parent) = open_relative_parent(root, &tx.track.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".audio-clip-track-{}.json", tx.id),
            decode_digest(&tx.track.sha256)?,
        )?;
        remove_optional_matching_file(
            &parent,
            &format!(".audio-clip-track-rollback-{}.json", tx.id),
            decode_digest(&tx.previous_track.sha256)?,
        )?;
    }
    remove_optional_matching_file(
        backups,
        &format!(".audio-clip-project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    remove_optional_matching_file(
        backups,
        &format!(".project-rollback-{}.json", tx.id),
        digest(&previous),
    )
}

fn audio_clip_transaction_id(
    directory_name: &str,
) -> Result<String, ProjectResourcePublicationError> {
    let id = directory_name
        .strip_prefix(AUDIO_CLIP_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-clip transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip transaction identifier is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip transaction identifier is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn clear_audio_clip_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    for name in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        SAMPLE_STAGING_NAME,
        CLIP_STAGING_NAME,
        TRACK_STAGING_NAME,
        PREVIOUS_TRACK_NAME,
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
                    format!("remove audio-clip transaction file {name}"),
                    error,
                ));
            }
        }
    }
    sync_dir(transaction)
}

#[cfg(not(windows))]
fn clear_and_remove_audio_clip_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".audio-clip-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine audio-clip transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined audio-clip transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined audio-clip transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-clip transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_audio_clip_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-clip transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_audio_clip_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    clear_audio_clip_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-clip transaction",
            error,
        )
    })?;
    sync_dir(backups)
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
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334459";
    const IMPORT_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334460";

    struct Fixture {
        root: PathBuf,
        original_track: Vec<u8>,
        track: Vec<u8>,
        clip: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-audio-clip-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("timeline")).unwrap();
            fs::create_dir_all(root.join("tracks")).unwrap();
            fs::create_dir_all(root.join("assets/recordings")).unwrap();
            let timeline = serde_json_canonicalizer::to_vec(&json!({
                "format": "fixture-timeline"
            }))
            .unwrap();
            fs::write(root.join("timeline/timeline.json"), &timeline).unwrap();
            let original_track = serde_json_canonicalizer::to_vec(&json!({
                "format":"hyper-track", "format_version":1,
                "id":TRACK_ID, "project_id":PROJECT_ID, "name":"Audio 1",
                "kind":"audio", "order":0, "active":true, "clip_ids":[],
                "extensions":{"org.example.track":{"kept":true}}
            }))
            .unwrap();
            fs::write(
                root.join(format!("tracks/{TRACK_ID}.json")),
                &original_track,
            )
            .unwrap();
            let mut track_value: Value = serde_json::from_slice(&original_track).unwrap();
            track_value["clip_ids"] = json!([CLIP_ID]);
            let track = serde_json_canonicalizer::to_vec(&track_value).unwrap();
            let recording = b"verified-recording-asset";
            fs::write(
                root.join(format!("assets/recordings/{ASSET_ID}.wav")),
                recording,
            )
            .unwrap();
            let clip = serde_json_canonicalizer::to_vec(&json!({
                "format":"hyper-audio-clip", "format_version":1,
                "id":CLIP_ID, "project_id":PROJECT_ID, "track_id":TRACK_ID,
                "name":"Take 1", "enabled":true, "duration_ticks":4,
                "source":{"asset_id":ASSET_ID,"start_frame":0,"frame_count":63},
                "loop":{"enabled":false,"start_tick":0,"end_tick":4},
                "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
                "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
                "warp_markers":[
                    {"tick":0,"source_frame_offset":0},
                    {"tick":4,"source_frame_offset":63}
                ],
                "extensions":{}
            }))
            .unwrap();
            let manifest = serde_json_canonicalizer::to_vec(&json!({
                "format":"hyper-project", "format_version":1,
                "project_id":PROJECT_ID, "revision":1,
                "name":"Audio clip fixture", "sample_rate_hz":48000,
                "required_capabilities":{
                    "format":"hyper-hardware-capabilities", "format_version":1,
                    "audio_inputs":2, "audio_outputs":2, "cv_inputs":0,
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
                        "id":TRACK_ID, "role":"track",
                        "path":format!("tracks/{TRACK_ID}.json"),
                        "media_type":"application/json",
                        "byte_length":original_track.len(),
                        "sha256":hex_digest(digest(&original_track)), "extensions":{}
                    }
                ],
                "assets":[{
                    "id":ASSET_ID, "role":"recording",
                    "path":format!("assets/recordings/{ASSET_ID}.wav"),
                    "media_type":"audio/wav", "byte_length":recording.len(),
                    "sha256":hex_digest(digest(recording)), "extensions":{}
                }],
                "extensions":{}
            }))
            .unwrap();
            fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
            verify_project_package(&root).unwrap();
            Self {
                root,
                original_track,
                track,
                clip,
            }
        }

        fn without_assets() -> Self {
            let fixture = Self::new();
            let manifest_path = fixture.root.join(MANIFEST_NAME);
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            manifest["assets"] = json!([]);
            fs::write(
                &manifest_path,
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            fs::remove_dir_all(fixture.root.join("assets")).unwrap();
            verify_project_package(&fixture.root).unwrap();
            fixture
        }

        fn request(&self) -> AudioClipPublicationRequest<'_> {
            AudioClipPublicationRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                clip_id: CLIP_ID,
                clip_bytes: &self.clip,
                track_id: TRACK_ID,
                track_bytes: &self.track,
                asset_id: ASSET_ID,
            }
        }

        fn removal_request(&self) -> AudioClipRemovalRequest<'_> {
            AudioClipRemovalRequest {
                expected_project_sha256: *verify_project_package(&self.root)
                    .unwrap()
                    .manifest_sha256(),
                clip_id: CLIP_ID,
                track_id: TRACK_ID,
                track_bytes: &self.original_track,
            }
        }

        fn publish_clip(&self) {
            publish_audio_clip(&self.root, self.request()).unwrap();
        }

        fn transaction_names(&self) -> Vec<String> {
            let mut names: Vec<_> = fs::read_dir(self.root.join("backups"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(AUDIO_CLIP_TRANSACTION_PREFIX)
                })
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }

        fn transaction_count(&self) -> usize {
            self.transaction_names().len()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn publishes_clip_and_owning_track_atomically() {
        let fixture = Fixture::new();
        assert!(!fixture.root.join("clips").exists());

        let outcome = publish_audio_clip(&fixture.root, fixture.request()).unwrap();

        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.clip_id(), CLIP_ID);
        assert_eq!(outcome.track_id(), TRACK_ID);
        assert_eq!(outcome.clip_path(), format!("clips/{CLIP_ID}.json"));
        assert_eq!(
            fs::read(fixture.root.join(outcome.clip_path())).unwrap(),
            fixture.clip
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.track
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(
            verified
                .manifest()
                .resources()
                .iter()
                .filter(|descriptor| descriptor.role() == ResourceRole::Clip)
                .count(),
            1
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn removes_clip_and_owning_track_reference_but_retains_asset() {
        let fixture = Fixture::new();
        fixture.publish_clip();
        let asset_path = fixture
            .root
            .join(format!("assets/recordings/{ASSET_ID}.wav"));
        let asset_before = fs::read(&asset_path).unwrap();

        let outcome = remove_audio_clip(&fixture.root, fixture.removal_request()).unwrap();

        assert_eq!(outcome.project_revision(), 3);
        assert_eq!(outcome.clip_id(), CLIP_ID);
        assert_eq!(outcome.track_id(), TRACK_ID);
        assert!(!fixture.root.join(format!("clips/{CLIP_ID}.json")).exists());
        assert!(!fixture.root.join("clips").exists());
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.original_track
        );
        assert_eq!(fs::read(asset_path).unwrap(), asset_before);
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().revision(), 3);
        assert!(
            verified
                .manifest()
                .resources()
                .iter()
                .all(|descriptor| descriptor.id() != CLIP_ID)
        );
        assert!(
            verified
                .manifest()
                .assets()
                .iter()
                .any(|asset| asset.id() == ASSET_ID)
        );
        assert!(fixture.transaction_names().is_empty());
    }

    #[test]
    fn audio_clip_removal_rejects_stale_identity_and_non_exact_track_delta() {
        let fixture = Fixture::new();
        fixture.publish_clip();
        let mut stale = fixture.removal_request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            remove_audio_clip(&fixture.root, stale).unwrap_err().kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );

        let mut changed: Value = serde_json::from_slice(&fixture.original_track).unwrap();
        changed["name"] = "Changed too".into();
        let changed = serde_json_canonicalizer::to_vec(&changed).unwrap();
        let mut request = fixture.removal_request();
        request.track_bytes = &changed;
        assert_eq!(
            remove_audio_clip(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("clips/{CLIP_ID}.json"))).unwrap(),
            fixture.clip
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.track
        );
    }

    #[test]
    fn audio_clip_removal_crashes_before_manifest_restore_clip_and_track() {
        for crash_phase in [
            AudioClipPublicationPhase::Transaction,
            AudioClipPublicationPhase::Track,
            AudioClipPublicationPhase::Clip,
        ] {
            let fixture = Fixture::new();
            fixture.publish_clip();
            let manifest_before = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
            let asset_path = fixture
                .root
                .join(format!("assets/recordings/{ASSET_ID}.wav"));
            let asset_before = fs::read(&asset_path).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = remove_audio_clip_observed(
                    &fixture.root,
                    fixture.removal_request(),
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_audio_clip_publications(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                AudioClipPublicationRecoveryDisposition::RolledBack
            );
            assert_eq!(
                fs::read(fixture.root.join(format!("clips/{CLIP_ID}.json"))).unwrap(),
                fixture.clip
            );
            assert!(fixture.root.join("clips").is_dir());
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
                fixture.track
            );
            assert_eq!(
                fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
                manifest_before
            );
            assert_eq!(fs::read(asset_path).unwrap(), asset_before);
            assert_eq!(
                verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest()
                    .revision(),
                2
            );
            assert!(fixture.transaction_names().is_empty());
        }
    }

    #[test]
    fn audio_clip_removal_crash_after_manifest_recovers_as_committed() {
        let fixture = Fixture::new();
        fixture.publish_clip();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                remove_audio_clip_observed(&fixture.root, fixture.removal_request(), |phase, _| {
                    assert_ne!(
                        phase,
                        AudioClipPublicationPhase::Manifest,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());

        let recovered = recover_audio_clip_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioClipPublicationRecoveryDisposition::Committed
        );
        assert!(!fixture.root.join(format!("clips/{CLIP_ID}.json")).exists());
        assert!(!fixture.root.join("clips").exists());
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.original_track
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            3
        );
        assert!(fixture.transaction_names().is_empty());
    }

    #[test]
    fn committed_removal_retains_publication_authority_when_cleanup_is_incomplete() {
        let fixture = Fixture::new();
        fixture.publish_clip();
        let asset_path = fixture
            .root
            .join(format!("assets/recordings/{ASSET_ID}.wav"));
        let asset_before = fs::read(&asset_path).unwrap();
        let error =
            remove_audio_clip_observed(&fixture.root, fixture.removal_request(), |phase, _| {
                if phase != AudioClipPublicationPhase::Manifest {
                    return Ok(());
                }
                let name = fixture.transaction_names().pop().unwrap();
                let id = name.strip_prefix(AUDIO_CLIP_TRANSACTION_PREFIX).unwrap();
                fs::write(
                    fixture
                        .root
                        .join("backups")
                        .join(format!(".audio-clip-project-{id}.json")),
                    b"unrelated cleanup residue",
                )
                .unwrap();
                Err(failure(
                    ProjectResourcePublicationErrorKind::Io,
                    "simulated post-manifest removal failure",
                ))
            })
            .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert!(error.published());
        assert!(!fixture.root.join(format!("clips/{CLIP_ID}.json")).exists());
        assert!(!fixture.root.join("clips").exists());
        assert_eq!(fs::read(asset_path).unwrap(), asset_before);
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            3
        );
        let name = fixture.transaction_names().pop().unwrap();
        let id = name.strip_prefix(AUDIO_CLIP_TRANSACTION_PREFIX).unwrap();
        fs::remove_file(
            fixture
                .root
                .join("backups")
                .join(format!(".audio-clip-project-{id}.json")),
        )
        .unwrap();
        let recovered = recover_audio_clip_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioClipPublicationRecoveryDisposition::Committed
        );
        assert!(fixture.transaction_names().is_empty());
    }

    #[test]
    fn publishes_imported_sample_clip_and_track_as_one_revision() {
        let fixture = Fixture::new();
        let sample = hyper_audio_source::encode_float_wav(
            &hyper_audio_source::DecodedAudio::from_mono(48_000, vec![0.25; 63]).unwrap(),
        )
        .unwrap();
        let mut clip: Value = serde_json::from_slice(&fixture.clip).unwrap();
        clip["source"]["asset_id"] = IMPORT_ASSET_ID.into();
        let clip = serde_json_canonicalizer::to_vec(&clip).unwrap();
        let request = AudioClipPublicationRequest {
            expected_project_sha256: *verify_project_package(&fixture.root)
                .unwrap()
                .manifest_sha256(),
            clip_id: CLIP_ID,
            clip_bytes: &clip,
            track_id: TRACK_ID,
            track_bytes: &fixture.track,
            asset_id: IMPORT_ASSET_ID,
        };

        let outcome = publish_imported_audio_clip(
            &fixture.root,
            ImportedAudioClipPublicationRequest {
                clip: request,
                sample_bytes: &sample,
            },
        )
        .unwrap();

        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("assets/samples/{IMPORT_ASSET_ID}.wav"))
            )
            .unwrap(),
            sample
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        let imported = verified
            .manifest()
            .assets()
            .iter()
            .find(|asset| asset.id() == IMPORT_ASSET_ID)
            .unwrap();
        assert_eq!(
            imported.path(),
            format!("assets/samples/{IMPORT_ASSET_ID}.wav")
        );
        assert!(fixture.transaction_names().is_empty());
    }

    #[test]
    fn missing_asset_tree_is_admitted_as_a_directory_creation_plan() {
        let root = std::env::temp_dir().join(format!(
            "hyper-imported-sample-empty-root-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let (_, authority) = open_stable_project_root(&root).unwrap();

        assert_eq!(
            inspect_sample_destination(
                &authority,
                "assets/samples/018f47a2-6b2d-7c11-8a44-001122334499.wav"
            )
            .unwrap(),
            SampleDestinationPlan {
                created_asset_directory: true,
                created_sample_directory: true,
            }
        );

        drop(authority);
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn imported_sample_crash_before_manifest_removes_every_published_file() {
        for crash_phase in [
            AudioClipPublicationPhase::Sample,
            AudioClipPublicationPhase::Clip,
            AudioClipPublicationPhase::Track,
        ] {
            let fixture = Fixture::without_assets();
            let sample = hyper_audio_source::encode_float_wav(
                &hyper_audio_source::DecodedAudio::from_mono(48_000, vec![0.25; 63]).unwrap(),
            )
            .unwrap();
            let mut clip: Value = serde_json::from_slice(&fixture.clip).unwrap();
            clip["source"]["asset_id"] = IMPORT_ASSET_ID.into();
            let clip = serde_json_canonicalizer::to_vec(&clip).unwrap();
            let request = AudioClipPublicationRequest {
                expected_project_sha256: *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256(),
                clip_id: CLIP_ID,
                clip_bytes: &clip,
                track_id: TRACK_ID,
                track_bytes: &fixture.track,
                asset_id: IMPORT_ASSET_ID,
            };
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_audio_clip_observed(
                    &fixture.root,
                    request,
                    Some(&sample),
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));

            let recovered = recover_audio_clip_publications(&fixture.root).unwrap();
            assert_eq!(
                recovered[0].disposition(),
                AudioClipPublicationRecoveryDisposition::RolledBack
            );
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
                fixture.original_track
            );
            assert!(!fixture.root.join(format!("clips/{CLIP_ID}.json")).exists());
            assert!(
                !fixture
                    .root
                    .join(format!("assets/samples/{IMPORT_ASSET_ID}.wav"))
                    .exists()
            );
            assert!(!fixture.root.join("assets").exists());
            assert!(!fixture.root.join("clips").exists());
            assert!(fixture.transaction_names().is_empty());
            verify_project_package(&fixture.root).unwrap();
        }
    }

    #[test]
    fn imported_sample_crash_after_manifest_recovers_the_complete_revision() {
        let fixture = Fixture::without_assets();
        let sample = hyper_audio_source::encode_float_wav(
            &hyper_audio_source::DecodedAudio::from_mono(48_000, vec![0.25; 63]).unwrap(),
        )
        .unwrap();
        let mut clip: Value = serde_json::from_slice(&fixture.clip).unwrap();
        clip["source"]["asset_id"] = IMPORT_ASSET_ID.into();
        let clip = serde_json_canonicalizer::to_vec(&clip).unwrap();
        let request = AudioClipPublicationRequest {
            expected_project_sha256: *verify_project_package(&fixture.root)
                .unwrap()
                .manifest_sha256(),
            clip_id: CLIP_ID,
            clip_bytes: &clip,
            track_id: TRACK_ID,
            track_bytes: &fixture.track,
            asset_id: IMPORT_ASSET_ID,
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_audio_clip_observed(&fixture.root, request, Some(&sample), |phase, _| {
                    assert_ne!(
                        phase,
                        AudioClipPublicationPhase::Manifest,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());

        let recovered = recover_audio_clip_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioClipPublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("assets/samples/{IMPORT_ASSET_ID}.wav"))
            )
            .unwrap(),
            sample
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("clips/{CLIP_ID}.json"))).unwrap(),
            clip
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.track
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert!(
            verified
                .manifest()
                .assets()
                .iter()
                .any(|asset| asset.id() == IMPORT_ASSET_ID)
        );
        assert!(fixture.transaction_names().is_empty());
    }

    #[test]
    fn rejects_stale_identity_and_non_exact_track_delta() {
        let fixture = Fixture::new();
        let mut stale = fixture.request();
        stale.expected_project_sha256 = [0; 32];
        assert_eq!(
            publish_audio_clip(&fixture.root, stale).unwrap_err().kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );

        let mut changed: Value = serde_json::from_slice(&fixture.track).unwrap();
        changed["name"] = "Changed too".into();
        let changed = serde_json_canonicalizer::to_vec(&changed).unwrap();
        let mut request = fixture.request();
        request.track_bytes = &changed;
        assert_eq!(
            publish_audio_clip(&fixture.root, request)
                .unwrap_err()
                .kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(!fixture.root.join("clips").exists());
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
            fixture.original_track
        );
    }

    #[test]
    fn crashes_before_manifest_roll_back_clip_track_and_owned_directory() {
        for crash_phase in [
            AudioClipPublicationPhase::Transaction,
            AudioClipPublicationPhase::Clip,
            AudioClipPublicationPhase::Track,
        ] {
            let fixture = Fixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_audio_clip_observed(
                    &fixture.root,
                    fixture.request(),
                    None,
                    |phase, _| {
                        assert_ne!(phase, crash_phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let recovered = recover_audio_clip_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                AudioClipPublicationRecoveryDisposition::RolledBack
            );
            assert!(!fixture.root.join("clips").exists());
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
                fixture.original_track
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
    fn crash_after_manifest_recovers_as_committed() {
        let fixture = Fixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_audio_clip_observed(&fixture.root, fixture.request(), None, |phase, _| {
                    assert_ne!(
                        phase,
                        AudioClipPublicationPhase::Manifest,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());

        let recovered = recover_audio_clip_publications(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioClipPublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("clips/{CLIP_ID}.json"))).unwrap(),
            fixture.clip
        );
        assert_eq!(
            fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
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
    fn committed_manifest_retains_publication_authority_when_recovery_is_incomplete() {
        let fixture = Fixture::new();
        let error =
            publish_audio_clip_observed(&fixture.root, fixture.request(), None, |phase, _| {
                if phase != AudioClipPublicationPhase::Manifest {
                    return Ok(());
                }
                let name = fixture.transaction_names().pop().unwrap();
                let id = name.strip_prefix(AUDIO_CLIP_TRANSACTION_PREFIX).unwrap();
                fs::write(
                    fixture
                        .root
                        .join("backups")
                        .join(format!(".audio-clip-project-{id}.json")),
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
        let name = fixture.transaction_names().pop().unwrap();
        let id = name.strip_prefix(AUDIO_CLIP_TRANSACTION_PREFIX).unwrap();
        fs::remove_file(
            fixture
                .root
                .join("backups")
                .join(format!(".audio-clip-project-{id}.json")),
        )
        .unwrap();
        let recovered = recover_audio_clip_publications(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            AudioClipPublicationRecoveryDisposition::Committed
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn prepublication_residue_is_discarded_only_for_owned_allowed_states() {
        for residue in ["empty", "intent", "partial"] {
            let fixture = Fixture::new();
            let id = Uuid::new_v4().to_string();
            let name = format!("{AUDIO_CLIP_TRANSACTION_PREFIX}{id}");
            fs::create_dir(fixture.root.join("backups")).unwrap();
            let path = fixture.root.join("backups").join(&name);
            fs::create_dir(&path).unwrap();
            if residue != "empty" {
                fs::write(
                    path.join(INTENT_NAME),
                    serde_json_canonicalizer::to_vec(&json!({
                        "format":"hyper-audio-clip-publication-intent",
                        "format_version":1,
                        "id":id,
                        "extensions":{}
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
            if residue == "partial" {
                fs::write(path.join(CLIP_STAGING_NAME), b"partial staging bytes").unwrap();
            }

            let recovered = recover_audio_clip_publications(&fixture.root).unwrap();

            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                AudioClipPublicationRecoveryDisposition::RolledBack
            );
            assert_eq!(recovered[0].transaction_id(), id);
            assert_eq!(fixture.transaction_count(), 0);
            assert!(!fixture.root.join("clips").exists());
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
                fixture.original_track
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
    fn ambiguous_prepublication_residue_fails_closed_and_retains_evidence() {
        for residue in ["invalid-intent", "corrupt-record", "unknown-entry"] {
            let fixture = Fixture::new();
            let id = Uuid::new_v4().to_string();
            let name = format!("{AUDIO_CLIP_TRANSACTION_PREFIX}{id}");
            fs::create_dir(fixture.root.join("backups")).unwrap();
            let path = fixture.root.join("backups").join(&name);
            fs::create_dir(&path).unwrap();
            match residue {
                "invalid-intent" => fs::write(
                    path.join(INTENT_NAME),
                    serde_json_canonicalizer::to_vec(&json!({
                        "format":"hyper-audio-clip-publication-intent",
                        "format_version":1,
                        "id":Uuid::new_v4().to_string(),
                        "extensions":{}
                    }))
                    .unwrap(),
                )
                .unwrap(),
                "corrupt-record" => {
                    fs::write(
                        path.join(INTENT_NAME),
                        serde_json_canonicalizer::to_vec(&json!({
                            "format":"hyper-audio-clip-publication-intent",
                            "format_version":1,
                            "id":id,
                            "extensions":{}
                        }))
                        .unwrap(),
                    )
                    .unwrap();
                    fs::write(path.join(TRANSACTION_NAME), b"{").unwrap();
                }
                "unknown-entry" => fs::write(path.join("unknown"), b"unowned").unwrap(),
                _ => unreachable!(),
            }

            let error = recover_audio_clip_publications(&fixture.root).unwrap_err();

            assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
            assert_eq!(fixture.transaction_names(), [name]);
            assert!(!fixture.root.join("clips").exists());
            assert_eq!(
                fs::read(fixture.root.join(format!("tracks/{TRACK_ID}.json"))).unwrap(),
                fixture.original_track
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
    fn recovery_refuses_unrelated_clip_and_track_content() {
        for replace_track in [false, true] {
            let fixture = Fixture::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_audio_clip_observed(
                    &fixture.root,
                    fixture.request(),
                    None,
                    |phase, _| {
                        assert_ne!(
                            phase,
                            AudioClipPublicationPhase::Track,
                            "simulated process crash"
                        );
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let unrelated = serde_json_canonicalizer::to_vec(&json!({"unrelated":true})).unwrap();
            let path = if replace_track {
                fixture.root.join(format!("tracks/{TRACK_ID}.json"))
            } else {
                fixture.root.join(format!("clips/{CLIP_ID}.json"))
            };
            fs::write(&path, &unrelated).unwrap();

            let error = recover_audio_clip_publications(&fixture.root).unwrap_err();

            assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
            assert_eq!(fs::read(path).unwrap(), unrelated);
            assert_eq!(fixture.transaction_count(), 1);
        }
    }

    #[test]
    fn staged_clip_blocks_other_publication_until_global_recovery() {
        let fixture = Fixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_audio_clip_observed(&fixture.root, fixture.request(), None, |phase, _| {
                    assert_ne!(
                        phase,
                        AudioClipPublicationPhase::Transaction,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());
        let replacement = serde_json_canonicalizer::to_vec(&json!({
            "format":"replacement-timeline"
        }))
        .unwrap();

        let error = super::super::publish_project_resource(
            &fixture.root,
            super::super::ProjectResourcePublicationRequest {
                expected_project_sha256: *verify_project_package(&fixture.root)
                    .unwrap()
                    .manifest_sha256(),
                resource_id: TIMELINE_ID,
                expected_role: ResourceRole::Timeline,
                resource_bytes: &replacement,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        let report = crate::recover_project_transactions(&fixture.root).unwrap();
        assert_eq!(report.audio_clips().len(), 1);
        assert_eq!(
            report.audio_clips()[0].disposition(),
            AudioClipPublicationRecoveryDisposition::RolledBack
        );
        assert!(report.resources().is_empty());
        assert!(report.audio_tracks().is_empty());
        assert_eq!(fixture.transaction_count(), 0);
        verify_project_package(&fixture.root).unwrap();
    }
}
