use std::io;
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{
    AssetRole, AudioMediaType, ProjectManifest, ResourceRole, is_valid_entity_id,
};
use hyper_recording_metadata::RecordingMetadataDocument;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_JSON_BYTES, MAX_SAFE_INTEGER,
    PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME, ProjectResourcePublicationError,
    ProjectResourcePublicationErrorKind, StructurallyVerifiedProject, TRANSACTION_NAME, caused,
    decode_digest, digest, failure, file_name, hex_digest, missing_file, open_or_create_dir,
    open_relative_parent, open_stable_project_root, publish_staged_file, read_bounded,
    require_canonical, sync_dir, verify_project_candidate, verify_project_package,
    write_exclusive_synced,
};
use crate::project_lock::{
    ProjectLock, RECORDING_DISCARD_TRANSACTION_PREFIX, find_unhandled_project_transaction,
};
use crate::stable_file::verify_open_stable;

const FORMAT: &str = "hyper-recording-discard-transaction";
const INTENT_FORMAT: &str = "hyper-recording-discard-intent";
const VERSION: u32 = 1;
const ASSET_BACKUP_NAME: &str = "recording.wav";
const METADATA_BACKUP_NAME: &str = "metadata.json";

#[derive(Clone, Copy, Debug)]
pub struct RecordingDiscardRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub recording_id: &'a str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingDiscardOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    recording_id: String,
    asset_id: String,
    removed_bytes: u64,
}

impl RecordingDiscardOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }
    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }
    #[must_use]
    pub fn recording_id(&self) -> &str {
        &self.recording_id
    }
    #[must_use]
    pub fn asset_id(&self) -> &str {
        &self.asset_id
    }
    #[must_use]
    pub const fn removed_bytes(&self) -> u64 {
        self.removed_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingDiscardRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingDiscardRecovery {
    transaction_id: String,
    disposition: RecordingDiscardRecoveryDisposition,
}

impl RecordingDiscardRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }
    #[must_use]
    pub const fn disposition(&self) -> RecordingDiscardRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Transaction,
    Asset,
    Metadata,
    RecordingsDirectory,
    AssetDirectory,
    Manifest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    format: String,
    format_version: u32,
    id: String,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FileRecord {
    id: String,
    path: String,
    byte_length: u64,
    sha256: String,
    backup_name: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    format: String,
    format_version: u32,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    asset: FileRecord,
    metadata: FileRecord,
    removed_recordings_directory: bool,
    removed_asset_directory: bool,
    extensions: serde_json::Map<String, Value>,
}

struct SelectedRecording {
    recording_id: String,
    asset: FileRecord,
    metadata: FileRecord,
}

/// Permanently removes one confirmed, unpromoted project recording.
///
/// # Errors
///
/// Returns a classified validation, concurrency, integrity, I/O, or recovery
/// failure. A published error retains whether the manifest commit occurred.
pub fn discard_recording_take(
    project_path: impl AsRef<Path>,
    request: RecordingDiscardRequest<'_>,
) -> Result<RecordingDiscardOutcome, ProjectResourcePublicationError> {
    discard_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable recorded-take discard transaction in one project.
///
/// # Errors
///
/// Returns a fail-closed error for ambiguous, corrupt, inaccessible, or
/// concurrently owned recovery evidence.
pub fn recover_recording_discards(
    project_path: impl AsRef<Path>,
) -> Result<Vec<RecordingDiscardRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for recording-discard recovery",
            error,
        )
    })?;
    let Some(backups) = super::open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    recover_locked(&root_path, &root, &backups)
}

