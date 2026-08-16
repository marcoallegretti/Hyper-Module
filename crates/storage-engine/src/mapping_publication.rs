use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_control_mappings::{ControlBinding, ControlMappingDocument, ControlSource};
use hyper_project_model::{AssetDescriptor, ProjectManifest, ResourceDescriptor, ResourceRole};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{
    CONTROL_MAPPING_TRANSACTION_PREFIX as TRANSACTION_PREFIX, ProjectLock,
    ProjectTransactionFamily, find_unhandled_project_transaction,
};
use super::stable_file::read_open_stable;
use super::{
    MANIFEST_NAME, MAX_JSON_BYTES, MAX_JSON_FILE_BYTES, StructurallyVerifiedProject,
    open_stable_project_root, same_directory, verify_project_candidate, verify_project_package,
    verify_retained_project_candidate,
};

const INTENT_NAME: &str = "intent.json";
const INTENT_STAGING_NAME: &str = ".intent.json.tmp";
const PROJECT_STAGING_NAME: &str = "project.json";
const PREVIOUS_PROJECT_NAME: &str = "previous-project.json";
const MAPPING_STAGING_NAME: &str = "mapping.json";
const PREVIOUS_MAPPING_NAME: &str = "previous-mapping.json";
const TRANSACTION_NAME: &str = "transaction.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug)]
pub struct ControlMappingPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub mapping_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingPublicationErrorKind {
    InvalidMapping,
    ProjectMismatch,
    ProjectChanged,
    ResourceConflict,
    TransactionUnavailable,
    Integrity,
    Io,
    Recovery,
}

#[derive(Debug)]
pub struct ControlMappingPublicationError {
    kind: ControlMappingPublicationErrorKind,
    message: String,
    published: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ControlMappingPublicationError {
    #[must_use]
    pub const fn kind(&self) -> ControlMappingPublicationErrorKind {
        self.kind
    }

    /// Returns true only when recovery verified that the requested mapping and
    /// manifest are already the durable active project state.
    #[must_use]
    pub const fn published(&self) -> bool {
        self.published
    }
}

impl fmt::Display for ControlMappingPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ControlMappingPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlMappingPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    mapping_id: String,
    mapping_path: String,
    changed: bool,
}

impl ControlMappingPublicationOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }

    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }

    #[must_use]
    pub fn mapping_id(&self) -> &str {
        &self.mapping_id
    }

    #[must_use]
    pub fn mapping_path(&self) -> &str {
        &self.mapping_path
    }

    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlMappingPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlMappingPublicationRecovery {
    transaction_id: String,
    disposition: ControlMappingPublicationRecoveryDisposition,
}

impl ControlMappingPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> ControlMappingPublicationRecoveryDisposition {
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
    mapping_id: String,
    mapping: TransactionFile,
    previous_mapping: Option<TransactionFile>,
    created_mappings_directory: bool,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    Mapping,
    Manifest,
}

/// Publishes one confirmed canonical control-mapping document and its updated
/// project manifest through a recoverable, serialized filesystem transaction.
///
/// The expected manifest identity binds the editor preview to the project
/// revision being changed. An identical existing mapping is a successful no-op
/// and does not increment the project revision.
///
/// # Errors
///
/// Returns a classified mapping, project, conflict, I/O, integrity, or recovery
/// error. [`ControlMappingPublicationError::published`] distinguishes the rare
/// case where publication verified successfully but transaction cleanup failed.
pub fn publish_control_mapping(
    project_path: impl AsRef<Path>,
    request: ControlMappingPublicationRequest<'_>,
) -> Result<ControlMappingPublicationOutcome, ControlMappingPublicationError> {
    publish_control_mapping_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers all durable control-mapping publication transactions in a project.
///
/// # Errors
///
/// Returns an error rather than overwriting a project manifest or mapping whose
/// identity does not match either side of the recorded transaction.
pub fn recover_control_mapping_publications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<ControlMappingPublicationRecovery>, ControlMappingPublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            "open project root for mapping recovery",
            error,
        )
    })?;
    let Some(backups) = open_optional_dir(&root, "backups")? else {
        return Ok(Vec::new());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::TransactionUnavailable,
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
fn publish_control_mapping_observed(
    project_path: &Path,
    request: ControlMappingPublicationRequest<'_>,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ControlMappingPublicationError>,
) -> Result<ControlMappingPublicationOutcome, ControlMappingPublicationError> {
    validate_canonical_mapping(request.mapping_bytes)?;
    let document = ControlMappingDocument::parse(request.mapping_bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "control mapping is invalid",
            error,
        )
    })?;

    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "project verification failed before mapping publication",
            error,
        )
    })?;
    validate_mapping_project(&verified, &document)?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "project changed after the mapping preview",
        ));
    }

    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let unhandled =
        find_unhandled_project_transaction(&backups, &[ProjectTransactionFamily::ControlMapping])
            .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::TransactionUnavailable,
                "inspect project transactions before mapping publication",
                error,
            )
        })?;
    if unhandled.is_some() {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    let recovered = recover_transactions_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "a prior mapping transaction was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "project changed before mapping publication",
            error,
        )
    })?;

    let active_bytes = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_bytes) != request.expected_project_sha256 {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "active project identity changed before mapping staging",
        ));
    }

    let existing = mapping_descriptor(&verified, document.id())?;
    let mapping_path = existing.map_or_else(
        || format!("mappings/{}.json", document.id()),
        |descriptor| descriptor.path().to_owned(),
    );
    let previous_bytes = existing
        .map(|_| verified.read_resource(document.id()))
        .transpose()
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Integrity,
                "read current mapping resource",
                error,
            )
        })?;
    let required = RequiredControlCapabilities::of_bindings(document.bindings());
    if previous_bytes.as_deref() == Some(request.mapping_bytes)
        && required.covered_by(verified.manifest.required_capabilities())
    {
        return Ok(ControlMappingPublicationOutcome {
            project_identity: request.expected_project_sha256,
            project_revision: verified.manifest.revision(),
            mapping_id: document.id().to_owned(),
            mapping_path,
            changed: false,
        });
    }

    let next_revision = verified
        .manifest
        .revision()
        .checked_add(1)
        .filter(|revision| *revision <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let candidate_bytes = candidate_manifest(
        &active_bytes,
        document.id(),
        &mapping_path,
        request.mapping_bytes,
        next_revision,
        existing.is_some(),
        required,
    )?;
    let mappings_exists = open_optional_dir(&verified.root_dir, "mappings")?.is_some();
    let created_mappings_directory = !mappings_exists && existing.is_none();
    if !mappings_exists && existing.is_some() {
        return Err(failure(
            ControlMappingPublicationErrorKind::Integrity,
            "declared mapping directory is unavailable",
        ));
    }

    let transaction_name = format!("{TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            "create mapping transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups
        .open_dir_nofollow(&transaction_name)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
                "open mapping transaction",
                error,
            )
        })?;

    let result = run_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        request.mapping_bytes,
        previous_bytes.as_deref(),
        &candidate_bytes,
        &mapping_path,
        document.id(),
        next_revision,
        created_mappings_directory,
        &mut observer,
    );

    match result {
        Ok(outcome) => {
            clear_and_remove_transaction(&backups, transaction, &transaction_name).map_err(
                |error| ControlMappingPublicationError {
                    kind: ControlMappingPublicationErrorKind::Recovery,
                    message: "mapping was published but transaction cleanup is incomplete"
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
                    return Err(ControlMappingPublicationError {
                        kind: ControlMappingPublicationErrorKind::Recovery,
                        message: "mapping publication failed and its durable transaction could not be read"
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
                Ok(ControlMappingPublicationRecoveryDisposition::RolledBack) => {
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)?;
                    Err(operation_error)
                }
                Ok(ControlMappingPublicationRecoveryDisposition::Committed) => {
                    let outcome = outcome_from_transaction(&tx, true)?;
                    clear_and_remove_transaction(&backups, transaction, &transaction_name)
                        .map_err(|error| ControlMappingPublicationError {
                            kind: ControlMappingPublicationErrorKind::Recovery,
                            message: "mapping was published but transaction cleanup is incomplete"
                                .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        })?;
                    Ok(outcome)
                }
                Err(recovery_error) => Err(ControlMappingPublicationError {
                    kind: ControlMappingPublicationErrorKind::Recovery,
                    message: "mapping publication and recovery both failed".to_owned(),
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
    mapping_bytes: &[u8],
    previous_mapping: Option<&[u8]>,
    candidate_bytes: &[u8],
    mapping_path: &str,
    mapping_id: &str,
    next_revision: u64,
    created_mappings_directory: bool,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ControlMappingPublicationError>,
) -> Result<ControlMappingPublicationOutcome, ControlMappingPublicationError> {
    write_intent(transaction, transaction_name)?;
    write_exclusive_synced(transaction, MAPPING_STAGING_NAME, mapping_bytes)?;
    if let Some(previous) = previous_mapping {
        write_exclusive_synced(transaction, PREVIOUS_MAPPING_NAME, previous)?;
    }
    let active_bytes = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_bytes) != *verified.manifest_sha256() {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectChanged,
            "active project changed during mapping staging",
        ));
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_bytes)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, &active_bytes)?;
    let tx = transaction_record(
        transaction_name,
        verified,
        candidate_bytes,
        mapping_id,
        mapping_path,
        mapping_bytes,
        previous_mapping,
        next_revision,
        created_mappings_directory,
    )?;
    let transaction_bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "serialize mapping transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &transaction_bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;

    let parent = if created_mappings_directory {
        open_or_create_dir(&verified.root_dir, "mappings")?
    } else {
        open_relative_parent(&verified.root_dir, mapping_path)?
    };
    publish_staged_file(
        transaction,
        MAPPING_STAGING_NAME,
        &parent,
        &parent,
        file_name(mapping_path)?,
        &format!(".mapping-{}.json", tx.id),
        "mapping resource",
    )?;
    observer(PublicationPhase::Mapping, verified.root())?;

    let candidate = verify_project_candidate(verified.root(), &verified.root_dir, candidate_bytes)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Integrity,
                "mapping publication candidate does not verify",
                error,
            )
        })?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ControlMappingPublicationErrorKind::Integrity,
            "mapping publication candidate revision changed",
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

    verify_committed(verified.root(), &verified.root_dir, digest(candidate_bytes))?;
    outcome_from_transaction(&tx, true)
}

