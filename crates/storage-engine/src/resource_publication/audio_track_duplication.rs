//! Durable publication for a complete independent audio-track duplication.
//!
//! A duplication is deliberately not composed from the single-resource
//! publisher: it adds track-owned resources while replacing project-wide
//! documents.  The manifest is therefore the sole commit record for one
//! retained-authority transaction.

use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_ENTRIES, MAX_JSON_BYTES,
    MAX_JSON_FILE_BYTES, MAX_SAFE_INTEGER, PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME,
    ProjectResourcePublicationError, ProjectResourcePublicationErrorKind, StructurallyVerifiedProject,
    TRANSACTION_NAME, TransactionFile, caused, decode_digest, digest, failure, file_name,
    hex_digest, open_optional_dir, open_or_create_dir, open_stable_project_root,
    missing_file, open_relative_parent, publish_staged_file, read_bounded, read_optional_file,
    remove_optional_matching_file, require_canonical, sync_dir, validate_canonical_resource, verify_committed,
    verify_project_candidate, verify_project_package, verify_retained_project_candidate,
    write_exclusive_synced,
};
use crate::project_lock::{
    AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX, ProjectLock, ProjectTransactionFamily,
    find_unhandled_project_transaction,
};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use hyper_native_device::DeviceChain;
use hyper_playback_json::{AudioTrackDuplicationDocument, AudioTrackDuplicationDocuments};
use hyper_project_model::{ProjectManifest, ResourceRole, is_valid_entity_id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use uuid::Uuid;

#[cfg(not(windows))]
use super::same_directory;

#[derive(Clone, Copy, Debug)]
pub struct AudioTrackDuplicationPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub mixer_id: &'a str,
    /// Canonical complete output from the playback-document duplication
    /// boundary. Storage never infers a subset of this document set.
    pub documents: &'a AudioTrackDuplicationDocuments,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackDuplicationPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

impl AudioTrackDuplicationPublicationOutcome {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTrackDuplicationPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackDuplicationPublicationRecovery {
    transaction_id: String,
    disposition: AudioTrackDuplicationPublicationRecoveryDisposition,
}

impl AudioTrackDuplicationPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> AudioTrackDuplicationPublicationRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DuplicationTransactionIntent {
    format: String,
    format_version: u8,
    id: String,
    entry_count: usize,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DuplicationTransactionEntry {
    id: String,
    role: String,
    resource: TransactionFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous_resource: Option<TransactionFile>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    created_parent_directory: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DuplicationTransactionRecord {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    duplicated_track_id: String,
    entries: Vec<DuplicationTransactionEntry>,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioTrackDuplicationPublicationPhase {
    Transaction,
    Resource(usize),
    Manifest,
}

#[derive(Debug)]
struct ActiveEntry<'a> {
    id: &'a str,
    role: ResourceRole,
    path: String,
    bytes: &'a [u8],
    previous: Option<Vec<u8>>,
    created_parent_directory: bool,
}

#[derive(Debug)]
struct StagedEntry {
    bytes: Vec<u8>,
    previous: Option<Vec<u8>>,
}

/// Publishes a complete independent audio-track duplication through one
/// recoverable, manifest-last transaction.
///
/// # Errors
///
/// Refuses stale, partial, noncanonical, conflicting, or unverified document
/// sets. A reported publication is committed only after its complete project
/// candidate verifies.
pub fn publish_audio_track_duplication(
    project_path: impl AsRef<Path>,
    request: AudioTrackDuplicationPublicationRequest<'_>,
) -> Result<AudioTrackDuplicationPublicationOutcome, ProjectResourcePublicationError> {
    publish_audio_track_duplication_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable independent audio-track duplication in a project.
///
/// # Errors
///
/// Fails closed rather than overwriting active bytes that cannot be proven to
/// belong to either recorded transaction side.
pub fn recover_audio_track_duplications(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackDuplicationPublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for audio-track duplication recovery",
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
    recover_audio_track_duplication_transactions_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "the batch authority, staging, publication, and recovery boundary remains auditable as one operation"
)]
fn publish_audio_track_duplication_observed(
    project_path: &Path,
    request: AudioTrackDuplicationPublicationRequest<'_>,
    mut observer: impl FnMut(
        AudioTrackDuplicationPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackDuplicationPublicationOutcome, ProjectResourcePublicationError> {
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before audio-track duplication",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after audio-track duplication preparation",
        ));
    }
    let entries = resolve_entries(&verified, request)?;
    let duplicated_track_id = request.documents.track().id().to_owned();
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered = recover_audio_track_duplication_transactions_locked(
        verified.root(),
        &verified.root_dir,
        &backups,
    )?;
    if !recovered.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior audio-track duplication was recovered; reopen the current project revision",
        ));
    }
    reject_other_transactions(&backups)?;
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before audio-track duplication staging",
            error,
        )
    })?;
    let previous_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before audio-track duplication staging",
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
    let candidate_project = candidate_duplication_manifest(&previous_project, &entries, next_revision)?;
    let transaction_name = format!("{AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create audio-track duplication transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&transaction_name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open audio-track duplication transaction",
            error,
        )
    })?;
    let result = run_audio_track_duplication_publication(
        &verified,
        &backups,
        &transaction,
        &transaction_name,
        &entries,
        &duplicated_track_id,
        &previous_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    match result {
        Ok(outcome) => {
            clear_and_remove_audio_track_duplication_transaction(
                &backups,
                transaction,
                &transaction_name,
            )
            .map_err(|error| ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message: "audio-track duplication was published but transaction cleanup is incomplete".to_owned(),
                published: true,
                source: Some(Box::new(error)),
            })?;
            Ok(outcome)
        }
        Err(operation_error) => recover_after_publication_error(
            verified.root(),
            &verified.root_dir,
            &backups,
            transaction,
            &transaction_name,
            operation_error,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the publication writes every transaction side before any active resource and commits the manifest last"
)]
fn run_audio_track_duplication_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    transaction_name: &str,
    entries: &[ActiveEntry<'_>],
    duplicated_track_id: &str,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(
        AudioTrackDuplicationPublicationPhase,
        &Path,
    ) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackDuplicationPublicationOutcome, ProjectResourcePublicationError> {
    write_duplication_intent(transaction, transaction_name, entries.len())?;
    for (index, entry) in entries.iter().enumerate() {
        write_exclusive_synced(transaction, &staged_name(index), entry.bytes)?;
        if let Some(previous) = &entry.previous {
            write_exclusive_synced(transaction, &previous_staged_name(index), previous)?;
        }
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = duplication_transaction_record(
        transaction_name,
        entries,
        duplicated_track_id,
        verified,
        candidate_project,
        next_revision,
    )?;
    let bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-track duplication transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(AudioTrackDuplicationPublicationPhase::Transaction, verified.root())?;
    for (index, entry) in tx.entries.iter().enumerate() {
        let parent = prepare_entry_parent(&verified.root_dir, entry)?;
        publish_staged_file(
            transaction,
            &staged_name(index),
            &parent,
            &parent,
            file_name(&entry.resource.path)?,
            &format!(".audio-track-duplication-{index}-{}.json", tx.id),
            "audio-track duplication resource",
        )?;
        observer(AudioTrackDuplicationPublicationPhase::Resource(index), verified.root())?;
    }
    let candidate = verify_project_candidate(verified.root(), &verified.root_dir, candidate_project)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "audio-track duplication candidate does not verify",
                error,
            )
        })?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-track duplication candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".audio-track-duplication-project-{}.json", tx.id),
        "audio-track duplication project manifest",
    )?;
    observer(AudioTrackDuplicationPublicationPhase::Manifest, verified.root())?;
    verify_committed(verified.root(), &verified.root_dir, digest(candidate_project))?;
    Ok(AudioTrackDuplicationPublicationOutcome {
        project_identity: digest(candidate_project),
        project_revision: next_revision,
        track_id: tx.duplicated_track_id,
    })
}