fn discard_observed(
    project_path: &Path,
    request: RecordingDiscardRequest<'_>,
    mut observer: impl FnMut(Phase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<RecordingDiscardOutcome, ProjectResourcePublicationError> {
    if !is_valid_entity_id(request.recording_id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "recording discard requires a canonical recording identity",
        ));
    }
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before recording discard",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after recording discard was confirmed",
        ));
    }
    let selected = validate_request(&verified, request.recording_id)?;
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    reject_unhandled(&backups)?;
    if !recover_locked(verified.root(), &verified.root_dir, &backups)?.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior recording discard was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before recording discard",
            error,
        )
    })?;
    let active = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before recording-discard staging",
        ));
    }
    let asset_authority = verified_authority(
        &verified.root_dir,
        &selected.asset,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify recording asset before discard staging",
    )?;
    let metadata_authority = verified_authority(
        &verified.root_dir,
        &selected.metadata,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify recording metadata before discard staging",
    )?;
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
    let candidate = candidate_manifest(&active, &selected, next_revision)?;
    let removed_recordings_directory = verified
        .manifest()
        .assets()
        .iter()
        .filter(|asset| asset.role() == AssetRole::Recording)
        .count()
        == 1
        && verified
            .manifest()
            .resources()
            .iter()
            .filter(|resource| resource.role() == ResourceRole::RecordingMetadata)
            .count()
            == 1;
    let removed_asset_directory =
        removed_recordings_directory && verified.manifest().assets().len() == 1;
    let name = format!("{RECORDING_DISCARD_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create recording-discard transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open recording-discard transaction",
            error,
        )
    })?;
    let result = run_discard(
        &verified,
        &backups,
        &transaction,
        &name,
        &selected,
        &asset_authority,
        &metadata_authority,
        &active,
        &candidate,
        next_revision,
        removed_recordings_directory,
        removed_asset_directory,
        &mut observer,
    );
    finish_result(
        verified.root(),
        &verified.root_dir,
        &backups,
        transaction,
        &name,
        result,
    )
}

fn validate_request(
    verified: &StructurallyVerifiedProject,
    recording_id: &str,
) -> Result<SelectedRecording, ProjectResourcePublicationError> {
    let resources = verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| resource.id() == recording_id)
        .collect::<Vec<_>>();
    let [metadata_descriptor] = resources.as_slice() else {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "recording discard requires exactly one recording-metadata resource",
        ));
    };
    if metadata_descriptor.role() != ResourceRole::RecordingMetadata {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "recording discard resource is not recording metadata",
        ));
    }
    let metadata_bytes = verified.read_resource(recording_id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read recording metadata before discard",
            error,
        )
    })?;
    require_canonical(&metadata_bytes, "recording metadata before discard")?;
    let metadata = RecordingMetadataDocument::parse(&metadata_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse recording metadata before discard",
            error,
        )
    })?;
    if metadata.id() != recording_id || metadata.project_id() != verified.manifest().project_id() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording metadata identity does not match its project authority",
        ));
    }
    let matching_metadata = verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| {
            resource.role() == ResourceRole::RecordingMetadata && resource.id() == recording_id
        })
        .count();
    if matching_metadata != 1 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording metadata authority is ambiguous",
        ));
    }
    let asset_id = metadata.asset_id();
    let assets = verified
        .manifest()
        .assets()
        .iter()
        .filter(|asset| asset.id() == asset_id)
        .collect::<Vec<_>>();
    let [asset_descriptor] = assets.as_slice() else {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording metadata does not resolve exactly one asset",
        ));
    };
    if asset_descriptor.role() != AssetRole::Recording
        || asset_descriptor.media_type() != AudioMediaType::AudioWav
        || asset_descriptor.path() != format!("assets/recordings/{asset_id}.wav")
        || metadata_descriptor.path() != format!("assets/recordings/{recording_id}.json")
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording discard binding has an unexpected role, media type, or path",
        ));
    }
    if verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| resource.role() == ResourceRole::RecordingMetadata)
        .any(|resource| {
            resource.id() != recording_id
                && verified
                    .read_resource(resource.id())
                    .ok()
                    .and_then(|bytes| RecordingMetadataDocument::parse(&bytes).ok())
                    .is_some_and(|other| other.asset_id() == asset_id)
        })
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording asset has another metadata binding",
        ));
    }
    let clip_references_recording = verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| resource.role() == ResourceRole::Clip)
        .try_fold(false, |referenced, resource| {
            if referenced {
                return Ok(true);
            }
            let bytes = verified.read_resource(resource.id()).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "read audio clip while classifying recording discard",
                    error,
                )
            })?;
            require_canonical(&bytes, "audio clip used for recording discard")?;
            let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "parse audio clip while classifying recording discard",
                    error,
                )
            })?;
            let source_asset = value
                .pointer("/source/asset_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "audio clip has no source asset identity",
                    )
                })?;
            Ok(source_asset == asset_id)
        })?;
    if clip_references_recording {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "recording discard refuses a take referenced by a reusable clip",
        ));
    }
    Ok(SelectedRecording {
        recording_id: recording_id.to_owned(),
        asset: record(
            asset_descriptor.id(),
            asset_descriptor.path(),
            asset_descriptor.byte_length(),
            asset_descriptor.sha256(),
            ASSET_BACKUP_NAME,
        ),
        metadata: record(
            metadata_descriptor.id(),
            metadata_descriptor.path(),
            metadata_descriptor.byte_length(),
            metadata_descriptor.sha256(),
            METADATA_BACKUP_NAME,
        ),
    })
}