fn validate_canonical_mapping(bytes: &[u8]) -> Result<(), ControlMappingPublicationError> {
    if bytes.is_empty() || bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "control mapping size is invalid",
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "parse control mapping",
            error,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "canonicalize control mapping",
            error,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "control mapping is not canonical RFC 8785 JSON",
        ));
    }
    Ok(())
}

fn validate_mapping_project(
    verified: &StructurallyVerifiedProject,
    document: &ControlMappingDocument,
) -> Result<(), ControlMappingPublicationError> {
    if document.project_id() != verified.manifest.project_id() {
        return Err(failure(
            ControlMappingPublicationErrorKind::ProjectMismatch,
            "control mapping belongs to another project",
        ));
    }
    let mut identities = BTreeSet::new();
    identities.extend(
        verified
            .manifest
            .resources()
            .iter()
            .map(ResourceDescriptor::id),
    );
    identities.extend(verified.manifest.assets().iter().map(AssetDescriptor::id));
    for binding in document.bindings() {
        if identities.contains(binding.id()) {
            return Err(failure(
                ControlMappingPublicationErrorKind::ResourceConflict,
                "control binding identity collides with a project record",
            ));
        }
    }
    mapping_descriptor(verified, document.id())?;
    Ok(())
}

fn mapping_descriptor<'a>(
    verified: &'a StructurallyVerifiedProject,
    mapping_id: &str,
) -> Result<Option<&'a hyper_project_model::ResourceDescriptor>, ControlMappingPublicationError> {
    let mut mapping = None;
    for descriptor in verified.manifest.resources() {
        if descriptor.role() == ResourceRole::Mapping && mapping.replace(descriptor).is_some() {
            return Err(failure(
                ControlMappingPublicationErrorKind::ResourceConflict,
                "project contains multiple control-mapping resources",
            ));
        }
        if descriptor.id() == mapping_id && descriptor.role() != ResourceRole::Mapping {
            return Err(failure(
                ControlMappingPublicationErrorKind::ResourceConflict,
                "control mapping identity belongs to another project resource",
            ));
        }
    }
    if verified
        .manifest
        .assets()
        .iter()
        .any(|asset| asset.id() == mapping_id)
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::ResourceConflict,
            "control mapping identity belongs to a project asset",
        ));
    }
    if mapping.is_some_and(|descriptor| descriptor.id() != mapping_id) {
        return Err(failure(
            ControlMappingPublicationErrorKind::ResourceConflict,
            "project already contains another control-mapping resource",
        ));
    }
    Ok(mapping)
}

/// Capabilities one set of control bindings requires from its project.
///
/// This is the single derivation rule. Any caller that must resolve or compile
/// bindings before they are published uses it too, so a preview and its
/// publication cannot disagree about what the project will declare.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequiredControlCapabilities {
    cv_inputs: u8,
    has_clock_input: bool,
    has_reset_input: bool,
}

impl RequiredControlCapabilities {
    /// Derives the capabilities `bindings` require, counting disabled bindings
    /// because the loader validates every persisted binding, not only the
    /// enabled ones.
    #[must_use]
    pub fn of_bindings(bindings: &[ControlBinding]) -> Self {
        let mut required = Self {
            cv_inputs: 0,
            has_clock_input: false,
            has_reset_input: false,
        };
        for binding in bindings {
            match binding.source() {
                ControlSource::CvInput { input_index } => {
                    required.cv_inputs = required.cv_inputs.max(input_index.saturating_add(1));
                }
                ControlSource::ClockInput { .. } => required.has_clock_input = true,
                ControlSource::ResetInput { .. } => required.has_reset_input = true,
            }
        }
        required
    }

    #[must_use]
    pub const fn cv_inputs(self) -> u8 {
        self.cv_inputs
    }

    #[must_use]
    pub const fn has_clock_input(self) -> bool {
        self.has_clock_input
    }

    #[must_use]
    pub const fn has_reset_input(self) -> bool {
        self.has_reset_input
    }

