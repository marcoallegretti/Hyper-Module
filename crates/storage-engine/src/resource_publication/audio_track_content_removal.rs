//! Durable publication for complete audio-track content removal.
//!
//! The document boundary supplies the complete, compiled delta. This module
//! owns the separate durable concern: every removed resource is retained in a
//! journal-backed transaction, replacements publish before the manifest, and
//! the manifest remains the single commit record.

use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_ENTRIES, MAX_JSON_BYTES, MAX_SAFE_INTEGER,
    PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME, ProjectResourcePublicationError,
    ProjectResourcePublicationErrorKind, StructurallyVerifiedProject, TRANSACTION_NAME, caused,
    decode_digest, digest, failure, file_name, hex_digest, missing_file, open_optional_dir,
    open_or_create_dir, open_relative_parent, open_stable_project_root, publish_staged_file,
    read_bounded, read_optional_file, remove_optional_matching_file, require_canonical, sync_dir,
    validate_canonical_resource, verify_committed, verify_project_candidate,
    verify_project_package, verify_retained_project_candidate, write_exclusive_synced,
};
use crate::project_lock::{
    AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX, ProjectLock, ProjectTransactionFamily,
    find_unhandled_project_transaction,
};
use crate::stable_file::verify_open_stable;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_fs_authority::{identity, rename_no_replace};
use hyper_native_device::DeviceChain;
use hyper_playback_json::{AudioTrackContentRemovalDocument, AudioTrackContentRemovalDocuments};
use hyper_project_model::{ProjectManifest, ResourceRole, is_valid_entity_id};
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io;
use std::path::Path;
use uuid::Uuid;

#[cfg(not(windows))]
use super::same_directory;

const FORMAT: &str = "hyper-audio-track-content-removal-transaction";
const INTENT_FORMAT: &str = "hyper-audio-track-content-removal-intent";
const VERSION: u8 = 1;

/// Complete, compiled document delta for one confirmed destructive track
/// deletion. The document type cannot be assembled from arbitrary external
/// bytes, but storage still rechecks all current project authority.
#[derive(Clone, Copy, Debug)]
pub struct AudioTrackContentRemovalPublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub documents: &'a AudioTrackContentRemovalDocuments,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackContentRemovalPublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

impl AudioTrackContentRemovalPublicationOutcome {
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
pub enum AudioTrackContentRemovalPublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackContentRemovalPublicationRecovery {
    transaction_id: String,
    disposition: AudioTrackContentRemovalPublicationRecoveryDisposition,
}

impl AudioTrackContentRemovalPublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }
    #[must_use]
    pub const fn disposition(&self) -> AudioTrackContentRemovalPublicationRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    ResourceMoved(usize),
    Resource(usize),
    Directory(usize),
    Manifest,
}

#[derive(Debug)]
struct Entry<'a> {
    id: String,
    role: ResourceRole,
    path: String,
    previous: Vec<u8>,
    replacement: Option<&'a [u8]>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    format: String,
    format_version: u8,
    id: String,
    entry_count: usize,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionEntry {
    id: String,
    role: String,
    previous: super::TransactionFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replacement: Option<super::TransactionFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backup_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Transaction {
    format: String,
    format_version: u8,
    id: String,
    before_project_sha256: String,
    after_project_sha256: String,
    before_project_revision: u64,
    after_project_revision: u64,
    removed_track_id: String,
    entries: Vec<TransactionEntry>,
    removed_directories: Vec<String>,
    extensions: serde_json::Map<String, Value>,
}

/// Publishes the complete destructive audio-track document delta as one
/// recoverable, manifest-last transaction.
///
/// # Errors
///
/// Refuses stale, incomplete, malformed, or noncanonical input. On an I/O
/// failure after staging, the transaction is recovered before this function
/// returns; a committed result always has a verified candidate manifest.
pub fn publish_audio_track_content_removal(
    project_path: impl AsRef<Path>,
    request: AudioTrackContentRemovalPublicationRequest<'_>,
) -> Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError> {
    publish_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every interrupted complete audio-track content removal.
///
/// # Errors
///
/// Refuses malformed journals, ambiguous active/backup resources, or a
/// manifest that cannot be proven to be one side of the transaction.
pub fn recover_audio_track_content_removals(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackContentRemovalPublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for track-content removal recovery",
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
    recover_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "the transaction authority and recovery handoff remain visible together"
)]
fn publish_observed(
    project_path: &Path,
    request: AudioTrackContentRemovalPublicationRequest<'_>,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError> {
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before track-content removal",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after track-content removal preparation",
        ));
    }
    let entries = resolve_entries(&verified, request.documents)?;
    let removed_track_id = request.documents.track().id().to_owned();
    let authorities = resource_authorities(&verified.root_dir, &entries)?;
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered = recover_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior track-content removal was recovered; reopen the current project revision",
        ));
    }
    reject_other_transactions(&backups)?;
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before track-content removal staging",
            error,
        )
    })?;
    let previous_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before track-content removal staging",
        ));
    }
    let next_revision = verified
        .manifest()
        .revision()
        .checked_add(1)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "project revision cannot be incremented",
            )
        })?;
    let candidate_project = candidate_manifest(&previous_project, &entries, next_revision)?;
    let name = format!(
        "{AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX}{}",
        Uuid::new_v4()
    );
    backups.create_dir(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create track-content removal transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-content removal transaction",
            error,
        )
    })?;
    let result = run_publication(
        &verified,
        &backups,
        &transaction,
        &name,
        &entries,
        &authorities,
        &removed_track_id,
        &previous_project,
        &candidate_project,
        next_revision,
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

fn resolve_entries<'a>(
    verified: &StructurallyVerifiedProject,
    documents: &'a AudioTrackContentRemovalDocuments,
) -> Result<Vec<Entry<'a>>, ProjectResourcePublicationError> {
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
                "allocate track-content removal entries",
            )
        })?;
    push_removal(
        verified,
        &mut entries,
        documents.track(),
        ResourceRole::Track,
    )?;
    for document in documents.clips() {
        push_removal(verified, &mut entries, document, ResourceRole::Clip)?;
    }
    push_replacement(
        verified,
        &mut entries,
        documents.mixer(),
        ResourceRole::Mixer,
    )?;
    if let Some(document) = documents.arrangement() {
        push_replacement(verified, &mut entries, document, ResourceRole::Arrangement)?;
    }
    for document in documents.scenes() {
        push_replacement(verified, &mut entries, document, ResourceRole::Scene)?;
    }
    for document in documents.automation_takes() {
        push_replacement(
            verified,
            &mut entries,
            document,
            ResourceRole::AutomationTake,
        )?;
    }
    if let Some(document) = documents.device_chain() {
        push_removal(verified, &mut entries, document, ResourceRole::DeviceChain)?;
    }
    if entries.is_empty() || entries.len() > MAX_ENTRIES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-content removal resource count is outside package limits",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in &entries {
        if !ids.insert(entry.id.as_str()) || !paths.insert(entry.path.as_str()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-content removal resource identities are not unique",
            ));
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn push_removal<'a>(
    verified: &StructurallyVerifiedProject,
    entries: &mut Vec<Entry<'a>>,
    document: &'a AudioTrackContentRemovalDocument,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    validate_document(document.id(), document.bytes(), role)?;
    let descriptor = descriptor(verified, document.id(), role)?;
    let previous = verified.read_resource(document.id()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read active track-content removal resource",
            error,
        )
    })?;
    if previous != document.bytes() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "removed track-content resource changed after preparation",
        ));
    }
    entries.push(Entry {
        id: document.id().to_owned(),
        role,
        path: descriptor.path().to_owned(),
        previous,
        replacement: None,
    });
    Ok(())
}