fn record(id: &str, path: &str, byte_length: u64, sha256: &str, backup_name: &str) -> FileRecord {
    FileRecord {
        id: id.to_owned(),
        path: path.to_owned(),
        byte_length,
        sha256: sha256.to_owned(),
        backup_name: backup_name.to_owned(),
    }
}

fn candidate_manifest(
    active: &[u8],
    selected: &SelectedRecording,
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
    let before_resources = resources.len();
    resources.retain(|resource| resource["id"].as_str() != Some(selected.recording_id.as_str()));
    if before_resources.checked_sub(resources.len()) != Some(1) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording discard manifest did not remove exactly one metadata resource",
        ));
    }
    let assets = value["assets"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project asset declarations are invalid",
        )
    })?;
    let before_assets = assets.len();
    assets.retain(|asset| asset["id"].as_str() != Some(selected.asset.id.as_str()));
    if before_assets.checked_sub(assets.len()) != Some(1) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording discard manifest did not remove exactly one recording asset",
        ));
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize recording-discard manifest",
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
            "candidate recording-discard manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn run_discard(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    name: &str,
    selected: &SelectedRecording,
    asset_authority: &Handle,
    metadata_authority: &Handle,
    previous: &[u8],
    candidate: &[u8],
    next_revision: u64,
    removed_recordings_directory: bool,
    removed_asset_directory: bool,
    observer: &mut impl FnMut(Phase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<RecordingDiscardOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, name)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous)?;
    let tx = Transaction {
        format: FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        before_project_sha256: hex_digest(digest(previous)),
        after_project_sha256: hex_digest(digest(candidate)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        asset: record(
            &selected.asset.id,
            &selected.asset.path,
            selected.asset.byte_length,
            &selected.asset.sha256,
            ASSET_BACKUP_NAME,
        ),
        metadata: record(
            &selected.metadata.id,
            &selected.metadata.path,
            selected.metadata.byte_length,
            &selected.metadata.sha256,
            METADATA_BACKUP_NAME,
        ),
        removed_recordings_directory,
        removed_asset_directory,
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize recording-discard transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(Phase::Transaction, verified.root())?;
    move_verified(
        &verified.root_dir,
        transaction,
        &tx.asset,
        asset_authority,
        "move recording asset into discard transaction",
    )?;
    observer(Phase::Asset, verified.root())?;
    move_verified(
        &verified.root_dir,
        transaction,
        &tx.metadata,
        metadata_authority,
        "move recording metadata into discard transaction",
    )?;
    observer(Phase::Metadata, verified.root())?;
    if tx.removed_recordings_directory {
        let assets = verified
            .root_dir
            .open_dir_nofollow("assets")
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::ProjectChanged,
                    "open recording asset directory before removal",
                    error,
                )
            })?;
        assets.remove_dir("recordings").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty recordings directory",
                error,
            )
        })?;
        sync_dir(&assets)?;
        observer(Phase::RecordingsDirectory, verified.root())?;
    }
    if tx.removed_asset_directory {
        verified.root_dir.remove_dir("assets").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty asset directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
        observer(Phase::AssetDirectory, verified.root())?;
    }
    verify_project_candidate(verified.root(), &verified.root_dir, candidate).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "recording-discard candidate does not verify",
            error,
        )
    })?;
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".recording-discard-project-{}.json", tx.id),
        "recording-discard project manifest",
    )?;
    observer(Phase::Manifest, verified.root())?;
    let committed = verify_project_package(verified.root()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "committed recording discard does not verify",
            error,
        )
    })?;
    if committed.manifest_sha256() != &digest(candidate) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "committed recording-discard identity changed",
        ));
    }
    Ok(RecordingDiscardOutcome {
        project_identity: digest(candidate),
        project_revision: next_revision,
        recording_id: selected.recording_id.clone(),
        asset_id: selected.asset.id.clone(),
        removed_bytes: selected.asset.byte_length,
    })
}