    const fn covered_by(self, declared: &hyper_project_model::HardwareCapabilities) -> bool {
        self.cv_inputs <= declared.cv_inputs()
            && (!self.has_clock_input || declared.has_clock_input())
            && (!self.has_reset_input || declared.has_reset_input())
    }
}

/// Raises the declared capability record to cover `required`, never lowering a
/// value. The version 1 binding domain bounds every derived value inside the
/// schema range, so no ceiling rejection is reachable here.
fn raise_required_capabilities(
    value: &mut Value,
    required: RequiredControlCapabilities,
) -> Result<(), ControlMappingPublicationError> {
    let declared = value["required_capabilities"]
        .as_object_mut()
        .ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::Integrity,
                "project capability declaration is invalid",
            )
        })?;
    let cv_inputs = declared
        .get("cv_inputs")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .max(u64::from(required.cv_inputs));
    let clock = declared
        .get("has_clock_input")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || required.has_clock_input;
    let reset = declared
        .get("has_reset_input")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || required.has_reset_input;
    declared.insert("cv_inputs".to_owned(), cv_inputs.into());
    declared.insert("has_clock_input".to_owned(), clock.into());
    declared.insert("has_reset_input".to_owned(), reset.into());
    Ok(())
}

fn candidate_manifest(
    active: &[u8],
    mapping_id: &str,
    mapping_path: &str,
    mapping_bytes: &[u8],
    revision: u64,
    replacing: bool,
    required: RequiredControlCapabilities,
) -> Result<Vec<u8>, ControlMappingPublicationError> {
    let mut value: Value = serde_json::from_slice(active).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "parse active project manifest",
            error,
        )
    })?;
    value["revision"] = revision.into();
    raise_required_capabilities(&mut value, required)?;
    let resources = value["resources"].as_array_mut().ok_or_else(|| {
        failure(
            ControlMappingPublicationErrorKind::Integrity,
            "project resource declarations are invalid",
        )
    })?;
    let mapping_sha256 = hex_digest(digest(mapping_bytes));
    if replacing {
        let descriptor = resources
            .iter_mut()
            .find(|resource| resource["id"].as_str() == Some(mapping_id))
            .ok_or_else(|| {
                failure(
                    ControlMappingPublicationErrorKind::Integrity,
                    "current mapping descriptor disappeared",
                )
            })?;
        descriptor["byte_length"] = mapping_bytes.len().into();
        descriptor["sha256"] = mapping_sha256.into();
    } else {
        resources.push(json!({
            "id": mapping_id,
            "role": "mapping",
            "path": mapping_path,
            "media_type": "application/json",
            "byte_length": mapping_bytes.len(),
            "sha256": mapping_sha256,
            "extensions": {}
        }));
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "serialize candidate project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ControlMappingPublicationErrorKind::ResourceConflict,
            "candidate project manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "candidate project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

#[allow(clippy::too_many_arguments)]
fn transaction_record(
    directory_name: &str,
    verified: &StructurallyVerifiedProject,
    candidate_bytes: &[u8],
    mapping_id: &str,
    mapping_path: &str,
    mapping_bytes: &[u8],
    previous_mapping: Option<&[u8]>,
    next_revision: u64,
    created_mappings_directory: bool,
) -> Result<TransactionRecord, ControlMappingPublicationError> {
    let id = transaction_id(directory_name)?;
    Ok(TransactionRecord {
        format: "hyper-control-mapping-publication-transaction".to_owned(),
        format_version: 1,
        id,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_bytes)),
        before_project_revision: verified.manifest.revision(),
        after_project_revision: next_revision,
        mapping_id: mapping_id.to_owned(),
        mapping: transaction_file(mapping_path, mapping_bytes),
        previous_mapping: previous_mapping.map(|bytes| transaction_file(mapping_path, bytes)),
        created_mappings_directory,
        extensions: serde_json::Map::new(),
    })
}

fn transaction_file(path: &str, bytes: &[u8]) -> TransactionFile {
    TransactionFile {
        path: path.to_owned(),
        byte_length: bytes.len() as u64,
        sha256: hex_digest(digest(bytes)),
    }
}

fn write_intent(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ControlMappingPublicationError> {
    let intent = TransactionIntent {
        format: "hyper-control-mapping-publication-intent".to_owned(),
        format_version: 1,
        id: transaction_id(directory_name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::InvalidMapping,
            "serialize mapping publication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
                "publish mapping transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn recover_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<ControlMappingPublicationRecovery>, ControlMappingPublicationError> {
    let mut names = Vec::new();
    for entry in backups.entries().map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            "read project backups",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
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
                ControlMappingPublicationErrorKind::Io,
                "open mapping recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(error) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_prepublication_residue(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove_transaction(backups, transaction, &name)?;
                outcomes.push(ControlMappingPublicationRecovery {
                    transaction_id: id,
                    disposition: ControlMappingPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition = recover_transaction(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove_transaction(backups, transaction, &name)?;
        outcomes.push(ControlMappingPublicationRecovery {
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
) -> Result<ControlMappingPublicationRecoveryDisposition, ControlMappingPublicationError> {
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
        return Ok(ControlMappingPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping recovery found an unrelated project identity",
        ));
    }

    rollback_mapping(root, transaction, tx)?;
    cleanup_publication_temporaries(root, backups, transaction, tx)?;
    if tx.created_mappings_directory {
        match root.remove_dir("mappings") {
            Ok(()) => sync_dir(root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ControlMappingPublicationErrorKind::Recovery,
                    "remove transaction-created mappings directory",
                    error,
                ));
            }
        }
    }
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != before {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "previous project manifest does not match its recorded identity",
        ));
    }
    verify_retained_project_candidate(root, &previous).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "rolled-back mapping project does not verify",
            error,
        )
    })?;
    Ok(ControlMappingPublicationRecoveryDisposition::RolledBack)
}

fn rollback_mapping(
    root: &Dir,
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ControlMappingPublicationError> {
    let parent = match open_relative_parent(root, &tx.mapping.path) {
        Ok(parent) => Some(parent),
        Err(error) if tx.created_mappings_directory && error_is_not_found(&error) => None,
        Err(error) => return Err(error),
    };
    let current = parent
        .as_ref()
        .map(|parent| read_optional_file(parent, file_name(&tx.mapping.path)?))
        .transpose()?
        .flatten();
    let current_digest = current.as_deref().map(digest);
    let after = decode_digest(&tx.mapping.sha256)?;
    if let Some(previous) = &tx.previous_mapping {
        let before = decode_digest(&previous.sha256)?;
        match current_digest {
            Some(found) if found == before => return Ok(()),
            Some(found) if found != after => {
                return Err(failure(
                    ControlMappingPublicationErrorKind::Recovery,
                    "mapping rollback refuses unrelated published content",
                ));
            }
            Some(_) | None => {}
        }
        let parent = parent.ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::Recovery,
                "mapping rollback parent directory is unavailable",
            )
        })?;
        publish_staged_file(
            transaction,
            PREVIOUS_MAPPING_NAME,
            &parent,
            &parent,
            file_name(&previous.path)?,
            &format!(".mapping-rollback-{}.json", tx.id),
            "previous mapping resource",
        )?;
    } else if let Some(found) = current_digest {
        if found != after {
            return Err(failure(
                ControlMappingPublicationErrorKind::Recovery,
                "mapping rollback refuses unrelated published content",
            ));
        }
        let parent = parent.ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::Recovery,
                "published mapping parent directory is unavailable",
            )
        })?;
        remove_verified_file(&parent, file_name(&tx.mapping.path)?, after, &tx.id)?;
    }
    Ok(())
}

