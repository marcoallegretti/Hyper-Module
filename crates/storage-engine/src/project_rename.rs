use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{ProjectManifest, is_valid_interactive_project_name, is_valid_project_name};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{
    PROJECT_RENAME_TRANSACTION_PREFIX as TRANSACTION_PREFIX, ProjectLock,
    ProjectTransactionFamily, find_unhandled_project_transaction,
};
use super::stable_file::read_open_stable;
use super::{
    MANIFEST_NAME, MAX_JSON_BYTES, StructurallyVerifiedProject, open_stable_project_root,
    same_directory, verify_project_candidate, verify_project_package,
    verify_retained_project_candidate,
};

const INTENT_NAME: &str = "intent.json";
const INTENT_STAGING_NAME: &str = ".intent.json.tmp";
const PROJECT_STAGING_NAME: &str = "project.json";
const PREVIOUS_PROJECT_NAME: &str = "previous-project.json";
const TRANSACTION_NAME: &str = "transaction.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub struct ProjectRenameRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub project_name: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectRenameErrorKind {
    InvalidName,
    ProjectChanged,
    TransactionUnavailable,
    Integrity,
    Io,
    Recovery,
}

#[derive(Debug)]
pub struct ProjectRenameError {
    kind: ProjectRenameErrorKind,
    message: String,
    published: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectRenameError {
    #[must_use]
    pub const fn kind(&self) -> ProjectRenameErrorKind {
        self.kind
    }

    /// Returns true only when recovery proved the renamed manifest is durable.
    #[must_use]
    pub const fn published(&self) -> bool {
        self.published
    }
}

impl fmt::Display for ProjectRenameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProjectRenameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRenameOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    project_name: String,
    changed: bool,
}

impl ProjectRenameOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn project_name(&self) -> &str {
        &self.project_name
    }

    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectRenameRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRenameRecovery {
    transaction_id: String,
    disposition: ProjectRenameRecoveryDisposition,
}

impl ProjectRenameRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> ProjectRenameRecoveryDisposition {
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
struct TransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    previous_name: String,
    project_name: String,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    Manifest,
}

