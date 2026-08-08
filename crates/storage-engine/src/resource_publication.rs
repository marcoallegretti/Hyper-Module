use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{ProjectManifest, ResourceDescriptor, ResourceRole};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::ProjectLock;
use super::stable_file::read_open_stable;
use super::{
    MANIFEST_NAME, MAX_JSON_BYTES, MAX_JSON_FILE_BYTES, StructurallyVerifiedProject,
    open_stable_project_root, same_directory, verify_project_candidate, verify_project_package,
    verify_retained_project_candidate,
};

const TRANSACTION_PREFIX: &str = ".project-resource-publication-";
const INTENT_NAME: &str = "intent.json";
const INTENT_STAGING_NAME: &str = ".intent.json.tmp";
const RESOURCE_STAGING_NAME: &str = "resource.json";
const PREVIOUS_RESOURCE_NAME: &str = "previous-resource.json";
const PROJECT_STAGING_NAME: &str = "project.json";
const PREVIOUS_PROJECT_NAME: &str = "previous-project.json";
const TRANSACTION_NAME: &str = "transaction.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub struct ProjectResourcePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub resource_id: &'a str,
    pub expected_role: ResourceRole,
    pub resource_bytes: &'a [u8],
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
    previous_resource: TransactionFile,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    Resource,
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
    publish_project_resource_observed(project_path.as_ref(), request, |_, _| Ok(()))
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

#[allow(
    clippy::too_many_lines,
    reason = "the publication coordinator keeps lock, recovery, staging, and disposition ordering visible in one fail-closed sequence"
)]
fn publish_project_resource_observed(
    project_path: &Path,
    request: ProjectResourcePublicationRequest<'_>,
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
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered = recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior resource transaction was recovered; reopen the current project revision",
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
    let descriptor = unique_resource(&verified, request.resource_id, request.expected_role)?;
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
    let candidate_project = candidate_manifest(
        &active_project,
        request.resource_id,
        request.resource_bytes,
        next_revision,
    )?;
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
        &previous,
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

#[allow(clippy::too_many_arguments)]
fn run_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    request: ProjectResourcePublicationRequest<'_>,
    previous_resource: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, RESOURCE_STAGING_NAME, request.resource_bytes)?;
    write_exclusive_synced(transaction, PREVIOUS_RESOURCE_NAME, previous_resource)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let descriptor = unique_resource(verified, request.resource_id, request.expected_role)?;
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
            path: descriptor.path().to_owned(),
            byte_length: request.resource_bytes.len() as u64,
            sha256: hex_digest(digest(request.resource_bytes)),
        },
        previous_resource: TransactionFile {
            path: descriptor.path().to_owned(),
            byte_length: previous_resource.len() as u64,
            sha256: hex_digest(digest(previous_resource)),
        },
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

    let parent = open_relative_parent(&verified.root_dir, descriptor.path())?;
    publish_staged_file(
        transaction,
        RESOURCE_STAGING_NAME,
        &parent,
        &parent,
        file_name(descriptor.path())?,
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
    let parent = open_relative_parent(root, &tx.resource.path)?;
    let current = read_optional_file(&parent, file_name(&tx.resource.path)?)?;
    let current_digest = current.as_deref().map(digest);
    let before = decode_digest(&tx.previous_resource.sha256)?;
    let after = decode_digest(&tx.resource.sha256)?;
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
        file_name(&tx.previous_resource.path)?,
        &format!(".resource-rollback-{}.json", tx.id),
        "previous project resource",
    )
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
    let previous_resource = read_bounded(transaction, PREVIOUS_RESOURCE_NAME, MAX_JSON_BYTES)?;
    validate_canonical_resource(&previous_resource).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous project resource is not canonical",
            error,
        )
    })?;
    if resource.len() as u64 != tx.resource.byte_length
        || digest(&resource) != decode_digest(&tx.resource.sha256)?
        || previous_resource.len() as u64 != tx.previous_resource.byte_length
        || digest(&previous_resource) != decode_digest(&tx.previous_resource.sha256)?
        || tx.resource.path != tx.previous_resource.path
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
        &previous_resource,
        &resource,
        tx,
    )
}

fn validate_transaction_descriptors(
    previous_manifest: &ProjectManifest,
    candidate_manifest: &ProjectManifest,
    previous_bytes: &[u8],
    candidate_bytes: &[u8],
    tx: &TransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let previous = manifest_resource(previous_manifest, &tx.resource_id)?;
    let candidate = manifest_resource(candidate_manifest, &tx.resource_id)?;
    if role_name(previous.role()) != tx.resource_role
        || candidate.role() != previous.role()
        || previous.path() != tx.previous_resource.path
        || candidate.path() != tx.resource.path
        || previous.byte_length() != previous_bytes.len() as u64
        || previous.sha256() != hex_digest(digest(previous_bytes))
        || candidate.byte_length() != candidate_bytes.len() as u64
        || candidate.sha256() != hex_digest(digest(candidate_bytes))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "resource descriptors do not match the transaction",
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
        || tx.resource.path != tx.previous_resource.path
        || tx.resource.byte_length == 0
        || tx.resource.byte_length > MAX_JSON_FILE_BYTES
        || tx.previous_resource.byte_length == 0
        || tx.previous_resource.byte_length > MAX_JSON_FILE_BYTES
        || decode_digest(&tx.resource.sha256).is_err()
        || decode_digest(&tx.previous_resource.sha256).is_err()
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
        remove_optional_matching_file(
            &parent,
            &format!(".resource-rollback-{}.json", tx.id),
            decode_digest(&tx.previous_resource.sha256)?,
        )?;
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
            let _ =
                publish_project_resource_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(
                        phase,
                        PublicationPhase::Transaction,
                        "simulated process crash"
                    );
                    Ok(())
                });
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

        let error =
            publish_project_resource_observed(&fixture.root, fixture.request(), |phase, _| {
                if phase == PublicationPhase::Resource {
                    fs::write(fixture.transaction_path().join(TRANSACTION_NAME), b"{}").unwrap();
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Io,
                        "simulated publication failure",
                    ));
                }
                Ok(())
            })
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
            let _ =
                publish_project_resource_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                });
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
}