fn resolve_entries<'a>(
    verified: &StructurallyVerifiedProject,
    request: AudioTrackDuplicationPublicationRequest<'a>,
) -> Result<Vec<ActiveEntry<'a>>, ProjectResourcePublicationError> {
    let documents = request.documents;
    if request.mixer_id.is_empty() || !is_valid_entity_id(request.mixer_id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track duplication mixer identity is invalid",
        ));
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(
            2 + documents.clips().len()
                + documents.scenes().len()
                + documents.automation_takes().len()
                + usize::from(documents.arrangement().is_some())
                + usize::from(documents.device_chain().is_some()),
        )
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate audio-track duplication resource entries",
            )
        })?;
    push_addition(verified, &mut entries, documents.track(), ResourceRole::Track)?;
    for document in documents.clips() {
        push_addition(verified, &mut entries, document, ResourceRole::Clip)?;
    }
    push_replacement(
        verified,
        &mut entries,
        request.mixer_id,
        documents.mixer().bytes(),
        ResourceRole::Mixer,
    )?;
    if let Some(document) = documents.arrangement() {
        push_replacement(
            verified,
            &mut entries,
            document.id(),
            document.bytes(),
            ResourceRole::Arrangement,
        )?;
    }
    for document in documents.scenes() {
        push_replacement(
            verified,
            &mut entries,
            document.id(),
            document.bytes(),
            ResourceRole::Scene,
        )?;
    }
    for document in documents.automation_takes() {
        push_replacement(
            verified,
            &mut entries,
            document.id(),
            document.bytes(),
            ResourceRole::AutomationTake,
        )?;
    }
    if let Some(document) = documents.device_chain() {
        push_addition(verified, &mut entries, document, ResourceRole::DeviceChain)?;
    }
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-track duplication resource count is outside package limits",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in &entries {
        if !ids.insert(entry.id) || !paths.insert(entry.path.as_str()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "audio-track duplication resource identities are not unique",
            ));
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    let mut created_parents = BTreeSet::new();
    for entry in &mut entries {
        if entry.created_parent_directory
            && !created_parents.insert(resource_parent(&entry.path)?.to_owned())
        {
            entry.created_parent_directory = false;
        }
    }
    Ok(entries)
}

fn push_addition<'a>(
    verified: &StructurallyVerifiedProject,
    entries: &mut Vec<ActiveEntry<'a>>,
    document: &'a AudioTrackDuplicationDocument,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    validate_new_document(document, role)?;
    let path = duplication_resource_path(role, document.id())?;
    if verified.manifest().resources().iter().any(|descriptor| {
        descriptor.id() == document.id()
            || descriptor.path().eq_ignore_ascii_case(&path)
            || descriptor.id() == document.id()
    }) || verified.manifest().assets().iter().any(|asset| asset.id() == document.id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-track duplication resource collides with the active project",
        ));
    }
    let parent = resource_parent(&path)?;
    let created_parent_directory = match verified.root_dir.symlink_metadata(parent) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-track duplication resource parent is not a real directory",
                ));
            }
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect audio-track duplication resource parent",
                error,
            ));
        }
    };
    entries.push(ActiveEntry {
        id: document.id(),
        role,
        path,
        bytes: document.bytes(),
        previous: None,
        created_parent_directory,
    });
    Ok(())
}

fn push_replacement<'a>(
    verified: &StructurallyVerifiedProject,
    entries: &mut Vec<ActiveEntry<'a>>,
    id: &'a str,
    bytes: &'a [u8],
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    validate_document(id, bytes, role)?;
    let descriptor = verified
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == id)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "audio-track duplication replacement is absent from the active project",
            )
        })?;
    if descriptor.role() != role {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "audio-track duplication replacement role differs from the active project",
        ));
    }
    let previous = verified.read_resource(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read active audio-track duplication resource",
            error,
        )
    })?;
    entries.push(ActiveEntry {
        id,
        role,
        path: descriptor.path().to_owned(),
        bytes,
        previous: Some(previous),
        created_parent_directory: false,
    });
    Ok(())
}

fn validate_new_document(
    document: &AudioTrackDuplicationDocument,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    validate_document(document.id(), document.bytes(), role)
}

fn validate_document(
    id: &str,
    bytes: &[u8],
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    if !is_valid_entity_id(id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track duplication resource identity is invalid",
        ));
    }
    validate_canonical_resource(bytes)?;
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse audio-track duplication resource",
            error,
        )
    })?;
    let expected = resource_format(role);
    if value["format"].as_str() != Some(expected)
        || value["id"].as_str() != Some(id)
        || !value["extensions"].is_object()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track duplication resource identity is invalid",
        ));
    }
    if role == ResourceRole::DeviceChain {
        let chain = DeviceChain::parse(bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "parse audio-track duplication device chain",
                error,
            )
        })?;
        if chain.id() != id {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "audio-track duplication device-chain identity is invalid",
            ));
        }
    } else if value["format_version"].as_u64() != Some(1) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "audio-track duplication resource format version is invalid",
        ));
    }
    Ok(())
}

fn duplication_resource_path(
    role: ResourceRole,
    id: &str,
) -> Result<String, ProjectResourcePublicationError> {
    let directory = match role {
        ResourceRole::Track => "tracks",
        ResourceRole::Clip => "clips",
        ResourceRole::DeviceChain => "devices",
        _ => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "audio-track duplication cannot add this resource role",
            ));
        }
    };
    Ok(format!("{directory}/{id}.json"))
}