fn verified_authority(
    root: &Dir,
    file: &FileRecord,
    kind: ProjectResourcePublicationErrorKind,
    operation: &'static str,
) -> Result<Handle, ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &file.path)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let opened = parent
        .open_with(file_name(&file.path)?, &options)
        .map_err(|error| caused(kind, operation, error))?;
    let retained = opened
        .try_clone()
        .map_err(|error| caused(kind, "retain recording-discard file authority", error))?;
    let authority = Handle::from_file(retained.into_std())
        .map_err(|error| caused(kind, "identify recording-discard file authority", error))?;
    verify_open_stable(
        opened.into_std(),
        operation,
        file.byte_length,
        decode_digest(&file.sha256)?,
        false,
    )
    .map_err(|error| caused(kind, operation, error))?;
    Ok(authority)
}

fn move_verified(
    root: &Dir,
    transaction: &Dir,
    file: &FileRecord,
    authority: &Handle,
    operation: &'static str,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &file.path)?;
    let name = file_name(&file.path)?;
    parent
        .rename(name, transaction, &file.backup_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                operation,
                error,
            )
        })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    let moved = verified_authority(
        transaction,
        &FileRecord {
            path: file.backup_name.clone(),
            ..file.clone()
        },
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify moved recording-discard file",
    );
    if moved.as_ref().is_ok_and(|moved| moved == authority) {
        return Ok(());
    }
    transaction
        .rename(&file.backup_name, &parent, name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "restore changed recording-discard file after authority mismatch",
                error,
            )
        })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    match moved {
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "recording-discard file path changed before removal",
        )),
        Err(error) => Err(error),
    }
}

fn reject_unhandled(backups: &Dir) -> Result<(), ProjectResourcePublicationError> {
    if find_unhandled_project_transaction(backups, &[])
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "inspect project transactions before recording discard",
                error,
            )
        })?
        .is_some()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    Ok(())
}