fn validate_staged_transaction(
    transaction: &Dir,
    tx: &TransactionRecord,
) -> Result<(), ControlMappingPublicationError> {
    let mapping = read_bounded(transaction, MAPPING_STAGING_NAME, MAX_JSON_BYTES)?;
    validate_canonical_mapping(&mapping).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "staged control mapping is not canonical",
            error,
        )
    })?;
    if mapping.len() as u64 != tx.mapping.byte_length
        || digest(&mapping) != decode_digest(&tx.mapping.sha256)?
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "staged mapping does not match its transaction record",
        ));
    }
    let document = ControlMappingDocument::parse(&mapping).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "staged mapping is invalid",
            error,
        )
    })?;
    if document.id() != tx.mapping_id {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "staged mapping identity changed",
        ));
    }
    match &tx.previous_mapping {
        Some(previous) => {
            let bytes = read_bounded(transaction, PREVIOUS_MAPPING_NAME, MAX_JSON_BYTES)?;
            if bytes.len() as u64 != previous.byte_length
                || digest(&bytes) != decode_digest(&previous.sha256)?
                || previous.path != tx.mapping.path
            {
                return Err(failure(
                    ControlMappingPublicationErrorKind::Recovery,
                    "staged previous mapping does not match its transaction record",
                ));
            }
        }
        None if !missing_file(transaction, PREVIOUS_MAPPING_NAME)? => {
            return Err(failure(
                ControlMappingPublicationErrorKind::Recovery,
                "mapping addition transaction contains an unexpected previous mapping",
            ));
        }
        None => {}
    }
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "staged project manifest identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous_project).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "staged previous project manifest is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate_project).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "staged candidate project manifest is invalid",
            error,
        )
    })?;
    if previous_manifest.project_id() != candidate_manifest.project_id()
        || document.project_id() != candidate_manifest.project_id()
        || previous_manifest.revision() != tx.before_project_revision
        || candidate_manifest.revision() != tx.after_project_revision
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping transaction project revisions are inconsistent",
        ));
    }
    validate_transaction_mapping_descriptors(
        &previous_manifest,
        &candidate_manifest,
        &mapping,
        tx,
    )?;
    Ok(())
}

fn validate_transaction_mapping_descriptors(
    previous_manifest: &ProjectManifest,
    candidate_manifest: &ProjectManifest,
    mapping_bytes: &[u8],
    tx: &TransactionRecord,
) -> Result<(), ControlMappingPublicationError> {
    let previous = unique_manifest_mapping(previous_manifest)?;
    let candidate = unique_manifest_mapping(candidate_manifest)?.ok_or_else(|| {
        failure(
            ControlMappingPublicationErrorKind::Recovery,
            "candidate project has no control-mapping descriptor",
        )
    })?;
    if candidate.id() != tx.mapping_id
        || candidate.path() != tx.mapping.path
        || candidate.byte_length() != tx.mapping.byte_length
        || candidate.sha256() != tx.mapping.sha256
        || candidate.byte_length() != mapping_bytes.len() as u64
        || candidate.sha256() != hex_digest(digest(mapping_bytes))
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "candidate mapping descriptor does not match the transaction",
        ));
    }
    match (previous, &tx.previous_mapping) {
        (Some(descriptor), Some(record))
            if descriptor.id() == tx.mapping_id
                && descriptor.path() == record.path
                && descriptor.byte_length() == record.byte_length
                && descriptor.sha256() == record.sha256 =>
        {
            Ok(())
        }
        (None, None) if tx.mapping.path == format!("mappings/{}.json", tx.mapping_id) => Ok(()),
        _ => Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "previous mapping descriptor does not match the transaction",
        )),
    }
}

fn unique_manifest_mapping(
    manifest: &ProjectManifest,
) -> Result<Option<&ResourceDescriptor>, ControlMappingPublicationError> {
    let mut found = None;
    for descriptor in manifest
        .resources()
        .iter()
        .filter(|descriptor| descriptor.role() == ResourceRole::Mapping)
    {
        if found.replace(descriptor).is_some() {
            return Err(failure(
                ControlMappingPublicationErrorKind::Recovery,
                "transaction project contains multiple mapping descriptors",
            ));
        }
    }
    Ok(found)
}

fn read_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<TransactionRecord, ControlMappingPublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "mapping transaction intent")?;
    let intent: TransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "parse mapping transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "mapping transaction")?;
    let tx: TransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "parse mapping transaction",
            error,
        )
    })?;
    let expected_id = transaction_id(directory_name)?;
    if intent.format != "hyper-control-mapping-publication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != expected_id
        || tx.format != "hyper-control-mapping-publication-transaction"
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || tx.id != expected_id
        || tx.mapping_id.is_empty()
        || tx.mapping.path != format!("mappings/{}.json", tx.mapping_id)
            && tx.previous_mapping.is_none()
        || tx.mapping.byte_length == 0
        || tx.mapping.byte_length > MAX_JSON_FILE_BYTES
        || decode_digest(&tx.mapping.sha256).is_err()
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision >= tx.after_project_revision
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping publication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn validate_prepublication_residue(
    transaction: &Dir,
    directory_name: &str,
) -> Result<(), ControlMappingPublicationError> {
    let id = transaction_id(directory_name)?;
    let mut files = BTreeSet::new();
    for entry in transaction.entries().map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "read incomplete mapping transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "read incomplete mapping transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(
            name.as_str(),
            INTENT_STAGING_NAME
                | INTENT_NAME
                | MAPPING_STAGING_NAME
                | PREVIOUS_MAPPING_NAME
                | PROJECT_STAGING_NAME
                | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ControlMappingPublicationErrorKind::Recovery,
                "incomplete mapping transaction contains an unknown entry",
            ));
        }
        files.insert(name);
    }
    if files.contains(INTENT_NAME) {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "mapping transaction intent")?;
        let intent: TransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "parse mapping transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-control-mapping-publication-intent"
            || intent.format_version != 1
            || intent.id != id
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ControlMappingPublicationErrorKind::Recovery,
                "incomplete mapping transaction intent is invalid",
            ));
        }
    }
    Ok(())
}

fn outcome_from_transaction(
    tx: &TransactionRecord,
    changed: bool,
) -> Result<ControlMappingPublicationOutcome, ControlMappingPublicationError> {
    Ok(ControlMappingPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        mapping_id: tx.mapping_id.clone(),
        mapping_path: tx.mapping.path.clone(),
        changed,
    })
}