fn push_replacement<'a>(
    verified: &StructurallyVerifiedProject,
    entries: &mut Vec<Entry<'a>>,
    document: &'a AudioTrackContentRemovalDocument,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    validate_document(document.id(), document.bytes(), role)?;
    let descriptor = descriptor(verified, document.id(), role)?;
    let previous = verified.read_resource(document.id()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read active track-content replacement resource",
            error,
        )
    })?;
    entries.push(Entry {
        id: document.id().to_owned(),
        role,
        path: descriptor.path().to_owned(),
        previous,
        replacement: Some(document.bytes()),
    });
    Ok(())
}

fn descriptor<'a>(
    verified: &'a StructurallyVerifiedProject,
    id: &str,
    expected_role: ResourceRole,
) -> Result<&'a hyper_project_model::ResourceDescriptor, ProjectResourcePublicationError> {
    let descriptor = verified
        .manifest()
        .resources()
        .iter()
        .find(|candidate| candidate.id() == id)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "track-content removal resource is absent from the active project",
            )
        })?;
    if descriptor.role() != expected_role {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-content removal resource role differs from the active project",
        ));
    }
    Ok(descriptor)
}

fn validate_document(
    id: &str,
    bytes: &[u8],
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    if !is_valid_entity_id(id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-content removal resource identity is invalid",
        ));
    }
    validate_canonical_resource(bytes)?;
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse track-content removal resource",
            error,
        )
    })?;
    if value["format"].as_str() != Some(resource_format(role))
        || value["id"].as_str() != Some(id)
        || !value["extensions"].is_object()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-content removal resource identity is invalid",
        ));
    }
    if role == ResourceRole::DeviceChain {
        let chain = DeviceChain::parse(bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "parse track-content removal device chain",
                error,
            )
        })?;
        if chain.id() != id {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-content removal device-chain identity is invalid",
            ));
        }
    } else if value["format_version"].as_u64() != Some(1) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-content removal resource format version is invalid",
        ));
    }
    Ok(())
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
        _ => "",
    }
}
const fn role_name(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Track => "track",
        ResourceRole::Clip => "clip",
        ResourceRole::Mixer => "mixer",
        ResourceRole::Arrangement => "arrangement",
        ResourceRole::Scene => "scene",
        ResourceRole::AutomationTake => "automation-take",
        ResourceRole::DeviceChain => "device-chain",
        _ => "",
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

fn resource_authorities(
    root: &Dir,
    entries: &[Entry<'_>],
) -> Result<Vec<Handle>, ProjectResourcePublicationError> {
    entries
        .iter()
        .map(|entry| {
            verified_file_authority(
                root,
                &entry.path,
                &entry.previous,
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "verify track-content resource before staging",
            )
        })
        .collect()
}

fn verified_file_authority(
    root: &Dir,
    path: &str,
    expected: &[u8],
    kind: ProjectResourcePublicationErrorKind,
    operation: &'static str,
) -> Result<Handle, ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, path)?;
    let name = file_name(path)?;
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = parent
        .open_with(name, &options)
        .map_err(|error| caused(kind, operation, error))?;
    let retained = file
        .try_clone()
        .map_err(|error| caused(kind, "retain track-content removal file authority", error))?;
    let authority = Handle::from_file(retained.into_std())
        .map_err(|error| caused(kind, "identify track-content removal file authority", error))?;
    verify_open_stable(
        file.into_std(),
        operation,
        expected.len() as u64,
        digest(expected),
        false,
    )
    .map_err(|error| caused(kind, operation, error))?;
    Ok(authority)
}

fn candidate_manifest(
    active: &[u8],
    entries: &[Entry<'_>],
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
        if let Some(bytes) = entry.replacement {
            let descriptor = resources
                .iter_mut()
                .find(|candidate| candidate["id"].as_str() == Some(&entry.id))
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "track-content replacement descriptor disappeared",
                    )
                })?;
            if descriptor["role"].as_str() != Some(role_name(entry.role))
                || descriptor["path"].as_str() != Some(&entry.path)
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-content replacement descriptor changed",
                ));
            }
            descriptor["byte_length"] = bytes.len().into();
            descriptor["sha256"] = hex_digest(digest(bytes)).into();
        } else {
            let before = resources.len();
            resources.retain(|candidate| candidate["id"].as_str() != Some(&entry.id));
            if resources.len().checked_add(1) != Some(before) {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-content removal descriptor disappeared",
                ));
            }
        }
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize track-content removal manifest",
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
            "candidate track-content removal manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the full staging, retained deletion, and manifest-last ordering must remain auditable together"
)]
fn run_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    name: &str,
    entries: &[Entry<'_>],
    authorities: &[Handle],
    removed_track_id: &str,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, name, entries.len())?;
    for (index, entry) in entries.iter().enumerate() {
        write_exclusive_synced(transaction, &previous_name(index), &entry.previous)?;
        if let Some(bytes) = entry.replacement {
            write_exclusive_synced(transaction, &staged_name(index), bytes)?;
        }
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = transaction_record(
        name,
        entries,
        removed_track_id,
        removed_directories(verified, entries)?,
        verified,
        candidate_project,
        next_revision,
    )?;
    let bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-content removal transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;
    for (index, (entry, authority)) in entries.iter().zip(authorities).enumerate() {
        move_verified_resource(
            &verified.root_dir,
            transaction,
            &tx.entries[index],
            &entry.previous,
            authority,
        )?;
        observer(PublicationPhase::ResourceMoved(index), verified.root())?;
        if entry.replacement.is_some() {
            let parent = open_relative_parent(&verified.root_dir, &entry.path)?;
            publish_staged_file_no_replace(
                transaction,
                &staged_name(index),
                &parent,
                file_name(&entry.path)?,
                &format!(".track-content-removal-{index}-{}.json", tx.id),
                "track-content replacement",
            )?;
        }
        observer(PublicationPhase::Resource(index), verified.root())?;
    }
    for (index, directory) in tx.removed_directories.iter().enumerate() {
        verified.root_dir.remove_dir(directory).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty track-content resource directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
        observer(PublicationPhase::Directory(index), verified.root())?;
    }
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-content removal candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-content removal candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".track-content-removal-project-{}.json", tx.id),
        "track-content removal project manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    outcome(&tx)
}