fn write_intent(transaction: &Dir, name: &str) -> Result<(), ProjectResourcePublicationError> {
    let bytes = serde_json_canonicalizer::to_vec(&Intent {
        format: INTENT_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        extensions: serde_json::Map::new(),
    })
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize recording-discard intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish recording-discard intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn finish_result(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: Dir,
    name: &str,
    result: Result<RecordingDiscardOutcome, ProjectResourcePublicationError>,
) -> Result<RecordingDiscardOutcome, ProjectResourcePublicationError> {
    match result {
        Ok(outcome) => {
            clear_and_remove(backups, transaction, name)
                .map_err(|error| published_cleanup_error(error))?;
            Ok(outcome)
        }
        Err(operation) => {
            if missing_file(&transaction, TRANSACTION_NAME)? {
                clear_and_remove(backups, transaction, name)?;
                return Err(operation);
            }
            let tx = read_transaction(&transaction, name)?;
            match recover_one(root_path, root, &transaction, &tx)? {
                RecordingDiscardRecoveryDisposition::RolledBack => {
                    clear_and_remove(backups, transaction, name)?;
                    Err(operation)
                }
                RecordingDiscardRecoveryDisposition::Committed => {
                    let outcome = outcome(&tx)?;
                    clear_and_remove(backups, transaction, name)
                        .map_err(|error| published_cleanup_error(error))?;
                    Ok(outcome)
                }
            }
        }
    }
}

fn published_cleanup_error(
    error: ProjectResourcePublicationError,
) -> ProjectResourcePublicationError {
    ProjectResourcePublicationError {
        kind: ProjectResourcePublicationErrorKind::Recovery,
        message: "recording discard committed but transaction cleanup is incomplete".to_owned(),
        published: true,
        source: Some(Box::new(error)),
    }
}

fn recover_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<RecordingDiscardRecovery>, ProjectResourcePublicationError> {
    let mut names = backups
        .entries()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read project backups",
                error,
            )
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with(RECORDING_DISCARD_TRANSACTION_PREFIX))
        .collect::<Vec<_>>();
    names.sort_unstable();
    let mut recovered = Vec::new();
    recovered.try_reserve_exact(names.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "recording-discard recovery capacity is unavailable",
        )
    })?;
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "open recording-discard transaction",
                error,
            )
        })?;
        if missing_file(&transaction, TRANSACTION_NAME)? {
            validate_incomplete(&transaction, &name)?;
            clear_and_remove(backups, transaction, &name)?;
            continue;
        }
        let tx = read_transaction(&transaction, &name)?;
        let disposition = recover_one(root_path, root, &transaction, &tx)?;
        clear_and_remove(backups, transaction, &name)?;
        recovered.push(RecordingDiscardRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(recovered)
}

fn recover_one(
    root_path: &Path,
    root: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<RecordingDiscardRecoveryDisposition, ProjectResourcePublicationError> {
    let active = read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active) == decode_digest(&tx.before_project_sha256)? {
        if tx.removed_asset_directory {
            create_directory_if_missing(root, "assets", "restore asset directory")?;
        }
        if tx.removed_recordings_directory {
            let assets = root.open_dir_nofollow("assets").map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "open restored asset directory",
                    error,
                )
            })?;
            create_directory_if_missing(&assets, "recordings", "restore recordings directory")?;
            sync_dir(&assets)?;
        }
        restore(root, transaction, &tx.asset)?;
        restore(root, transaction, &tx.metadata)?;
        let restored = verify_project_package(root_path).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "verify rolled-back recording discard",
                error,
            )
        })?;
        if restored.manifest_sha256() != &decode_digest(&tx.before_project_sha256)? {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "rolled-back recording discard identity changed",
            ));
        }
        Ok(RecordingDiscardRecoveryDisposition::RolledBack)
    } else if digest(&active) == decode_digest(&tx.after_project_sha256)? {
        for file in [&tx.asset, &tx.metadata] {
            if entry_present(root, &file.path, "inspect committed recording-discard path")? {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "committed recording discard retained an active owned file",
                ));
            }
            if entry_present(
                transaction,
                &file.backup_name,
                "inspect committed recording-discard backup",
            )? {
                verified_authority(
                    transaction,
                    &FileRecord {
                        path: file.backup_name.clone(),
                        ..file.clone()
                    },
                    ProjectResourcePublicationErrorKind::Recovery,
                    "verify committed recording-discard backup",
                )?;
            }
        }
        let committed = verify_project_package(root_path).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "verify committed recording discard",
                error,
            )
        })?;
        if committed.manifest_sha256() != &decode_digest(&tx.after_project_sha256)? {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "committed recording-discard identity changed",
            ));
        }
        Ok(RecordingDiscardRecoveryDisposition::Committed)
    } else {
        Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "recording-discard project manifest matches neither transaction side",
        ))
    }
}