/// Renames the manifest display name of a verified project without moving its
/// retained package directory.
///
/// # Errors
///
/// Returns a classified request, identity, I/O, integrity, or recovery error.
/// [`ProjectRenameError::published`] distinguishes a committed rename whose
/// transaction cleanup could not be completed.
pub fn rename_project(
    project_path: impl AsRef<Path>,
    request: ProjectRenameRequest<'_>,
) -> Result<ProjectRenameOutcome, ProjectRenameError> {
    rename_project_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable manifest-name transaction in a project.
///
/// # Errors
///
/// Returns an error rather than changing an active manifest that is unrelated
/// to either recorded transaction side.
pub fn recover_project_renames(
    project_path: impl AsRef<Path>,
) -> Result<Vec<ProjectRenameRecovery>, ProjectRenameError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Io,
            "open project root for project-name recovery",
            error,
        )
    })?;
    let Some(backups) = open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;
    recover_transactions_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "the retained-root, recovery, staging, and publication order remains visible in one coordinator"
)]
fn rename_project_observed(
    project_path: &Path,
    request: ProjectRenameRequest<'_>,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectRenameError>,
) -> Result<ProjectRenameOutcome, ProjectRenameError> {
    validate_name(request.project_name)?;
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "project verification failed before project rename",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectRenameErrorKind::ProjectChanged,
            "project changed after the project-name preview",
        ));
    }
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;
    let unhandled = find_unhandled_project_transaction(&backups, &[ProjectTransactionFamily::ProjectRename])
        .map_err(|error| {
            caused(
                ProjectRenameErrorKind::TransactionUnavailable,
                "inspect project transactions before project rename",
                error,
            )
        })?;
    if unhandled.is_some() {
        return Err(failure(
            ProjectRenameErrorKind::ProjectChanged,
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    let recovered = recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() {
        return Err(failure(
            ProjectRenameErrorKind::ProjectChanged,
            "a prior project-name transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectRenameErrorKind::ProjectChanged,
            "project changed before project rename",
            error,
        )
    })?;
    let active = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active) != request.expected_project_sha256 {
        return Err(failure(
            ProjectRenameErrorKind::ProjectChanged,
            "active project identity changed before project-name staging",
        ));
    }
    if verified.manifest().name() == request.project_name {
        return Ok(ProjectRenameOutcome {
            project_identity: request.expected_project_sha256,
            project_revision: verified.manifest().revision(),
            project_name: request.project_name.to_owned(),
            changed: false,
        });
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectRenameErrorKind::ProjectChanged,
                "project revision cannot be incremented",
            )
        })?;
    let candidate = candidate_manifest(&active, request.project_name, next_revision)?;
    let transaction_name = format!("{TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Io,
            "create project-name transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&transaction_name).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Io,
            "open project-name transaction",
            error,
        )
    })?;
    let result = run_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        &active,
        &candidate,
        request.project_name,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_transaction(&backups, transaction, &transaction_name).map_err(
                |error| ProjectRenameError {
                    kind: ProjectRenameErrorKind::Recovery,
                    message: "project name was changed but transaction cleanup is incomplete"
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
                    return Err(ProjectRenameError {
                        kind: ProjectRenameErrorKind::Recovery,
                        message: "project-name publication failed and its durable transaction could not be read"
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
                Ok(ProjectRenameRecoveryDisposition::RolledBack) => {
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)?;
                    Err(operation_error)
                }
                Ok(ProjectRenameRecoveryDisposition::Committed) => {
                    let outcome = outcome_from_transaction(&tx, true)?;
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)
                        .map_err(|error| ProjectRenameError {
                            kind: ProjectRenameErrorKind::Recovery,
                            message: "project name was changed but transaction cleanup is incomplete"
                                .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(ProjectRenameError {
                    kind: ProjectRenameErrorKind::Recovery,
                    message: "project-name publication and recovery both failed".to_owned(),
                    published: false,
                    source: Some(Box::new(recovery_error)),
                }),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    active: &[u8],
    candidate: &[u8],
    project_name: &str,
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectRenameError>,
) -> Result<ProjectRenameOutcome, ProjectRenameError> {
    write_intent(transaction, transaction_name)?;
    let current = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&current) != *verified.manifest_sha256() || current != active {
        return Err(failure(
            ProjectRenameErrorKind::ProjectChanged,
            "active project changed during project-name staging",
        ));
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, active)?;
    let tx = TransactionRecord {
        format: "hyper-project-rename-transaction".to_owned(),
        format_version: 1,
        id: transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        previous_name: verified.manifest().name().to_owned(),
        project_name: project_name.to_owned(),
        extensions: serde_json::Map::new(),
    };
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "serialize project-name transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;
    let candidate_manifest = verify_project_candidate(verified.root(), &verified.root_dir, candidate)
        .map_err(|error| {
            caused(
                ProjectRenameErrorKind::Integrity,
                "project-name candidate does not verify",
                error,
            )
        })?;
    if candidate_manifest.revision() != next_revision || candidate_manifest.name() != project_name {
        return Err(failure(
            ProjectRenameErrorKind::Integrity,
            "project-name candidate differs from the requested manifest",
        ));
    }
    publish_staged_manifest(
        transaction,
        backups,
        &verified.root_dir,
        &format!(".project-rename-candidate-{}.json", tx.id),
        PROJECT_STAGING_NAME,
        "project-name manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(verified.root(), &verified.root_dir, digest(candidate))?;
    outcome_from_transaction(&tx, true)
}

fn candidate_manifest(
    active: &[u8],
    project_name: &str,
    revision: u64,
) -> Result<Vec<u8>, ProjectRenameError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["name"] = project_name.into();
    value["revision"] = revision.into();
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "serialize renamed project manifest",
            error,
        )
    })?;
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "renamed project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn recover_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<ProjectRenameRecovery>, ProjectRenameError> {
    let mut names = Vec::new();
    for entry in backups.entries().map_err(|error| {
        caused(ProjectRenameErrorKind::Io, "read project backups", error)
    })? {
        let entry = entry.map_err(|error| {
            caused(ProjectRenameErrorKind::Io, "read project backup entry", error)
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
                ProjectRenameErrorKind::Io,
                "open project-name recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_prepublication_residue(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove_transaction(backups, transaction, &name)?;
                outcomes.push(ProjectRenameRecovery {
                    transaction_id: id,
                    disposition: ProjectRenameRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition = recover_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_transaction(backups, transaction, &name)?;
        outcomes.push(ProjectRenameRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

fn recover_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<ProjectRenameRecoveryDisposition, ProjectRenameError> {
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
        cleanup_publication_temporaries(backups, transaction, tx)?;
        verify_committed(root_path, root, after)?;
        return Ok(ProjectRenameRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "project-name recovery found an unrelated project identity",
        ));
    }
    cleanup_publication_temporaries(backups, transaction, tx)?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    verify_retained_project_candidate(root, &previous).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "rolled-back project name does not verify",
            error,
        )
    })?;
    Ok(ProjectRenameRecoveryDisposition::RolledBack)
}

fn validate_staged_transaction(
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectRenameError> {
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    require_canonical(&previous, "previous project manifest")?;
    require_canonical(&candidate, "candidate project manifest")?;
    if digest(&previous) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "staged project manifest identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "staged previous project manifest is invalid",
            error,
        )
    })?;
    let renamed_manifest = ProjectManifest::parse(&candidate).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "staged candidate project manifest is invalid",
            error,
        )
    })?;
    if previous_manifest.project_id() != renamed_manifest.project_id()
        || previous_manifest.revision() != tx.before_project_revision
        || renamed_manifest.revision() != tx.after_project_revision
        || previous_manifest.name() != tx.previous_name
        || renamed_manifest.name() != tx.project_name
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
        || candidate_manifest(&previous, &tx.project_name, tx.after_project_revision)? != candidate
    {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "project-name transaction contains unrelated manifest changes",
        ));
    }
    Ok(())
}