const fn resource_format(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Track => "hyper-track",
        ResourceRole::Clip => "hyper-audio-clip",
        ResourceRole::Mixer => "hyper-mixer",
        ResourceRole::Arrangement => "hyper-arrangement",
        ResourceRole::Scene => "hyper-scene",
        ResourceRole::AutomationTake => "hyper-automation-take",
        ResourceRole::DeviceChain => "hyper-device-chain",
        ResourceRole::Timeline | ResourceRole::Mapping | ResourceRole::RecordingMetadata => "",
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

fn role_from_name(value: &str) -> Option<ResourceRole> {
    Some(match value {
        "track" => ResourceRole::Track,
        "clip" => ResourceRole::Clip,
        "mixer" => ResourceRole::Mixer,
        "arrangement" => ResourceRole::Arrangement,
        "scene" => ResourceRole::Scene,
        "automation-take" => ResourceRole::AutomationTake,
        "device-chain" => ResourceRole::DeviceChain,
        _ => return None,
    })
}

fn candidate_duplication_manifest(
    active: &[u8],
    entries: &[ActiveEntry<'_>],
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
    for entry in entries {
        if entry.previous.is_some() {
            let descriptor = resources
                .iter_mut()
                .find(|descriptor| descriptor["id"].as_str() == Some(entry.id))
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "audio-track duplication replacement descriptor disappeared",
                    )
                })?;
            if descriptor["role"].as_str() != Some(role_name(entry.role))
                || descriptor["path"].as_str() != Some(&entry.path)
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-track duplication replacement descriptor changed",
                ));
            }
            descriptor["byte_length"] = entry.bytes.len().into();
            descriptor["sha256"] = hex_digest(digest(entry.bytes)).into();
        } else {
            resources.push(serde_json::json!({
                "id": entry.id,
                "role": role_name(entry.role),
                "path": entry.path,
                "media_type": "application/json",
                "byte_length": entry.bytes.len(),
                "sha256": hex_digest(digest(entry.bytes)),
                "extensions": {},
            }));
        }
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize audio-track duplication project manifest",
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
            "candidate audio-track duplication project manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn duplication_transaction_record(
    transaction_name: &str,
    entries: &[ActiveEntry<'_>],
    duplicated_track_id: &str,
    verified: &StructurallyVerifiedProject,
    candidate_project: &[u8],
    next_revision: u64,
) -> Result<DuplicationTransactionRecord, ProjectResourcePublicationError> {
    let mut records = Vec::new();
    records.try_reserve_exact(entries.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "allocate audio-track duplication transaction entries",
        )
    })?;
    for entry in entries {
        records.push(DuplicationTransactionEntry {
            id: entry.id.to_owned(),
            role: role_name(entry.role).to_owned(),
            resource: transaction_file(&entry.path, entry.bytes),
            previous_resource: entry
                .previous
                .as_deref()
                .map(|previous| transaction_file(&entry.path, previous)),
            created_parent_directory: entry.created_parent_directory,
        });
    }
    Ok(DuplicationTransactionRecord {
        format: "hyper-audio-track-duplication-transaction".to_owned(),
        format_version: 1,
        id: transaction_id(transaction_name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        duplicated_track_id: duplicated_track_id.to_owned(),
        entries: records,
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

fn staged_name(index: usize) -> String {
    format!("resource-{index}.json")
}

fn previous_staged_name(index: usize) -> String {
    format!("previous-resource-{index}.json")
}

fn write_duplication_intent(
    transaction: &Dir,
    transaction_name: &str,
    entry_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = DuplicationTransactionIntent {
        format: "hyper-audio-track-duplication-intent".to_owned(),
        format_version: 1,
        id: transaction_id(transaction_name)?,
        entry_count,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize audio-track duplication intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction.rename(INTENT_STAGING_NAME, transaction, INTENT_NAME).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "publish audio-track duplication intent",
            error,
        )
    })?;
    sync_dir(transaction)
}

fn transaction_id(directory_name: &str) -> Result<String, ProjectResourcePublicationError> {
    let id = directory_name
        .strip_prefix(AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track duplication transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication transaction identity is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication transaction identity is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn resource_parent(path: &str) -> Result<&str, ProjectResourcePublicationError> {
    path.rsplit_once('/').map(|(parent, _)| parent).ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-track duplication resource path has no parent",
        )
    })
}

fn prepare_entry_parent(
    root: &Dir,
    entry: &DuplicationTransactionEntry,
) -> Result<Dir, ProjectResourcePublicationError> {
    let parent = resource_parent(&entry.resource.path)?;
    match root.open_dir_nofollow(parent) {
        Ok(directory) => {
            if entry.created_parent_directory {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio-track duplication resource parent appeared after transaction staging",
                ));
            }
            Ok(directory)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && entry.created_parent_directory => {
            open_or_create_dir(root, parent)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "audio-track duplication replacement parent disappeared",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "open audio-track duplication resource parent",
            error,
        )),
    }
}

fn reject_other_transactions(backups: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let unhandled = find_unhandled_project_transaction(
        backups,
        &[ProjectTransactionFamily::AudioTrackDuplication],
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "inspect project transactions before audio-track duplication",
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

fn recover_after_publication_error(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: Dir,
    transaction_name: &str,
    operation_error: ProjectResourcePublicationError,
) -> Result<AudioTrackDuplicationPublicationOutcome, ProjectResourcePublicationError> {
    let tx = match read_duplication_transaction(&transaction, transaction_name) {
        Ok(tx) => tx,
        Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
            validate_duplication_prepublication_residue(&transaction, transaction_name)?;
            clear_and_remove_audio_track_duplication_transaction(backups, transaction, transaction_name)?;
            return Err(operation_error);
        }
        Err(source) => {
            return Err(ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message: "audio-track duplication failed and its durable transaction could not be read".to_owned(),
                published: false,
                source: Some(Box::new(source)),
            });
        }
    };
    match recover_audio_track_duplication_transaction(root_path, root, backups, &transaction, &tx) {
        Ok(AudioTrackDuplicationPublicationRecoveryDisposition::RolledBack) => {
            clear_and_remove_audio_track_duplication_transaction(backups, transaction, transaction_name)?;
            Err(operation_error)
        }
        Ok(AudioTrackDuplicationPublicationRecoveryDisposition::Committed) => {
            let outcome = duplication_outcome(&tx)?;
            clear_and_remove_audio_track_duplication_transaction(backups, transaction, transaction_name)
                .map_err(|error| ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "audio-track duplication was published but transaction cleanup is incomplete".to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                })?;
            Ok(outcome)
        }
        Err(source) => Err(ProjectResourcePublicationError {
            kind: ProjectResourcePublicationErrorKind::Recovery,
            message: if source.published() {
                "audio-track duplication was published but verification or transaction recovery is incomplete"
            } else {
                "audio-track duplication and recovery both failed"
            }
            .to_owned(),
            published: source.published(),
            source: Some(Box::new(source)),
        }),
    }
}