fn restore(
    root: &Dir,
    transaction: &Dir,
    file: &FileRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let active = entry_present(root, &file.path, "inspect recording-discard rollback path")?;
    let backup = entry_present(
        transaction,
        &file.backup_name,
        "inspect recording-discard rollback backup",
    )?;
    match (active, backup) {
        (true, false) => {
            verified_authority(
                root,
                file,
                ProjectResourcePublicationErrorKind::Recovery,
                "verify active recording-discard rollback file",
            )?;
            Ok(())
        }
        (false, true) => {
            let backup_record = FileRecord {
                path: file.backup_name.clone(),
                ..file.clone()
            };
            verified_authority(
                transaction,
                &backup_record,
                ProjectResourcePublicationErrorKind::Recovery,
                "verify recording-discard backup before rollback",
            )?;
            let parent = open_relative_parent(root, &file.path)?;
            transaction
                .rename(&file.backup_name, &parent, file_name(&file.path)?)
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "restore discarded recording file",
                        error,
                    )
                })?;
            sync_dir(&parent)?;
            sync_dir(transaction)
        }
        _ => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "recording-discard file exists on both or neither transaction side",
        )),
    }
}

fn read_transaction(
    transaction: &Dir,
    name: &str,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "recording-discard transaction")?;
    let tx: Transaction = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse recording-discard transaction",
            error,
        )
    })?;
    if tx.format != FORMAT
        || tx.format_version != VERSION
        || tx.id != transaction_id(name)?
        || !tx.extensions.is_empty()
        || tx.after_project_revision != tx.before_project_revision.checked_add(1).unwrap_or(0)
        || tx.after_project_revision > MAX_SAFE_INTEGER
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || !valid_record(&tx.asset, ASSET_BACKUP_NAME, ".wav")
        || !valid_record(&tx.metadata, METADATA_BACKUP_NAME, ".json")
        || tx.asset.id == tx.metadata.id
        || tx.asset.path != format!("assets/recordings/{}.wav", tx.asset.id)
        || tx.metadata.path != format!("assets/recordings/{}.json", tx.metadata.id)
        || (tx.removed_asset_directory && !tx.removed_recordings_directory)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "recording-discard transaction is invalid",
        ));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read recording-discard transaction entries",
            error,
        )
    })? {
        let name = entry
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "read recording-discard transaction entry",
                    error,
                )
            })?
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "recording-discard transaction has a non-Unicode entry",
                )
            })?;
        if !matches!(
            name.as_str(),
            INTENT_NAME
                | PROJECT_STAGING_NAME
                | PREVIOUS_PROJECT_NAME
                | TRANSACTION_NAME
                | ASSET_BACKUP_NAME
                | METADATA_BACKUP_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "recording-discard transaction contains an unknown entry",
            ));
        }
    }
    Ok(tx)
}

fn valid_record(file: &FileRecord, backup: &str, extension: &str) -> bool {
    is_valid_entity_id(&file.id)
        && file.path.starts_with("assets/recordings/")
        && file.byte_length > 0
        && file.backup_name == backup
        && decode_digest(&file.sha256).is_ok()
        && file.path.ends_with(extension)
}

fn validate_incomplete(
    transaction: &Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete recording-discard transaction",
            error,
        )
    })? {
        let entry_name = entry
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "read incomplete recording-discard entry",
                    error,
                )
            })?
            .file_name()
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "incomplete recording-discard transaction has a non-Unicode entry",
                )
            })?;
        if !matches!(
            entry_name.as_str(),
            INTENT_STAGING_NAME | INTENT_NAME | PROJECT_STAGING_NAME | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete recording-discard transaction contains an unknown entry",
            ));
        }
    }
    if !missing_file(transaction, INTENT_NAME)? {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "recording-discard intent")?;
        let intent: Intent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse recording-discard intent",
                error,
            )
        })?;
        if intent.format != INTENT_FORMAT
            || intent.format_version != VERSION
            || intent.id != transaction_id(name)?
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "recording-discard intent is invalid",
            ));
        }
    }
    Ok(())
}