fn verify_committed(
    root_path: &Path,
    retained_root: &Dir,
    expected: [u8; 32],
) -> Result<(), ControlMappingPublicationError> {
    let committed = verify_project_package(root_path).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "committed mapping project does not verify",
            error,
        )
    })?;
    if !same_directory(retained_root, &committed.root_dir).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            "compare committed mapping project root",
            error,
        )
    })? || *committed.manifest_sha256() != expected
    {
        return Err(failure(
            ControlMappingPublicationErrorKind::Integrity,
            "committed mapping project identity changed",
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
) -> Result<(), ControlMappingPublicationError> {
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&previous, "previous project manifest")?;
    ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "previous project manifest is invalid",
            error,
        )
    })?;
    if digest(&previous) != expected {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
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
) -> Result<(), ControlMappingPublicationError> {
    let bytes = read_bounded(transaction, staged_name, MAX_JSON_BYTES)?;
    write_exclusive_synced(temporary_directory, temporary_name, &bytes)?;
    sync_dir(temporary_directory)?;
    temporary_directory
        .rename(temporary_name, destination, destination_name)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
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
) -> Result<(), ControlMappingPublicationError> {
    if let Ok(parent) = open_relative_parent(root, &tx.mapping.path) {
        remove_optional_matching_file(
            &parent,
            &format!(".mapping-{}.json", tx.id),
            decode_digest(&tx.mapping.sha256)?,
        )?;
        if let Some(previous) = &tx.previous_mapping {
            remove_optional_matching_file(
                &parent,
                &format!(".mapping-rollback-{}.json", tx.id),
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
) -> Result<(), ControlMappingPublicationError> {
    let Some(bytes) = read_optional_file(directory, name)? else {
        return Ok(());
    };
    if digest(&bytes) != expected {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping recovery refuses an unrelated temporary file",
        ));
    }
    directory.remove_file(name).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "remove mapping publication temporary file",
            error,
        )
    })?;
    sync_dir(directory)
}

fn remove_verified_file(
    directory: &Dir,
    name: &str,
    expected: [u8; 32],
    id: &str,
) -> Result<(), ControlMappingPublicationError> {
    let bytes = read_optional_file(directory, name)?.ok_or_else(|| {
        failure(
            ControlMappingPublicationErrorKind::Recovery,
            "published mapping disappeared during rollback",
        )
    })?;
    if digest(&bytes) != expected {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping rollback refuses changed published content",
        ));
    }
    let quarantine = format!(".mapping-rollback-{id}.json");
    directory
        .rename(name, directory, &quarantine)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "quarantine published mapping",
                error,
            )
        })?;
    directory.remove_file(&quarantine).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "remove published mapping",
            error,
        )
    })?;
    sync_dir(directory)
}

fn open_relative_parent(root: &Dir, relative: &str) -> Result<Dir, ControlMappingPublicationError> {
    let mut segments = relative.split('/').peekable();
    let mut directory = root.try_clone().map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
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
                ControlMappingPublicationErrorKind::Io,
                "inspect mapping path ancestor",
                error,
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(failure(
                ControlMappingPublicationErrorKind::Integrity,
                "mapping path ancestor is not a real directory",
            ));
        }
        directory = directory.open_dir_nofollow(segment).map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
                "open mapping path ancestor",
                error,
            )
        })?;
    }
    Err(failure(
        ControlMappingPublicationErrorKind::Integrity,
        "mapping path has no file component",
    ))
}

fn file_name(relative: &str) -> Result<&str, ControlMappingPublicationError> {
    relative
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::Integrity,
                "mapping path has no file name",
            )
        })
}

fn read_optional_file(
    directory: &Dir,
    name: &str,
) -> Result<Option<Vec<u8>>, ControlMappingPublicationError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(caused(
                ControlMappingPublicationErrorKind::Io,
                "inspect mapping resource",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(failure(
            ControlMappingPublicationErrorKind::Integrity,
            "mapping resource is not a regular non-link file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            "open mapping resource",
            error,
        )
    })?;
    read_open_stable(file.into_std(), name, MAX_JSON_FILE_BYTES)
        .map(Some)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Integrity,
                "read stable mapping resource",
                error,
            )
        })
}

fn read_bounded(
    directory: &Dir,
    name: &str,
    maximum: usize,
) -> Result<Vec<u8>, ControlMappingPublicationError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            format!("open {name}"),
            error,
        )
    })?;
    read_open_stable(file.into_std(), name, maximum as u64).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Integrity,
            format!("read stable {name}"),
            error,
        )
    })
}

fn write_exclusive_synced(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), ControlMappingPublicationError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            format!("create {name}"),
            error,
        )
    })?;
    file.write_all(bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            format!("write {name}"),
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            format!("sync {name}"),
            error,
        )
    })
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir, ControlMappingPublicationError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(caused(
                ControlMappingPublicationErrorKind::Io,
                format!("create {name} directory"),
                error,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Io,
            format!("open {name} directory"),
            error,
        )
    })
}

fn open_optional_dir(
    parent: &Dir,
    name: &str,
) -> Result<Option<Dir>, ControlMappingPublicationError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(caused(
            ControlMappingPublicationErrorKind::Io,
            format!("open {name} directory"),
            error,
        )),
    }
}

fn sync_dir(directory: &Dir) -> Result<(), ControlMappingPublicationError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
                "open directory for synchronization",
                error,
            )
        })?
        .sync_all()
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Io,
                "sync directory",
                error,
            )
        })
}

fn transaction_id(directory_name: &str) -> Result<String, ControlMappingPublicationError> {
    let id = directory_name
        .strip_prefix(TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ControlMappingPublicationErrorKind::Recovery,
                "mapping transaction directory name is invalid",
            )
        })?;
    Uuid::parse_str(id).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping transaction identifier is invalid",
            error,
        )
    })?;
    Ok(id.to_owned())
}

fn require_canonical(bytes: &[u8], label: &str) -> Result<(), ControlMappingPublicationError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            format!("parse {label}"),
            error,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            format!("canonicalize {label}"),
            error,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            format!("{label} is not canonical"),
        ));
    }
    Ok(())
}

fn missing_file(directory: &Dir, name: &str) -> Result<bool, ControlMappingPublicationError> {
    match directory.symlink_metadata(name) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(caused(
            ControlMappingPublicationErrorKind::Io,
            format!("inspect {name}"),
            error,
        )),
    }
}

fn error_is_not_found(error: &ControlMappingPublicationError) -> bool {
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

fn clear_transaction(transaction: &Dir) -> Result<(), ControlMappingPublicationError> {
    for name in [
        INTENT_STAGING_NAME,
        INTENT_NAME,
        MAPPING_STAGING_NAME,
        PREVIOUS_MAPPING_NAME,
        PROJECT_STAGING_NAME,
        PREVIOUS_PROJECT_NAME,
        TRANSACTION_NAME,
    ] {
        match transaction.remove_file(name) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(caused(
                    ControlMappingPublicationErrorKind::Recovery,
                    format!("remove mapping transaction file {name}"),
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
) -> Result<(), ControlMappingPublicationError> {
    let quarantine = format!(".control-mapping-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "quarantine mapping transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "open quarantined mapping transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "compare quarantined mapping transaction",
            error,
        )
    })? {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "mapping transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "remove mapping transaction",
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
) -> Result<(), ControlMappingPublicationError> {
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ControlMappingPublicationErrorKind::Recovery,
            "remove mapping transaction",
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

fn decode_digest(value: &str) -> Result<[u8; 32], ControlMappingPublicationError> {
    if value.len() != 64 {
        return Err(failure(
            ControlMappingPublicationErrorKind::Recovery,
            "SHA-256 digest is invalid",
        ));
    }
    let mut output = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let text = std::str::from_utf8(pair).map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "SHA-256 digest is invalid",
                error,
            )
        })?;
        output[index] = u8::from_str_radix(text, 16).map_err(|error| {
            caused(
                ControlMappingPublicationErrorKind::Recovery,
                "SHA-256 digest is invalid",
                error,
            )
        })?;
    }
    Ok(output)
}