fn recover_audio_track_duplication_transactions_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<AudioTrackDuplicationPublicationRecovery>, ProjectResourcePublicationError> {
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
        if name.starts_with(AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX) {
            names.push(name);
        }
    }
    names.sort();
    let mut outcomes = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open audio-track duplication recovery transaction",
                error,
            )
        })?;
        let tx = match read_duplication_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_duplication_prepublication_residue(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove_audio_track_duplication_transaction(backups, transaction, &name)?;
                outcomes.push(AudioTrackDuplicationPublicationRecovery {
                    transaction_id: id,
                    disposition: AudioTrackDuplicationPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition = recover_audio_track_duplication_transaction(
            root_path,
            root,
            backups,
            &transaction,
            &tx,
        )?;
        clear_and_remove_audio_track_duplication_transaction(backups, transaction, &name)?;
        outcomes.push(AudioTrackDuplicationPublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(outcomes)
}

#[allow(
    clippy::too_many_lines,
    reason = "the retained journal must be fully authenticated before recovery can mutate any active project path"
)]
fn recover_audio_track_duplication_transaction(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &DuplicationTransactionRecord,
) -> Result<AudioTrackDuplicationPublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_duplication_transaction(transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES).map(|bytes| digest(&bytes))?;
    if active == after {
        verify_committed(root_path, root, after).map_err(|source| ProjectResourcePublicationError {
            kind: ProjectResourcePublicationErrorKind::Recovery,
            message: "committed audio-track duplication could not be verified".to_owned(),
            published: true,
            source: Some(Box::new(source)),
        })?;
        cleanup_duplication_temporaries(root, backups, tx)?;
        return Ok(AudioTrackDuplicationPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication recovery found an unrelated project identity",
        ));
    }
    validate_duplication_rollback_authority(root, tx)?;
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Some(previous) = &entry.previous_resource {
            let parent = open_relative_parent(root, &entry.resource.path)?;
            let current = read_optional_file(&parent, file_name(&entry.resource.path)?)?;
            let previous_digest = decode_digest(&previous.sha256)?;
            if current
                .as_deref()
                .is_some_and(|bytes| digest(bytes) == previous_digest)
            {
                continue;
            }
            publish_staged_file(
                transaction,
                &previous_staged_name(index),
                &parent,
                &parent,
                file_name(&previous.path)?,
                &format!(".audio-track-duplication-{index}-rollback-{}.json", tx.id),
                "previous audio-track duplication resource",
            )?;
        } else {
            remove_added_entry(root, entry)?;
        }
    }
    remove_created_parents(root, tx)?;
    verify_retained_project_candidate(root, &read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "rolled-back audio-track duplication project does not verify",
                error,
            )
        })?;
    cleanup_duplication_temporaries(root, backups, tx)?;
    Ok(AudioTrackDuplicationPublicationRecoveryDisposition::RolledBack)
}

fn validate_duplication_rollback_authority(
    root: &Dir,
    tx: &DuplicationTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    for entry in &tx.entries {
        let parent = match open_relative_parent(root, &entry.resource.path) {
            Ok(parent) => parent,
            Err(_) if entry.previous_resource.is_none() && entry.created_parent_directory => continue,
            Err(error) => return Err(error),
        };
        let current = read_optional_file(&parent, file_name(&entry.resource.path)?)?;
        let after = decode_digest(&entry.resource.sha256)?;
        match (&entry.previous_resource, current) {
            (Some(previous), Some(bytes)) => {
                let before = decode_digest(&previous.sha256)?;
                if digest(&bytes) != before && digest(&bytes) != after {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "audio-track duplication rollback refuses unrelated replacement content",
                    ));
                }
            }
            (Some(_), None) => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "audio-track duplication rollback is missing a replacement resource",
                ));
            }
            (None, Some(bytes)) if digest(&bytes) != after => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "audio-track duplication rollback refuses unrelated added content",
                ));
            }
            (None, _) => {}
        }
    }
    Ok(())
}

fn remove_added_entry(
    root: &Dir,
    entry: &DuplicationTransactionEntry,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = match open_relative_parent(root, &entry.resource.path) {
        Ok(parent) => parent,
        Err(_) if entry.created_parent_directory => return Ok(()),
        Err(error) => return Err(error),
    };
    let name = file_name(&entry.resource.path)?;
    let Some(bytes) = read_optional_file(&parent, name)? else {
        return Ok(());
    };
    if digest(&bytes) != decode_digest(&entry.resource.sha256)? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication rollback refuses unrelated added content",
        ));
    }
    parent.remove_file(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove rolled-back audio-track duplication resource",
            error,
        )
    })?;
    sync_dir(&parent)
}

fn remove_created_parents(
    root: &Dir,
    tx: &DuplicationTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let mut parents = BTreeSet::new();
    for entry in &tx.entries {
        if entry.created_parent_directory {
            parents.insert(resource_parent(&entry.resource.path)?.to_owned());
        }
    }
    for parent in parents {
        let metadata = match root.symlink_metadata(&parent) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "inspect created audio-track duplication resource parent",
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "created audio-track duplication resource parent is not a real directory",
            ));
        }
        root.remove_dir(&parent).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "remove rolled-back audio-track duplication resource parent",
                error,
            )
        })?;
        sync_dir(root)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "all journal evidence and its exact candidate manifest must be authenticated before recovery"
)]
fn validate_staged_duplication_transaction(
    transaction: &Dir,
    tx: &DuplicationTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    let previous_project = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate_project = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate_project) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication staged project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous audio-track duplication project is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate_project).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate audio-track duplication project is invalid",
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
            "audio-track duplication project revisions are inconsistent",
        ));
    }
    let mut staged = Vec::new();
    staged.try_reserve_exact(tx.entries.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "allocate staged audio-track duplication entries",
        )
    })?;
    for (index, entry) in tx.entries.iter().enumerate() {
        let role = role_from_name(&entry.role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track duplication transaction has an unsupported resource role",
            )
        })?;
        let bytes = read_bounded(transaction, &staged_name(index), MAX_JSON_BYTES)?;
        validate_document(&entry.id, &bytes, role).map_err(|error| ProjectResourcePublicationError {
            kind: ProjectResourcePublicationErrorKind::Recovery,
            message: "staged audio-track duplication resource is invalid".to_owned(),
            published: false,
            source: Some(Box::new(error)),
        })?;
        if bytes.len() as u64 != entry.resource.byte_length
            || digest(&bytes) != decode_digest(&entry.resource.sha256)?
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "staged audio-track duplication resource does not match the transaction",
            ));
        }
        let previous = match &entry.previous_resource {
            Some(previous) => {
                let bytes = read_bounded(transaction, &previous_staged_name(index), MAX_JSON_BYTES)?;
                validate_document(&entry.id, &bytes, role).map_err(|error| {
                    ProjectResourcePublicationError {
                        kind: ProjectResourcePublicationErrorKind::Recovery,
                        message: "staged previous audio-track duplication resource is invalid".to_owned(),
                        published: false,
                        source: Some(Box::new(error)),
                    }
                })?;
                if previous.path != entry.resource.path
                    || bytes.len() as u64 != previous.byte_length
                    || digest(&bytes) != decode_digest(&previous.sha256)?
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "staged previous audio-track duplication resource does not match the transaction",
                    ));
                }
                Some(bytes)
            }
            None => None,
        };
        staged.push(StagedEntry { bytes, previous });
    }
    let mut entries = Vec::new();
    entries.try_reserve_exact(tx.entries.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "allocate staged audio-track duplication candidate entries",
        )
    })?;
    for (entry, staged) in tx.entries.iter().zip(&staged) {
        let role = role_from_name(&entry.role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track duplication transaction has an unsupported resource role",
            )
        })?;
        validate_manifest_entry(
            &previous_manifest,
            &candidate_manifest,
            entry,
            role,
            staged.previous.as_deref(),
            &staged.bytes,
        )?;
        entries.push(ActiveEntry {
            id: &entry.id,
            role,
            path: entry.resource.path.clone(),
            bytes: &staged.bytes,
            previous: staged.previous.clone(),
            created_parent_directory: entry.created_parent_directory,
        });
    }
    let expected = candidate_duplication_manifest(&previous_project, &entries, tx.after_project_revision)?;
    if expected != candidate_project {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication candidate manifest contains unrelated changes",
        ));
    }
    Ok(())
}