fn transaction_record(
    name: &str,
    entries: &[Entry<'_>],
    track_id: &str,
    removed_directories: Vec<String>,
    verified: &StructurallyVerifiedProject,
    candidate_project: &[u8],
    revision: u64,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let records = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| TransactionEntry {
            id: entry.id.clone(),
            role: role_name(entry.role).to_owned(),
            previous: transaction_file(&entry.path, &entry.previous),
            replacement: entry
                .replacement
                .map(|bytes| transaction_file(&entry.path, bytes)),
            backup_name: Some(backup_name(index)),
        })
        .collect();
    Ok(Transaction {
        format: FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: revision,
        removed_track_id: track_id.to_owned(),
        entries: records,
        removed_directories,
        extensions: serde_json::Map::new(),
    })
}

fn removed_directories(
    verified: &StructurallyVerifiedProject,
    entries: &[Entry<'_>],
) -> Result<Vec<String>, ProjectResourcePublicationError> {
    let removed_ids = entries
        .iter()
        .filter(|entry| entry.replacement.is_none())
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut candidates = BTreeSet::new();
    for entry in entries.iter().filter(|entry| entry.replacement.is_none()) {
        candidates.insert(resource_parent(&entry.path)?.to_owned());
    }
    candidates.retain(|directory| {
        verified.manifest().resources().iter().any(|descriptor| {
            resource_parent(descriptor.path()).is_ok_and(|parent| parent == directory)
        }) && verified
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| {
                resource_parent(descriptor.path()).is_ok_and(|parent| parent == directory)
            })
            .all(|descriptor| removed_ids.contains(descriptor.id()))
    });
    Ok(candidates.into_iter().collect())
}

fn resource_parent(path: &str) -> Result<&str, ProjectResourcePublicationError> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-content resource path has no parent",
            )
        })
}
fn transaction_file(path: &str, bytes: &[u8]) -> super::TransactionFile {
    super::TransactionFile {
        path: path.to_owned(),
        byte_length: bytes.len() as u64,
        sha256: hex_digest(digest(bytes)),
    }
}
fn staged_name(index: usize) -> String {
    format!("resource-{index}.json")
}
fn previous_name(index: usize) -> String {
    format!("previous-resource-{index}.json")
}
fn backup_name(index: usize) -> String {
    format!("removed-resource-{index}.json")
}

fn move_verified_resource(
    root: &Dir,
    transaction: &Dir,
    entry: &TransactionEntry,
    expected: &[u8],
    authority: &Handle,
) -> Result<(), ProjectResourcePublicationError> {
    let backup = entry.backup_name.as_deref().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-content transaction entry has no backup name",
        )
    })?;
    let parent = open_relative_parent(root, &entry.previous.path)?;
    let name = file_name(&entry.previous.path)?;
    parent.rename(name, transaction, backup).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "move removed track-content resource into transaction",
            error,
        )
    })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    let moved = verified_file_authority(
        transaction,
        backup,
        expected,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify moved track-content resource",
    );
    if moved.as_ref().is_ok_and(|candidate| candidate == authority) {
        return Ok(());
    }
    transaction.rename(backup, &parent, name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "restore changed track-content resource after authority mismatch",
            error,
        )
    })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    match moved {
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "track-content resource path changed before publication",
        )),
        Err(error) => Err(error),
    }
}

fn publish_staged_file_no_replace(
    transaction: &Dir,
    staged_name: &str,
    destination: &Dir,
    destination_name: &str,
    temporary_name: &str,
    label: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, staged_name, MAX_JSON_BYTES)?;
    write_exclusive_synced(destination, temporary_name, &bytes)?;
    sync_dir(destination)?;
    let metadata = destination
        .symlink_metadata(temporary_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                format!("inspect staged {label}"),
                error,
            )
        })?;
    match rename_no_replace(
        destination,
        temporary_name,
        identity(&metadata),
        destination,
        destination_name,
    ) {
        Ok(()) => sync_dir(destination),
        Err(error) => {
            remove_optional_matching_file(destination, temporary_name, digest(&bytes))?;
            sync_dir(destination)?;
            Err(caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                format!("publish {label} without overwriting a concurrent resource"),
                error,
            ))
        }
    }
}

fn write_intent(
    transaction: &Dir,
    name: &str,
    count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = Intent {
        format: INTENT_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        entry_count: count,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-content removal intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish track-content removal intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn transaction_id(name: &str) -> Result<String, ProjectResourcePublicationError> {
    let id = name
        .strip_prefix(AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction identity is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction identity is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn reject_other_transactions(backups: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let found = find_unhandled_project_transaction(
        backups,
        &[ProjectTransactionFamily::AudioTrackContentRemoval],
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "inspect project transactions before track-content removal",
            error,
        )
    })?;
    if found.is_some() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "another project transaction requires recovery; reopen the current project revision",
        ));
    }
    Ok(())
}

fn finish_result(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: Dir,
    name: &str,
    result: Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError>,
) -> Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError> {
    match result {
        Ok(outcome) => {
            clear_and_remove(backups, transaction, name).map_err(|error| {
                ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message:
                        "track-content removal committed but transaction cleanup is incomplete"
                            .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                }
            })?;
            Ok(outcome)
        }
        Err(operation) => {
            let tx = match read_transaction(&transaction, name) {
                Ok(tx) => tx,
                Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                    validate_incomplete(&transaction, name)?;
                    clear_and_remove(backups, transaction, name)?;
                    return Err(operation);
                }
                Err(error) => return Err(error),
            };
            match recover_one(root_path, root, backups, &transaction, &tx)? {
                AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack => {
                    clear_and_remove(backups, transaction, name)?;
                    Err(operation)
                }
                AudioTrackContentRemovalPublicationRecoveryDisposition::Committed => {
                    let outcome = outcome(&tx)?;
                    clear_and_remove(backups, transaction, name).map_err(|error| ProjectResourcePublicationError { kind: ProjectResourcePublicationErrorKind::Recovery, message: "track-content removal committed but transaction cleanup is incomplete".to_owned(), published: true, source: Some(Box::new(error)) })?;
                    Ok(outcome)
                }
            }
        }
    }
}