fn outcome(tx: &Transaction) -> Result<RecordingDiscardOutcome, ProjectResourcePublicationError> {
    Ok(RecordingDiscardOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        recording_id: tx.metadata.id.clone(),
        asset_id: tx.asset.id.clone(),
        removed_bytes: tx.asset.byte_length,
    })
}
fn transaction_id(name: &str) -> Result<String, ProjectResourcePublicationError> {
    name.strip_prefix(RECORDING_DISCARD_TRANSACTION_PREFIX)
        .filter(|id| is_valid_entity_id(id))
        .map(str::to_owned)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "recording-discard transaction name is invalid",
            )
        })
}
fn entry_present(
    directory: &Dir,
    name: &str,
    operation: &'static str,
) -> Result<bool, ProjectResourcePublicationError> {
    match directory.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            operation,
            error,
        )),
    }
}
fn create_directory_if_missing(
    parent: &Dir,
    name: &str,
    operation: &'static str,
) -> Result<(), ProjectResourcePublicationError> {
    match parent.create_dir(name) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            operation,
            error,
        )),
    }
}
fn clear_and_remove(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    for file in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        PROJECT_STAGING_NAME,
        PREVIOUS_PROJECT_NAME,
        TRANSACTION_NAME,
        ASSET_BACKUP_NAME,
        METADATA_BACKUP_NAME,
    ] {
        match transaction.remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "clear recording-discard transaction",
                    error,
                ));
            }
        }
    }
    sync_dir(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove recording-discard transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RecordingImportRequest, import_recording};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const RECORDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334477";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334478";

    struct Fixture {
        root: std::path::PathBuf,
        source: std::path::PathBuf,
        source_bytes: Vec<u8>,
        metadata: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-recording-discard-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("timeline")).unwrap();
            let timeline = br#"{"format":"fixture-timeline"}"#;
            fs::write(root.join("timeline/timeline.json"), timeline).unwrap();
            let manifest = json!({
                "format":"hyper-project", "format_version":1, "project_id":PROJECT_ID, "revision":1, "name":"Discard fixture", "sample_rate_hz":48000,
                "required_capabilities":{"format":"hyper-hardware-capabilities","format_version":1,"audio_inputs":2,"audio_outputs":2,"cv_inputs":0,"cv_outputs":0,"gate_outputs":0,"has_clock_input":false,"has_reset_input":false,"has_clock_output":false,"usb_mode":"none","has_ethernet":true,"extensions":{}},
                "resources":[{"id":TIMELINE_ID,"role":"timeline","path":"timeline/timeline.json","media_type":"application/json","byte_length":timeline.len(),"sha256":hex(Sha256::digest(timeline).into()),"extensions":{}}], "assets":[], "extensions":{}
            });
            fs::write(
                root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let source = root.with_extension("source.wav");
            let source_bytes = wav();
            fs::write(&source, &source_bytes).unwrap();
            let metadata = serde_json_canonicalizer::to_vec(&json!({
                "format":"hyper-recording-metadata","format_version":1,"id":RECORDING_ID,"project_id":PROJECT_ID,"asset_id":ASSET_ID,"track_id":TIMELINE_ID,"capture_kind":"new","captured_at_utc":"2026-08-26T12:00:00.000Z","timeline_start_tick":0,"source_input_indices":[0,1],"sample_rate_hz":48000,"channels":2,"frame_count":1,"extensions":{}
            })).unwrap();
            Self {
                root,
                source,
                source_bytes,
                metadata,
            }
        }
        fn import(&self) {
            import_recording(
                &self.root,
                RecordingImportRequest {
                    recording_path: &self.source,
                    recording_byte_length: self.source_bytes.len() as u64,
                    recording_sha256: Sha256::digest(&self.source_bytes).into(),
                    metadata_bytes: &self.metadata,
                },
            )
            .unwrap();
        }
        fn identity(&self) -> [u8; 32] {
            *verify_project_package(&self.root)
                .unwrap()
                .manifest_sha256()
        }
        fn request(&self) -> RecordingDiscardRequest<'_> {
            RecordingDiscardRequest {
                expected_project_sha256: self.identity(),
                recording_id: RECORDING_ID,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_file(&self.source);
        }
    }

    fn hex(value: [u8; 32]) -> String {
        value.iter().map(|byte| format!("{byte:02x}")).collect()
    }
    fn wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 4]);
        bytes
    }

    #[test]
    fn discards_exact_unpromoted_recording_binding() {
        let fixture = Fixture::new();
        fixture.import();
        let outcome = discard_recording_take(&fixture.root, fixture.request()).unwrap();
        assert_eq!(outcome.recording_id(), RECORDING_ID);
        assert_eq!(outcome.asset_id(), ASSET_ID);
        assert_eq!(outcome.project_revision(), 3);
        let package = verify_project_package(&fixture.root).unwrap();
        assert!(
            !package
                .manifest()
                .resources()
                .iter()
                .any(|resource| resource.id() == RECORDING_ID)
        );
        assert!(
            !package
                .manifest()
                .assets()
                .iter()
                .any(|asset| asset.id() == ASSET_ID)
        );
        assert!(
            !fixture
                .root
                .join(format!("assets/recordings/{RECORDING_ID}.json"))
                .exists()
        );
        assert!(
            !fixture
                .root
                .join(format!("assets/recordings/{ASSET_ID}.wav"))
                .exists()
        );
        assert!(
            recover_recording_discards(&fixture.root)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn refuses_take_referenced_by_reusable_clip() {
        let fixture = Fixture::new();
        fixture.import();
        let clip_id = "018f47a2-6b2d-7c11-8a44-001122334479";
        let clip = serde_json_canonicalizer::to_vec(&json!({"format":"hyper-audio-clip","format_version":1,"id":clip_id,"project_id":PROJECT_ID,"track_id":TIMELINE_ID,"name":"Protected","source":{"asset_id":ASSET_ID,"start_frame":0,"frame_count":1},"duration_ticks":1,"enabled":true,"gain_decibels":0,"loop":{"enabled":false,"start_tick":0,"end_tick":1},"reverse":false,"fade_in_ticks":0,"fade_out_ticks":0,"extensions":{}})).unwrap();
        fs::create_dir_all(fixture.root.join("clips")).unwrap();
        fs::write(fixture.root.join("clips/protected.json"), &clip).unwrap();
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap()).unwrap();
        manifest["resources"].as_array_mut().unwrap().push(json!({"id":clip_id,"role":"clip","path":"clips/protected.json","media_type":"application/json","byte_length":clip.len(),"sha256":hex(Sha256::digest(&clip).into()),"extensions":{}}));
        manifest["revision"] = 3.into();
        fs::write(
            fixture.root.join(MANIFEST_NAME),
            serde_json_canonicalizer::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let error = discard_recording_take(&fixture.root, fixture.request()).unwrap_err();
        assert!(error.to_string().contains("referenced by a reusable clip"));
        assert!(
            fixture
                .root
                .join(format!("assets/recordings/{ASSET_ID}.wav"))
                .exists()
        );
    }

    #[test]
    fn recovery_preserves_one_complete_side_at_each_commit_boundary() {
        for phase in [
            Phase::Transaction,
            Phase::Asset,
            Phase::Metadata,
            Phase::RecordingsDirectory,
            Phase::AssetDirectory,
            Phase::Manifest,
        ] {
            let fixture = Fixture::new();
            fixture.import();
            let before = fixture.identity();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = discard_observed(&fixture.root, fixture.request(), |observed, _| {
                    assert_ne!(observed, phase, "simulated process crash");
                    Ok(())
                });
            }));
            assert!(result.is_err());
            let recovered = recover_recording_discards(&fixture.root).unwrap();
            assert_eq!(recovered.len(), 1);
            assert_eq!(
                recovered[0].disposition(),
                if phase == Phase::Manifest {
                    RecordingDiscardRecoveryDisposition::Committed
                } else {
                    RecordingDiscardRecoveryDisposition::RolledBack
                }
            );
            let package = verify_project_package(&fixture.root).unwrap();
            if phase == Phase::Manifest {
                assert!(
                    !package
                        .manifest()
                        .assets()
                        .iter()
                        .any(|asset| asset.id() == ASSET_ID)
                );
            } else {
                assert_eq!(*package.manifest_sha256(), before);
            }
        }
    }
}