fn validate_manifest_entry(
    previous: &ProjectManifest,
    candidate: &ProjectManifest,
    entry: &DuplicationTransactionEntry,
    role: ResourceRole,
    previous_bytes: Option<&[u8]>,
    bytes: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let before = previous
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == entry.id);
    let after = candidate
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == entry.id)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "audio-track duplication candidate descriptor is absent",
            )
        })?;
    if after.role() != role
        || after.path() != entry.resource.path
        || after.byte_length() != bytes.len() as u64
        || after.sha256() != hex_digest(digest(bytes))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication candidate descriptor differs from its resource",
        ));
    }
    match (before, previous_bytes, &entry.previous_resource) {
        (None, None, None) => Ok(()),
        (Some(before), Some(bytes), Some(file))
            if before.role() == role
                && before.path() == entry.resource.path
                && before.byte_length() == bytes.len() as u64
                && before.sha256() == hex_digest(digest(bytes))
                && file.path == entry.resource.path => Ok(()),
        _ => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication transaction does not match the previous manifest",
        )),
    }
}

fn read_duplication_transaction(
    transaction: &Dir,
    directory_name: &str,
) -> Result<DuplicationTransactionRecord, ProjectResourcePublicationError> {
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "audio-track duplication transaction intent")?;
    let intent: DuplicationTransactionIntent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-track duplication transaction intent",
            error,
        )
    })?;
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "audio-track duplication transaction")?;
    let tx: DuplicationTransactionRecord = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse audio-track duplication transaction",
            error,
        )
    })?;
    let id = transaction_id(directory_name)?;
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let entries_valid = tx.entries.iter().all(|entry| {
        let role = role_from_name(&entry.role);
        let resource_valid = entry.resource.byte_length > 0
            && entry.resource.byte_length <= MAX_JSON_FILE_BYTES
            && decode_digest(&entry.resource.sha256).is_ok();
        let previous_valid = entry.previous_resource.as_ref().is_none_or(|previous| {
            previous.path == entry.resource.path
                && previous.byte_length > 0
                && previous.byte_length <= MAX_JSON_FILE_BYTES
                && decode_digest(&previous.sha256).is_ok()
        });
        role.is_some()
            && is_valid_entity_id(&entry.id)
            && ids.insert(entry.id.as_str())
            && paths.insert(entry.resource.path.as_str())
            && resource_valid
            && previous_valid
            && (!entry.created_parent_directory || entry.previous_resource.is_none())
    });
    let duplicate_track_is_addition = tx.entries.iter().any(|entry| {
        entry.id == tx.duplicated_track_id
            && entry.role == "track"
            && entry.previous_resource.is_none()
    });
    if intent.format != "hyper-audio-track-duplication-intent"
        || intent.format_version != 1
        || !intent.extensions.is_empty()
        || intent.id != id
        || intent.entry_count != tx.entries.len()
        || tx.format != "hyper-audio-track-duplication-transaction"
        || tx.format_version != 1
        || !tx.extensions.is_empty()
        || tx.id != id
        || !is_valid_entity_id(&tx.duplicated_track_id)
        || tx.entries.is_empty()
        || tx.entries.len() > MAX_ENTRIES
        || !entries_valid
        || !duplicate_track_is_addition
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication transaction is invalid",
        ));
    }
    Ok(tx)
}

fn validate_duplication_prepublication_residue(
    transaction: &Dir,
    transaction_name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let id = transaction_id(transaction_name)?;
    let count = if missing_file(transaction, INTENT_NAME)? {
        0
    } else {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "incomplete audio-track duplication transaction intent")?;
        let intent: DuplicationTransactionIntent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse incomplete audio-track duplication transaction intent",
                error,
            )
        })?;
        if intent.format != "hyper-audio-track-duplication-intent"
            || intent.format_version != 1
            || !intent.extensions.is_empty()
            || intent.id != id
            || intent.entry_count == 0
            || intent.entry_count > MAX_ENTRIES
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-track duplication transaction intent is invalid",
            ));
        }
        intent.entry_count
    };
    let mut expected = BTreeSet::new();
    expected.insert(INTENT_STAGING_NAME.to_owned());
    expected.insert(INTENT_NAME.to_owned());
    expected.insert(PROJECT_STAGING_NAME.to_owned());
    expected.insert(PREVIOUS_PROJECT_NAME.to_owned());
    for index in 0..count {
        expected.insert(staged_name(index));
        expected.insert(previous_staged_name(index));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete audio-track duplication transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete audio-track duplication transaction entry",
                error,
            )
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !expected.contains(&name) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete audio-track duplication transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn cleanup_duplication_temporaries(
    root: &Dir,
    backups: &Dir,
    tx: &DuplicationTransactionRecord,
) -> Result<(), ProjectResourcePublicationError> {
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Ok(parent) = open_relative_parent(root, &entry.resource.path) {
            remove_optional_matching_file(
                &parent,
                &format!(".audio-track-duplication-{index}-{}.json", tx.id),
                decode_digest(&entry.resource.sha256)?,
            )?;
            if let Some(previous) = &entry.previous_resource {
                remove_optional_matching_file(
                    &parent,
                    &format!(".audio-track-duplication-{index}-rollback-{}.json", tx.id),
                    decode_digest(&previous.sha256)?,
                )?;
            }
        }
    }
    remove_optional_matching_file(
        backups,
        &format!(".audio-track-duplication-project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )?;
    Ok(())
}

fn duplication_outcome(
    tx: &DuplicationTransactionRecord,
) -> Result<AudioTrackDuplicationPublicationOutcome, ProjectResourcePublicationError> {
    Ok(AudioTrackDuplicationPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.duplicated_track_id.clone(),
    })
}