fn recover_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<AudioTrackContentRemovalPublicationRecovery>, ProjectResourcePublicationError> {
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
        .filter(|name| name.starts_with(AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX))
        .collect::<Vec<_>>();
    names.sort();
    let mut recovered = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open track-content removal recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_incomplete(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove(backups, transaction, &name)?;
                recovered.push(AudioTrackContentRemovalPublicationRecovery {
                    transaction_id: id,
                    disposition: AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        let disposition = recover_one(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove(backups, transaction, &name)?;
        recovered.push(AudioTrackContentRemovalPublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(recovered)
}

#[allow(
    clippy::too_many_lines,
    reason = "the journal must be fully validated before recovery changes active project paths"
)]
fn recover_one(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<AudioTrackContentRemovalPublicationRecoveryDisposition, ProjectResourcePublicationError>
{
    validate_staged_transaction(transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = digest(&read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES)?);
    if active == after {
        verify_committed(root_path, root, after).map_err(|source| {
            ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message: "committed track-content removal could not be verified".to_owned(),
                published: true,
                source: Some(Box::new(source)),
            }
        })?;
        cleanup_temporaries(root, backups, tx)?;
        return Ok(AudioTrackContentRemovalPublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal recovery found an unrelated project identity",
        ));
    }
    restore_removed_directories(root, tx)?;
    preflight_rollback(root, transaction, tx)?;
    for (index, entry) in tx.entries.iter().enumerate() {
        let parent = open_relative_parent(root, &entry.previous.path)?;
        let name = file_name(&entry.previous.path)?;
        if let Some(replacement) = &entry.replacement {
            let current = read_optional_file(&parent, name)?;
            let previous_digest = decode_digest(&entry.previous.sha256)?;
            if current
                .as_deref()
                .is_some_and(|bytes| digest(bytes) == previous_digest)
            {
                continue;
            }
            publish_staged_file(
                transaction,
                &previous_name(index),
                &parent,
                &parent,
                file_name(&replacement.path)?,
                &format!(".track-content-removal-{index}-rollback-{}.json", tx.id),
                "previous track-content resource",
            )?;
        } else if let Some(backup) = &entry.backup_name
            && !entry_present(&parent, name)?
        {
            transaction.rename(backup, &parent, name).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "restore removed track-content resource",
                    error,
                )
            })?;
            sync_dir(&parent)?;
            sync_dir(transaction)?;
        }
    }
    verify_retained_project_candidate(
        root,
        &read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?,
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "rolled-back track-content project does not verify",
            error,
        )
    })?;
    cleanup_temporaries(root, backups, tx)?;
    Ok(AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack)
}

fn preflight_rollback(
    root: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for entry in &tx.entries {
        let parent = open_relative_parent(root, &entry.previous.path)?;
        let active = read_optional_file(&parent, file_name(&entry.previous.path)?)?;
        if let Some(replacement) = &entry.replacement {
            let found = active.as_deref().map(digest);
            let before = decode_digest(&entry.previous.sha256)?;
            let after = decode_digest(&replacement.sha256)?;
            let backup = entry.backup_name.as_deref();
            let (backup_present, backup_matches) = if let Some(name) = backup {
                let present = entry_present(transaction, name)?;
                let matches =
                    !present || digest(&read_bounded(transaction, name, MAX_JSON_BYTES)?) == before;
                (present, matches)
            } else {
                (false, true)
            };
            let legacy_replacement = backup.is_none();
            let valid = if legacy_replacement {
                matches!(found, Some(value) if value == before || value == after)
            } else {
                match (found, backup_present, backup_matches) {
                    (Some(value), false, true) => value == before,
                    (Some(value), true, true) => value == after,
                    (None, true, true) => true,
                    _ => false,
                }
            };
            if !valid {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-content rollback refuses unrelated replacement content",
                ));
            }
        } else {
            let backup = entry.backup_name.as_deref().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "removed track-content entry lacks backup authority",
                )
            })?;
            let backup_present = entry_present(transaction, backup)?;
            let expected = decode_digest(&entry.previous.sha256)?;
            match (active.as_deref().map(digest), backup_present) {
                (Some(value), false) if value == expected => {}
                (None, true) => {
                    let bytes = read_bounded(transaction, backup, MAX_JSON_BYTES)?;
                    if digest(&bytes) != expected {
                        return Err(failure(
                            ProjectResourcePublicationErrorKind::Recovery,
                            "track-content rollback backup changed",
                        ));
                    }
                }
                _ => {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-content rollback removal authority is ambiguous",
                    ));
                }
            }
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the staged journal must be completely verified before recovery mutates any project path"
)]
fn validate_staged_transaction(
    transaction: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal staged project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous track-content project is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate track-content project is invalid",
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
            "track-content removal staged project revisions are inconsistent",
        ));
    }
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "track-content removal transaction intent")?;
    let intent: Intent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-content removal transaction intent",
            error,
        )
    })?;
    if intent.format != INTENT_FORMAT
        || intent.format_version != VERSION
        || intent.id != tx.id
        || intent.entry_count != tx.entries.len()
        || !intent.extensions.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction intent is invalid",
        ));
    }
    let mut expected_names = BTreeSet::from([
        INTENT_NAME.to_owned(),
        PROJECT_STAGING_NAME.to_owned(),
        PREVIOUS_PROJECT_NAME.to_owned(),
        TRANSACTION_NAME.to_owned(),
    ]);
    for (index, entry) in tx.entries.iter().enumerate() {
        let prior = read_bounded(transaction, &previous_name(index), MAX_JSON_BYTES)?;
        if prior.len() as u64 != entry.previous.byte_length
            || digest(&prior) != decode_digest(&entry.previous.sha256)?
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal staged previous resource changed",
            ));
        }
        let role = role_from_name(&entry.role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal transaction has unsupported resource role",
            )
        })?;
        validate_document(&entry.id, &prior, role)?;
        let previous_descriptor = previous_manifest
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == entry.id)
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-content removal previous resource descriptor is absent",
                )
            })?;
        if previous_descriptor.role() != role || previous_descriptor.path() != entry.previous.path {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal previous resource descriptor changed",
            ));
        }
        expected_names.insert(previous_name(index));
        match (&entry.replacement, &entry.backup_name) {
            (Some(replacement), backup) => {
                if let Some(backup) = backup
                    && backup != &backup_name(index)
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-content replacement backup name is invalid",
                    ));
                }
                let bytes = read_bounded(transaction, &staged_name(index), MAX_JSON_BYTES)?;
                if bytes.len() as u64 != replacement.byte_length
                    || digest(&bytes) != decode_digest(&replacement.sha256)?
                    || replacement.path != entry.previous.path
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-content removal staged replacement changed",
                    ));
                }
                validate_document(&entry.id, &bytes, role)?;
                let candidate_descriptor = candidate_manifest
                    .resources()
                    .iter()
                    .find(|descriptor| descriptor.id() == entry.id)
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Recovery,
                            "track-content removal candidate replacement descriptor is absent",
                        )
                    })?;
                if candidate_descriptor.role() != role
                    || candidate_descriptor.path() != replacement.path
                    || candidate_descriptor.byte_length() != replacement.byte_length
                    || candidate_descriptor.sha256() != replacement.sha256
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-content removal candidate replacement descriptor changed",
                    ));
                }
                expected_names.insert(staged_name(index));
                if let Some(backup) = backup {
                    expected_names.insert(backup.clone());
                }
            }
            (None, Some(backup)) if backup == &backup_name(index) => {
                if candidate_manifest
                    .resources()
                    .iter()
                    .any(|descriptor| descriptor.id() == entry.id)
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-content removal candidate retained a removed resource",
                    ));
                }
                expected_names.insert(backup.clone());
            }
            _ => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-content removal transaction entry operation is invalid",
                ));
            }
        }
    }
    if !tx.entries.iter().any(|entry| {
        entry.id == tx.removed_track_id && entry.role == "track" && entry.replacement.is_none()
    }) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction has no removed audio track",
        ));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read track-content removal transaction entries",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read track-content removal transaction entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal transaction has a non-Unicode entry",
            ));
        };
        if !expected_names.contains(&name) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn read_transaction(
    transaction: &Dir,
    name: &str,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "track-content removal transaction")?;
    let tx: Transaction = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-content removal transaction",
            error,
        )
    })?;
    let id = transaction_id(name)?;
    if tx.format != FORMAT
        || tx.format_version != VERSION
        || tx.id != id
        || !is_valid_entity_id(&tx.removed_track_id)
        || tx.entries.is_empty()
        || tx.entries.len() > MAX_ENTRIES
        || !tx.extensions.is_empty()
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction is invalid",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in &tx.entries {
        if !is_valid_entity_id(&entry.id)
            || !ids.insert(entry.id.as_str())
            || !paths.insert(entry.previous.path.as_str())
            || entry.previous.byte_length == 0
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-content removal transaction entries are invalid",
            ));
        }
    }
    let mut directories = BTreeSet::new();
    if tx.removed_directories.iter().any(|directory| {
        directory.is_empty()
            || directory.contains('/')
            || !directories.insert(directory.as_str())
            || !tx.entries.iter().any(|entry| {
                entry.replacement.is_none()
                    && resource_parent(&entry.previous.path).is_ok_and(|parent| parent == directory)
            })
    }) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction directories are invalid",
        ));
    }
    Ok(tx)
}

