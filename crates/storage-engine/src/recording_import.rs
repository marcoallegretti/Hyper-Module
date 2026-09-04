use core::error::Error;
use core::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use cap_fs_ext::OpenOptionsMaybeDirExt;
use cap_std::fs::{Dir, OpenOptions};
use hyper_recording_metadata::{
    FinalizedRecordingHandoff, MAX_FINALIZED_RECORDING_BYTE_LENGTH, RecordingMetadataDocument,
};
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    MANIFEST_NAME, StructurallyVerifiedProject, open_stable_project_root, same_directory,
    verify_project_candidate, verify_project_package,
};
use crate::project_lock::{
    ProjectLock, RECORDING_IMPORT_TRANSACTION_PREFIX as TRANSACTION_PREFIX,
    find_unhandled_project_transaction,
};
use crate::stable_file::copy_open_stable;

const MAX_MANIFEST_BYTES: usize = 1_048_576;
const TRANSACTION_INTENT_NAME: &str = "intent.json";
const TRANSACTION_INTENT_STAGING_NAME: &str = ".intent.json.tmp";

#[derive(Clone, Copy, Debug)]
pub struct RecordingImportRequest<'a> {
    pub recording_path: &'a Path,
    pub recording_byte_length: u64,
    pub recording_sha256: [u8; 32],
    pub metadata_bytes: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingImportOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    recording_path: String,
    metadata_path: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingImportRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingImportRecovery {
    transaction_id: String,
    disposition: RecordingImportRecoveryDisposition,
}

impl RecordingImportRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> RecordingImportRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    recording: TransactionFile,
    metadata: TransactionFile,
    created_directories: Vec<String>,
    extensions: serde_json::Map<String, Value>,
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

impl RecordingImportOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn recording_path(&self) -> &str {
        &self.recording_path
    }

    #[must_use]
    pub fn metadata_path(&self) -> &str {
        &self.metadata_path
    }
}

#[derive(Debug)]
pub struct RecordingImportError {
    message: String,
    published: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RecordingImportError {
    /// Returns whether a verified recording import became the active project
    /// state before this error was observed.
    #[must_use]
    pub const fn published(&self) -> bool {
        self.published
    }

    /// Constructs a controlled recording-import error for higher-layer tests.
    ///
    /// This is unavailable from the default production build so callers cannot
    /// fabricate a durable-import outcome.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_test(message: impl Into<String>, published: bool) -> Self {
        Self {
            message: message.into(),
            published,
            source: None,
        }
    }
}

impl fmt::Display for RecordingImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RecordingImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

fn failure(message: impl Into<String>) -> RecordingImportError {
    RecordingImportError {
        message: message.into(),
        published: false,
        source: None,
    }
}