#[cfg(not(windows))]
fn clear_and_remove_audio_track_duplication_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".audio-track-duplication-cleanup-{}", Uuid::new_v4());
    backups.rename(name, backups, &quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "quarantine audio-track duplication transaction",
            error,
        )
    })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined audio-track duplication transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined audio-track duplication transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "audio-track duplication transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_duplication_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-track duplication transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(windows)]
fn clear_and_remove_audio_track_duplication_transaction(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    clear_duplication_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove audio-track duplication transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

fn clear_duplication_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let mut names = Vec::new();
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read audio-track duplication transaction for cleanup",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read audio-track duplication transaction cleanup entry",
                error,
            )
        })?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    for name in names {
        transaction.remove_file(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "remove audio-track duplication transaction file",
                error,
            )
        })?;
    }
    sync_dir(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verify_project_package;
    use hyper_native_device::{DeviceChain, NativeCompressorState, NativeDeviceInstance};
    use hyper_playback_json::{
        AudioTrackDuplicationSource, LoaderLimits, MediaAssetFact, PlaybackJsonInput, load,
        serialize_audio_track_duplication,
    };
    use serde_json::json;
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337001";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337002";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337003";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337004";
    const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337005";
    const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337006";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337007";
    const PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337008";
    const AUTOMATION_TAKE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337009";
    const AUTOMATION_LANE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337010";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337011";
    const DEVICE_CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337012";
    const DEVICE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337013";
    const DUPLICATED_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337101";
    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: std::path::PathBuf,
        parent: std::path::PathBuf,
        timeline: Vec<u8>,
        track: Vec<u8>,
        mixer: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-audio-track-duplication-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            let root = parent.join("Session");
            fs::create_dir_all(root.join("timeline")).unwrap();
            fs::create_dir_all(root.join("tracks")).unwrap();
            fs::create_dir_all(root.join("mixer")).unwrap();
            let timeline = canonical(json!({
                "format":"hyper-timeline","format_version":1,"id":TIMELINE_ID,
                "project_id":PROJECT_ID,"ticks_per_quarter":1920,
                "tempo_events":[{"tick":0,"microseconds_per_quarter":1_000_000}],
                "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
            }));
            let track = canonical(json!({
                "format":"hyper-track","format_version":1,"id":TRACK_ID,
                "project_id":PROJECT_ID,"name":"Lead","kind":"audio","order":0,
                "active":true,"clip_ids":[],"extensions":{}
            }));
            let mixer = canonical(json!({
                "format":"hyper-mixer","format_version":1,"id":MIXER_ID,"project_id":PROJECT_ID,
                "channels":[{"track_id":TRACK_ID,"gain_millionths":1_000_000,"pan_millionths":0,
                    "mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}],
                "return_buses":[],"main":{"gain_millionths":1_000_000,"mute":false},
                "cue":{"source":"main","gain_millionths":1_000_000,"mute":false},"extensions":{}
            }));
            fs::write(root.join("timeline/timeline.json"), &timeline).unwrap();
            fs::write(root.join("tracks/lead.json"), &track).unwrap();
            fs::write(root.join("mixer/main.json"), &mixer).unwrap();
            let manifest = canonical(json!({
                "format":"hyper-project","format_version":1,"project_id":PROJECT_ID,"revision":1,
                "name":"Duplication fixture","sample_rate_hz":48_000,
                "required_capabilities":{
                    "format":"hyper-hardware-capabilities","format_version":1,
                    "audio_inputs":0,"audio_outputs":0,"cv_inputs":0,"cv_outputs":0,
                    "gate_outputs":0,"has_clock_input":false,"has_reset_input":false,
                    "has_clock_output":false,"usb_mode":"none","has_ethernet":false,"extensions":{}
                },
                "resources":[
                    descriptor(TIMELINE_ID,"timeline","timeline/timeline.json",&timeline),
                    descriptor(TRACK_ID,"track","tracks/lead.json",&track),
                    descriptor(MIXER_ID,"mixer","mixer/main.json",&mixer)
                ],"assets":[],"extensions":{}
            }));
            fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
            Self { root, parent, timeline, track, mixer }
        }

        fn documents(&self) -> ([u8; 32], AudioTrackDuplicationDocuments) {
            let package = verify_project_package(&self.root).unwrap();
            let tracks = [self.track.as_slice()];
            let project = load(
                PlaybackJsonInput {
                    project_id: PROJECT_ID,
                    sample_rate_hz: 48_000,
                    timeline: &self.timeline,
                    tracks: &tracks,
                    clips: &[],
                    scenes: &[],
                    arrangement: None,
                },
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            let documents = serialize_audio_track_duplication(
                &project,
                &[],
                AudioTrackDuplicationSource {
                    track: &self.track,
                    clips: &[],
                    scenes: &[],
                    arrangement: None,
                    mixer: &self.mixer,
                    automation_take_ids: &[],
                    automation_takes: &[],
                    device_chain_id: None,
                    device_chain: None,
                },
                TRACK_ID,
                "Lead copy",
                LoaderLimits::VERSION_1,
                || DUPLICATED_TRACK_ID.to_owned(),
            )
            .unwrap();
            (*package.manifest_sha256(), documents)
        }

        #[allow(
            clippy::too_many_lines,
            reason = "one fixture creates the complete mutable graph needed to exercise durable batch publication"
        )]
        fn complete_documents(&self) -> ([u8; 32], AudioTrackDuplicationDocuments) {
            let track = canonical(json!({
                "format":"hyper-track","format_version":1,"id":TRACK_ID,
                "project_id":PROJECT_ID,"name":"Lead","kind":"audio","order":0,
                "active":true,"clip_ids":[CLIP_ID],"extensions":{}
            }));
            let clip = canonical(json!({
                "format":"hyper-audio-clip","format_version":1,"id":CLIP_ID,
                "project_id":PROJECT_ID,"track_id":TRACK_ID,"name":"Lead clip","enabled":true,
                "duration_ticks":960,
                "source":{"asset_id":ASSET_ID,"start_frame":0,"frame_count":24_000},
                "loop":{"enabled":false,"start_tick":0,"end_tick":960},
                "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
                "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
                "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":960,"source_frame_offset":24_000}],
                "extensions":{}
            }));
            let scene = canonical(json!({
                "format":"hyper-scene","format_version":1,"id":SCENE_ID,
                "project_id":PROJECT_ID,"name":"Verse","order":0,"enabled":true,
                "launch_quantization":{"mode":"immediate"},
                "slots":[{"track_id":TRACK_ID,"action":"play","clip_id":CLIP_ID}],"extensions":{}
            }));
            let arrangement = canonical(json!({
                "format":"hyper-arrangement","format_version":1,"id":ARRANGEMENT_ID,
                "project_id":PROJECT_ID,"name":"Main","enabled":true,
                "placements":[{"id":PLACEMENT_ID,"track_id":TRACK_ID,"clip_id":CLIP_ID,
                    "start_tick":0,"clip_start_tick":0,"duration_ticks":960,"enabled":true}],
                "scene_launches":[],"extensions":{}
            }));
            let automation = canonical(json!({
                "format":"hyper-automation-take","format_version":1,"id":AUTOMATION_TAKE_ID,
                "project_id":PROJECT_ID,"name":"Lead automation","enabled":true,
                "captured_at_utc":"2026-08-23T12:00:00.000Z","start_tick":0,"end_tick":960,
                "lanes":[{"id":AUTOMATION_LANE_ID,
                    "target":{"kind":"mixer-channel","track_id":TRACK_ID,"parameter":"gain"},
                    "interpolation":"linear","points":[{"tick":0,"value":1_000_000},{"tick":959,"value":750_000}]}],
                "extensions":{}
            }));
            let chain = DeviceChain::try_new(
                DEVICE_CHAIN_ID.to_owned(),
                PROJECT_ID.to_owned(),
                TRACK_ID.to_owned(),
                vec![
                    NativeDeviceInstance::try_new_compressor(
                        DEVICE_ID.to_owned(),
                        true,
                        NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 100_000, 0)
                            .unwrap(),
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
            .to_canonical_vec()
            .unwrap();
            let asset = vec![0_u8; 64];
            fs::create_dir_all(self.root.join("clips")).unwrap();
            fs::create_dir_all(self.root.join("scenes")).unwrap();
            fs::create_dir_all(self.root.join("arrangement")).unwrap();
            fs::create_dir_all(self.root.join("takes")).unwrap();
            fs::create_dir_all(self.root.join("devices")).unwrap();
            fs::create_dir_all(self.root.join("assets/recordings")).unwrap();
            fs::write(self.root.join("tracks/lead.json"), &track).unwrap();
            fs::write(self.root.join("clips/lead.json"), &clip).unwrap();
            fs::write(self.root.join("scenes/verse.json"), &scene).unwrap();
            fs::write(self.root.join("arrangement/main.json"), &arrangement).unwrap();
            fs::write(self.root.join("takes/lead.json"), &automation).unwrap();
            fs::write(self.root.join("devices/lead.json"), &chain).unwrap();
            fs::write(self.root.join("assets/recordings/lead.wav"), &asset).unwrap();
            let manifest = canonical(json!({
                "format":"hyper-project","format_version":1,"project_id":PROJECT_ID,"revision":1,
                "name":"Complete duplication fixture","sample_rate_hz":48_000,
                "required_capabilities":{
                    "format":"hyper-hardware-capabilities","format_version":1,
                    "audio_inputs":0,"audio_outputs":0,"cv_inputs":0,"cv_outputs":0,
                    "gate_outputs":0,"has_clock_input":false,"has_reset_input":false,
                    "has_clock_output":false,"usb_mode":"none","has_ethernet":false,"extensions":{}
                },
                "resources":[
                    descriptor(TIMELINE_ID,"timeline","timeline/timeline.json",&self.timeline),
                    descriptor(TRACK_ID,"track","tracks/lead.json",&track),
                    descriptor(CLIP_ID,"clip","clips/lead.json",&clip),
                    descriptor(SCENE_ID,"scene","scenes/verse.json",&scene),
                    descriptor(ARRANGEMENT_ID,"arrangement","arrangement/main.json",&arrangement),
                    descriptor(MIXER_ID,"mixer","mixer/main.json",&self.mixer),
                    descriptor(AUTOMATION_TAKE_ID,"automation-take","takes/lead.json",&automation),
                    descriptor(DEVICE_CHAIN_ID,"device-chain","devices/lead.json",&chain)
                ],
                "assets":[{"id":ASSET_ID,"role":"recording","path":"assets/recordings/lead.wav",
                    "media_type":"audio/wav","byte_length":asset.len(),"sha256":hex_digest(digest(&asset)),"extensions":{}}],
                "extensions":{}
            }));
            fs::write(self.root.join(MANIFEST_NAME), manifest).unwrap();
            let package = verify_project_package(&self.root).unwrap();
            let tracks = [track.as_slice()];
            let clips = [clip.as_slice()];
            let scenes = [scene.as_slice()];
            let project = load(
                PlaybackJsonInput {
                    project_id: PROJECT_ID,
                    sample_rate_hz: 48_000,
                    timeline: &self.timeline,
                    tracks: &tracks,
                    clips: &clips,
                    scenes: &scenes,
                    arrangement: Some(&arrangement),
                },
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            let assets = [MediaAssetFact {
                id: ASSET_ID,
                sample_rate_hz: 48_000,
                frame_count: 24_000,
            }];
            let identities = [
                DUPLICATED_TRACK_ID,
                "018f47a2-6b2d-7c11-8a44-001122337102",
                "018f47a2-6b2d-7c11-8a44-001122337103",
                "018f47a2-6b2d-7c11-8a44-001122337104",
                "018f47a2-6b2d-7c11-8a44-001122337105",
                "018f47a2-6b2d-7c11-8a44-001122337106",
            ];
            let mut identities = identities.into_iter();
            let documents = serialize_audio_track_duplication(
                &project,
                &assets,
                AudioTrackDuplicationSource {
                    track: &track,
                    clips: &[(CLIP_ID, clip.as_slice())],
                    scenes: &[(SCENE_ID, scene.as_slice())],
                    arrangement: Some(&arrangement),
                    mixer: &self.mixer,
                    automation_take_ids: &[AUTOMATION_TAKE_ID],
                    automation_takes: &[(AUTOMATION_TAKE_ID, automation.as_slice())],
                    device_chain_id: Some(DEVICE_CHAIN_ID),
                    device_chain: Some(&chain),
                },
                TRACK_ID,
                "Lead copy",
                LoaderLimits::VERSION_1,
                || identities.next().unwrap().to_owned(),
            )
            .unwrap();
            (*package.manifest_sha256(), documents)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test fixtures deliberately consume short JSON builders before canonicalization"
    )]
    fn canonical(value: serde_json::Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&value).unwrap()
    }

    fn descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> serde_json::Value {
        json!({
            "id":id,"role":role,"path":path,"media_type":"application/json",
            "byte_length":bytes.len(),"sha256":hex_digest(digest(bytes)),"extensions":{}
        })
    }

    #[test]
    fn publishes_the_complete_minimal_track_duplication_as_one_revision() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.documents();
        let outcome = publish_audio_track_duplication(
            &fixture.root,
            AudioTrackDuplicationPublicationRequest {
                expected_project_sha256: identity,
                mixer_id: MIXER_ID,
                documents: &documents,
            },
        )
        .unwrap();
        assert_eq!(outcome.track_id(), DUPLICATED_TRACK_ID);
        assert_eq!(outcome.project_revision(), 2);
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(
            package.read_resource(DUPLICATED_TRACK_ID).unwrap(),
            documents.track().bytes()
        );
        assert_eq!(
            package.read_resource(MIXER_ID).unwrap(),
            documents.mixer().bytes()
        );
    }

    #[test]
    fn persists_the_complete_graph_and_current_device_schema_together() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.complete_documents();
        let outcome = publish_audio_track_duplication(
            &fixture.root,
            AudioTrackDuplicationPublicationRequest {
                expected_project_sha256: identity,
                mixer_id: MIXER_ID,
                documents: &documents,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        assert_eq!(
            package.read_resource(documents.track().id()).unwrap(),
            documents.track().bytes()
        );
        assert_eq!(
            package.read_resource(documents.clips()[0].id()).unwrap(),
            documents.clips()[0].bytes()
        );
        assert_eq!(
            package.read_resource(documents.scenes()[0].id()).unwrap(),
            documents.scenes()[0].bytes()
        );
        assert_eq!(
            package.read_resource(documents.arrangement().unwrap().id()).unwrap(),
            documents.arrangement().unwrap().bytes()
        );
        assert_eq!(
            package.read_resource(documents.automation_takes()[0].id()).unwrap(),
            documents.automation_takes()[0].bytes()
        );
        let device = documents.device_chain().unwrap();
        assert_eq!(package.read_resource(device.id()).unwrap(), device.bytes());
        assert!(fixture.root.join("devices").is_dir());
        assert_eq!(DeviceChain::parse(device.bytes()).unwrap().format_version(), 2);
    }

    #[test]
    fn accepts_the_current_version_two_device_chain_document() {
        let chain_id = "018f47a2-6b2d-7c11-8a44-001122337201";
        let device_id = "018f47a2-6b2d-7c11-8a44-001122337202";
        let chain = DeviceChain::try_new(
            chain_id.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![
                NativeDeviceInstance::try_new_compressor(
                    device_id.to_owned(),
                    true,
                    NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 100_000, 0)
                        .unwrap(),
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let bytes = chain.to_canonical_vec().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["format_version"],
            Value::from(2),
            "a compressor chain uses the current v2 document envelope"
        );
        assert!(validate_document(chain_id, &bytes, ResourceRole::DeviceChain).is_ok());
        assert!(validate_document(TRACK_ID, &bytes, ResourceRole::DeviceChain).is_err());
    }

    #[test]
    fn crash_recovery_keeps_one_complete_duplication_identity() {
        for phase in [
            AudioTrackDuplicationPublicationPhase::Transaction,
            AudioTrackDuplicationPublicationPhase::Resource(0),
            AudioTrackDuplicationPublicationPhase::Resource(1),
            AudioTrackDuplicationPublicationPhase::Manifest,
        ] {
            let fixture = Fixture::new();
            let (identity, documents) = fixture.documents();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_audio_track_duplication_observed(
                    &fixture.root,
                    AudioTrackDuplicationPublicationRequest {
                        expected_project_sha256: identity,
                        mixer_id: MIXER_ID,
                        documents: &documents,
                    },
                    |current, _| {
                        assert_ne!(current, phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let recovered = crate::recover_project_transactions(&fixture.root).unwrap();
            assert_eq!(recovered.audio_track_duplications().len(), 1);
            let expected = if phase == AudioTrackDuplicationPublicationPhase::Manifest {
                AudioTrackDuplicationPublicationRecoveryDisposition::Committed
            } else {
                AudioTrackDuplicationPublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(recovered.audio_track_duplications()[0].disposition(), expected);
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackDuplicationPublicationRecoveryDisposition::Committed {
                assert_eq!(package.manifest().revision(), 2);
                assert_eq!(
                    package.read_resource(DUPLICATED_TRACK_ID).unwrap(),
                    documents.track().bytes()
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert!(package
                    .manifest()
                    .resources()
                    .iter()
                    .all(|resource| resource.id() != DUPLICATED_TRACK_ID));
            }
        }
    }

    #[test]
    fn complete_graph_recovery_keeps_every_resource_or_exact_prior_tree() {
        for phase in [
            AudioTrackDuplicationPublicationPhase::Resource(3),
            AudioTrackDuplicationPublicationPhase::Manifest,
        ] {
            let fixture = Fixture::new();
            let (identity, documents) = fixture.complete_documents();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_audio_track_duplication_observed(
                    &fixture.root,
                    AudioTrackDuplicationPublicationRequest {
                        expected_project_sha256: identity,
                        mixer_id: MIXER_ID,
                        documents: &documents,
                    },
                    |current, _| {
                        assert_ne!(current, phase, "simulated process crash");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let recovered = crate::recover_project_transactions(&fixture.root).unwrap();
            let expected = if phase == AudioTrackDuplicationPublicationPhase::Manifest {
                AudioTrackDuplicationPublicationRecoveryDisposition::Committed
            } else {
                AudioTrackDuplicationPublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(recovered.audio_track_duplications().len(), 1);
            assert_eq!(recovered.audio_track_duplications()[0].disposition(), expected);
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackDuplicationPublicationRecoveryDisposition::Committed {
                assert_eq!(package.manifest().revision(), 2);
                assert_eq!(
                    package.read_resource(documents.clips()[0].id()).unwrap(),
                    documents.clips()[0].bytes()
                );
                assert_eq!(
                    package
                        .read_resource(documents.automation_takes()[0].id())
                        .unwrap(),
                    documents.automation_takes()[0].bytes()
                );
                assert_eq!(
                    package
                        .read_resource(documents.device_chain().unwrap().id())
                        .unwrap(),
                    documents.device_chain().unwrap().bytes()
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert!(fixture.root.join("devices").is_dir());
                assert_eq!(
                    package.read_resource(DEVICE_CHAIN_ID).unwrap(),
                    fs::read(fixture.root.join("devices/lead.json")).unwrap()
                );
                assert!(package
                    .manifest()
                    .resources()
                    .iter()
                    .all(|resource| resource.id() != documents.track().id()));
                assert_eq!(
                    package.read_resource(ARRANGEMENT_ID).unwrap(),
                    fs::read(fixture.root.join("arrangement/main.json")).unwrap()
                );
                assert_eq!(
                    package.read_resource(AUTOMATION_TAKE_ID).unwrap(),
                    fs::read(fixture.root.join("takes/lead.json")).unwrap()
                );
            }
        }
    }
}