fn read_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<TransactionRecord, ProjectRenameError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "project-name transaction intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "parse project-name transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "project-name transaction")?;
    let tx: TransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "parse project-name transaction",
            error,
        )
    })?;
    let expected_id = transaction_id(directory_name)?;
    if intent.format != "hyper-project-rename-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || tx.format != "hyper-project-rename-transaction"
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || tx.id != expected_id
        || !is_valid_project_name(&tx.previous_name)
        || !is_valid_interactive_project_name(&tx.project_name)
        || tx.previous_name == tx.project_name
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "project-name transaction is invalid",
        ));
    }
    Ok(tx)
}

fn validate_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ProjectRenameError> {
    let id = transaction_id(directory_name)?;
    let mut files = BTreeSet::new();
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "read incomplete project-name transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectRenameErrorKind::Recovery,
                "read incomplete project-name transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            INTENT_STAGING_NAME | INTENT_NAME | PROJECT_STAGING_NAME | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectRenameErrorKind::Recovery,
                "incomplete project-name transaction contains an unknown entry",
            ));
        }
        files.insert(name);
    }
    if files.contains(INTENT_NAME) {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "project-name transaction intent")?;
        let intent: TransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectRenameErrorKind::Recovery,
                "parse incomplete project-name transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-project-rename-intent"
            || intent.format_version != 1
            || intent.id != id
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectRenameErrorKind::Recovery,
                "incomplete project-name transaction intent is invalid",
            ));
        }
    }
    Ok(())
}

fn outcome_from_transaction(
    tx: &TransactionRecord,
    changed: bool,
) -> Result<ProjectRenameOutcome, ProjectRenameError> {
    Ok(ProjectRenameOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        project_name: tx.project_name.clone(),
        changed,
    })
}

fn verify_committed(
    root_path: &Path,
    retained_root: &Dir,
    expected: [u8; 32],
) -> Result<(), ProjectRenameError> {
    let committed = verify_project_package(root_path).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "committed renamed project does not verify",
            error,
        )
    })?;
    if !same_directory(retained_root, &committed.root_dir).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "compare committed project root",
            error,
        )
    })? || *committed.manifest_sha256() != expected
    {
        return Err(failure(
            ProjectRenameErrorKind::Integrity,
            "committed renamed project identity changed",
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
) -> Result<(), ProjectRenameError> {
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&previous, "previous project manifest")?;
    ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "previous project manifest is invalid",
            error,
        )
    })?;
    if digest(&previous) != expected {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    publish_staged_manifest(
        transaction,
        backups,
        root,
        &format!(".project-rename-rollback-{id}.json"),
        PREVIOUS_PROJECT_NAME,
        "previous project manifest",
    )
}