fn caused(
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> RecordingImportError {
    RecordingImportError {
        message: message.into(),
        published: false,
        source: Some(Box::new(source)),
    }
}

fn committed_cleanup_failure(source: RecordingImportError) -> RecordingImportError {
    RecordingImportError {
        message: "recording import committed but transaction cleanup is incomplete".to_owned(),
        published: true,
        source: Some(Box::new(source)),
    }
}

/// Imports one canonical finalized-recording handoff into a project.
///
/// # Errors
///
/// Returns an error for a malformed handoff or any error reported by
/// [`import_recording`].
pub fn import_finalized_recording_handoff(
    project_path: impl AsRef<Path>,
    handoff_bytes: &[u8],
) -> Result<RecordingImportOutcome, RecordingImportError> {
    let handoff = FinalizedRecordingHandoff::parse(handoff_bytes)
        .map_err(|error| caused("recording handoff is invalid", error))?;
    import_recording(
        project_path,
        RecordingImportRequest {
            recording_path: handoff.recording_path(),
            recording_byte_length: handoff.recording_byte_length(),
            recording_sha256: *handoff.recording_sha256(),
            metadata_bytes: handoff.metadata_bytes(),
        },
    )
}

/// Imports one verified recording and its canonical metadata into a project.
///
/// # Errors
///
/// Returns an integrity, domain, conflict, durability, or recovery error.
pub fn import_recording(
    project_path: impl AsRef<Path>,
    request: RecordingImportRequest<'_>,
) -> Result<RecordingImportOutcome, RecordingImportError> {
    import_recording_observed(project_path.as_ref(), request, |_| Ok(()))
}

/// Recovers every durable recording-import transaction for a project.
///
/// # Errors
///
/// Returns an error when the project, transaction, or owned publication has
/// changed, or when recovery cannot be completed durably.
pub fn recover_recording_imports(
    project_path: impl AsRef<Path>,
) -> Result<Vec<RecordingImportRecovery>, RecordingImportError> {
    let (root_path, root_dir) = open_stable_project_root(project_path.as_ref())
        .map_err(|error| caused("open project root for recovery", error))?;
    let Some(backups) = open_optional_dir(&root_dir, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups)
        .map_err(|error| caused("acquire project storage lock", error))?;
    let mut transaction_names = Vec::new();
    let entries = backups
        .entries()
        .map_err(|error| caused("read backups directory", error))?;
    for entry in entries {
        let entry = entry.map_err(|error| caused("read backups entry", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TRANSACTION_PREFIX) {
            continue;
        }
        transaction_names.push(name.to_owned());
    }
    transaction_names.sort();
    let mut outcomes = Vec::new();
    for name in transaction_names {
        let transaction = backups
            .open_dir(&name)
            .map_err(|error| caused(format!("open recovery transaction {name}"), error))?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error)
                if transaction
                    .metadata("transaction.json")
                    .is_err_and(|source| source.kind() == io::ErrorKind::NotFound) =>
            {
                let id = validate_prepublication_residue(&transaction, &name)?;
                clear_and_remove_transaction(&backups, transaction, &name)?;
                outcomes.push(RecordingImportRecovery {
                    transaction_id: id,
                    disposition: RecordingImportRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition =
            recover_transaction(&root_path, &root_dir, &backups, &transaction, &tx, None)?;
        clear_and_remove_transaction(&backups, transaction, &name).map_err(|error| {
            if disposition == RecordingImportRecoveryDisposition::Committed {
                committed_cleanup_failure(error)
            } else {
                error
            }
        })?;
        outcomes.push(RecordingImportRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn import_recording_observed(
    project_path: &Path,
    request: RecordingImportRequest<'_>,
    after_transaction_sync: impl FnOnce(&Path) -> Result<(), RecordingImportError>,
) -> Result<RecordingImportOutcome, RecordingImportError> {
    validate_request(request)?;
    let metadata = RecordingMetadataDocument::parse(request.metadata_bytes)
        .map_err(|error| caused("recording metadata is invalid", error))?;
    require_canonical_json(request.metadata_bytes, "recording metadata")?;
    let verified = verify_project_package(project_path)
        .map_err(|error| caused("project verification failed", error))?;
    if metadata.project_id() != verified.manifest.project_id() {
        return Err(failure("recording metadata belongs to another project"));
    }
    let next_revision = verified
        .manifest
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= 9_007_199_254_740_991)
        .ok_or_else(|| failure("project revision cannot be incremented"))?;
    let recording_relative = format!("assets/recordings/{}.wav", metadata.asset_id());
    let metadata_relative = format!("assets/recordings/{}.json", metadata.id());
    reject_collisions(
        &verified,
        metadata.id(),
        metadata.asset_id(),
        &recording_relative,
        &metadata_relative,
    )?;

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups)
        .map_err(|error| caused("acquire project storage lock", error))?;
    reject_unhandled_recording_transactions(&backups)?;
    verified
        .confirm_unchanged()
        .map_err(|error| caused("project changed before import", error))?;

    let existing_assets = open_optional_dir(&verified.root_dir, "assets")?;
    let existing_recordings = if let Some(assets) = existing_assets.as_ref() {
        open_optional_dir(assets, "recordings")?
    } else {
        None
    };
    let created_directories = match (&existing_assets, &existing_recordings) {
        (None, _) => vec!["assets".to_owned(), "assets/recordings".to_owned()],
        (Some(_), None) => vec!["assets/recordings".to_owned()],
        (Some(_), Some(_)) => Vec::new(),
    };

    let transaction_name = format!("{TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups
        .create_dir(&transaction_name)
        .map_err(|error| caused("create recording import transaction", error))?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir(&transaction_name)
        .map_err(|error| caused("open recording import transaction", error))?;

    let result = run_import(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request,
        &metadata,
        next_revision,
        &recording_relative,
        &metadata_relative,
        existing_assets.as_ref(),
        existing_recordings.as_ref(),
        &created_directories,
        after_transaction_sync,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_transaction(&backups, transaction, &transaction_name)
                .map_err(committed_cleanup_failure)?;
            Ok(outcome)
        }
        Err(operation_error) => recover_failed_recording_import(
            &verified,
            &backups,
            transaction,
            &transaction_name,
            existing_recordings.as_ref(),
            operation_error,
        ),
    }
}

fn recover_failed_recording_import(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: Dir,
    transaction_name: &str,
    existing_recordings: Option<&Dir>,
    operation_error: RecordingImportError,
) -> Result<RecordingImportOutcome, RecordingImportError> {
    let Ok(tx) = read_transaction(&transaction, transaction_name) else {
        clear_and_remove_transaction(backups, transaction, transaction_name)?;
        return Err(operation_error);
    };
    match recover_transaction(
        verified.root(),
        &verified.root_dir,
        backups,
        &transaction,
        &tx,
        existing_recordings,
    ) {
        Ok(RecordingImportRecoveryDisposition::RolledBack) => {
            clear_and_remove_transaction(backups, transaction, transaction_name)?;
            Err(operation_error)
        }
        Ok(RecordingImportRecoveryDisposition::Committed) => {
            let outcome = recovered_import_outcome(verified.root(), &tx)?;
            clear_and_remove_transaction(backups, transaction, transaction_name)
                .map_err(committed_cleanup_failure)?;
            Ok(outcome)
        }
        Err(recovery_error) => Err(RecordingImportError {
            message: "recording import and recovery both failed".to_owned(),
            published: recovery_error.published(),
            source: Some(Box::new(recovery_error)),
        }),
    }
}

fn recovered_import_outcome(
    project_path: &Path,
    transaction: &TransactionRecord,
) -> Result<RecordingImportOutcome, RecordingImportError> {
    let committed = verify_project_package(project_path)
        .map_err(|error| caused("recovered recording import does not verify", error))?;
    let identity = decode_digest(&transaction.after_project_sha256)?;
    if committed.manifest_sha256 != identity {
        return Err(failure(
            "recovered recording import project identity changed",
        ));
    }
    Ok(RecordingImportOutcome {
        project_identity: identity,
        project_revision: committed.manifest.revision(),
        recording_path: transaction.recording.path.clone(),
        metadata_path: transaction.metadata.path.clone(),
    })
}

#[allow(clippy::too_many_arguments)]
fn run_import(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: RecordingImportRequest<'_>,
    metadata: &RecordingMetadataDocument,
    next_revision: u64,
    recording_relative: &str,
    metadata_relative: &str,
    existing_assets: Option<&Dir>,
    existing_recordings: Option<&Dir>,
    created_directories: &[String],
    after_transaction_sync: impl FnOnce(&Path) -> Result<(), RecordingImportError>,
) -> Result<RecordingImportOutcome, RecordingImportError> {
    write_transaction_intent(transaction, transaction_name)?;
    copy_verified_recording(request, transaction, "recording.wav")?;
    validate_staged_recording(transaction, "recording.wav", metadata)?;
    write_exclusive_synced(transaction, "metadata.json", request.metadata_bytes)?;
    let active_bytes = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_MANIFEST_BYTES)?;
    if Sha256::digest(&active_bytes).as_slice() != verified.manifest_sha256 {
        return Err(failure("active project changed before recording staging"));
    }
    let candidate_bytes = candidate_manifest(
        &active_bytes,
        metadata,
        request,
        next_revision,
        recording_relative,
        metadata_relative,
    )?;
    write_exclusive_synced(transaction, "project.json", &candidate_bytes)?;
    write_exclusive_synced(transaction, "previous-project.json", &active_bytes)?;
    let transaction_bytes = transaction_document(
        verified.manifest_sha256,
        Sha256::digest(&candidate_bytes).into(),
        recording_relative,
        metadata_relative,
        request,
        metadata,
        transaction_name,
        created_directories,
    )?;
    write_exclusive_synced(transaction, "transaction.json", &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    after_transaction_sync(verified.root())?;

    let created_assets;
    let assets = if let Some(assets) = existing_assets {
        assets
    } else {
        created_assets = open_or_create_dir(&verified.root_dir, "assets")?;
        &created_assets
    };
    let created_recordings;
    let recordings = if let Some(recordings) = existing_recordings {
        recordings
    } else {
        created_recordings = open_or_create_dir(assets, "recordings")?;
        &created_recordings
    };
    transaction
        .hard_link(
            "recording.wav",
            recordings,
            format!("{}.wav", metadata.asset_id()),
        )
        .map_err(|error| caused("publish recording", error))?;
    sync_dir(recordings)?;
    transaction
        .hard_link(
            "metadata.json",
            recordings,
            format!("{}.json", metadata.id()),
        )
        .map_err(|error| caused("publish recording metadata", error))?;
    sync_dir(recordings)?;

    let candidate = verify_project_candidate(verified.root(), &verified.root_dir, &candidate_bytes)
        .map_err(|error| caused("recording import candidate does not verify", error))?;
    if candidate.revision() != next_revision {
        return Err(failure("recording import candidate revision changed"));
    }

    let manifest_temporary = format!(".project-{}.json", Uuid::new_v4());
    transaction
        .hard_link("project.json", backups, &manifest_temporary)
        .map_err(|error| caused("stage project manifest", error))?;
    sync_dir(backups)?;
    backups
        .rename(&manifest_temporary, &verified.root_dir, MANIFEST_NAME)
        .map_err(|error| caused("publish project manifest", error))?;
    sync_dir(&verified.root_dir)?;

    let committed = verify_project_package(verified.root())
        .map_err(|error| caused("committed project does not verify", error))?;
    if !same_directory(&verified.root_dir, &committed.root_dir)
        .map_err(|error| caused("compare committed project root", error))?
    {
        return Err(failure("project root changed during recording import"));
    }
    let identity: [u8; 32] = Sha256::digest(&candidate_bytes).into();
    if committed.manifest_sha256 != identity {
        return Err(failure("committed project identity changed"));
    }
    Ok(RecordingImportOutcome {
        project_identity: identity,
        project_revision: next_revision,
        recording_path: recording_relative.to_owned(),
        metadata_path: metadata_relative.to_owned(),
    })
}

fn reject_unhandled_recording_transactions(backups: &Dir) -> Result<(), RecordingImportError> {
    let unhandled = find_unhandled_project_transaction(backups, &[]).map_err(|error| {
        caused(
            "inspect project transactions before recording import",
            error,
        )
    })?;
    if unhandled.is_some() {
        return Err(failure(
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    Ok(())
}

fn validate_request(request: RecordingImportRequest<'_>) -> Result<(), RecordingImportError> {
    if request.recording_byte_length == 0
        || request.recording_byte_length > MAX_FINALIZED_RECORDING_BYTE_LENGTH
    {
        return Err(failure("recording byte length is invalid"));
    }
    if request.metadata_bytes.is_empty() || request.metadata_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(failure("recording metadata size is invalid"));
    }
    Ok(())
}

fn reject_collisions(
    verified: &StructurallyVerifiedProject,
    metadata_id: &str,
    asset_id: &str,
    recording_path: &str,
    metadata_path: &str,
) -> Result<(), RecordingImportError> {
    for resource in verified.manifest.resources() {
        if resource.id() == metadata_id || resource.id() == asset_id {
            return Err(failure("recording import identity already exists"));
        }
        if resource.path().eq_ignore_ascii_case(recording_path)
            || resource.path().eq_ignore_ascii_case(metadata_path)
        {
            return Err(failure("recording import path already exists"));
        }
    }
    for asset in verified.manifest.assets() {
        if asset.id() == metadata_id || asset.id() == asset_id {
            return Err(failure("recording import identity already exists"));
        }
        if asset.path().eq_ignore_ascii_case(recording_path)
            || asset.path().eq_ignore_ascii_case(metadata_path)
        {
            return Err(failure("recording import path already exists"));
        }
    }
    Ok(())
}

fn candidate_manifest(
    active: &[u8],
    metadata: &RecordingMetadataDocument,
    request: RecordingImportRequest<'_>,
    revision: u64,
    recording_path: &str,
    metadata_path: &str,
) -> Result<Vec<u8>, RecordingImportError> {
    let mut value: Value =
        serde_json::from_slice(active).map_err(|error| caused("parse active project", error))?;
    value["revision"] = revision.into();
    let resources = value["resources"]
        .as_array_mut()
        .ok_or_else(|| failure("project resources are invalid"))?;
    resources.push(json!({
        "id": metadata.id(), "role": "recording-metadata", "path": metadata_path,
        "media_type": "application/json", "byte_length": request.metadata_bytes.len(),
        "sha256": hex_digest(Sha256::digest(request.metadata_bytes).into()), "extensions": {}
    }));
    let assets = value["assets"]
        .as_array_mut()
        .ok_or_else(|| failure("project assets are invalid"))?;
    assets.push(json!({
        "id": metadata.asset_id(), "role": "recording", "path": recording_path,
        "media_type": "audio/wav", "byte_length": request.recording_byte_length,
        "sha256": hex_digest(request.recording_sha256), "extensions": {}
    }));
    let bytes = serde_json_canonicalizer::to_vec(&value)
        .map_err(|error| caused("serialize candidate project", error))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(failure("candidate project manifest exceeds its size limit"));
    }
    hyper_project_model::ProjectManifest::parse(&bytes)
        .map_err(|error| caused("candidate project is invalid", error))?;
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn transaction_document(
    before: [u8; 32],
    after: [u8; 32],
    recording_path: &str,
    metadata_path: &str,
    request: RecordingImportRequest<'_>,
    metadata: &RecordingMetadataDocument,
    id: &str,
    created_directories: &[String],
) -> Result<Vec<u8>, RecordingImportError> {
    let value = TransactionRecord {
        format: "hyper-recording-import-transaction".to_owned(),
        format_version: 2,
        id: id.strip_prefix(TRANSACTION_PREFIX).unwrap_or(id).to_owned(),
        before_project_sha256: hex_digest(before),
        after_project_sha256: hex_digest(after),
        recording: TransactionFile {
            path: recording_path.to_owned(),
            byte_length: request.recording_byte_length,
            sha256: hex_digest(request.recording_sha256),
        },
        metadata: TransactionFile {
            path: metadata_path.to_owned(),
            byte_length: request.metadata_bytes.len() as u64,
            sha256: hex_digest(Sha256::digest(request.metadata_bytes).into()),
        },
        created_directories: created_directories.to_vec(),
        extensions: serde_json::Map::new(),
    };
    if value.recording.path != format!("assets/recordings/{}.wav", metadata.asset_id())
        || value.metadata.path != format!("assets/recordings/{}.json", metadata.id())
    {
        return Err(failure(
            "recording import transaction paths are inconsistent",
        ));
    }
    serde_json_canonicalizer::to_vec(&value)
        .map_err(|error| caused("serialize recording import transaction", error))
}

fn write_transaction_intent(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), RecordingImportError> {
    let id = transaction_id(directory_name)?;
    let intent = TransactionIntent {
        format: "hyper-recording-import-intent".to_owned(),
        format_version: 1,
        id,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent)
        .map_err(|error| caused("serialize recording import intent", error))?;
    write_exclusive_synced(transaction, TRANSACTION_INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(
            TRANSACTION_INTENT_STAGING_NAME,
            transaction,
            TRANSACTION_INTENT_NAME,
        )
        .map_err(|error| caused("publish recording import intent", error))?;
    sync_dir(transaction)
}

fn transaction_id(directory_name: &str) -> Result<String, RecordingImportError> {
    let id = directory_name
        .strip_prefix(TRANSACTION_PREFIX)
        .ok_or_else(|| failure("recording import transaction name is invalid"))?;
    Uuid::parse_str(id)
        .map_err(|error| caused("recording import transaction id is invalid", error))?;
    Ok(id.to_owned())
}

fn validate_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<String, RecordingImportError> {
    let id = transaction_id(directory_name)?;
    let mut names = Vec::new();
    for entry in transaction
        .entries()
        .map_err(|error| caused("read incomplete recording import transaction", error))?
    {
        let entry = entry.map_err(|error| caused("read incomplete transaction entry", error))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| failure("incomplete transaction entry name is not Unicode"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| caused("inspect incomplete transaction entry", error))?;
        if !file_type.is_file()
            || !matches!(
                name.as_str(),
                TRANSACTION_INTENT_STAGING_NAME
                    | TRANSACTION_INTENT_NAME
                    | "recording.wav"
                    | "metadata.json"
                    | "project.json"
                    | "previous-project.json"
            )
        {
            return Err(failure(
                "incomplete recording import transaction has unknown content",
            ));
        }
        names.push(name);
    }
    names.sort();
    if names.is_empty() || names == [TRANSACTION_INTENT_STAGING_NAME] {
        return Ok(id);
    }
    if names
        .iter()
        .any(|name| name == TRANSACTION_INTENT_STAGING_NAME)
        || !names.iter().any(|name| name == TRANSACTION_INTENT_NAME)
    {
        return Err(failure(
            "incomplete recording import transaction has no durable intent",
        ));
    }
    let bytes = read_bounded(transaction, TRANSACTION_INTENT_NAME, MAX_MANIFEST_BYTES)?;
    require_canonical_json(&bytes, "recording import intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&bytes)
        .map_err(|error| caused("parse recording import intent", error))?;
    if intent.format != "hyper-recording-import-intent"
        || intent.format_version != 1
        || intent.id != id
        || !intent.extensions.is_empty()
    {
        return Err(failure("recording import intent is invalid"));
    }
    Ok(id)
}

fn validate_staged_recording(
    transaction: &Dir,
    path: &str,
    metadata: &RecordingMetadataDocument,
) -> Result<(), RecordingImportError> {
    let file = transaction
        .open(path)
        .map_err(|error| caused("open staged recording for media validation", error))?
        .into_std();
    let mut reader =
        hound::WavReader::new(file).map_err(|error| caused("parse staged recording WAV", error))?;
    let spec = reader.spec();
    if spec.sample_rate != metadata.sample_rate_hz()
        || spec.channels != u16::from(metadata.channels())
        || u64::from(reader.duration()) != metadata.frame_count()
    {
        return Err(failure(
            "recording WAV facts do not match recording metadata",
        ));
    }
    match spec.sample_format {
        hound::SampleFormat::Float => {
            if spec.bits_per_sample != 32 {
                return Err(failure("recording WAV float width is unsupported"));
            }
            for sample in reader.samples::<f32>() {
                let sample =
                    sample.map_err(|error| caused("decode staged recording WAV", error))?;
                if !sample.is_finite() {
                    return Err(failure("recording WAV contains a non-finite sample"));
                }
            }
        }
        hound::SampleFormat::Int => {
            for sample in reader.samples::<i32>() {
                sample.map_err(|error| caused("decode staged recording WAV", error))?;
            }
        }
    }
    Ok(())
}

fn copy_verified_recording(
    request: RecordingImportRequest<'_>,
    destination_dir: &Dir,
    destination: &str,
) -> Result<(), RecordingImportError> {
    let path_metadata = fs::symlink_metadata(request.recording_path)
        .map_err(|error| caused("inspect recording source", error))?;
    if !path_metadata.is_file() || super::stable_file::is_link_like(&path_metadata) {
        return Err(failure("recording source must be a real regular file"));
    }
    let source = File::open(request.recording_path)
        .map_err(|error| caused("open recording source", error))?;
    let source_handle = Handle::from_file(
        source
            .try_clone()
            .map_err(|error| caused("retain recording source", error))?,
    )
    .map_err(|error| caused("identify recording source", error))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut destination_file = destination_dir
        .open_with(destination, &options)
        .map_err(|error| caused("create staged recording", error))?;
    copy_open_stable(
        source,
        "recording source",
        request.recording_byte_length,
        request.recording_sha256,
        &mut destination_file,
    )
    .map_err(|error| caused("copy verified recording source", error))?;
    destination_file
        .sync_all()
        .map_err(|error| caused("sync staged recording", error))?;
    let current = Handle::from_path(request.recording_path)
        .map_err(|error| caused("reopen recording source", error))?;
    if current != source_handle {
        return Err(failure("recording source path changed during import"));
    }
    Ok(())
}

fn require_canonical_json(bytes: &[u8], label: &str) -> Result<(), RecordingImportError> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|error| caused(format!("parse {label}"), error))?;
    let canonical = serde_json_canonicalizer::to_vec(&value)
        .map_err(|error| caused(format!("canonicalize {label}"), error))?;
    if canonical != bytes {
        return Err(failure(format!("{label} is not canonical")));
    }
    Ok(())
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir, RecordingImportError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(caused(format!("create {name} directory"), error)),
    }
    parent
        .open_dir(name)
        .map_err(|error| caused(format!("open {name} directory"), error))
}

fn open_optional_dir(parent: &Dir, name: &str) -> Result<Option<Dir>, RecordingImportError> {
    match parent.open_dir(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(caused(format!("open {name} directory"), error)),
    }
}

fn write_exclusive_synced(dir: &Dir, path: &str, bytes: &[u8]) -> Result<(), RecordingImportError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = dir
        .open_with(path, &options)
        .map_err(|error| caused(format!("create {path}"), error))?;
    file.write_all(bytes)
        .map_err(|error| caused(format!("write {path}"), error))?;
    file.sync_all()
        .map_err(|error| caused(format!("sync {path}"), error))
}

fn read_bounded(dir: &Dir, path: &str, maximum: usize) -> Result<Vec<u8>, RecordingImportError> {
    read_bounded_observed(dir, path, maximum, || Ok(()))
}

fn read_bounded_observed(
    dir: &Dir,
    path: &str,
    maximum: usize,
    after_metadata: impl FnOnce() -> Result<(), RecordingImportError>,
) -> Result<Vec<u8>, RecordingImportError> {
    let file = dir
        .open(path)
        .map_err(|error| caused(format!("open {path}"), error))?;
    let length = file
        .metadata()
        .map_err(|error| caused(format!("inspect {path}"), error))?
        .len();
    if length > maximum as u64 {
        return Err(failure(format!("{path} exceeds its size limit")));
    }
    after_metadata()?;
    let mut bytes = Vec::new();
    let capacity = usize::try_from(length)
        .map_err(|_| failure(format!("{path} exceeds addressable memory")))?;
    bytes
        .try_reserve_exact(capacity)
        .map_err(|error| caused(format!("allocate {path}"), error))?;
    let limit = u64::try_from(maximum)
        .map_err(|_| failure(format!("{path} size limit is not addressable")))?
        .checked_add(1)
        .ok_or_else(|| failure(format!("{path} size limit overflows")))?;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| caused(format!("read {path}"), error))?;
    if bytes.len() > maximum {
        return Err(failure(format!("{path} exceeds its size limit")));
    }
    Ok(bytes)
}

fn sync_dir(dir: &Dir) -> Result<(), RecordingImportError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    dir.open_with(".", &options)
        .map_err(|error| caused("open directory for synchronization", error))?
        .sync_all()
        .map_err(|error| caused("sync directory", error))
}

fn clear_transaction(transaction: &Dir) -> Result<(), RecordingImportError> {
    for file in [
        TRANSACTION_INTENT_STAGING_NAME,
        TRANSACTION_INTENT_NAME,
        "recording.wav",
        "metadata.json",
        "project.json",
        "previous-project.json",
        "transaction.json",
    ] {
        match transaction.remove_file(file) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(format!("remove transaction file {file}"), error));
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
) -> Result<(), RecordingImportError> {
    let quarantine = format!(".recording-import-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| caused("quarantine recording import transaction", error))?;
    let moved = backups
        .open_dir(&quarantine)
        .map_err(|error| caused("open quarantined recording import transaction", error))?;
    if !same_directory(&transaction, &moved)
        .map_err(|error| caused("compare quarantined recording import transaction", error))?
    {
        return Err(failure(
            "recording import transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups
        .remove_dir(&quarantine)
        .map_err(|error| caused("remove recording import transaction", error))?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), RecordingImportError> {
    clear_transaction(&transaction)?;
    drop(transaction);
    backups
        .remove_dir(name)
        .map_err(|error| caused("remove recording import transaction", error))?;
    sync_dir(backups)
}

fn read_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<TransactionRecord, RecordingImportError> {
    let bytes = read_bounded(transaction, "transaction.json", MAX_MANIFEST_BYTES)?;
    require_canonical_json(&bytes, "recording import transaction")?;
    let tx: TransactionRecord = serde_json::from_slice(&bytes)
        .map_err(|error| caused("parse recording import transaction", error))?;
    if tx.format != "hyper-recording-import-transaction"
        || tx.format_version != 2
        || directory_name != format!("{TRANSACTION_PREFIX}{}", tx.id)
        || Uuid::parse_str(&tx.id).is_err()
        || !tx.extensions.is_empty()
        || !valid_transaction_file(&tx.recording, "wav")
        || !valid_transaction_file(&tx.metadata, "json")
        || !valid_created_directories(&tx.created_directories)
    {
        return Err(failure("recording import transaction is invalid"));
    }
    decode_digest(&tx.before_project_sha256)?;
    decode_digest(&tx.after_project_sha256)?;
    Ok(tx)
}

fn valid_transaction_file(file: &TransactionFile, extension: &str) -> bool {
    file.byte_length > 0
        && file.byte_length <= MAX_FINALIZED_RECORDING_BYTE_LENGTH
        && file.path.starts_with("assets/recordings/")
        && file.path.ends_with(&format!(".{extension}"))
        && decode_digest(&file.sha256).is_ok()
}

fn valid_created_directories(directories: &[String]) -> bool {
    directories.is_empty()
        || matches!(directories, [one] if one == "assets/recordings")
        || directories == ["assets", "assets/recordings"]
}

fn recover_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
    recordings_authority: Option<&Dir>,
) -> Result<RecordingImportRecoveryDisposition, RecordingImportError> {
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = match read_bounded(root, MANIFEST_NAME, MAX_MANIFEST_BYTES) {
        Ok(bytes) => <[u8; 32]>::from(Sha256::digest(bytes)),
        Err(error)
            if error.source().is_some_and(|source| {
                source
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
            }) =>
        {
            let previous = read_bounded(transaction, "previous-project.json", MAX_MANIFEST_BYTES)?;
            require_canonical_json(&previous, "previous project manifest")?;
            hyper_project_model::ProjectManifest::parse(&previous)
                .map_err(|error| caused("previous project manifest is invalid", error))?;
            if <[u8; 32]>::from(Sha256::digest(&previous)) != before {
                return Err(failure(
                    "previous project manifest does not match its recorded identity",
                ));
            }
            restore_previous_manifest(root, backups, transaction, &tx.id)?;
            before
        }
        Err(error) => return Err(error),
    };
    if active == after {
        let committed = verify_project_package(root_path)
            .map_err(|error| caused("committed recording import does not verify", error))?;
        if !same_directory(root, &committed.root_dir)
            .map_err(|error| caused("compare recovered project root", error))?
            || committed.manifest_sha256 != after
        {
            return Err(failure("committed recording import identity changed"));
        }
        return Ok(RecordingImportRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            "recording import recovery found an unrelated project identity",
        ));
    }

    let opened_recordings;
    let recordings = match recordings_authority {
        Some(directory) => Some(directory),
        None => match root.open_dir("assets") {
            Ok(assets) => match assets.open_dir("recordings") {
                Ok(directory) => {
                    opened_recordings = directory;
                    Some(&opened_recordings)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(caused("open recordings directory for recovery", error)),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(caused("open assets directory for recovery", error)),
        },
    };
    if let Some(recordings) = recordings {
        remove_matching_file(recordings, &tx.metadata)?;
        remove_matching_file(recordings, &tx.recording)?;
    }
    remove_created_directories(root, &tx.created_directories)?;
    let restored_bytes = read_bounded(root, MANIFEST_NAME, MAX_MANIFEST_BYTES)?;
    if <[u8; 32]>::from(Sha256::digest(&restored_bytes)) != before {
        return Err(failure("rolled-back project identity changed"));
    }
    super::verify_retained_project_candidate(root, &restored_bytes)
        .map_err(|error| caused("rolled-back recording import does not verify", error))?;
    Ok(RecordingImportRecoveryDisposition::RolledBack)
}

fn restore_previous_manifest(
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    id: &str,
) -> Result<(), RecordingImportError> {
    let temporary = format!(".project-{id}.json");
    transaction
        .hard_link("previous-project.json", backups, &temporary)
        .map_err(|error| caused("stage previous project manifest", error))?;
    sync_dir(backups)?;
    backups
        .rename(&temporary, root, MANIFEST_NAME)
        .map_err(|error| caused("restore previous project manifest", error))?;
    sync_dir(root)
}

fn verified_matching_file(
    dir: &Dir,
    record: &TransactionFile,
) -> Result<Option<(cap_std::fs::File, std::ffi::OsString)>, RecordingImportError> {
    let name = Path::new(&record.path)
        .file_name()
        .ok_or_else(|| failure("transaction file path has no name"))?;
    let mut file = match dir.open(name) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(caused("open published recording import file", error)),
    };
    let mut hasher = Sha256::new();
    let length = io::copy(&mut file, &mut hasher_writer(&mut hasher))
        .map_err(|error| caused("verify published recording import file", error))?;
    if length != record.byte_length
        || <[u8; 32]>::from(hasher.finalize()) != decode_digest(&record.sha256)?
    {
        return Err(failure("recording import rollback refuses changed content"));
    }
    Ok(Some((file, name.to_owned())))
}

fn remove_matching_file(dir: &Dir, record: &TransactionFile) -> Result<(), RecordingImportError> {
    let Some((file, name)) = verified_matching_file(dir, record)? else {
        return Ok(());
    };
    let verified = Handle::from_file(
        file.try_clone()
            .map_err(|error| caused("retain published recording import file", error))?
            .into_std(),
    )
    .map_err(|error| caused("identify published recording import file", error))?;
    let quarantine = format!(".recording-import-rollback-{}", Uuid::new_v4());
    dir.rename(name, dir, &quarantine)
        .map_err(|error| caused("quarantine published recording import file", error))?;
    let moved = dir
        .open(&quarantine)
        .map_err(|error| caused("open quarantined recording import file", error))?;
    let moved_identity = Handle::from_file(
        moved
            .try_clone()
            .map_err(|error| caused("retain quarantined recording import file", error))?
            .into_std(),
    )
    .map_err(|error| caused("identify quarantined recording import file", error))?;
    if moved_identity != verified {
        return Err(failure(
            "published recording import file changed before rollback",
        ));
    }
    drop(moved);
    drop(file);
    dir.remove_file(&quarantine)
        .map_err(|error| caused("remove quarantined recording import file", error))?;
    sync_dir(dir)
}

fn hasher_writer(hasher: &mut Sha256) -> impl Write + '_ {
    struct DigestWriter<'a>(&'a mut Sha256);
    impl Write for DigestWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    DigestWriter(hasher)
}

fn remove_created_directories(root: &Dir, created: &[String]) -> Result<(), RecordingImportError> {
    if created.iter().any(|path| path == "assets/recordings")
        && let Ok(assets) = root.open_dir("assets")
    {
        match assets.remove_dir("recordings") {
            Ok(()) => sync_dir(&assets)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(caused("remove created recordings directory", error)),
        }
    }
    if created.iter().any(|path| path == "assets") {
        match root.remove_dir("assets") {
            Ok(()) => sync_dir(root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(caused("remove created assets directory", error)),
        }
    }
    Ok(())
}

fn decode_digest(value: &str) -> Result<[u8; 32], RecordingImportError> {
    if value.len() != 64 {
        return Err(failure("SHA-256 digest is invalid"));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|_| failure("SHA-256 digest is invalid"))?;
        output[index] =
            u8::from_str_radix(text, 16).map_err(|_| failure("SHA-256 digest is invalid"))?;
    }
    Ok(output)
}

fn hex_digest(digest: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const METADATA_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334477";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334478";

    fn pcm16_stereo_wav() -> Vec<u8> {
        let samples = [0_i16, 0_i16];
        let data_bytes = u32::try_from(samples.len() * size_of::<i16>()).unwrap();
        let channels = 2_u16;
        let block_align = channels * 2;
        let mut bytes = Vec::with_capacity(44 + usize::try_from(data_bytes).unwrap());
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&(48_000 * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    struct Fixture {
        root: std::path::PathBuf,
        recording: std::path::PathBuf,
        recording_bytes: Vec<u8>,
        metadata_bytes: Vec<u8>,
    }

    impl Fixture {
        fn new(with_recordings: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-recording-import-native-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("timeline")).unwrap();
            fs::create_dir_all(root.join("assets/samples")).unwrap();
            if with_recordings {
                fs::create_dir_all(root.join("assets/recordings")).unwrap();
            }
            let timeline = br#"{"format":"fixture-timeline"}"#;
            let sample = b"fixture-sample";
            fs::write(root.join("timeline/timeline.json"), timeline).unwrap();
            fs::write(root.join("assets/samples/sample.wav"), sample).unwrap();
            let mut assets = vec![json!({
                "id": "018f47a2-6b2d-7c11-8a44-001122334457", "role": "sample",
                "path": "assets/samples/sample.wav", "media_type": "audio/wav",
                "byte_length": sample.len(), "sha256": hex_digest(Sha256::digest(sample).into()),
                "extensions": {}
            })];
            if with_recordings {
                let existing = b"existing-recording";
                fs::write(root.join("assets/recordings/existing.wav"), existing).unwrap();
                assets.push(json!({
                    "id": "018f47a2-6b2d-7c11-8a44-001122334479", "role": "recording",
                    "path": "assets/recordings/existing.wav", "media_type": "audio/wav",
                    "byte_length": existing.len(), "sha256": hex_digest(Sha256::digest(existing).into()),
                    "extensions": {}
                }));
            }
            let manifest = json!({
                "format": "hyper-project", "format_version": 1,
                "project_id": PROJECT_ID, "revision": 1, "name": "Recording fixture",
                "sample_rate_hz": 48000,
                "required_capabilities": {
                    "format": "hyper-hardware-capabilities", "format_version": 1,
                    "audio_inputs": 2, "audio_outputs": 2, "cv_inputs": 0,
                    "cv_outputs": 0, "gate_outputs": 0, "has_clock_input": false,
                    "has_reset_input": false, "has_clock_output": false,
                    "usb_mode": "none", "has_ethernet": true, "extensions": {}
                },
                "resources": [{
                    "id": "018f47a2-6b2d-7c11-8a44-001122334456", "role": "timeline",
                    "path": "timeline/timeline.json", "media_type": "application/json",
                    "byte_length": timeline.len(), "sha256": hex_digest(Sha256::digest(timeline).into()),
                    "extensions": {}
                }],
                "assets": assets,
                "extensions": {}
            });
            fs::write(
                root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let recording = root.with_extension("source.wav");
            let recording_bytes = pcm16_stereo_wav();
            fs::write(&recording, &recording_bytes).unwrap();
            let metadata = json!({
                "format": "hyper-recording-metadata", "format_version": 1,
                "id": METADATA_ID, "project_id": PROJECT_ID, "asset_id": ASSET_ID,
                "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
                "capture_kind": "new", "captured_at_utc": "2026-07-27T12:00:00.000Z",
                "timeline_start_tick": 0, "source_input_indices": [0, 1],
                "sample_rate_hz": 48000, "channels": 2, "frame_count": 1,
                "extensions": {}
            });
            let metadata_bytes = serde_json_canonicalizer::to_vec(&metadata).unwrap();
            Self {
                root,
                recording,
                recording_bytes,
                metadata_bytes,
            }
        }

        fn request(&self) -> RecordingImportRequest<'_> {
            RecordingImportRequest {
                recording_path: &self.recording,
                recording_byte_length: self.recording_bytes.len() as u64,
                recording_sha256: Sha256::digest(&self.recording_bytes).into(),
                metadata_bytes: &self.metadata_bytes,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_file(&self.recording);
        }
    }

    #[test]
    fn commits_through_retained_project_directories() {
        let fixture = Fixture::new(false);

        let outcome = import_recording(&fixture.root, fixture.request()).unwrap();

        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(
            fs::read(fixture.root.join(outcome.recording_path())).unwrap(),
            fixture.recording_bytes
        );
        assert_eq!(
            fs::read(fixture.root.join(outcome.metadata_path())).unwrap(),
            fixture.metadata_bytes
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
        assert!(recover_recording_imports(&fixture.root).unwrap().is_empty());
    }

    #[test]
    fn committed_import_retains_its_publication_truth_when_cleanup_fails() {
        let fixture = Fixture::new(false);
        let error = import_recording_observed(&fixture.root, fixture.request(), |root| {
            let transaction = fs::read_dir(root.join("backups"))
                .map_err(|error| caused("find recording cleanup fixture transaction", error))?
                .filter_map(Result::ok)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TRANSACTION_PREFIX)
                })
                .ok_or_else(|| failure("recording cleanup fixture lost its transaction"))?;
            fs::write(
                transaction.path().join("cleanup-residue"),
                b"fixture residue",
            )
            .map_err(|error| caused("write recording cleanup fixture residue", error))
        })
        .unwrap_err();

        assert!(error.published(), "the manifest-last import was verified");
        assert!(
            error
                .to_string()
                .contains("transaction cleanup is incomplete")
        );
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(
            package
                .read_asset(
                    ASSET_ID,
                    u64::try_from(fixture.recording_bytes.len()).unwrap() + 1,
                )
                .unwrap(),
            fixture.recording_bytes
        );
        assert!(
            package
                .manifest()
                .resources()
                .iter()
                .any(|resource| resource.id() == METADATA_ID)
        );
    }

    #[test]
    fn recording_import_refuses_unrecovered_project_transactions_before_staging() {
        for prefix in [
            crate::project_lock::AUDIO_TRACK_TRANSACTION_PREFIX,
            TRANSACTION_PREFIX,
        ] {
            let fixture = Fixture::new(false);
            let foreign = fixture
                .root
                .join("backups")
                .join(format!("{prefix}fixture"));
            fs::create_dir_all(&foreign).unwrap();

            let error = import_recording(&fixture.root, fixture.request()).unwrap_err();

            assert_eq!(
                error.to_string(),
                "another project transaction requires recovery; reopen the current project revision"
            );
            assert!(foreign.exists());
            assert!(!fixture.root.join("assets/recordings").exists());
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
    fn imports_the_canonical_finalized_recording_handoff() {
        let fixture = Fixture::new(false);
        let metadata: Value = serde_json::from_slice(&fixture.metadata_bytes).unwrap();
        let handoff = json!({
            "format": "hyper-finalized-recording-handoff", "format_version": 1,
            "recording": {
                "path": fixture.recording.to_str().unwrap(), "media_type": "audio/wav",
                "byte_length": fixture.recording_bytes.len(),
                "sha256": hex_digest(Sha256::digest(&fixture.recording_bytes).into())
            },
            "metadata": metadata, "extensions": {}
        });
        let bytes = serde_json_canonicalizer::to_vec(&handoff).unwrap();

        let outcome = import_finalized_recording_handoff(&fixture.root, &bytes).unwrap();

        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(
            fs::read(fixture.root.join(outcome.recording_path())).unwrap(),
            fixture.recording_bytes
        );
    }

    #[test]
    fn rejects_digest_matching_non_wav_before_publication() {
        let mut fixture = Fixture::new(false);
        fixture.recording_bytes = b"RIFFnative-recording-import".to_vec();
        fs::write(&fixture.recording, &fixture.recording_bytes).unwrap();

        let result = import_recording(&fixture.root, fixture.request());

        assert!(result.is_err());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
        assert!(recover_recording_imports(&fixture.root).unwrap().is_empty());
    }

    #[test]
    fn recovers_prepublication_transaction_residue() {
        let fixture = Fixture::new(false);
        let backups = fixture.root.join("backups");
        fs::create_dir_all(&backups).unwrap();
        let id = Uuid::new_v4();
        let transaction_name = format!("{TRANSACTION_PREFIX}{id}");
        let transaction_path = backups.join(&transaction_name);
        fs::create_dir(&transaction_path).unwrap();
        let intent = TransactionIntent {
            format: "hyper-recording-import-intent".to_owned(),
            format_version: 1,
            id: id.to_string(),
            extensions: serde_json::Map::new(),
        };
        fs::write(
            transaction_path.join(TRANSACTION_INTENT_NAME),
            serde_json_canonicalizer::to_vec(&intent).unwrap(),
        )
        .unwrap();
        fs::write(transaction_path.join("recording.wav"), b"partial").unwrap();

        let report = crate::recover_project_transactions(&fixture.root).unwrap();
        let recovered = report.recordings();

        assert_eq!(recovered.len(), 1);
        assert!(report.control_mappings().is_empty());
        assert_eq!(recovered[0].transaction_id(), id.to_string());
        assert_eq!(
            recovered[0].disposition(),
            RecordingImportRecoveryDisposition::RolledBack
        );
        assert!(!transaction_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_rejects_growth_after_metadata_observation() {
        let root = std::env::temp_dir().join(format!(
            "hyper-recording-import-bounded-read-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("transaction.json"), b"small").unwrap();
        let dir = Dir::open_ambient_dir(&root, cap_std::ambient_authority()).unwrap();

        let result = read_bounded_observed(&dir, "transaction.json", 8, || {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(root.join("transaction.json"))
                .map_err(|error| caused("open bounded read fixture for growth", error))?;
            file.write_all(b" and now too large")
                .map_err(|error| caused("grow bounded read fixture", error))
        });

        assert!(result.is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pretransaction_failure_leaves_no_import_residue() {
        let fixture = Fixture::new(false);
        let mut request = fixture.request();
        request.recording_sha256 = [0_u8; 32];

        let result = import_recording(&fixture.root, request);

        assert!(result.is_err());
        let residue = fs::read_dir(fixture.root.join("backups"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().starts_with(TRANSACTION_PREFIX))
            .collect::<Vec<_>>();
        assert!(residue.is_empty());
        assert!(!fixture.root.join("assets/recordings").exists());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn recovers_a_crash_after_durable_transaction_staging() {
        let fixture = Fixture::new(false);
        let result = std::panic::catch_unwind(|| {
            let _ = import_recording_observed(&fixture.root, fixture.request(), |_| {
                panic!("simulated process crash")
            });
        });
        assert!(result.is_err());
        fs::remove_file(fixture.root.join(MANIFEST_NAME)).unwrap();

        let recovered = recover_recording_imports(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            RecordingImportRecoveryDisposition::RolledBack
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn refuses_corrupt_previous_manifest_during_recovery() {
        let fixture = Fixture::new(false);
        let result = std::panic::catch_unwind(|| {
            let _ = import_recording_observed(&fixture.root, fixture.request(), |_| {
                panic!("simulated process crash")
            });
        });
        assert!(result.is_err());
        let backups = fixture.root.join("backups");
        let transaction = fs::read_dir(&backups)
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(TRANSACTION_PREFIX)
            })
            .unwrap()
            .path();
        fs::write(transaction.join("previous-project.json"), b"{}").unwrap();
        fs::remove_file(fixture.root.join(MANIFEST_NAME)).unwrap();

        let recovery = recover_recording_imports(&fixture.root);

        assert!(recovery.is_err());
        assert!(!fixture.root.join(MANIFEST_NAME).exists());
        assert!(transaction.exists());
    }

    #[test]
    fn completes_recovery_when_publication_was_already_committed() {
        let fixture = Fixture::new(false);
        let result = std::panic::catch_unwind(|| {
            let _ = import_recording_observed(&fixture.root, fixture.request(), |_| {
                panic!("simulated process crash")
            });
        });
        assert!(result.is_err());
        let backups = fixture.root.join("backups");
        let transaction_name = fs::read_dir(&backups)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .find(|name| name.to_string_lossy().starts_with(TRANSACTION_PREFIX))
            .unwrap();
        let transaction = backups.join(&transaction_name);
        let recordings = fixture.root.join("assets/recordings");
        fs::create_dir_all(&recordings).unwrap();
        fs::hard_link(
            transaction.join("recording.wav"),
            recordings.join(format!("{ASSET_ID}.wav")),
        )
        .unwrap();
        fs::hard_link(
            transaction.join("metadata.json"),
            recordings.join(format!("{METADATA_ID}.json")),
        )
        .unwrap();
        let temporary = backups.join("candidate-project.json");
        fs::hard_link(transaction.join("project.json"), &temporary).unwrap();
        fs::rename(temporary, fixture.root.join(MANIFEST_NAME)).unwrap();

        let recovered = recover_recording_imports(&fixture.root).unwrap();

        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            RecordingImportRecoveryDisposition::Committed
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );
        assert!(!transaction.exists());
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_replacement_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new(true);
        let recordings = fixture.root.join("assets/recordings");
        let displaced = fixture.root.join("assets/displaced-recordings");
        let external = fixture.root.with_extension("external");
        fs::create_dir(&external).unwrap();

        let result = import_recording_observed(&fixture.root, fixture.request(), |_| {
            fs::rename(&recordings, &displaced)
                .map_err(|error| caused("replace recordings ancestor", error))?;
            symlink(&external, &recordings)
                .map_err(|error| caused("install recordings symlink", error))?;
            Ok(())
        });

        let error = result.unwrap_err();
        assert_eq!(
            error.to_string(),
            "recording import and recovery both failed",
            "{error:?}"
        );
        assert!(fs::read_dir(&external).unwrap().next().is_none());
        assert!(!displaced.join(format!("{ASSET_ID}.wav")).exists());
        assert!(!displaced.join(format!("{METADATA_ID}.json")).exists());
        fs::remove_file(&recordings).unwrap();
        fs::rename(&displaced, &recordings).unwrap();
        fs::remove_dir(&external).unwrap();
        let recovered = recover_recording_imports(&fixture.root).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(
            recovered[0].disposition(),
            RecordingImportRecoveryDisposition::RolledBack
        );
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn retained_windows_directory_blocks_ancestor_replacement() {
        let fixture = Fixture::new(true);
        let recordings = fixture.root.join("assets/recordings");
        let displaced = fixture.root.join("assets/displaced-recordings");
        let replacement_blocked = std::cell::Cell::new(false);

        let outcome = import_recording_observed(&fixture.root, fixture.request(), |_| {
            replacement_blocked.set(fs::rename(&recordings, &displaced).is_err());
            Ok(())
        })
        .unwrap();

        assert!(replacement_blocked.get());
        assert_eq!(outcome.project_revision(), 2);
        assert!(!displaced.exists());
    }
}