fn failure(
    kind: ControlMappingPublicationErrorKind,
    message: impl Into<String>,
) -> ControlMappingPublicationError {
    ControlMappingPublicationError {
        kind,
        message: message.into(),
        published: false,
        source: None,
    }
}

fn caused(
    kind: ControlMappingPublicationErrorKind,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> ControlMappingPublicationError {
    ControlMappingPublicationError {
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
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334474";
    const BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334475";

    struct Fixture {
        root: PathBuf,
        original_mapping: Vec<u8>,
        replacement_mapping: Vec<u8>,
    }

    impl Fixture {
        fn new(with_mapping: bool) -> Self {
            Self::with_clock_capability(with_mapping, true)
        }

        fn with_clock_capability(with_mapping: bool, has_clock_input: bool) -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-control-mapping-publication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("timeline")).unwrap();
            let timeline = br#"{"format":"fixture-timeline"}"#;
            fs::write(root.join("timeline/timeline.json"), timeline).unwrap();
            let original_mapping = mapping_bytes(true);
            let replacement_mapping = mapping_bytes(false);
            let mut resources = vec![json!({
                "id": TIMELINE_ID,
                "role": "timeline",
                "path": "timeline/timeline.json",
                "media_type": "application/json",
                "byte_length": timeline.len(),
                "sha256": hex_digest(digest(timeline)),
                "extensions": {}
            })];
            if with_mapping {
                fs::create_dir(root.join("mappings")).unwrap();
                fs::write(root.join("mappings/manual.json"), &original_mapping).unwrap();
                resources.push(json!({
                    "id": MAPPING_ID,
                    "role": "mapping",
                    "path": "mappings/manual.json",
                    "media_type": "application/json",
                    "byte_length": original_mapping.len(),
                    "sha256": hex_digest(digest(&original_mapping)),
                    "extensions": {}
                }));
            }
            let manifest = json!({
                "format": "hyper-project",
                "format_version": 1,
                "project_id": PROJECT_ID,
                "revision": 1,
                "name": "Mapping publication fixture",
                "sample_rate_hz": 48000,
                "required_capabilities": {
                    "format": "hyper-hardware-capabilities",
                    "format_version": 1,
                    "audio_inputs": 0,
                    "audio_outputs": 0,
                    "cv_inputs": 0,
                    "cv_outputs": 0,
                    "gate_outputs": 0,
                    "has_clock_input": has_clock_input,
                    "has_reset_input": false,
                    "has_clock_output": false,
                    "usb_mode": "none",
                    "has_ethernet": false,
                    "extensions": {}
                },
                "resources": resources,
                "assets": [],
                "extensions": {}
            });
            fs::write(
                root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            Self {
                root,
                original_mapping,
                replacement_mapping,
            }
        }

        fn identity(&self) -> [u8; 32] {
            *verify_project_package(&self.root)
                .unwrap()
                .manifest_sha256()
        }

        fn request(&self) -> ControlMappingPublicationRequest<'_> {
            ControlMappingPublicationRequest {
                expected_project_sha256: self.identity(),
                mapping_bytes: &self.replacement_mapping,
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

    fn mapping_bytes(enabled: bool) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&json!({
            "format": "hyper-control-mappings",
            "format_version": 1,
            "id": MAPPING_ID,
            "project_id": PROJECT_ID,
            "bindings": [{
                "id": BINDING_ID,
                "enabled": enabled,
                "source": {"kind": "clock-input", "edge": "rising"},
                "scope": {"kind": "global"},
                "transform": {"kind": "trigger", "behavior": "invoke"},
                "target": {"kind": "transport", "action": "play"}
            }],
            "extensions": {}
        }))
        .unwrap()
    }

    #[test]
    fn replaces_mapping_and_treats_identical_republication_as_a_no_op() {
        let fixture = Fixture::new(true);

        let published = publish_control_mapping(&fixture.root, fixture.request()).unwrap();
        assert!(published.changed());
        assert_eq!(published.project_revision(), 2);
        assert_eq!(published.mapping_path(), "mappings/manual.json");
        assert_eq!(
            fs::read(fixture.root.join(published.mapping_path())).unwrap(),
            fixture.replacement_mapping
        );
        assert_eq!(fixture.transaction_count(), 0);

        let unchanged = publish_control_mapping(
            &fixture.root,
            ControlMappingPublicationRequest {
                expected_project_sha256: *published.project_identity(),
                mapping_bytes: &fixture.replacement_mapping,
            },
        )
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 2);
        assert_eq!(unchanged.project_identity(), published.project_identity());
    }

    #[test]
    fn mapping_publication_refuses_a_foreign_transaction_before_staging() {
        let fixture = Fixture::new(true);
        let foreign_name = format!(
            "{}fixture",
            crate::project_lock::AUDIO_TRACK_TRANSACTION_PREFIX
        );
        let foreign = fixture.root.join("backups").join(&foreign_name);
        fs::create_dir_all(&foreign).unwrap();

        let error = publish_control_mapping(&fixture.root, fixture.request()).unwrap_err();

        assert_eq!(
            error.kind(),
            ControlMappingPublicationErrorKind::ProjectChanged
        );
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            fixture.original_mapping
        );
        assert!(foreign.exists());
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn publication_raises_only_the_capabilities_the_mapping_requires() {
        let parent = std::env::temp_dir().join(format!(
            "hyper-mapping-capability-{}-{}",
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
        let project_id = created.project().manifest().project_id().to_owned();
        let declared = created.project().manifest().required_capabilities();
        assert_eq!(declared.cv_inputs(), 0);
        assert!(!declared.has_clock_input());
        assert!(!declared.has_reset_input());
        let audio_outputs = declared.audio_outputs();

        let mapping = serde_json_canonicalizer::to_vec(&json!({
            "format": "hyper-control-mappings",
            "format_version": 1,
            "id": MAPPING_ID,
            "project_id": project_id,
            "bindings": [{
                "id": BINDING_ID,
                "enabled": true,
                "source": {"kind": "cv-input", "input_index": 2},
                "scope": {"kind": "global"},
                "transform": {
                    "kind": "linear",
                    "input_min": -1_000_000,
                    "input_max": 1_000_000,
                    "output_min": 0,
                    "output_max": 1_000_000
                },
                "target": {"kind": "mixer-main", "parameter": "gain"}
            }],
            "extensions": {}
        }))
        .unwrap();

        let published = publish_control_mapping(
            &root,
            ControlMappingPublicationRequest {
                expected_project_sha256: *verify_project_package(&root)
                    .unwrap()
                    .manifest_sha256(),
                mapping_bytes: &mapping,
            },
        )
        .unwrap();
        assert!(published.changed());

        let verified = verify_project_package(&root).unwrap();
        let raised = verified.manifest().required_capabilities();
        assert_eq!(raised.cv_inputs(), 3, "raised to cover CV input index 2");
        assert!(!raised.has_clock_input(), "clock is not required");
        assert!(!raised.has_reset_input(), "reset is not required");
        assert_eq!(
            raised.audio_outputs(),
            audio_outputs,
            "unrelated capabilities are untouched"
        );

        let unchanged = publish_control_mapping(
            &root,
            ControlMappingPublicationRequest {
                expected_project_sha256: *verified.manifest_sha256(),
                mapping_bytes: &mapping,
            },
        )
        .unwrap();
        assert!(
            !unchanged.changed(),
            "an already covered republication stays a no-op"
        );
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn required_capabilities_count_disabled_bindings_and_every_source() {
        use hyper_control_mappings::{ControlTarget, ControlTransform};

        let linear = ControlTransform::Linear {
            input_min: -1_000_000,
            input_max: 1_000_000,
            output_min: 0,
            output_max: 1_000_000,
        };
        let disabled_cv = ControlBinding::try_new(
            BINDING_ID.to_owned(),
            false,
            ControlSource::CvInput { input_index: 3 },
            hyper_control_mappings::ControlScope::Global,
            linear,
            ControlTarget::MixerMain {
                parameter: hyper_control_mappings::MainParameter::Gain,
            },
        )
        .unwrap();
        let required = RequiredControlCapabilities::of_bindings(&[disabled_cv]);
        assert_eq!(
            required.cv_inputs(),
            4,
            "a disabled binding still requires its input, because the loader validates it"
        );
        assert!(!required.has_clock_input());
        assert!(!required.has_reset_input());

        let trigger = ControlTransform::Trigger {
            behavior: hyper_control_mappings::TriggerBehavior::Invoke,
        };
        let clock = ControlBinding::try_new(
            "018f47a2-6b2d-7c11-8a44-0011223344d0".to_owned(),
            true,
            ControlSource::ClockInput {
                edge: hyper_control_mappings::Edge::Rising,
            },
            hyper_control_mappings::ControlScope::Global,
            trigger,
            ControlTarget::Transport {
                action: hyper_control_mappings::TransportAction::Play,
            },
        )
        .unwrap();
        let reset = ControlBinding::try_new(
            "018f47a2-6b2d-7c11-8a44-0011223344d1".to_owned(),
            true,
            ControlSource::ResetInput {
                edge: hyper_control_mappings::Edge::Falling,
            },
            hyper_control_mappings::ControlScope::Global,
            trigger,
            ControlTarget::Transport {
                action: hyper_control_mappings::TransportAction::Stop,
            },
        )
        .unwrap();
        let clock_only = RequiredControlCapabilities::of_bindings(std::slice::from_ref(&clock));
        assert!(clock_only.has_clock_input() && !clock_only.has_reset_input());
        let reset_only = RequiredControlCapabilities::of_bindings(std::slice::from_ref(&reset));
        assert!(reset_only.has_reset_input() && !reset_only.has_clock_input());
        let both = RequiredControlCapabilities::of_bindings(&[clock, reset]);
        assert!(both.has_clock_input() && both.has_reset_input());
        assert_eq!(both.cv_inputs(), 0);
        assert_eq!(RequiredControlCapabilities::of_bindings(&[]).cv_inputs(), 0);
    }

    #[test]
    fn publication_raises_each_edge_source_capability_independently() {
        for (source, expect_clock, expect_reset) in [
            (json!({"kind": "clock-input", "edge": "rising"}), true, false),
            (json!({"kind": "reset-input", "edge": "falling"}), false, true),
        ] {
            let parent = std::env::temp_dir().join(format!(
                "hyper-mapping-edge-capability-{}-{}",
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
            let project_id = created.project().manifest().project_id().to_owned();
            let mapping = serde_json_canonicalizer::to_vec(&json!({
                "format": "hyper-control-mappings",
                "format_version": 1,
                "id": MAPPING_ID,
                "project_id": project_id,
                "bindings": [{
                    "id": BINDING_ID,
                    "enabled": true,
                    "source": source,
                    "scope": {"kind": "global"},
                    "transform": {"kind": "trigger", "behavior": "invoke"},
                    "target": {"kind": "transport", "action": "play"}
                }],
                "extensions": {}
            }))
            .unwrap();

            publish_control_mapping(
                &root,
                ControlMappingPublicationRequest {
                    expected_project_sha256: *verify_project_package(&root)
                        .unwrap()
                        .manifest_sha256(),
                    mapping_bytes: &mapping,
                },
            )
            .unwrap();

            let verified = verify_project_package(&root).unwrap();
            let raised = verified.manifest().required_capabilities();
            assert_eq!(raised.has_clock_input(), expect_clock);
            assert_eq!(raised.has_reset_input(), expect_reset);
            assert_eq!(raised.cv_inputs(), 0, "an edge source requires no CV input");
            let _ = fs::remove_dir_all(&parent);
        }
    }

    #[test]
    fn republishing_an_identical_mapping_repairs_an_under_declared_capability() {
        let fixture = Fixture::with_clock_capability(true, false);
        let identical = fixture.original_mapping.clone();

        let repaired = publish_control_mapping(
            &fixture.root,
            ControlMappingPublicationRequest {
                expected_project_sha256: fixture.identity(),
                mapping_bytes: &identical,
            },
        )
        .unwrap();

        assert!(
            repaired.changed(),
            "an under-declared capability is not an unchanged project"
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        assert!(verified.manifest().required_capabilities().has_clock_input());
        assert_eq!(
            verified.read_resource(MAPPING_ID).unwrap(),
            identical,
            "the mapping bytes are unchanged by the repair"
        );
    }

    #[test]
    fn adds_the_first_mapping_and_its_owned_directory() {
        let fixture = Fixture::new(false);

        let published = publish_control_mapping(&fixture.root, fixture.request()).unwrap();

        assert!(published.changed());
        assert_eq!(
            published.mapping_path(),
            format!("mappings/{MAPPING_ID}.json")
        );
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(
            verified.read_resource(MAPPING_ID).unwrap(),
            fixture.replacement_mapping
        );
        assert!(
            recover_control_mapping_publications(&fixture.root)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_stale_identity_and_noncanonical_mapping_without_project_changes() {
        let fixture = Fixture::new(true);
        let original_identity = fixture.identity();
        let stale = publish_control_mapping(
            &fixture.root,
            ControlMappingPublicationRequest {
                expected_project_sha256: [0_u8; 32],
                mapping_bytes: &fixture.replacement_mapping,
            },
        )
        .unwrap_err();
        assert_eq!(
            stale.kind(),
            ControlMappingPublicationErrorKind::ProjectChanged
        );

        let value: Value = serde_json::from_slice(&fixture.replacement_mapping).unwrap();
        let noncanonical = serde_json::to_vec_pretty(&value).unwrap();
        let invalid = publish_control_mapping(
            &fixture.root,
            ControlMappingPublicationRequest {
                expected_project_sha256: original_identity,
                mapping_bytes: &noncanonical,
            },
        )
        .unwrap_err();
        assert_eq!(
            invalid.kind(),
            ControlMappingPublicationErrorKind::InvalidMapping
        );
        assert_eq!(fixture.identity(), original_identity);
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            fixture.original_mapping
        );
    }

    #[test]
    fn recovers_a_crash_after_durable_staging_as_a_rollback() {
        let fixture = Fixture::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
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

        let report = crate::recover_project_transactions(&fixture.root).unwrap();
        let recovered = report.control_mappings();

        assert_eq!(recovered.len(), 1);
        assert!(report.recordings().is_empty());
        assert_eq!(
            recovered[0].disposition(),
            ControlMappingPublicationRecoveryDisposition::RolledBack
        );
        assert!(!fixture.root.join("mappings").exists());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            1
        );
    }

    #[test]
    fn staged_mapping_blocks_resource_publication_until_global_recovery() {
        let fixture = Fixture::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
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
        let package = verify_project_package(&fixture.root).unwrap();
        let replacement = serde_json_canonicalizer::to_vec(&json!({
            "format":"fixture-timeline",
            "revision":2
        }))
        .unwrap();

        let error = crate::publish_project_resource(
            &fixture.root,
            crate::ProjectResourcePublicationRequest {
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: TIMELINE_ID,
                expected_role: ResourceRole::Timeline,
                resource_bytes: &replacement,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            crate::ProjectResourcePublicationErrorKind::ProjectChanged
        );
        assert_eq!(fixture.transaction_count(), 1);
        let recovered = crate::recover_project_transactions(&fixture.root).unwrap();
        assert_eq!(recovered.control_mappings().len(), 1);
        assert_eq!(
            recovered.control_mappings()[0].disposition(),
            ControlMappingPublicationRecoveryDisposition::RolledBack
        );
        assert_eq!(
            fs::read(fixture.root.join("timeline/timeline.json")).unwrap(),
            br#"{"format":"fixture-timeline"}"#
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
    fn coordinated_recovery_reports_completed_mapping_work_before_recording_failure() {
        let fixture = Fixture::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(
                        phase,
                        PublicationPhase::Transaction,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());
        let recording_transaction = fixture
            .root
            .join("backups")
            .join(format!(".recording-import-{}", Uuid::new_v4()));
        fs::create_dir(&recording_transaction).unwrap();
        fs::write(recording_transaction.join("transaction.json"), b"{}").unwrap();

        let error = crate::recover_project_transactions(&fixture.root).unwrap_err();

        assert_eq!(error.completed().control_mappings().len(), 1);
        assert_eq!(
            error.completed().control_mappings()[0].disposition(),
            ControlMappingPublicationRecoveryDisposition::RolledBack
        );
        assert!(error.completed().recordings().is_empty());
        assert!(matches!(
            error.recovery_source(),
            crate::ProjectTransactionRecoveryErrorSource::Recording(_)
        ));
        assert_eq!(fixture.transaction_count(), 0);
        assert!(recording_transaction.exists());
        assert!(!fixture.root.join("mappings").exists());
    }

    #[test]
    fn recovers_a_crash_after_mapping_replacement_as_a_rollback() {
        let fixture = Fixture::new(true);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(phase, PublicationPhase::Mapping, "simulated process crash");
                    Ok(())
                });
        }));
        assert!(result.is_err());
        assert!(verify_project_package(&fixture.root).is_err());

        let recovered = recover_control_mapping_publications(&fixture.root).unwrap();

        assert_eq!(
            recovered[0].disposition(),
            ControlMappingPublicationRecoveryDisposition::RolledBack
        );
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            fixture.original_mapping
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
    fn failed_mapping_publication_retains_a_present_but_unreadable_transaction() {
        let fixture = Fixture::new(true);

        let error =
            publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                if phase == PublicationPhase::Mapping {
                    fs::write(fixture.transaction_path().join(TRANSACTION_NAME), b"{}").unwrap();
                    return Err(failure(
                        ControlMappingPublicationErrorKind::Io,
                        "simulated publication failure",
                    ));
                }
                Ok(())
            })
            .unwrap_err();

        assert_eq!(error.kind(), ControlMappingPublicationErrorKind::Recovery);
        assert_eq!(fixture.transaction_count(), 1);
        assert!(fixture.transaction_path().join(TRANSACTION_NAME).exists());
        assert!(verify_project_package(&fixture.root).is_err());
    }

    #[test]
    fn completes_a_crash_after_manifest_publication_as_committed() {
        let fixture = Fixture::new(true);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(phase, PublicationPhase::Manifest, "simulated process crash");
                    Ok(())
                });
        }));
        assert!(result.is_err());
        assert_eq!(
            verify_project_package(&fixture.root)
                .unwrap()
                .manifest()
                .revision(),
            2
        );

        let recovered = recover_control_mapping_publications(&fixture.root).unwrap();

        assert_eq!(
            recovered[0].disposition(),
            ControlMappingPublicationRecoveryDisposition::Committed
        );
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            fixture.replacement_mapping
        );
        assert_eq!(fixture.transaction_count(), 0);
    }

    #[test]
    fn recovery_refuses_to_overwrite_unrelated_mapping_content() {
        let fixture = Fixture::new(true);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(phase, PublicationPhase::Mapping, "simulated process crash");
                    Ok(())
                });
        }));
        assert!(result.is_err());
        let unrelated = mapping_bytes(true)
            .into_iter()
            .chain([b' '])
            .collect::<Vec<_>>();
        fs::write(fixture.root.join("mappings/manual.json"), &unrelated).unwrap();

        let error = recover_control_mapping_publications(&fixture.root).unwrap_err();

        assert_eq!(
            error.kind(),
            ControlMappingPublicationErrorKind::Recovery,
            "{error:?}"
        );
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            unrelated
        );
        assert_eq!(fixture.transaction_count(), 1);
    }

    #[test]
    fn recovery_refuses_corrupt_evidence_when_the_active_manifest_is_missing() {
        let fixture = Fixture::new(true);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ =
                publish_control_mapping_observed(&fixture.root, fixture.request(), |phase, _| {
                    assert_ne!(
                        phase,
                        PublicationPhase::Transaction,
                        "simulated process crash"
                    );
                    Ok(())
                });
        }));
        assert!(result.is_err());
        fs::write(
            fixture.transaction_path().join(PREVIOUS_PROJECT_NAME),
            b"{}",
        )
        .unwrap();
        fs::remove_file(fixture.root.join(MANIFEST_NAME)).unwrap();

        let error = recover_control_mapping_publications(&fixture.root).unwrap_err();

        assert_eq!(error.kind(), ControlMappingPublicationErrorKind::Recovery);
        assert!(!fixture.root.join(MANIFEST_NAME).exists());
        assert_eq!(fixture.transaction_count(), 1);
    }

    #[test]
    fn shared_project_lock_rejects_concurrent_mapping_publication() {
        let fixture = Fixture::new(true);
        let (_, root) = open_stable_project_root(&fixture.root).unwrap();
        let backups = open_or_create_dir(&root, "backups").unwrap();
        let _lock = ProjectLock::acquire(&backups).unwrap();

        let error = publish_control_mapping(&fixture.root, fixture.request()).unwrap_err();

        assert_eq!(
            error.kind(),
            ControlMappingPublicationErrorKind::TransactionUnavailable
        );
        assert_eq!(
            fs::read(fixture.root.join("mappings/manual.json")).unwrap(),
            fixture.original_mapping
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