fn publish_staged_manifest(
    transaction: &Dir,
    backups: &Dir,
    root: &Dir,
    temporary: &str,
    staged_name: &str,
    label: &str,
) -> Result<(), ProjectRenameError> {
    let bytes = read_bounded(transaction, staged_name, MAX_JSON_BYTES)?;
    write_exclusive_synced(backups, temporary, &bytes)?;
    sync_dir(backups)?;
    backups.rename(temporary, root, MANIFEST_NAME).map_err(|error| {
        caused(ProjectRenameErrorKind::Io, format!("publish {label}"), error)
    })?;
    sync_dir(root)
}

fn cleanup_publication_temporaries(
    backups: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ProjectRenameError> {
    remove_optional_matching_file(
        backups,
        &format!(".project-rename-candidate-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    remove_optional_matching_file(
        backups,
        &format!(".project-rename-rollback-{}.json", tx.id),
        digest(&previous),
    )
}

fn remove_optional_matching_file(
    directory: &Dir,
    name: &str,
    expected: [u8; 32],
) -> Result<(), ProjectRenameError> {
    let Some(bytes) = read_optional_file(directory, name)? else {
        return Ok(());
    };
    if digest(&bytes) != expected {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "project-name recovery refuses an unrelated temporary file",
        ));
    }
    directory.remove_file(name).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "remove project-name publication temporary file",
            error,
        )
    })?;
    sync_dir(directory)
}

fn write_intent(transaction: &Dir, directory_name: &str) -> Result<(), ProjectRenameError> {
    let intent = TransactionIntent {
        format: "hyper-project-rename-intent".to_owned(),
        format_version: 1,
        id: transaction_id(directory_name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Integrity,
            "serialize project-name transaction intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectRenameErrorKind::Io,
                "publish project-name transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn validate_name(name: &str) -> Result<(), ProjectRenameError> {
    if !is_valid_interactive_project_name(name) {
        return Err(failure(
            ProjectRenameErrorKind::InvalidName,
            "project name must contain 1 to 120 characters without outer whitespace or control characters",
        ));
    }
    Ok(())
}

fn read_bounded(directory: &Dir, name: &str, maximum: usize) -> Result<Vec<u8>, ProjectRenameError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|error| {
        caused(ProjectRenameErrorKind::Io, format!("open {name}"), error)
    })?;
    read_open_stable(file.into_std(), name, maximum as u64).map_err(|error| {
        caused(ProjectRenameErrorKind::Integrity, format!("read stable {name}"), error)
    })
}

fn read_optional_file(directory: &Dir, name: &str) -> Result<Option<Vec<u8>>, ProjectRenameError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(caused(
                ProjectRenameErrorKind::Io,
                "inspect project-name temporary file",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(failure(
            ProjectRenameErrorKind::Integrity,
            "project-name temporary file is not a regular non-link file",
        ));
    }
    read_bounded(directory, name, MAX_JSON_BYTES).map(Some)
}

fn write_exclusive_synced(directory: &Dir, name: &str, bytes: &[u8]) -> Result<(), ProjectRenameError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options).map_err(|error| {
        caused(ProjectRenameErrorKind::Io, format!("create {name}"), error)
    })?;
    file.write_all(bytes).map_err(|error| {
        caused(ProjectRenameErrorKind::Io, format!("write {name}"), error)
    })?;
    file.sync_all().map_err(|error| {
        caused(ProjectRenameErrorKind::Io, format!("sync {name}"), error)
    })
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir, ProjectRenameError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(caused(
                ProjectRenameErrorKind::Io,
                format!("create {name} directory"),
                error,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Io,
            format!("open {name} directory"),
            error,
        )
    })
}

fn open_optional_dir(parent: &Dir, name: &str) -> Result<Option<Dir>, ProjectRenameError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(caused(
            ProjectRenameErrorKind::Io,
            format!("open {name} directory"),
            error,
        )),
    }
}

fn sync_dir(directory: &Dir) -> Result<(), ProjectRenameError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|error| caused(ProjectRenameErrorKind::Io, "open directory for synchronization", error))?
        .sync_all()
        .map_err(|error| caused(ProjectRenameErrorKind::Io, "sync directory", error))
}

fn transaction_id(directory_name: &str) -> Result<String, ProjectRenameError> {
    let id = directory_name.strip_prefix(TRANSACTION_PREFIX).ok_or_else(|| {
        failure(
            ProjectRenameErrorKind::Recovery,
            "project-name transaction directory name is invalid",
        )
    })?;
    Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "project-name transaction identifier is invalid",
            error,
        )
    })?;
    Ok(id.to_owned())
}