fn restore_removed_directories(
    root: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for directory in &tx.removed_directories {
        match root.create_dir(directory) {
            Ok(()) => sync_dir(root)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = root.symlink_metadata(directory).map_err(|source| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "inspect restored track-content resource directory",
                        source,
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "restored track-content resource directory is not real",
                    ));
                }
            }
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "restore track-content resource directory",
                    error,
                ));
            }
        }
    }
    Ok(())
}

fn validate_incomplete(
    transaction: &Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let count = if missing_file(transaction, INTENT_NAME)? {
        0
    } else {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "incomplete track-content removal intent")?;
        let intent: Intent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse incomplete track-content removal intent",
                error,
            )
        })?;
        if intent.format != INTENT_FORMAT
            || intent.format_version != VERSION
            || intent.id != transaction_id(name)?
            || intent.entry_count == 0
            || intent.entry_count > MAX_ENTRIES
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-content removal intent is invalid",
            ));
        }
        intent.entry_count
    };
    let mut expected = BTreeSet::from([
        INTENT_STAGING_NAME.to_owned(),
        INTENT_NAME.to_owned(),
        PROJECT_STAGING_NAME.to_owned(),
        PREVIOUS_PROJECT_NAME.to_owned(),
    ]);
    for index in 0..count {
        expected.insert(previous_name(index));
        expected.insert(staged_name(index));
    }
    for item in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete track-content transaction",
            error,
        )
    })? {
        let item = item.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete track-content transaction entry",
                error,
            )
        })?;
        let Some(item) = item.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-content transaction has non-Unicode entry",
            ));
        };
        if !expected.contains(&item) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-content transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn entry_present(directory: &Dir, name: &str) -> Result<bool, ProjectResourcePublicationError> {
    match directory.symlink_metadata(name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "inspect track-content transaction entry",
            error,
        )),
    }
}

fn cleanup_temporaries(
    root: &Dir,
    backups: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Some(replacement) = &entry.replacement {
            let parent = open_relative_parent(root, &entry.previous.path)?;
            remove_optional_matching_file(
                &parent,
                &format!(".track-content-removal-{index}-{}.json", tx.id),
                decode_digest(&replacement.sha256)?,
            )?;
            remove_optional_matching_file(
                &parent,
                &format!(".track-content-removal-{index}-rollback-{}.json", tx.id),
                decode_digest(&entry.previous.sha256)?,
            )?;
        }
    }
    remove_optional_matching_file(
        backups,
        &format!(".track-content-removal-project-{}.json", tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )
}

fn outcome(
    tx: &Transaction,
) -> Result<AudioTrackContentRemovalPublicationOutcome, ProjectResourcePublicationError> {
    Ok(AudioTrackContentRemovalPublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.removed_track_id.clone(),
    })
}