fn require_canonical(bytes: &[u8], label: &str) -> Result<(), ProjectRenameError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(ProjectRenameErrorKind::Recovery, format!("parse {label}"), error)
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            format!("canonicalize {label}"),
            error,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
}

fn missing_file(directory: &Dir, name: &str) -> Result<bool, ProjectRenameError> {
    match directory.symlink_metadata(name) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(caused(
            ProjectRenameErrorKind::Io,
            format!("inspect {name}"),
            error,
        )),
    }
}

fn error_is_not_found(error: &ProjectRenameError) -> bool {
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

fn clear_transaction(transaction: &Dir) -> Result<(), ProjectRenameError> {
    for name in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        PROJECT_STAGING_NAME,
        PREVIOUS_PROJECT_NAME,
        TRANSACTION_NAME,
    ] {
        match transaction.remove_file(name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ProjectRenameErrorKind::Recovery,
                    format!("remove project-name transaction file {name}"),
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
) -> Result<(), ProjectRenameError> {
    let quarantine = format!(".project-rename-cleanup-{}", Uuid::new_v4());
    backups.rename(name, backups, &quarantine).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "quarantine project-name transaction",
            error,
        )
    })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "open quarantined project-name transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "compare quarantined project-name transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "project-name transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "remove project-name transaction",
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
) -> Result<(), ProjectRenameError> {
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectRenameErrorKind::Recovery,
            "remove project-name transaction",
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

fn decode_digest(value: &str) -> Result<[u8; 32], ProjectRenameError> {
    if value.len() != 64 {
        return Err(failure(
            ProjectRenameErrorKind::Recovery,
            "SHA-256 digest is invalid",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|error| {
            caused(ProjectRenameErrorKind::Recovery, "SHA-256 digest is invalid", error)
        })?;
        output[index] = u8::from_str_radix(text, 16).map_err(|error| {
            caused(ProjectRenameErrorKind::Recovery, "SHA-256 digest is invalid", error)
        })?;
    }
    Ok(output)
}

fn lock_error(error: super::project_lock::ProjectLockError) -> ProjectRenameError {
    let kind = match error {
        super::project_lock::ProjectLockError::Active => ProjectRenameErrorKind::TransactionUnavailable,
        super::project_lock::ProjectLockError::Io { .. } => ProjectRenameErrorKind::Io,
    };
    caused(kind, "acquire project-name transaction lock", error)
}

fn failure(kind: ProjectRenameErrorKind, message: impl Into<String>) -> ProjectRenameError {
    ProjectRenameError {
        kind,
        message: message.into(),
        published: false,
        source: None,
    }
}

fn caused(
    kind: ProjectRenameErrorKind,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> ProjectRenameError {
    ProjectRenameError {
        kind,
        message: message.into(),
        published: false,
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
        original_manifest: Vec<u8>,
        payloads: Vec<(String, Vec<u8>)>,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-project-rename-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let created = crate::create_project(
                &parent,
                crate::ProjectCreationRequest {
                    directory_name: "Stable-directory",
                    project_name: "Original project",
                },
            )
            .unwrap();
            let root = created.project().root().to_path_buf();
            let original_manifest = fs::read(root.join(MANIFEST_NAME)).unwrap();
            let payloads = created
                .project()
                .manifest()
                .resources()
                .iter()
                .map(|resource| {
                    (
                        resource.path().to_owned(),
                        fs::read(root.join(resource.path())).unwrap(),
                    )
                })
                .chain(created.project().manifest().assets().iter().map(|asset| {
                    (
                        asset.path().to_owned(),
                        fs::read(root.join(asset.path())).unwrap(),
                    )
                }))
                .collect();
            Self {
                parent,
                root,
                original_manifest,
                payloads,
            }
        }

        fn identity(&self) -> [u8; 32] {
            *verify_project_package(&self.root)
                .unwrap()
                .manifest_sha256()
        }

        fn request<'a>(&self, name: &'a str) -> ProjectRenameRequest<'a> {
            ProjectRenameRequest {
                expected_project_sha256: self.identity(),
                project_name: name,
            }
        }

        fn transactions(&self) -> usize {
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

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn rename_changes_only_the_canonical_manifest_name_and_revision() {
        let fixture = Fixture::new();
        let original_identity = fixture.identity();

        let outcome = rename_project(&fixture.root, fixture.request("Renamed project")).unwrap();

        assert!(outcome.changed());
        assert_eq!(outcome.project_name(), "Renamed project");
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(fixture.root.file_name().unwrap(), "Stable-directory");
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().name(), "Renamed project");
        assert_eq!(verified.manifest().revision(), 2);
        assert_ne!(verified.manifest_sha256(), &original_identity);
        for (path, bytes) in &fixture.payloads {
            assert_eq!(fs::read(fixture.root.join(path)).unwrap(), *bytes, "{path}");
        }
        assert_eq!(fixture.transactions(), 0);

        let unchanged = rename_project(
            &fixture.root,
            ProjectRenameRequest {
                expected_project_sha256: *verified.manifest_sha256(),
                project_name: "Renamed project",
            },
        )
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 2);
        assert_eq!(fixture.transactions(), 0);
    }

    #[test]
    fn rename_refuses_invalid_or_stale_requests_without_mutating_the_package() {
        let fixture = Fixture::new();

        for name in [" outer whitespace ", "control\u{0007} name"] {
            let invalid = rename_project(&fixture.root, fixture.request(name)).unwrap_err();
            assert_eq!(invalid.kind(), ProjectRenameErrorKind::InvalidName);
            assert_eq!(
                fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
                fixture.original_manifest
            );
        }
        let stale = rename_project(
            &fixture.root,
            ProjectRenameRequest {
                expected_project_sha256: [0; 32],
                project_name: "Different",
            },
        )
        .unwrap_err();
        assert_eq!(stale.kind(), ProjectRenameErrorKind::ProjectChanged);
        assert_eq!(fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(), fixture.original_manifest);
        assert_eq!(fixture.transactions(), 0);
    }

    #[test]
    fn no_op_does_not_bypass_an_incomplete_rename_transaction() {
        let fixture = Fixture::new();
        let result = catch_unwind(AssertUnwindSafe(|| {
            rename_project_observed(&fixture.root, fixture.request("Pending rename"), |phase, _| {
                assert!(
                    phase != PublicationPhase::Transaction,
                    "simulate process loss after durable rename staging"
                );
                Ok(())
            })
        }));
        assert!(result.is_err());
        assert_eq!(fixture.transactions(), 1);

        let recovery_required = rename_project(&fixture.root, fixture.request("Original project"))
            .unwrap_err();
        assert_eq!(recovery_required.kind(), ProjectRenameErrorKind::ProjectChanged);
        assert_eq!(fixture.transactions(), 0);
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            fixture.original_manifest
        );
    }

    #[test]
    fn recovery_rolls_back_before_manifest_publication_and_commits_after_it() {
        let before_manifest = Fixture::new();
        let before_result = catch_unwind(AssertUnwindSafe(|| {
            rename_project_observed(&before_manifest.root, before_manifest.request("Unpublished"), |phase, _| {
                assert!(
                    phase != PublicationPhase::Transaction,
                    "simulate process loss before manifest publication"
                );
                Ok(())
            })
        }));
        assert!(before_result.is_err());
        assert_eq!(before_manifest.transactions(), 1);
        let rolled_back = recover_project_renames(&before_manifest.root).unwrap();
        assert_eq!(rolled_back.len(), 1);
        assert_eq!(
            rolled_back[0].disposition(),
            ProjectRenameRecoveryDisposition::RolledBack
        );
        assert_eq!(
            fs::read(before_manifest.root.join(MANIFEST_NAME)).unwrap(),
            before_manifest.original_manifest
        );
        assert_eq!(before_manifest.transactions(), 0);

        let after_manifest = Fixture::new();
        let after_result = catch_unwind(AssertUnwindSafe(|| {
            rename_project_observed(&after_manifest.root, after_manifest.request("Published"), |phase, _| {
                assert!(
                    phase != PublicationPhase::Manifest,
                    "simulate process loss after manifest publication"
                );
                Ok(())
            })
        }));
        assert!(after_result.is_err());
        assert_eq!(after_manifest.transactions(), 1);
        let report = crate::recover_project_transactions(&after_manifest.root).unwrap();
        assert_eq!(report.renames().len(), 1);
        assert_eq!(
            report.renames()[0].disposition(),
            ProjectRenameRecoveryDisposition::Committed
        );
        let verified = verify_project_package(&after_manifest.root).unwrap();
        assert_eq!(verified.manifest().name(), "Published");
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(after_manifest.transactions(), 0);
    }
}