#[cfg(not(windows))]
fn clear_and_remove(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".track-content-removal-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine track-content removal transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined track-content removal transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined track-content removal transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-content removal transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove track-content removal transaction",
            error,
        )
    })?;
    sync_dir(backups)
}
#[cfg(windows)]
fn clear_and_remove(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove track-content removal transaction",
            error,
        )
    })?;
    sync_dir(backups)
}
fn clear_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let mut names = transaction
        .entries()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read track-content transaction for cleanup",
                error,
            )
        })?
        .map(|entry| entry.map(|value| value.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read track-content transaction cleanup entry",
                error,
            )
        })?;
    names.sort();
    for name in names {
        transaction.remove_file(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "remove track-content transaction file",
                error,
            )
        })?;
    }
    sync_dir(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{recover_project_transactions, verify_project_package};
    use hyper_native_device::{DeviceChain, NativeCompressorState, NativeDeviceInstance};
    use hyper_playback_json::{
        AudioTrackContentRemovalSource, LoaderLimits, PlaybackJsonInput, load,
        serialize_audio_track_content_removal,
    };
    use serde_json::json;
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338001";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338002";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338003";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338004";
    const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338006";
    const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338008";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338009";
    const TAKE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338010";
    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338011";
    const DEVICE_CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338012";
    const DEVICE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338013";
    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        parent: std::path::PathBuf,
        root: std::path::PathBuf,
        timeline: Vec<u8>,
        track: Vec<u8>,
        mixer: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-track-content-removal-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed),
            ));
            let root = parent.join("Session");
            fs::create_dir_all(root.join("timeline")).unwrap();
            fs::create_dir_all(root.join("tracks")).unwrap();
            fs::create_dir_all(root.join("mixer")).unwrap();
            fs::create_dir_all(root.join("assets/recordings")).unwrap();
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
            let recording = vec![0xA5; 64];
            fs::write(root.join("timeline/timeline.json"), &timeline).unwrap();
            fs::write(root.join("tracks/lead.json"), &track).unwrap();
            fs::write(root.join("mixer/main.json"), &mixer).unwrap();
            fs::write(root.join("assets/recordings/retained.wav"), &recording).unwrap();
            let manifest = canonical(json!({
                "format":"hyper-project","format_version":1,"project_id":PROJECT_ID,"revision":1,
                "name":"Track content removal fixture","sample_rate_hz":48_000,
                "required_capabilities":{"format":"hyper-hardware-capabilities","format_version":1,
                    "audio_inputs":0,"audio_outputs":0,"cv_inputs":0,"cv_outputs":0,
                    "gate_outputs":0,"has_clock_input":false,"has_reset_input":false,
                    "has_clock_output":false,"usb_mode":"none","has_ethernet":false,"extensions":{}},
                "resources":[
                    descriptor(TIMELINE_ID,"timeline","timeline/timeline.json",&timeline),
                    descriptor(TRACK_ID,"track","tracks/lead.json",&track),
                    descriptor(MIXER_ID,"mixer","mixer/main.json",&mixer)
                ],
                "assets":[{"id":"018f47a2-6b2d-7c11-8a44-001122338005","role":"recording",
                    "path":"assets/recordings/retained.wav","media_type":"audio/wav",
                    "byte_length":recording.len(),"sha256":hex_digest(digest(&recording)),"extensions":{}}],
                "extensions":{}
            }));
            fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
            Self {
                parent,
                root,
                timeline,
                track,
                mixer,
            }
        }

        fn documents(&self) -> ([u8; 32], AudioTrackContentRemovalDocuments) {
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
            let documents = serialize_audio_track_content_removal(
                &project,
                &[],
                AudioTrackContentRemovalSource {
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
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            (*package.manifest_sha256(), documents)
        }

        #[allow(
            clippy::too_many_lines,
            reason = "one complete fixture verifies every mutable resource edge of the storage transaction"
        )]
        fn complete_documents(&self) -> ([u8; 32], AudioTrackContentRemovalDocuments) {
            let track = canonical(json!({
                "format":"hyper-track","format_version":1,"id":TRACK_ID,"project_id":PROJECT_ID,
                "name":"Lead","kind":"audio","order":0,"active":true,"clip_ids":[CLIP_ID],"extensions":{}
            }));
            let clip = canonical(json!({
                "format":"hyper-audio-clip","format_version":1,"id":CLIP_ID,"project_id":PROJECT_ID,
                "track_id":TRACK_ID,"name":"Lead","enabled":true,"duration_ticks":960,
                "source":{"asset_id":ASSET_ID,"start_frame":0,"frame_count":24_000},
                "loop":{"enabled":false,"start_tick":0,"end_tick":960},
                "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
                "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
                "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":960,"source_frame_offset":24_000}],"extensions":{}
            }));
            let scene = canonical(json!({
                "format":"hyper-scene","format_version":1,"id":SCENE_ID,"project_id":PROJECT_ID,
                "name":"Verse","order":0,"enabled":true,"launch_quantization":{"mode":"immediate"},
                "slots":[{"track_id":TRACK_ID,"action":"play","clip_id":CLIP_ID}],"extensions":{}
            }));
            let arrangement = canonical(json!({
                "format":"hyper-arrangement","format_version":1,"id":ARRANGEMENT_ID,"project_id":PROJECT_ID,
                "name":"Main","enabled":true,"placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122338014",
                    "track_id":TRACK_ID,"clip_id":CLIP_ID,"start_tick":0,"clip_start_tick":0,"duration_ticks":960,"enabled":true}],
                "scene_launches":[],"extensions":{}
            }));
            let mixer = canonical(json!({
                "format":"hyper-mixer","format_version":1,"id":MIXER_ID,"project_id":PROJECT_ID,
                "channels":[
                    {"track_id":TRACK_ID,"gain_millionths":800_000,"pan_millionths":0,"mute":false,"solo":false,
                        "destination":{"kind":"main"},"sends":[]}
                ],"return_buses":[],"main":{"gain_millionths":1_000_000,"mute":false},
                "cue":{"source":"main","gain_millionths":1_000_000,"mute":false},"extensions":{}
            }));
            let take = canonical(json!({
                "format":"hyper-automation-take","format_version":1,"id":TAKE_ID,"project_id":PROJECT_ID,
                "name":"Lead automation","enabled":true,"captured_at_utc":"2026-08-23T12:00:00.000Z",
                "start_tick":0,"end_tick":960,"lanes":[
                    {"id":"018f47a2-6b2d-7c11-8a44-001122338015",
                        "target":{"kind":"mixer-channel","track_id":TRACK_ID,"parameter":"gain"},"interpolation":"linear",
                        "points":[{"tick":0,"value":800_000},{"tick":959,"value":900_000}]},
                    {"id":"018f47a2-6b2d-7c11-8a44-001122338016",
                        "target":{"kind":"mixer-main","parameter":"gain"},"interpolation":"hold",
                        "points":[{"tick":0,"value":1_000_000}]}
                ],"extensions":{}
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
            let asset = vec![0x3C; 64];
            for directory in [
                "clips",
                "scenes",
                "arrangement",
                "takes",
                "devices",
                "assets/recordings",
            ] {
                fs::create_dir_all(self.root.join(directory)).unwrap();
            }
            fs::write(self.root.join("tracks/lead.json"), &track).unwrap();
            fs::write(self.root.join("clips/lead.json"), &clip).unwrap();
            fs::write(self.root.join("scenes/verse.json"), &scene).unwrap();
            fs::write(self.root.join("arrangement/main.json"), &arrangement).unwrap();
            fs::write(self.root.join("mixer/main.json"), &mixer).unwrap();
            fs::write(self.root.join("takes/lead.json"), &take).unwrap();
            fs::write(self.root.join("devices/lead.json"), &chain).unwrap();
            fs::write(self.root.join("assets/recordings/retained.wav"), &asset).unwrap();
            let manifest = canonical(json!({
                "format":"hyper-project","format_version":1,"project_id":PROJECT_ID,"revision":1,
                "name":"Complete track-content removal fixture","sample_rate_hz":48_000,
                "required_capabilities":{"format":"hyper-hardware-capabilities","format_version":1,
                    "audio_inputs":0,"audio_outputs":0,"cv_inputs":0,"cv_outputs":0,"gate_outputs":0,
                    "has_clock_input":false,"has_reset_input":false,"has_clock_output":false,"usb_mode":"none","has_ethernet":false,"extensions":{}},
                "resources":[
                    descriptor(TIMELINE_ID,"timeline","timeline/timeline.json",&self.timeline),
                    descriptor(TRACK_ID,"track","tracks/lead.json",&track),
                    descriptor(CLIP_ID,"clip","clips/lead.json",&clip),
                    descriptor(SCENE_ID,"scene","scenes/verse.json",&scene),
                    descriptor(ARRANGEMENT_ID,"arrangement","arrangement/main.json",&arrangement),
                    descriptor(MIXER_ID,"mixer","mixer/main.json",&mixer),
                    descriptor(TAKE_ID,"automation-take","takes/lead.json",&take),
                    descriptor(DEVICE_CHAIN_ID,"device-chain","devices/lead.json",&chain)
                ],"assets":[{"id":ASSET_ID,"role":"recording","path":"assets/recordings/retained.wav",
                    "media_type":"audio/wav","byte_length":asset.len(),"sha256":hex_digest(digest(&asset)),"extensions":{}}],"extensions":{}
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
            let assets = [hyper_playback_json::MediaAssetFact {
                id: ASSET_ID,
                sample_rate_hz: 48_000,
                frame_count: 24_000,
            }];
            let documents = serialize_audio_track_content_removal(
                &project,
                &assets,
                AudioTrackContentRemovalSource {
                    track: &track,
                    clips: &[(CLIP_ID, clip.as_slice())],
                    scenes: &[(SCENE_ID, scene.as_slice())],
                    arrangement: Some(&arrangement),
                    mixer: &mixer,
                    automation_take_ids: &[TAKE_ID],
                    automation_takes: &[(TAKE_ID, take.as_slice())],
                    device_chain_id: Some(DEVICE_CHAIN_ID),
                    device_chain: Some(&chain),
                },
                TRACK_ID,
                LoaderLimits::VERSION_1,
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
        reason = "fixture builders deliberately consume short JSON values before canonicalization"
    )]
    fn canonical(value: serde_json::Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&value).unwrap()
    }

    fn descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> serde_json::Value {
        json!({"id":id,"role":role,"path":path,"media_type":"application/json",
            "byte_length":bytes.len(),"sha256":hex_digest(digest(bytes)),"extensions":{}})
    }

    #[test]
    fn publishes_track_content_removal_without_collecting_immutable_media() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.documents();
        let outcome = publish_audio_track_content_removal(
            &fixture.root,
            AudioTrackContentRemovalPublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(outcome.track_id(), TRACK_ID);
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        assert!(
            package
                .manifest()
                .resources()
                .iter()
                .all(|resource| resource.id() != TRACK_ID)
        );
        assert_eq!(
            package.read_resource(MIXER_ID).unwrap(),
            documents.mixer().bytes()
        );
        assert!(!fixture.root.join("tracks/lead.json").exists());
        assert_eq!(
            fs::read(fixture.root.join("assets/recordings/retained.wav")).unwrap(),
            vec![0xA5; 64]
        );
    }

    #[test]
    fn recovers_track_content_removal_to_one_verified_manifest_side() {
        for phase in [
            PublicationPhase::Transaction,
            PublicationPhase::Resource(0),
            PublicationPhase::Resource(1),
            PublicationPhase::Manifest,
        ] {
            let fixture = Fixture::new();
            let (identity, documents) = fixture.documents();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_observed(
                    &fixture.root,
                    AudioTrackContentRemovalPublicationRequest {
                        expected_project_sha256: identity,
                        documents: &documents,
                    },
                    |current, _| {
                        assert_ne!(current, phase, "simulate process termination");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let report = recover_project_transactions(&fixture.root).unwrap();
            assert_eq!(report.audio_track_content_removals().len(), 1);
            let expected = if phase == PublicationPhase::Manifest {
                AudioTrackContentRemovalPublicationRecoveryDisposition::Committed
            } else {
                AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(
                report.audio_track_content_removals()[0].disposition(),
                expected
            );
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackContentRemovalPublicationRecoveryDisposition::Committed {
                assert!(
                    package
                        .manifest()
                        .resources()
                        .iter()
                        .all(|resource| resource.id() != TRACK_ID)
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert_eq!(package.read_resource(TRACK_ID).unwrap(), fixture.track);
            }
        }
    }

    #[test]
    fn publishes_the_complete_track_graph_delta_and_preserves_media() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.complete_documents();
        let outcome = publish_audio_track_content_removal(
            &fixture.root,
            AudioTrackContentRemovalPublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        for removed in [TRACK_ID, CLIP_ID, DEVICE_CHAIN_ID] {
            assert!(
                package
                    .manifest()
                    .resources()
                    .iter()
                    .all(|resource| resource.id() != removed)
            );
        }
        assert_eq!(
            package.read_resource(MIXER_ID).unwrap(),
            documents.mixer().bytes()
        );
        assert_eq!(
            package.read_resource(SCENE_ID).unwrap(),
            documents.scenes()[0].bytes()
        );
        assert_eq!(
            package.read_resource(ARRANGEMENT_ID).unwrap(),
            documents.arrangement().unwrap().bytes()
        );
        assert_eq!(
            package.read_resource(TAKE_ID).unwrap(),
            documents.automation_takes()[0].bytes()
        );
        assert!(!fixture.root.join("tracks/lead.json").exists());
        assert!(!fixture.root.join("clips/lead.json").exists());
        assert!(!fixture.root.join("devices/lead.json").exists());
        assert_eq!(
            fs::read(fixture.root.join("assets/recordings/retained.wav")).unwrap(),
            vec![0x3C; 64]
        );
    }

    #[test]
    fn complete_track_graph_rollback_restores_each_removed_resource_and_replacement() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.complete_documents();
        let previous_mixer = fs::read(fixture.root.join("mixer/main.json")).unwrap();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = publish_observed(
                &fixture.root,
                AudioTrackContentRemovalPublicationRequest {
                    expected_project_sha256: identity,
                    documents: &documents,
                },
                |phase, _| {
                    assert_ne!(
                        phase,
                        PublicationPhase::Resource(5),
                        "simulate process termination"
                    );
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());
        let report = recover_project_transactions(&fixture.root).unwrap();
        assert_eq!(report.audio_track_content_removals().len(), 1);
        assert_eq!(
            report.audio_track_content_removals()[0].disposition(),
            AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack,
        );
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(*package.manifest_sha256(), identity);
        assert_eq!(
            package.read_resource(TRACK_ID).unwrap(),
            fs::read(fixture.root.join("tracks/lead.json")).unwrap()
        );
        assert_eq!(
            package.read_resource(CLIP_ID).unwrap(),
            fs::read(fixture.root.join("clips/lead.json")).unwrap()
        );
        assert_eq!(
            package.read_resource(DEVICE_CHAIN_ID).unwrap(),
            fs::read(fixture.root.join("devices/lead.json")).unwrap()
        );
        assert_eq!(package.read_resource(MIXER_ID).unwrap(), previous_mixer);
        assert_eq!(
            fs::read(fixture.root.join("assets/recordings/retained.wav")).unwrap(),
            vec![0x3C; 64]
        );
    }

    #[test]
    fn complete_track_graph_recovery_covers_every_resource_move_and_directory_removal() {
        let probe = Fixture::new();
        let (_, documents) = probe.complete_documents();
        let resource_count = 2
            + documents.clips().len()
            + documents.scenes().len()
            + documents.automation_takes().len()
            + usize::from(documents.arrangement().is_some())
            + usize::from(documents.device_chain().is_some());
        let mut phases = vec![PublicationPhase::Transaction];
        for index in 0..resource_count {
            phases.push(PublicationPhase::ResourceMoved(index));
            phases.push(PublicationPhase::Resource(index));
        }
        for index in 0..3 {
            phases.push(PublicationPhase::Directory(index));
        }
        phases.push(PublicationPhase::Manifest);

        for phase in phases {
            let fixture = Fixture::new();
            let (identity, documents) = fixture.complete_documents();
            let before_track = fs::read(fixture.root.join("tracks/lead.json")).unwrap();
            let before_clip = fs::read(fixture.root.join("clips/lead.json")).unwrap();
            let before_device = fs::read(fixture.root.join("devices/lead.json")).unwrap();
            let before_mixer = fs::read(fixture.root.join("mixer/main.json")).unwrap();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_observed(
                    &fixture.root,
                    AudioTrackContentRemovalPublicationRequest {
                        expected_project_sha256: identity,
                        documents: &documents,
                    },
                    |current, _| {
                        assert_ne!(current, phase, "simulate process termination");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());

            let report = recover_project_transactions(&fixture.root).unwrap();
            let expected = if phase == PublicationPhase::Manifest {
                AudioTrackContentRemovalPublicationRecoveryDisposition::Committed
            } else {
                AudioTrackContentRemovalPublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(
                report.audio_track_content_removals()[0].disposition(),
                expected,
            );
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackContentRemovalPublicationRecoveryDisposition::Committed {
                for id in [TRACK_ID, CLIP_ID, DEVICE_CHAIN_ID] {
                    assert!(
                        package
                            .manifest()
                            .resources()
                            .iter()
                            .all(|resource| resource.id() != id)
                    );
                }
                for path in ["tracks", "clips", "devices"] {
                    assert!(!fixture.root.join(path).exists());
                }
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert_eq!(
                    fs::read(fixture.root.join("tracks/lead.json")).unwrap(),
                    before_track
                );
                assert_eq!(
                    fs::read(fixture.root.join("clips/lead.json")).unwrap(),
                    before_clip
                );
                assert_eq!(
                    fs::read(fixture.root.join("devices/lead.json")).unwrap(),
                    before_device
                );
                assert_eq!(
                    fs::read(fixture.root.join("mixer/main.json")).unwrap(),
                    before_mixer
                );
            }
            assert_eq!(
                fs::read(fixture.root.join("assets/recordings/retained.wav")).unwrap(),
                vec![0x3C; 64],
            );
            assert!(
                fs::read_dir(fixture.root.join("backups")).map_or(true, |entries| entries
                    .flatten()
                    .all(|entry| {
                        !entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX)
                    }))
            );
        }
    }

    #[test]
    fn refuses_a_replaced_removed_resource_without_deleting_the_replacement() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.documents();
        let previous_manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let replacement = canonical(json!({
            "format":"hyper-track","format_version":1,"id":TRACK_ID,
            "project_id":PROJECT_ID,"name":"Changed","kind":"audio","order":0,
            "active":true,"clip_ids":[],"extensions":{}
        }));
        let error = publish_observed(
            &fixture.root,
            AudioTrackContentRemovalPublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
            |phase, _| {
                if phase == PublicationPhase::Transaction {
                    fs::write(fixture.root.join("tracks/lead.json"), &replacement).unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(
            fs::read(fixture.root.join("tracks/lead.json")).unwrap(),
            replacement
        );
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            previous_manifest
        );
        assert!(verify_project_package(&fixture.root).is_err());
    }

    #[test]
    fn refuses_a_replaced_replacement_resource_without_overwriting_it() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.documents();
        let previous_manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let mut replacement: serde_json::Value = serde_json::from_slice(&fixture.mixer).unwrap();
        replacement["main"]["gain_millionths"] = 900_000.into();
        let replacement = canonical(replacement);
        let error = publish_observed(
            &fixture.root,
            AudioTrackContentRemovalPublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
            |phase, _| {
                if phase == PublicationPhase::Transaction {
                    fs::write(fixture.root.join("mixer/main.json"), &replacement).unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            replacement
        );
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            previous_manifest
        );
        assert!(verify_project_package(&fixture.root).is_err());
    }

    #[test]
    fn refuses_a_replacement_created_after_current_authority_is_moved() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.documents();
        let previous_manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let mut replacement: serde_json::Value = serde_json::from_slice(&fixture.mixer).unwrap();
        replacement["main"]["gain_millionths"] = 900_000.into();
        let replacement = canonical(replacement);
        let error = publish_observed(
            &fixture.root,
            AudioTrackContentRemovalPublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
            |phase, _| {
                if phase == PublicationPhase::ResourceMoved(0) {
                    fs::write(fixture.root.join("mixer/main.json"), &replacement).unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        assert_eq!(
            fs::read(fixture.root.join("mixer/main.json")).unwrap(),
            replacement
        );
        assert_eq!(
            fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(),
            previous_manifest
        );
        assert!(
            fs::read_dir(fixture.root.join("mixer"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".track-content-removal-"))
        );
        assert!(verify_project_package(&fixture.root).is_err());
    }
}
