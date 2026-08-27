//! Durable publication for one reversible rendered audio-track freeze.
//!
//! A freeze replaces the active track and arrangement, retires its source
//! clips, and adds a rendered clip, sample, and retained unfreeze authority.
//! Those facts have one commit point: the project manifest.

use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_ENTRIES, MAX_JSON_BYTES, MAX_SAFE_INTEGER,
    PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME, ProjectResourcePublicationError,
    ProjectResourcePublicationErrorKind, StructurallyVerifiedProject, TRANSACTION_NAME,
    TransactionFile, caused, decode_digest, digest, failure, file_name, hex_digest, missing_file,
    open_optional_dir, open_or_create_dir, open_relative_parent, open_stable_project_root,
    publish_staged_file, read_bounded, remove_optional_matching_file, require_canonical, sync_dir,
    validate_canonical_resource, verify_committed, verify_project_candidate,
    verify_project_package, verify_retained_project_candidate, write_exclusive_synced,
};
use crate::project_lock::{
    AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX, AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX, ProjectLock,
    ProjectTransactionFamily, find_unhandled_project_transaction,
};
use crate::stable_file::verify_open_stable;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_audio_source::{AudioContainer, DecodeLimits, decode_bytes};
use hyper_fs_authority::{identity, rename_no_replace};
use hyper_playback_json::{AudioTrackFreezeDocuments, AudioTrackUnfreezeDocuments};
use hyper_project_model::{
    AssetRole, AudioMediaType, ProjectManifest, ResourceRole, is_valid_entity_id,
};
use hyper_track_freeze::TrackFreezeState;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use uuid::Uuid;

#[cfg(not(windows))]
use super::same_directory;

const FORMAT: &str = "hyper-audio-track-freeze-transaction";
const INTENT_FORMAT: &str = "hyper-audio-track-freeze-intent";
const UNFREEZE_FORMAT: &str = "hyper-audio-track-unfreeze-transaction";
const UNFREEZE_INTENT_FORMAT: &str = "hyper-audio-track-unfreeze-intent";
const VERSION: u8 = 1;
const SAMPLE_STAGING_NAME: &str = "rendered-sample.wav";

/// The complete rendered result and document delta for one confirmed freeze.
#[derive(Clone, Copy, Debug)]
pub struct AudioTrackFreezePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub documents: &'a AudioTrackFreezeDocuments,
    pub rendered_sample: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackFreezePublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

/// The complete document delta for restoring one still-canonical rendered
/// freeze. The rendered sample remains an asset until reference-aware cleanup
/// can prove it unused.
#[derive(Clone, Copy, Debug)]
pub struct AudioTrackUnfreezePublicationRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub documents: &'a AudioTrackUnfreezeDocuments,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackUnfreezePublicationOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    track_id: String,
}

impl AudioTrackUnfreezePublicationOutcome {
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

impl AudioTrackFreezePublicationOutcome {
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
pub enum AudioTrackFreezePublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackFreezePublicationRecovery {
    transaction_id: String,
    disposition: AudioTrackFreezePublicationRecoveryDisposition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTrackUnfreezePublicationRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioTrackUnfreezePublicationRecovery {
    transaction_id: String,
    disposition: AudioTrackUnfreezePublicationRecoveryDisposition,
}

impl AudioTrackUnfreezePublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> AudioTrackUnfreezePublicationRecoveryDisposition {
        self.disposition
    }
}

impl AudioTrackFreezePublicationRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }

    #[must_use]
    pub const fn disposition(&self) -> AudioTrackFreezePublicationRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationPhase {
    Transaction,
    SourceMoved(usize),
    ResourcePublished(usize),
    SamplePublished,
    Manifest,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum TransactionOperation {
    #[default]
    Freeze,
    Unfreeze,
}

#[derive(Debug)]
struct Entry<'a> {
    id: String,
    role: ResourceRole,
    path: String,
    before: Option<Vec<u8>>,
    after: Option<&'a [u8]>,
    created_parent_directory: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SamplePlan {
    created_assets_directory: bool,
    created_samples_directory: bool,
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
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before: Option<TransactionFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    after: Option<TransactionFile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backup_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    created_parent_directory: bool,
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
    track_id: String,
    frozen_clip_id: String,
    frozen_asset_id: String,
    freeze_state_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendered_sample: Option<TransactionFile>,
    #[serde(default)]
    created_assets_directory: bool,
    #[serde(default)]
    created_samples_directory: bool,
    #[serde(default)]
    operation: TransactionOperation,
    entries: Vec<TransactionEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed_directories: Vec<String>,
    extensions: serde_json::Map<String, Value>,
}

/// Publishes one complete reversible freeze as a recoverable manifest-last
/// mutation. The operation either leaves the previous project intact or a
/// verifier-accepted frozen project; no intermediate graph is committed.
///
/// # Errors
///
/// Refuses stale source authority, malformed render output, incomplete
/// document deltas, path collisions, and all ambiguous recovery states.
pub fn publish_audio_track_freeze(
    project_path: impl AsRef<Path>,
    request: AudioTrackFreezePublicationRequest<'_>,
) -> Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError> {
    publish_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers interrupted frozen-track publications without accepting an
/// ambiguous active project or unrelated filesystem content.
///
/// # Errors
///
/// Returns a recovery error rather than overwriting content not authenticated
/// by the journal.
pub fn recover_audio_track_freezes(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackFreezePublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for track-freeze recovery",
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

/// Restores one unchanged rendered freeze through a recoverable,
/// manifest-last transaction. The frozen sample asset is intentionally
/// retained; only reference-aware sample cleanup may remove it later.
///
/// # Errors
///
/// Refuses a stale project, a diverged frozen track, resource collisions, or
/// any document delta that cannot be proven against retained freeze authority.
pub fn publish_audio_track_unfreeze(
    project_path: impl AsRef<Path>,
    request: AudioTrackUnfreezePublicationRequest<'_>,
) -> Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError> {
    publish_unfreeze_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers interrupted unfreeze publications without accepting filesystem
/// contents outside the authenticated journal.
///
/// # Errors
///
/// Returns an error when neither the pre-unfreeze nor post-unfreeze package
/// can be proven from the transaction record.
pub fn recover_audio_track_unfreezes(
    project_path: impl AsRef<Path>,
) -> Result<Vec<AudioTrackUnfreezePublicationRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for track-unfreeze recovery",
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
    recover_unfreeze_locked(&root_path, &root, &backups)
}

#[allow(
    clippy::too_many_lines,
    reason = "freeze validates its render, source authority, journal, and manifest boundary together"
)]
fn publish_observed(
    project_path: &Path,
    request: AudioTrackFreezePublicationRequest<'_>,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError> {
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before track freeze",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after track-freeze preparation",
        ));
    }
    let (entries, state, sample_plan) = resolve_entries(&verified, request)?;
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
            "a prior track freeze was recovered; reopen the current project revision",
        ));
    }
    reject_other_transactions(&backups, ProjectTransactionFamily::AudioTrackFreeze)?;
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before track-freeze staging",
            error,
        )
    })?;
    let previous_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before track-freeze staging",
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
    let candidate_project = candidate_manifest(
        &previous_project,
        &entries,
        request.documents.frozen_asset_id(),
        request.rendered_sample,
        next_revision,
    )?;
    let name = format!("{AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create track-freeze transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-freeze transaction",
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
        &state,
        sample_plan,
        request.rendered_sample,
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

#[allow(
    clippy::too_many_lines,
    reason = "unfreeze keeps source authority, staged replacements, and its manifest commit visible together"
)]
fn publish_unfreeze_observed(
    project_path: &Path,
    request: AudioTrackUnfreezePublicationRequest<'_>,
    mut observer: impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError> {
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before track unfreeze",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after track-unfreeze preparation",
        ));
    }
    let (entries, state) = resolve_unfreeze_entries(&verified, request.documents)?;
    let authorities = resource_authorities(&verified.root_dir, &entries)?;
    let backups = open_or_create_dir(&verified.root_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "acquire project storage transaction lock",
            error,
        )
    })?;
    let recovered = recover_unfreeze_locked(verified.root(), &verified.root_dir, &backups)?;
    if !recovered.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "a prior track unfreeze was recovered; reopen the current project revision",
        ));
    }
    reject_other_transactions(&backups, ProjectTransactionFamily::AudioTrackUnfreeze)?;
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before track-unfreeze staging",
            error,
        )
    })?;
    let previous_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&previous_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before track-unfreeze staging",
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
        candidate_unfreeze_manifest(&previous_project, &entries, next_revision)?;
    let name = format!(
        "{AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX}{}",
        Uuid::new_v4()
    );
    backups.create_dir(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create track-unfreeze transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-unfreeze transaction",
            error,
        )
    })?;
    let result = run_unfreeze_publication(
        &verified,
        &backups,
        &transaction,
        &name,
        &entries,
        &authorities,
        &state,
        request.documents,
        &previous_project,
        &candidate_project,
        next_revision,
        &mut observer,
    );
    finish_unfreeze_result(
        verified.root(),
        &verified.root_dir,
        &backups,
        transaction,
        &name,
        result,
    )
}

#[allow(
    clippy::too_many_lines,
    reason = "freeze source, rendered replacement, and retained unfreeze authority are validated as one graph"
)]
fn resolve_entries<'a>(
    verified: &StructurallyVerifiedProject,
    request: AudioTrackFreezePublicationRequest<'a>,
) -> Result<(Vec<Entry<'a>>, TrackFreezeState, SamplePlan), ProjectResourcePublicationError> {
    let documents = request.documents;
    validate_canonical_resource(documents.track())?;
    validate_canonical_resource(documents.clip())?;
    validate_canonical_resource(documents.arrangement())?;
    validate_canonical_resource(documents.freeze_state())?;
    let state = TrackFreezeState::parse(documents.freeze_state()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse track-freeze state",
            error,
        )
    })?;
    if state.id() != documents.freeze_state_id()
        || state.project_id() != verified.manifest().project_id()
        || state.frozen_clip_id() != documents.frozen_clip_id()
        || state.frozen_asset_id() != documents.frozen_asset_id()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze state does not match its publication documents",
        ));
    }
    if !state.has_complete_rendered_authority() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "new track freezes must retain complete rendered authority",
        ));
    }
    if let Some(chain_id) = state.device_chain_id() {
        let descriptor = verified
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| {
                descriptor.id() == chain_id && descriptor.role() == ResourceRole::DeviceChain
            })
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::InvalidResource,
                    "track-freeze state references a missing device chain",
                )
            })?;
        if state.device_chain_sha256() != Some(descriptor.sha256()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-freeze device-chain authority differs from the active project",
            ));
        }
    }
    let frozen_state_clip = serde_json_canonicalizer::to_vec(state.frozen_clip().document())
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "serialize retained frozen clip",
                error,
            )
        })?;
    if frozen_state_clip != documents.clip() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze state does not retain the rendered clip exactly",
        ));
    }
    validate_document(documents.track(), state.track_id(), ResourceRole::Track)?;
    validate_document(
        documents.clip(),
        documents.frozen_clip_id(),
        ResourceRole::Clip,
    )?;
    validate_document(
        documents.arrangement(),
        state.arrangement_id(),
        ResourceRole::Arrangement,
    )?;
    validate_freeze_state_document(documents.freeze_state(), state.id())?;
    validate_rendered_sample(
        request.rendered_sample,
        verified.manifest().sample_rate_hz(),
        documents.clip(),
    )?;
    let track_before = current_resource(verified, state.track_id(), ResourceRole::Track)?;
    let arrangement_before =
        current_resource(verified, state.arrangement_id(), ResourceRole::Arrangement)?;
    validate_freeze_document_delta(&state, documents, &track_before.1, &arrangement_before.1)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(4 + state.original_clips().len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate track-freeze resource entries",
            )
        })?;
    entries.push(Entry {
        id: state.track_id().to_owned(),
        role: ResourceRole::Track,
        path: track_before.0,
        before: Some(track_before.1),
        after: Some(documents.track()),
        created_parent_directory: false,
    });
    entries.push(Entry {
        id: state.arrangement_id().to_owned(),
        role: ResourceRole::Arrangement,
        path: arrangement_before.0,
        before: Some(arrangement_before.1),
        after: Some(documents.arrangement()),
        created_parent_directory: false,
    });
    for clip in state.original_clips() {
        let (path, bytes) = current_resource(verified, clip.id(), ResourceRole::Clip)?;
        let retained = serde_json_canonicalizer::to_vec(clip.document()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "serialize retained original clip",
                error,
            )
        })?;
        if bytes != retained {
            return Err(failure(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "original clip differs from retained track-freeze authority",
            ));
        }
        entries.push(Entry {
            id: clip.id().to_owned(),
            role: ResourceRole::Clip,
            path,
            before: Some(bytes),
            after: None,
            created_parent_directory: false,
        });
    }
    let frozen_clip_path =
        fresh_resource_path(verified, ResourceRole::Clip, documents.frozen_clip_id())?;
    entries.push(Entry {
        id: documents.frozen_clip_id().to_owned(),
        role: ResourceRole::Clip,
        path: frozen_clip_path,
        before: None,
        after: Some(documents.clip()),
        created_parent_directory: false,
    });
    let (freeze_state_path, created_parent_directory) =
        fresh_freeze_state_path(verified, documents.freeze_state_id())?;
    entries.push(Entry {
        id: documents.freeze_state_id().to_owned(),
        role: ResourceRole::FreezeState,
        path: freeze_state_path,
        before: None,
        after: Some(documents.freeze_state()),
        created_parent_directory,
    });
    let sample_plan = fresh_sample_plan(verified, documents.frozen_asset_id())?;
    if entries.len() > MAX_ENTRIES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-freeze resource count exceeds package limits",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in &entries {
        if !ids.insert(entry.id.as_str()) || !paths.insert(entry.path.as_str()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-freeze resource identities are not unique",
            ));
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((entries, state, sample_plan))
}

#[allow(
    clippy::too_many_lines,
    reason = "unfreeze reauthenticates the live rendered state and every retained source before staging"
)]
fn resolve_unfreeze_entries<'a>(
    verified: &StructurallyVerifiedProject,
    documents: &'a AudioTrackUnfreezeDocuments,
) -> Result<(Vec<Entry<'a>>, TrackFreezeState), ProjectResourcePublicationError> {
    validate_canonical_resource(documents.track())?;
    validate_canonical_resource(documents.arrangement())?;
    let track_id = track_id_from_track(documents.track())?;
    let arrangement_id = arrangement_id_from_arrangement(documents.arrangement())?;
    validate_document(documents.track(), &track_id, ResourceRole::Track)?;
    validate_document(
        documents.arrangement(),
        &arrangement_id,
        ResourceRole::Arrangement,
    )?;
    let (track_path, track_before) = current_resource(verified, &track_id, ResourceRole::Track)?;
    let (arrangement_path, arrangement_before) =
        current_resource(verified, &arrangement_id, ResourceRole::Arrangement)?;
    let (state_path, state_bytes) = current_resource(
        verified,
        documents.freeze_state_id(),
        ResourceRole::FreezeState,
    )?;
    validate_canonical_resource(&state_bytes)?;
    validate_freeze_state_document(&state_bytes, documents.freeze_state_id())?;
    let state = TrackFreezeState::parse(&state_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse active track-unfreeze state",
            error,
        )
    })?;
    if state.project_id() != verified.manifest().project_id()
        || state.track_id() != track_id
        || state.arrangement_id() != arrangement_id
        || state.frozen_clip_id() != documents.frozen_clip_id()
        || state.frozen_asset_id() != documents.frozen_asset_id()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-unfreeze documents do not match active retained authority",
        ));
    }
    let (frozen_clip_path, frozen_clip_before) =
        current_resource(verified, documents.frozen_clip_id(), ResourceRole::Clip)?;
    validate_unfreeze_document_delta(
        &state,
        documents,
        &track_before,
        &arrangement_before,
        &frozen_clip_before,
    )?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(4 + documents.clips().len())
        .map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "allocate track-unfreeze resource entries",
            )
        })?;
    entries.push(Entry {
        id: state.track_id().to_owned(),
        role: ResourceRole::Track,
        path: track_path,
        before: Some(track_before),
        after: Some(documents.track()),
        created_parent_directory: false,
    });
    entries.push(Entry {
        id: state.arrangement_id().to_owned(),
        role: ResourceRole::Arrangement,
        path: arrangement_path,
        before: Some(arrangement_before),
        after: Some(documents.arrangement()),
        created_parent_directory: false,
    });
    entries.push(Entry {
        id: documents.frozen_clip_id().to_owned(),
        role: ResourceRole::Clip,
        path: frozen_clip_path,
        before: Some(frozen_clip_before),
        after: None,
        created_parent_directory: false,
    });
    entries.push(Entry {
        id: documents.freeze_state_id().to_owned(),
        role: ResourceRole::FreezeState,
        path: state_path,
        before: Some(state_bytes),
        after: None,
        created_parent_directory: false,
    });
    for clip in documents.clips() {
        validate_canonical_resource(clip.bytes())?;
        validate_document(clip.bytes(), clip.id(), ResourceRole::Clip)?;
        entries.push(Entry {
            id: clip.id().to_owned(),
            role: ResourceRole::Clip,
            path: fresh_resource_path(verified, ResourceRole::Clip, clip.id())?,
            before: None,
            after: Some(clip.bytes()),
            created_parent_directory: false,
        });
    }
    if entries.len() > MAX_ENTRIES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-unfreeze resource count exceeds package limits",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    if entries
        .iter()
        .any(|entry| !ids.insert(entry.id.as_str()) || !paths.insert(entry.path.as_str()))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-unfreeze resource identities are not unique",
        ));
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok((entries, state))
}

fn track_id_from_track(bytes: &[u8]) -> Result<String, ProjectResourcePublicationError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse restored track",
            error,
        )
    })?;
    value["id"].as_str().map(str::to_owned).ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "restored track has no identity",
        )
    })
}

fn arrangement_id_from_arrangement(
    bytes: &[u8],
) -> Result<String, ProjectResourcePublicationError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse restored arrangement",
            error,
        )
    })?;
    value["id"].as_str().map(str::to_owned).ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "restored arrangement has no identity",
        )
    })
}

fn current_resource(
    verified: &StructurallyVerifiedProject,
    id: &str,
    role: ResourceRole,
) -> Result<(String, Vec<u8>), ProjectResourcePublicationError> {
    let descriptor = verified
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == id)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "track-freeze source resource is absent from the active project",
            )
        })?;
    if descriptor.role() != role {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-freeze source resource role differs from the active project",
        ));
    }
    let bytes = verified.read_resource(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "read active track-freeze source resource",
            error,
        )
    })?;
    Ok((descriptor.path().to_owned(), bytes))
}

fn fresh_resource_path(
    verified: &StructurallyVerifiedProject,
    role: ResourceRole,
    id: &str,
) -> Result<String, ProjectResourcePublicationError> {
    if !is_valid_entity_id(id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze resource identity is invalid",
        ));
    }
    let directory = match role {
        ResourceRole::Clip => "clips",
        _ => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "track-freeze cannot create this resource role",
            ));
        }
    };
    let path = format!("{directory}/{id}.json");
    if verified
        .manifest()
        .resources()
        .iter()
        .any(|descriptor| descriptor.id() == id || descriptor.path().eq_ignore_ascii_case(&path))
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|asset| asset.id() == id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-freeze resource collides with the active project",
        ));
    }
    let parent = open_relative_parent(&verified.root_dir, &path)?;
    match parent.symlink_metadata(file_name(&path)?) {
        Ok(_) => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "track-freeze resource path already exists",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect track-freeze resource destination",
                error,
            ));
        }
    }
    Ok(path)
}

fn fresh_freeze_state_path(
    verified: &StructurallyVerifiedProject,
    id: &str,
) -> Result<(String, bool), ProjectResourcePublicationError> {
    if !is_valid_entity_id(id)
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == id)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|asset| asset.id() == id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "track-freeze state identity collides with the active project",
        ));
    }
    let path = format!("freezes/{id}.json");
    let created_parent_directory = match verified.root_dir.symlink_metadata("freezes") {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-freeze state parent is not a real directory",
            ));
        }
        Ok(_) => {
            let parent = verified
                .root_dir
                .open_dir_nofollow("freezes")
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Io,
                        "open track-freeze state parent",
                        error,
                    )
                })?;
            match parent.symlink_metadata(file_name(&path)?) {
                Ok(_) => {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::ResourceConflict,
                        "track-freeze state path already exists",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(caused(
                        ProjectResourcePublicationErrorKind::Io,
                        "inspect track-freeze state destination",
                        error,
                    ));
                }
            }
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect track-freeze state parent",
                error,
            ));
        }
    };
    Ok((path, created_parent_directory))
}

fn fresh_sample_plan(
    verified: &StructurallyVerifiedProject,
    id: &str,
) -> Result<SamplePlan, ProjectResourcePublicationError> {
    if !is_valid_entity_id(id)
        || verified
            .manifest()
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == id)
        || verified
            .manifest()
            .assets()
            .iter()
            .any(|asset| asset.id() == id)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "rendered track-freeze sample identity collides with the active project",
        ));
    }
    let assets = match verified.root_dir.symlink_metadata("assets") {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-freeze asset parent is not a real directory",
            ));
        }
        Ok(_) => Some(
            verified
                .root_dir
                .open_dir_nofollow("assets")
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Io,
                        "open track-freeze asset parent",
                        error,
                    )
                })?,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Io,
                "inspect track-freeze asset parent",
                error,
            ));
        }
    };
    let Some(assets) = assets else {
        return Ok(SamplePlan {
            created_assets_directory: true,
            created_samples_directory: true,
        });
    };
    match assets.symlink_metadata("samples") {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze sample parent is not a real directory",
        )),
        Ok(_) => {
            let samples = assets.open_dir_nofollow("samples").map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Io,
                    "open track-freeze sample parent",
                    error,
                )
            })?;
            let name = format!("{id}.wav");
            match samples.symlink_metadata(&name) {
                Ok(_) => {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::ResourceConflict,
                        "track-freeze sample path already exists",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(caused(
                        ProjectResourcePublicationErrorKind::Io,
                        "inspect track-freeze sample destination",
                        error,
                    ));
                }
            }
            Ok(SamplePlan {
                created_assets_directory: false,
                created_samples_directory: false,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(SamplePlan {
            created_assets_directory: false,
            created_samples_directory: true,
        }),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "inspect track-freeze sample parent",
            error,
        )),
    }
}

fn validate_document(
    bytes: &[u8],
    id: &str,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    if !is_valid_entity_id(id) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze resource identity is invalid",
        ));
    }
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse track-freeze resource",
            error,
        )
    })?;
    let version = value["format_version"].as_u64();
    let valid_version = match role {
        ResourceRole::Clip => matches!(version, Some(1 | 2)),
        ResourceRole::Track | ResourceRole::Arrangement => version == Some(1),
        _ => false,
    };
    if value["format"].as_str() != Some(resource_format(role))
        || value["id"].as_str() != Some(id)
        || !value["extensions"].is_object()
        || !valid_version
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze resource document is invalid",
        ));
    }
    Ok(())
}

fn validate_freeze_state_document(
    bytes: &[u8],
    id: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let state = TrackFreezeState::parse(bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse track-freeze state resource",
            error,
        )
    })?;
    if state.id() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze state identity is invalid",
        ));
    }
    Ok(())
}

fn validate_rendered_sample(
    bytes: &[u8],
    sample_rate_hz: u32,
    clip_bytes: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let byte_limit = u64::try_from(bytes.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "rendered track-freeze sample is not addressable",
        )
    })?;
    if byte_limit == 0 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "rendered track-freeze sample is empty",
        ));
    }
    let limits = DecodeLimits::new(byte_limit, byte_limit, bytes.len()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "construct rendered track-freeze sample limits",
            error,
        )
    })?;
    let audio = decode_bytes(bytes.to_vec(), AudioContainer::Wav, sample_rate_hz, limits).map_err(
        |error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "decode rendered track-freeze sample",
                error,
            )
        },
    )?;
    let clip: Value = serde_json::from_slice(clip_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse rendered track-freeze clip",
            error,
        )
    })?;
    if clip["source"]["start_frame"].as_u64() != Some(0)
        || clip["source"]["frame_count"].as_u64() != u64::try_from(audio.frame_count()).ok()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "rendered track-freeze clip does not cover its complete sample",
        ));
    }
    Ok(())
}

fn validate_freeze_document_delta(
    state: &TrackFreezeState,
    documents: &AudioTrackFreezeDocuments,
    previous_track: &[u8],
    previous_arrangement: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let track: Value = serde_json::from_slice(documents.track()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse candidate frozen track",
            error,
        )
    })?;
    let old_track: Value = serde_json::from_slice(previous_track).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active frozen-track source",
            error,
        )
    })?;
    let arrangement: Value = serde_json::from_slice(documents.arrangement()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse candidate frozen arrangement",
            error,
        )
    })?;
    let old_arrangement: Value = serde_json::from_slice(previous_arrangement).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "parse active frozen arrangement source",
            error,
        )
    })?;
    if track["project_id"].as_str() != Some(state.project_id())
        || track["kind"].as_str() != Some("audio")
        || track["clip_ids"] != serde_json::json!([state.frozen_clip_id()])
        || old_track["id"].as_str() != Some(state.track_id())
        || old_track["project_id"].as_str() != Some(state.project_id())
        || old_track["kind"].as_str() != Some("audio")
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze track transformation is invalid",
        ));
    }
    let source_ids = old_track["clip_ids"].as_array().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "active track-freeze source clip collection is invalid",
        )
    })?;
    let retained_ids = state
        .original_clips()
        .iter()
        .map(|clip| Value::String(clip.id().to_owned()))
        .collect::<Vec<_>>();
    if source_ids != retained_ids.as_slice() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "track-freeze retained clips do not match the active track",
        ));
    }
    if arrangement["id"].as_str() != Some(state.arrangement_id())
        || arrangement["project_id"].as_str() != Some(state.project_id())
        || old_arrangement["id"].as_str() != Some(state.arrangement_id())
        || old_arrangement["project_id"].as_str() != Some(state.project_id())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze arrangement transformation is invalid",
        ));
    }
    let old_target = target_placements(&old_arrangement, state.track_id())?;
    let retained = state
        .original_placements()
        .iter()
        .map(|placement| placement.document().clone())
        .collect::<Vec<_>>();
    if old_target != retained {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "track-freeze retained placements do not match the active arrangement",
        ));
    }
    let candidate_target = target_placements(&arrangement, state.track_id())?;
    if candidate_target.len() != 1
        || candidate_target[0]["id"].as_str() != Some(state.frozen_placement_id())
        || candidate_target[0]["clip_id"].as_str() != Some(state.frozen_clip_id())
        || candidate_target[0]["start_tick"].as_u64() != Some(0)
        || candidate_target[0]["clip_start_tick"].as_u64() != Some(0)
        || candidate_target[0]["enabled"].as_bool() != Some(true)
        || state
            .frozen_placement()
            .is_none_or(|retained| &candidate_target[0] != retained.document())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "track-freeze rendered placement is invalid",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the prepublication unfreeze authority is validated as one complete track, clip, and placement graph"
)]
fn validate_unfreeze_document_delta(
    state: &TrackFreezeState,
    documents: &AudioTrackUnfreezeDocuments,
    previous_track: &[u8],
    previous_arrangement: &[u8],
    previous_frozen_clip: &[u8],
) -> Result<(), ProjectResourcePublicationError> {
    let retained_frozen_clip = serde_json_canonicalizer::to_vec(state.frozen_clip().document())
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "canonicalize retained frozen clip",
                error,
            )
        })?;
    if retained_frozen_clip != previous_frozen_clip {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active rendered clip differs from retained track-freeze authority",
        ));
    }
    let previous_track: Value = serde_json::from_slice(previous_track).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse active frozen track",
            error,
        )
    })?;
    let restored_track: Value = serde_json::from_slice(documents.track()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "parse restored track",
            error,
        )
    })?;
    if previous_track["clip_ids"]
        .as_array()
        .is_none_or(|ids| ids.len() != 1 || ids[0].as_str() != Some(state.frozen_clip_id()))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active track no longer contains exactly its rendered clip",
        ));
    }
    let expected_ids = state
        .original_clips()
        .iter()
        .map(|clip| Value::String(clip.id().to_owned()))
        .collect::<Vec<_>>();
    if restored_track["clip_ids"].as_array() != Some(&expected_ids) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "restored track does not retain the exact original clip order",
        ));
    }
    let mut restored = BTreeMap::new();
    for clip in documents.clips() {
        if restored.insert(clip.id(), clip.bytes()).is_some() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "restored clip identities are duplicated",
            ));
        }
    }
    if restored.len() != state.original_clips().len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "restored clip count differs from retained authority",
        ));
    }
    for original in state.original_clips() {
        let expected = serde_json_canonicalizer::to_vec(original.document()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "canonicalize retained original clip",
                error,
            )
        })?;
        if restored.get(original.id()).copied() != Some(expected.as_slice()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "restored clip differs from retained authority",
            ));
        }
    }
    let previous_arrangement: Value =
        serde_json::from_slice(previous_arrangement).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "parse active frozen arrangement",
                error,
            )
        })?;
    let restored_arrangement: Value =
        serde_json::from_slice(documents.arrangement()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "parse restored arrangement",
                error,
            )
        })?;
    let previous_placements = previous_arrangement["placements"]
        .as_array()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "active arrangement placements are invalid",
            )
        })?;
    let active_track_placements = previous_placements
        .iter()
        .filter(|placement| placement["track_id"].as_str() == Some(state.track_id()))
        .collect::<Vec<_>>();
    let frozen_duration = state
        .frozen_clip()
        .document()
        .get("duration_ticks")
        .and_then(Value::as_u64);
    if active_track_placements.len() != 1
        || active_track_placements[0]["id"].as_str() != Some(state.frozen_placement_id())
        || active_track_placements[0]["clip_id"].as_str() != Some(state.frozen_clip_id())
        || active_track_placements[0]["start_tick"].as_u64() != Some(0)
        || active_track_placements[0]["clip_start_tick"].as_u64() != Some(0)
        || active_track_placements[0]["duration_ticks"].as_u64() != frozen_duration
        || active_track_placements[0]["enabled"].as_bool() != Some(true)
        || state
            .frozen_placement()
            .is_some_and(|retained| active_track_placements[0] != retained.document())
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active arrangement no longer contains exactly its rendered placement",
        ));
    }
    let restored_placements = restored_arrangement["placements"]
        .as_array()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "restored arrangement placements are invalid",
            )
        })?;
    let restored_track_placements = restored_placements
        .iter()
        .filter(|placement| placement["track_id"].as_str() == Some(state.track_id()))
        .collect::<Vec<_>>();
    if restored_track_placements.len() != state.original_placements().len()
        || restored_track_placements
            .iter()
            .any(|placement| placement["id"].as_str() == Some(state.frozen_placement_id()))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "restored arrangement does not replace the rendered placement exactly",
        ));
    }
    for original in state.original_placements() {
        if !restored_track_placements
            .iter()
            .any(|placement| *placement == original.document())
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "restored arrangement placement differs from retained authority",
            ));
        }
    }
    Ok(())
}

fn target_placements(
    arrangement: &Value,
    track_id: &str,
) -> Result<Vec<Value>, ProjectResourcePublicationError> {
    arrangement["placements"]
        .as_array()
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-freeze arrangement placement collection is invalid",
            )
        })
        .map(|placements| {
            placements
                .iter()
                .filter(|placement| placement["track_id"].as_str() == Some(track_id))
                .cloned()
                .collect()
        })
}

fn candidate_manifest(
    active: &[u8],
    entries: &[Entry<'_>],
    frozen_asset_id: &str,
    rendered_sample: &[u8],
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
        match (entry.before.is_some(), entry.after) {
            (true, Some(after)) => {
                let descriptor = resources
                    .iter_mut()
                    .find(|descriptor| descriptor["id"].as_str() == Some(&entry.id))
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Integrity,
                            "track-freeze replacement descriptor disappeared",
                        )
                    })?;
                if descriptor["role"].as_str() != Some(role_name(entry.role))
                    || descriptor["path"].as_str() != Some(&entry.path)
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "track-freeze replacement descriptor changed",
                    ));
                }
                descriptor["byte_length"] = after.len().into();
                descriptor["sha256"] = hex_digest(digest(after)).into();
            }
            (false, Some(after)) => resources.push(serde_json::json!({
                "id": entry.id,
                "role": role_name(entry.role),
                "path": entry.path,
                "media_type": "application/json",
                "byte_length": after.len(),
                "sha256": hex_digest(digest(after)),
                "extensions": {},
            })),
            (true, None) => {
                let prior = resources.len();
                resources.retain(|descriptor| descriptor["id"].as_str() != Some(&entry.id));
                if resources.len().checked_add(1) != Some(prior) {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "track-freeze removed descriptor disappeared",
                    ));
                }
            }
            (false, None) => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-freeze transaction entry has no active side",
                ));
            }
        }
    }
    let assets = value["assets"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project asset declarations are invalid",
        )
    })?;
    assets.push(serde_json::json!({
        "id": frozen_asset_id,
        "role": "sample",
        "path": format!("assets/samples/{frozen_asset_id}.wav"),
        "media_type": "audio/wav",
        "byte_length": rendered_sample.len(),
        "sha256": hex_digest(digest(rendered_sample)),
        "extensions": {},
    }));
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize track-freeze project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate track-freeze manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate track-freeze manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

fn candidate_unfreeze_manifest(
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
        match (entry.before.is_some(), entry.after) {
            (true, Some(after)) => {
                let descriptor = resources
                    .iter_mut()
                    .find(|descriptor| descriptor["id"].as_str() == Some(&entry.id))
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Integrity,
                            "track-unfreeze replacement descriptor disappeared",
                        )
                    })?;
                if descriptor["role"].as_str() != Some(role_name(entry.role))
                    || descriptor["path"].as_str() != Some(&entry.path)
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "track-unfreeze replacement descriptor changed",
                    ));
                }
                descriptor["byte_length"] = after.len().into();
                descriptor["sha256"] = hex_digest(digest(after)).into();
            }
            (false, Some(after)) => resources.push(serde_json::json!({
                "id": entry.id, "role": role_name(entry.role), "path": entry.path,
                "media_type": "application/json", "byte_length": after.len(),
                "sha256": hex_digest(digest(after)), "extensions": {},
            })),
            (true, None) => {
                let before = resources.len();
                resources.retain(|descriptor| descriptor["id"].as_str() != Some(&entry.id));
                if resources.len().checked_add(1) != Some(before) {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Integrity,
                        "track-unfreeze retired descriptor disappeared",
                    ));
                }
            }
            (false, None) => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-unfreeze entry has no active side",
                ));
            }
        }
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize track-unfreeze project manifest",
            error,
        )
    })?;
    if bytes.len() > MAX_JSON_BYTES {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ResourceConflict,
            "candidate track-unfreeze manifest exceeds its size limit",
        ));
    }
    ProjectManifest::parse(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "candidate track-unfreeze manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

const fn resource_format(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Track => "hyper-track",
        ResourceRole::Clip => "hyper-audio-clip",
        ResourceRole::Arrangement => "hyper-arrangement",
        _ => "",
    }
}

const fn role_name(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Track => "track",
        ResourceRole::Clip => "clip",
        ResourceRole::Arrangement => "arrangement",
        ResourceRole::FreezeState => "freeze-state",
        _ => "",
    }
}

fn role_from_name(value: &str) -> Option<ResourceRole> {
    Some(match value {
        "track" => ResourceRole::Track,
        "clip" => ResourceRole::Clip,
        "arrangement" => ResourceRole::Arrangement,
        "freeze-state" => ResourceRole::FreezeState,
        _ => return None,
    })
}

fn resource_authorities(
    root: &Dir,
    entries: &[Entry<'_>],
) -> Result<BTreeMap<String, Handle>, ProjectResourcePublicationError> {
    let mut authorities = BTreeMap::new();
    for entry in entries.iter().filter(|entry| entry.before.is_some()) {
        let expected = entry.before.as_deref().expect("filtered");
        let authority = verified_file_authority(
            root,
            &entry.path,
            expected,
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "verify track-freeze source before staging",
        )?;
        authorities.insert(entry.path.clone(), authority);
    }
    Ok(authorities)
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
        .map_err(|error| caused(kind, "retain track-freeze source authority", error))?;
    let authority = Handle::from_file(retained.into_std())
        .map_err(|error| caused(kind, "identify track-freeze source authority", error))?;
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

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the render, source retirement, recovery journal, and manifest commit share one durable boundary"
)]
fn run_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    name: &str,
    entries: &[Entry<'_>],
    authorities: &BTreeMap<String, Handle>,
    state: &TrackFreezeState,
    sample_plan: SamplePlan,
    rendered_sample: &[u8],
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, name, entries.len())?;
    for (index, entry) in entries.iter().enumerate() {
        if let Some(before) = &entry.before {
            write_exclusive_synced(transaction, &previous_name(index), before)?;
        }
        if let Some(after) = entry.after {
            write_exclusive_synced(transaction, &staged_name(index), after)?;
        }
    }
    write_exclusive_synced(transaction, SAMPLE_STAGING_NAME, rendered_sample)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = transaction_record(
        name,
        entries,
        state,
        sample_plan,
        rendered_sample,
        verified,
        candidate_project,
        next_revision,
    )?;
    let bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-freeze transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Some(before) = &entry.before {
            let authority = authorities.get(&entry.path).ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-freeze source authority disappeared before publication",
                )
            })?;
            move_verified_resource(&verified.root_dir, transaction, entry, before, authority)?;
            observer(PublicationPhase::SourceMoved(index), verified.root())?;
        }
        if entry.after.is_some() {
            let parent = prepare_parent(&verified.root_dir, entry)?;
            publish_staged_file_no_replace(
                transaction,
                &staged_name(index),
                &parent,
                file_name(&entry.path)?,
                &format!(".track-freeze-{index}-{}.json", tx.id),
                "track-freeze resource",
            )?;
            observer(PublicationPhase::ResourcePublished(index), verified.root())?;
        }
    }
    let sample = tx.rendered_sample.as_ref().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze transaction has no rendered sample",
        )
    })?;
    let sample_parent = prepare_sample_parent(&verified.root_dir, &tx)?;
    transaction
        .hard_link(
            SAMPLE_STAGING_NAME,
            &sample_parent,
            file_name(&sample.path)?,
        )
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ResourceConflict,
                "publish rendered track-freeze sample without overwriting",
                error,
            )
        })?;
    sync_dir(&sample_parent)?;
    observer(PublicationPhase::SamplePublished, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-freeze candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".track-freeze-project-{}.json", tx.id),
        "track-freeze project manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    outcome(&tx)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "unfreeze retains every current rendered resource until a verified manifest commits its restoration"
)]
fn run_unfreeze_publication(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    name: &str,
    entries: &[Entry<'_>],
    authorities: &BTreeMap<String, Handle>,
    state: &TrackFreezeState,
    documents: &AudioTrackUnfreezeDocuments,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(PublicationPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError> {
    write_unfreeze_intent(transaction, name, entries.len())?;
    for (index, entry) in entries.iter().enumerate() {
        if let Some(before) = &entry.before {
            write_exclusive_synced(transaction, &previous_name(index), before)?;
        }
        if let Some(after) = entry.after {
            write_exclusive_synced(transaction, &staged_name(index), after)?;
        }
    }
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx = unfreeze_transaction_record(
        name,
        entries,
        state,
        documents,
        verified,
        candidate_project,
        next_revision,
    )?;
    let bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-unfreeze transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(PublicationPhase::Transaction, verified.root())?;
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Some(before) = &entry.before {
            let authority = authorities.get(&entry.path).ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-unfreeze source authority disappeared before publication",
                )
            })?;
            move_verified_resource(&verified.root_dir, transaction, entry, before, authority)?;
            observer(PublicationPhase::SourceMoved(index), verified.root())?;
        }
        if entry.after.is_some() {
            let parent = prepare_parent(&verified.root_dir, entry)?;
            publish_staged_file_no_replace(
                transaction,
                &staged_name(index),
                &parent,
                file_name(&entry.path)?,
                &format!(".track-unfreeze-{index}-{}.json", tx.id),
                "track-unfreeze resource",
            )?;
            observer(PublicationPhase::ResourcePublished(index), verified.root())?;
        }
    }
    for directory in &tx.removed_directories {
        verified.root_dir.remove_dir(directory).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty track-unfreeze resource directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
    }
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "track-unfreeze candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-unfreeze candidate revision changed",
        ));
    }
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".track-unfreeze-project-{}.json", tx.id),
        "track-unfreeze project manifest",
    )?;
    observer(PublicationPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    unfreeze_outcome(&tx)
}

fn unfreeze_transaction_record(
    name: &str,
    entries: &[Entry<'_>],
    state: &TrackFreezeState,
    documents: &AudioTrackUnfreezeDocuments,
    verified: &StructurallyVerifiedProject,
    candidate_project: &[u8],
    revision: u64,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let removed_directories = removed_directories(verified, entries)?;
    let transaction_entries = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| TransactionEntry {
            id: entry.id.clone(),
            role: role_name(entry.role).to_owned(),
            path: entry.path.clone(),
            before: entry
                .before
                .as_deref()
                .map(|bytes| transaction_file(&entry.path, bytes)),
            after: entry
                .after
                .map(|bytes| transaction_file(&entry.path, bytes)),
            backup_name: entry.before.as_ref().map(|_| backup_name(index)),
            created_parent_directory: entry.created_parent_directory,
        })
        .collect::<Vec<_>>();
    Ok(Transaction {
        format: UNFREEZE_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: revision,
        track_id: state.track_id().to_owned(),
        frozen_clip_id: documents.frozen_clip_id().to_owned(),
        frozen_asset_id: documents.frozen_asset_id().to_owned(),
        freeze_state_id: documents.freeze_state_id().to_owned(),
        rendered_sample: None,
        created_assets_directory: false,
        created_samples_directory: false,
        operation: TransactionOperation::Unfreeze,
        removed_directories,
        entries: transaction_entries,
        extensions: serde_json::Map::new(),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "the complete transaction record binds every durable publication input"
)]
fn transaction_record(
    name: &str,
    entries: &[Entry<'_>],
    state: &TrackFreezeState,
    sample_plan: SamplePlan,
    rendered_sample: &[u8],
    verified: &StructurallyVerifiedProject,
    candidate_project: &[u8],
    revision: u64,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let entries = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| TransactionEntry {
            id: entry.id.clone(),
            role: role_name(entry.role).to_owned(),
            path: entry.path.clone(),
            before: entry
                .before
                .as_deref()
                .map(|bytes| transaction_file(&entry.path, bytes)),
            after: entry
                .after
                .map(|bytes| transaction_file(&entry.path, bytes)),
            backup_name: entry.before.as_ref().map(|_| backup_name(index)),
            created_parent_directory: entry.created_parent_directory,
        })
        .collect::<Vec<_>>();
    Ok(Transaction {
        format: FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: revision,
        track_id: state.track_id().to_owned(),
        frozen_clip_id: state.frozen_clip_id().to_owned(),
        frozen_asset_id: state.frozen_asset_id().to_owned(),
        freeze_state_id: state.id().to_owned(),
        rendered_sample: Some(transaction_file(
            &format!("assets/samples/{}.wav", state.frozen_asset_id()),
            rendered_sample,
        )),
        created_assets_directory: sample_plan.created_assets_directory,
        created_samples_directory: sample_plan.created_samples_directory,
        operation: TransactionOperation::Freeze,
        entries,
        removed_directories: Vec::new(),
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

fn previous_name(index: usize) -> String {
    format!("previous-resource-{index}.json")
}

fn backup_name(index: usize) -> String {
    format!("source-resource-{index}.json")
}

fn prepare_parent(
    root: &Dir,
    entry: &TransactionEntry,
) -> Result<Dir, ProjectResourcePublicationError> {
    let parent = resource_parent(&entry.path)?;
    match root.open_dir_nofollow(parent) {
        Ok(parent) if !entry.created_parent_directory => Ok(parent),
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze resource parent appeared after transaction staging",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound && entry.created_parent_directory => {
            open_or_create_dir(root, parent)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze resource parent disappeared before publication",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-freeze resource parent",
            error,
        )),
    }
}

fn prepare_sample_parent(
    root: &Dir,
    tx: &Transaction,
) -> Result<Dir, ProjectResourcePublicationError> {
    if tx.created_assets_directory {
        root.create_dir("assets").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "create track-freeze asset directory",
                error,
            )
        })?;
        sync_dir(root)?;
    }
    let assets = root.open_dir_nofollow("assets").map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-freeze asset directory",
            error,
        )
    })?;
    if tx.created_samples_directory {
        assets.create_dir("samples").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "create track-freeze sample directory",
                error,
            )
        })?;
        sync_dir(&assets)?;
    }
    assets.open_dir_nofollow("samples").map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open track-freeze sample directory",
            error,
        )
    })
}

fn move_verified_resource(
    root: &Dir,
    transaction: &Dir,
    entry: &TransactionEntry,
    before: &TransactionFile,
    authority: &Handle,
) -> Result<(), ProjectResourcePublicationError> {
    let backup = entry.backup_name.as_deref().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "track-freeze source entry has no backup name",
        )
    })?;
    let parent = open_relative_parent(root, &entry.path)?;
    let name = file_name(&entry.path)?;
    parent.rename(name, transaction, backup).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "move track-freeze source into transaction",
            error,
        )
    })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    let expected = read_bounded(
        transaction,
        backup,
        usize::try_from(before.byte_length).map_err(|_| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-freeze source byte length is not addressable",
            )
        })?,
    )?;
    let moved = verified_file_authority(
        transaction,
        backup,
        &expected,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify moved track-freeze source",
    );
    if moved.as_ref().is_ok_and(|candidate| candidate == authority)
        && digest(&expected) == decode_digest(&before.sha256)?
    {
        return Ok(());
    }
    transaction.rename(backup, &parent, name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "restore changed track-freeze source after authority mismatch",
            error,
        )
    })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    match moved {
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "track-freeze source path changed before publication",
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
    entry_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = Intent {
        format: INTENT_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        entry_count,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-freeze transaction intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish track-freeze transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn write_unfreeze_intent(
    transaction: &Dir,
    name: &str,
    entry_count: usize,
) -> Result<(), ProjectResourcePublicationError> {
    let intent = Intent {
        format: UNFREEZE_INTENT_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        entry_count,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize track-unfreeze transaction intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish track-unfreeze transaction intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn transaction_id(name: &str) -> Result<String, ProjectResourcePublicationError> {
    let id = name
        .strip_prefix(AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX)
        .or_else(|| name.strip_prefix(AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction directory name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction identity is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction identity is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn resource_parent(path: &str) -> Result<&str, ProjectResourcePublicationError> {
    path.rsplit_once('/')
        .map(|(parent, _)| parent)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Integrity,
                "track-freeze resource path has no parent",
            )
        })
}

fn removed_directories(
    verified: &StructurallyVerifiedProject,
    entries: &[Entry<'_>],
) -> Result<Vec<String>, ProjectResourcePublicationError> {
    let retired = entries
        .iter()
        .filter(|entry| entry.before.is_some() && entry.after.is_none())
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut directories = BTreeSet::new();
    for entry in entries
        .iter()
        .filter(|entry| entry.before.is_some() && entry.after.is_none())
    {
        let directory = resource_parent(&entry.path)?;
        if entries.iter().any(|entry| {
            entry.after.is_some()
                && resource_parent(&entry.path).is_ok_and(|parent| parent == directory)
        }) {
            continue;
        }
        if verified
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| {
                resource_parent(descriptor.path()).is_ok_and(|parent| parent == directory)
            })
            .all(|descriptor| retired.contains(descriptor.id()))
        {
            directories.insert(directory.to_owned());
        }
    }
    Ok(directories.into_iter().collect())
}

fn reject_other_transactions(
    backups: &Dir,
    family: ProjectTransactionFamily,
) -> Result<(), ProjectResourcePublicationError> {
    let found = find_unhandled_project_transaction(backups, &[family]).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::TransactionUnavailable,
            "inspect project transactions before track freeze",
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
    result: Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError>,
) -> Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError> {
    match result {
        Ok(outcome) => {
            clear_and_remove(backups, transaction, name).map_err(|error| {
                ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "track freeze committed but transaction cleanup is incomplete"
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
                AudioTrackFreezePublicationRecoveryDisposition::RolledBack => {
                    clear_and_remove(backups, transaction, name)?;
                    Err(operation)
                }
                AudioTrackFreezePublicationRecoveryDisposition::Committed => {
                    let outcome = outcome(&tx)?;
                    clear_and_remove(backups, transaction, name).map_err(|error| {
                        ProjectResourcePublicationError {
                            kind: ProjectResourcePublicationErrorKind::Recovery,
                            message: "track freeze committed but transaction cleanup is incomplete"
                                .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        }
                    })?;
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
) -> Result<Vec<AudioTrackFreezePublicationRecovery>, ProjectResourcePublicationError> {
    let mut names = backups
        .entries()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read track-freeze backups",
                error,
            )
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with(AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX))
        .collect::<Vec<_>>();
    names.sort();
    let mut recovered = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open track-freeze recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_incomplete(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove(backups, transaction, &name)?;
                recovered.push(AudioTrackFreezePublicationRecovery {
                    transaction_id: id,
                    disposition: AudioTrackFreezePublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        if tx.operation != TransactionOperation::Freeze {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze journal has the wrong operation",
            ));
        }
        let disposition = recover_one(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove(backups, transaction, &name)?;
        recovered.push(AudioTrackFreezePublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(recovered)
}

fn finish_unfreeze_result(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: Dir,
    name: &str,
    result: Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError>,
) -> Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError> {
    match result {
        Ok(outcome) => {
            clear_and_remove(backups, transaction, name).map_err(|error| {
                ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "track unfreeze committed but transaction cleanup is incomplete"
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
                AudioTrackFreezePublicationRecoveryDisposition::RolledBack => {
                    clear_and_remove(backups, transaction, name)?;
                    Err(operation)
                }
                AudioTrackFreezePublicationRecoveryDisposition::Committed => {
                    let outcome = unfreeze_outcome(&tx)?;
                    clear_and_remove(backups, transaction, name).map_err(|error| {
                        ProjectResourcePublicationError {
                            kind: ProjectResourcePublicationErrorKind::Recovery,
                            message:
                                "track unfreeze committed but transaction cleanup is incomplete"
                                    .to_owned(),
                            published: true,
                            source: Some(Box::new(error)),
                        }
                    })?;
                    Ok(outcome)
                }
            }
        }
    }
}

fn recover_unfreeze_locked(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
) -> Result<Vec<AudioTrackUnfreezePublicationRecovery>, ProjectResourcePublicationError> {
    let mut names = backups
        .entries()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "read track-unfreeze backups",
                error,
            )
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| name.starts_with(AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX))
        .collect::<Vec<_>>();
    names.sort();
    let mut recovered = Vec::new();
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "open track-unfreeze recovery transaction",
                error,
            )
        })?;
        let tx = match read_transaction(&transaction, &name) {
            Ok(tx) => tx,
            Err(_) if missing_file(&transaction, TRANSACTION_NAME)? => {
                validate_incomplete(&transaction, &name)?;
                let id = transaction_id(&name)?;
                clear_and_remove(backups, transaction, &name)?;
                recovered.push(AudioTrackUnfreezePublicationRecovery {
                    transaction_id: id,
                    disposition: AudioTrackUnfreezePublicationRecoveryDisposition::RolledBack,
                });
                continue;
            }
            Err(error) => return Err(error),
        };
        if tx.operation != TransactionOperation::Unfreeze {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze journal has the wrong operation",
            ));
        }
        let disposition = match recover_one(root_path, root, backups, &transaction, &tx)? {
            AudioTrackFreezePublicationRecoveryDisposition::Committed => {
                AudioTrackUnfreezePublicationRecoveryDisposition::Committed
            }
            AudioTrackFreezePublicationRecoveryDisposition::RolledBack => {
                AudioTrackUnfreezePublicationRecoveryDisposition::RolledBack
            }
        };
        clear_and_remove(backups, transaction, &name)?;
        recovered.push(AudioTrackUnfreezePublicationRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(recovered)
}

#[allow(
    clippy::too_many_lines,
    reason = "every before-or-after recovery state is checked before active paths are changed"
)]
fn recover_one(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<AudioTrackFreezePublicationRecoveryDisposition, ProjectResourcePublicationError> {
    validate_staged_transaction(transaction, tx)?;
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    let active = digest(&read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES)?);
    if active == after {
        verify_committed(root_path, root, after).map_err(|source| {
            ProjectResourcePublicationError {
                kind: ProjectResourcePublicationErrorKind::Recovery,
                message: "committed track freeze could not be verified".to_owned(),
                published: true,
                source: Some(Box::new(source)),
            }
        })?;
        cleanup_temporaries(root, backups, tx)?;
        return Ok(AudioTrackFreezePublicationRecoveryDisposition::Committed);
    }
    if active != before {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze recovery found an unrelated project identity",
        ));
    }
    restore_removed_directories(root, tx)?;
    preflight_rollback(root, transaction, tx)?;
    for entry in &tx.entries {
        let Some(parent) = optional_resource_parent(root, entry)? else {
            continue;
        };
        let name = file_name(&entry.path)?;
        if let Some(after) = &entry.after {
            let after_digest = decode_digest(&after.sha256)?;
            let current = optional_bytes(&parent, name, MAX_JSON_BYTES as u64)?;
            if current
                .as_deref()
                .is_some_and(|bytes| digest(bytes) == after_digest)
            {
                parent.remove_file(name).map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "remove rolled-back track-freeze resource",
                        error,
                    )
                })?;
                sync_dir(&parent)?;
            }
        }
        if entry.before.is_some() && optional_bytes(&parent, name, MAX_JSON_BYTES as u64)?.is_none()
        {
            let backup = entry.backup_name.as_deref().ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-freeze source backup is absent from its journal",
                )
            })?;
            transaction.rename(backup, &parent, name).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "restore track-freeze source resource",
                    error,
                )
            })?;
            sync_dir(&parent)?;
            sync_dir(transaction)?;
        }
        if entry.created_parent_directory {
            drop(parent);
            remove_optional_empty_directory(root, resource_parent(&entry.path)?)?;
        }
    }
    if let Some(rendered_sample) = &tx.rendered_sample {
        let (sample_parent, sample) = optional_sample(root, rendered_sample)?;
        if let (Some(sample_parent), Some(_)) = (sample_parent, sample) {
            let sample_name = file_name(&rendered_sample.path)?;
            sample_parent.remove_file(sample_name).map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "remove rolled-back rendered track-freeze sample",
                    error,
                )
            })?;
            sync_dir(&sample_parent)?;
        }
        if tx.created_samples_directory
            && let Ok(assets) = root.open_dir_nofollow("assets")
        {
            remove_optional_empty_directory(&assets, "samples")?;
        }
        if tx.created_assets_directory {
            remove_optional_empty_directory(root, "assets")?;
        }
    }
    verify_retained_project_candidate(
        root,
        &read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?,
    )
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "rolled-back track-freeze project does not verify",
            error,
        )
    })?;
    cleanup_temporaries(root, backups, tx)?;
    Ok(AudioTrackFreezePublicationRecoveryDisposition::RolledBack)
}

fn preflight_rollback(
    root: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for entry in &tx.entries {
        let current = optional_resource_parent(root, entry)?
            .map(|parent| optional_bytes(&parent, file_name(&entry.path)?, MAX_JSON_BYTES as u64))
            .transpose()?
            .flatten();
        let before = entry
            .before
            .as_ref()
            .map(|before| decode_digest(&before.sha256))
            .transpose()?;
        let after = entry
            .after
            .as_ref()
            .map(|after| decode_digest(&after.sha256))
            .transpose()?;
        let backup = entry
            .backup_name
            .as_deref()
            .map(|name| optional_bytes(transaction, name, MAX_JSON_BYTES as u64))
            .transpose()?
            .flatten();
        match (
            before,
            after,
            current.as_deref().map(digest),
            backup.as_deref().map(digest),
        ) {
            (Some(before), Some(after), Some(value), Some(saved))
                if value == after && saved == before => {}
            (Some(before), Some(_), Some(value), None) if value == before => {}
            (Some(before), Some(_), None, Some(saved)) if saved == before => {}
            (Some(before), None, Some(value), None) if value == before => {}
            (Some(before), None, None, Some(saved)) if saved == before => {}
            (None, Some(after), None, None) => {
                let _ = after;
            }
            (None, Some(after), Some(value), None) if value == after => {}
            _ => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-freeze rollback refuses unrelated resource content",
                ));
            }
        }
    }
    if let Some(rendered_sample) = &tx.rendered_sample {
        let (sample_parent, sample) = optional_sample(root, rendered_sample)?;
        if let Some(sample) = sample
            && digest(&sample) != decode_digest(&rendered_sample.sha256)?
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze rollback refuses an unrelated rendered sample",
            ));
        }
        let _ = sample_parent;
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "all durable journal evidence is authenticated before recovery changes active paths"
)]
fn validate_staged_transaction(
    transaction: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    if tx.operation == TransactionOperation::Unfreeze {
        return validate_unfreeze_staged_transaction(transaction, tx);
    }
    let rendered_sample = tx.rendered_sample.as_ref().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction has no rendered sample",
        )
    })?;
    let previous = read_bounded(transaction, PREVIOUS_PROJECT_NAME, MAX_JSON_BYTES)?;
    let candidate = read_bounded(transaction, PROJECT_STAGING_NAME, MAX_JSON_BYTES)?;
    if digest(&previous) != decode_digest(&tx.before_project_sha256)?
        || digest(&candidate) != decode_digest(&tx.after_project_sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze staged project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous track-freeze project is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate track-freeze project is invalid",
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
            "track-freeze staged project revisions are inconsistent",
        ));
    }
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "track-freeze transaction intent")?;
    let intent: Intent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-freeze transaction intent",
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
            "track-freeze transaction intent is invalid",
        ));
    }
    let mut expected_names = BTreeSet::from([
        INTENT_NAME.to_owned(),
        PROJECT_STAGING_NAME.to_owned(),
        PREVIOUS_PROJECT_NAME.to_owned(),
        TRANSACTION_NAME.to_owned(),
        SAMPLE_STAGING_NAME.to_owned(),
    ]);
    let sample_max = usize::try_from(rendered_sample.byte_length).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged rendered sample length is not addressable",
        )
    })?;
    let sample = read_bounded(transaction, SAMPLE_STAGING_NAME, sample_max)?;
    if sample.len() as u64 != rendered_sample.byte_length
        || digest(&sample) != decode_digest(&rendered_sample.sha256)?
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged rendered track-freeze sample changed",
        ));
    }
    let mut resource_ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut retained_state = None;
    let mut frozen_clip_bytes = None;
    for (index, entry) in tx.entries.iter().enumerate() {
        if !resource_ids.insert(entry.id.as_str()) || !paths.insert(entry.path.as_str()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction resource identities are not unique",
            ));
        }
        let role = role_from_name(&entry.role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction contains an unsupported resource role",
            )
        })?;
        if !is_valid_entity_id(&entry.id) || entry.before.is_none() && entry.after.is_none() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction resource entry is invalid",
            ));
        }
        match (&entry.before, &entry.after, &entry.backup_name) {
            (Some(before), after, Some(backup)) if backup == &backup_name(index) => {
                let max = usize::try_from(before.byte_length).map_err(|_| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "staged track-freeze source length is not addressable",
                    )
                })?;
                let bytes = read_bounded(transaction, &previous_name(index), max)?;
                if bytes.len() as u64 != before.byte_length
                    || digest(&bytes) != decode_digest(&before.sha256)?
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "staged track-freeze source changed",
                    ));
                }
                validate_entry_document(&bytes, &entry.id, role)?;
                expected_names.insert(previous_name(index));
                expected_names.insert(backup.clone());
                let previous_descriptor = previous_manifest
                    .resources()
                    .iter()
                    .find(|descriptor| descriptor.id() == entry.id)
                    .ok_or_else(|| {
                        failure(
                            ProjectResourcePublicationErrorKind::Recovery,
                            "track-freeze source descriptor is absent from previous project",
                        )
                    })?;
                if previous_descriptor.role() != role || previous_descriptor.path() != entry.path {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-freeze source descriptor changed",
                    ));
                }
                if after.is_none()
                    && candidate_manifest
                        .resources()
                        .iter()
                        .any(|descriptor| descriptor.id() == entry.id)
                {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "candidate track-freeze project retained a retired source",
                    ));
                }
            }
            (None, _, None) => {}
            _ => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-freeze transaction resource operation is invalid",
                ));
            }
        }
        if let Some(after) = &entry.after {
            let max = usize::try_from(after.byte_length).map_err(|_| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "staged track-freeze replacement length is not addressable",
                )
            })?;
            let bytes = read_bounded(transaction, &staged_name(index), max)?;
            if bytes.len() as u64 != after.byte_length
                || digest(&bytes) != decode_digest(&after.sha256)?
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "staged track-freeze replacement changed",
                ));
            }
            validate_entry_document(&bytes, &entry.id, role)?;
            if entry.id == tx.frozen_clip_id {
                frozen_clip_bytes = Some(bytes.clone());
            }
            if role == ResourceRole::FreezeState {
                retained_state = Some(TrackFreezeState::parse(&bytes).map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "parse staged track-freeze state",
                        error,
                    )
                })?);
            }
            expected_names.insert(staged_name(index));
            let descriptor = candidate_manifest
                .resources()
                .iter()
                .find(|descriptor| descriptor.id() == entry.id)
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "candidate track-freeze descriptor is absent",
                    )
                })?;
            if descriptor.role() != role
                || descriptor.path() != entry.path
                || descriptor.byte_length() != after.byte_length
                || descriptor.sha256() != after.sha256
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "candidate track-freeze descriptor changed",
                ));
            }
        }
    }
    let state = retained_state.ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction has no retained freeze-state resource",
        )
    })?;
    let frozen_clip_bytes = frozen_clip_bytes.ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction has no rendered clip resource",
        )
    })?;
    let retained_frozen_clip = serde_json_canonicalizer::to_vec(state.frozen_clip().document())
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "canonicalize retained track-freeze clip",
                error,
            )
        })?;
    let retired_source_ids = tx
        .entries
        .iter()
        .filter(|entry| entry.before.is_some() && entry.after.is_none())
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    if state.id() != tx.freeze_state_id
        || state.track_id() != tx.track_id
        || state.frozen_clip_id() != tx.frozen_clip_id
        || state.frozen_asset_id() != tx.frozen_asset_id
        || retained_frozen_clip != frozen_clip_bytes
        || state
            .original_clips()
            .iter()
            .map(hyper_track_freeze::FrozenClipState::id)
            .collect::<BTreeSet<_>>()
            != retired_source_ids
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze journal does not match retained unfreeze authority",
        ));
    }
    let sample_descriptor = candidate_manifest
        .assets()
        .iter()
        .find(|descriptor| descriptor.path() == rendered_sample.path)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "candidate rendered track-freeze sample descriptor is absent",
            )
        })?;
    if sample_descriptor.id() != tx.frozen_asset_id
        || sample_descriptor.role() != AssetRole::Sample
        || sample_descriptor.media_type() != AudioMediaType::AudioWav
        || sample_descriptor.byte_length() != rendered_sample.byte_length
        || sample_descriptor.sha256() != rendered_sample.sha256
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "candidate rendered track-freeze sample descriptor changed",
        ));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read track-freeze transaction entries",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read track-freeze transaction entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction contains a non-Unicode entry",
            ));
        };
        if !expected_names.contains(&name) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn validate_entry_document(
    bytes: &[u8],
    id: &str,
    role: ResourceRole,
) -> Result<(), ProjectResourcePublicationError> {
    match role {
        ResourceRole::FreezeState => validate_freeze_state_document(bytes, id),
        _ => validate_document(bytes, id, role),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "unfreeze recovery authenticates every retained source and restored destination before changing paths"
)]
fn validate_unfreeze_staged_transaction(
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
            "track-unfreeze staged project identity changed",
        ));
    }
    let previous_manifest = ProjectManifest::parse(&previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged previous track-unfreeze project is invalid",
            error,
        )
    })?;
    let candidate_manifest = ProjectManifest::parse(&candidate).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "staged candidate track-unfreeze project is invalid",
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
            "track-unfreeze staged project revisions are inconsistent",
        ));
    }
    let intent_bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
    require_canonical(&intent_bytes, "track-unfreeze transaction intent")?;
    let intent: Intent = serde_json::from_slice(&intent_bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-unfreeze transaction intent",
            error,
        )
    })?;
    if intent.format != UNFREEZE_INTENT_FORMAT
        || intent.format_version != VERSION
        || intent.id != tx.id
        || intent.entry_count != tx.entries.len()
        || !intent.extensions.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze transaction intent is invalid",
        ));
    }
    let previous_value: Value = serde_json::from_slice(&previous).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse previous track-unfreeze manifest",
            error,
        )
    })?;
    let candidate_value: Value = serde_json::from_slice(&candidate).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse candidate track-unfreeze manifest",
            error,
        )
    })?;
    if previous_value["assets"] != candidate_value["assets"] {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze changed asset declarations",
        ));
    }
    let mut expected_names = BTreeSet::from([
        INTENT_NAME.to_owned(),
        PROJECT_STAGING_NAME.to_owned(),
        PREVIOUS_PROJECT_NAME.to_owned(),
        TRANSACTION_NAME.to_owned(),
    ]);
    let mut state_bytes = None;
    let mut frozen_clip_bytes = None;
    let mut restored_clips = BTreeMap::new();
    let mut restored_track = None;
    let mut restored_arrangement = None;
    for (index, entry) in tx.entries.iter().enumerate() {
        let role = role_from_name(&entry.role).ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze transaction has unsupported resource role",
            )
        })?;
        if !is_valid_entity_id(&entry.id) || entry.before.is_none() && entry.after.is_none() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze transaction resource entry is invalid",
            ));
        }
        if let Some(before) = &entry.before {
            if entry.backup_name.as_deref() != Some(&backup_name(index)) {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-unfreeze source backup name is invalid",
                ));
            }
            let bytes = read_bounded(
                transaction,
                &previous_name(index),
                usize::try_from(before.byte_length).map_err(|_| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-unfreeze source length is not addressable",
                    )
                })?,
            )?;
            if bytes.len() as u64 != before.byte_length
                || digest(&bytes) != decode_digest(&before.sha256)?
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-unfreeze staged source changed",
                ));
            }
            if role == ResourceRole::FreezeState {
                validate_freeze_state_document(&bytes, &entry.id)?;
                state_bytes = Some(bytes.clone());
            } else {
                validate_entry_document(&bytes, &entry.id, role)?;
            }
            let descriptor = previous_manifest
                .resources()
                .iter()
                .find(|descriptor| descriptor.id() == entry.id)
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-unfreeze previous descriptor is absent",
                    )
                })?;
            if descriptor.role() != role || descriptor.path() != entry.path {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-unfreeze previous descriptor changed",
                ));
            }
            expected_names.insert(previous_name(index));
            expected_names.insert(backup_name(index));
            if entry.id == tx.frozen_clip_id {
                frozen_clip_bytes = Some(bytes);
            }
        } else if entry.backup_name.is_some() {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze added resource has backup authority",
            ));
        }
        if let Some(after) = &entry.after {
            let bytes = read_bounded(
                transaction,
                &staged_name(index),
                usize::try_from(after.byte_length).map_err(|_| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-unfreeze replacement length is not addressable",
                    )
                })?,
            )?;
            if bytes.len() as u64 != after.byte_length
                || digest(&bytes) != decode_digest(&after.sha256)?
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-unfreeze staged replacement changed",
                ));
            }
            validate_entry_document(&bytes, &entry.id, role)?;
            let descriptor = candidate_manifest
                .resources()
                .iter()
                .find(|descriptor| descriptor.id() == entry.id)
                .ok_or_else(|| {
                    failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "track-unfreeze candidate descriptor is absent",
                    )
                })?;
            if descriptor.role() != role
                || descriptor.path() != entry.path
                || descriptor.byte_length() != after.byte_length
                || descriptor.sha256() != after.sha256
            {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-unfreeze candidate descriptor changed",
                ));
            }
            expected_names.insert(staged_name(index));
            match role {
                ResourceRole::Track => restored_track = Some(bytes),
                ResourceRole::Arrangement => restored_arrangement = Some(bytes),
                ResourceRole::Clip if entry.before.is_none() => {
                    restored_clips.insert(entry.id.as_str(), bytes);
                }
                _ => {}
            }
        } else if candidate_manifest
            .resources()
            .iter()
            .any(|descriptor| descriptor.id() == entry.id)
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze candidate retained a retired resource",
            ));
        }
    }
    let state = TrackFreezeState::parse(&state_bytes.ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze transaction has no retained freeze state",
        )
    })?)
    .map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse retained track-unfreeze state",
            error,
        )
    })?;
    let frozen_clip = frozen_clip_bytes.ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze transaction has no retained rendered clip",
        )
    })?;
    let retained_frozen_clip = serde_json_canonicalizer::to_vec(state.frozen_clip().document())
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "canonicalize retained rendered clip",
                error,
            )
        })?;
    if state.id() != tx.freeze_state_id
        || state.track_id() != tx.track_id
        || state.frozen_clip_id() != tx.frozen_clip_id
        || state.frozen_asset_id() != tx.frozen_asset_id
        || frozen_clip != retained_frozen_clip
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze retained authority is inconsistent",
        ));
    }
    if restored_track.is_none()
        || restored_arrangement.is_none()
        || restored_clips.len() != state.original_clips().len()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze restored resource set is incomplete",
        ));
    }
    for original in state.original_clips() {
        let expected = serde_json_canonicalizer::to_vec(original.document()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "canonicalize retained original clip",
                error,
            )
        })?;
        if restored_clips
            .get(original.id())
            .is_none_or(|bytes| *bytes != expected)
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze restored clip differs from retained authority",
            ));
        }
    }
    let retired = tx
        .entries
        .iter()
        .filter(|entry| entry.before.is_some() && entry.after.is_none())
        .map(|entry| entry.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut expected_directories = BTreeSet::new();
    for entry in tx
        .entries
        .iter()
        .filter(|entry| entry.before.is_some() && entry.after.is_none())
    {
        let directory = resource_parent(&entry.path)?;
        if tx.entries.iter().any(|entry| {
            entry.after.is_some()
                && resource_parent(&entry.path).is_ok_and(|parent| parent == directory)
        }) {
            continue;
        }
        if previous_manifest
            .resources()
            .iter()
            .filter(|descriptor| {
                resource_parent(descriptor.path()).is_ok_and(|parent| parent == directory)
            })
            .all(|descriptor| retired.contains(descriptor.id()))
        {
            expected_directories.insert(directory.to_owned());
        }
    }
    if tx.removed_directories.iter().collect::<BTreeSet<_>>()
        != expected_directories.iter().collect::<BTreeSet<_>>()
        || tx
            .removed_directories
            .iter()
            .any(|directory| directory.is_empty() || directory.contains('/'))
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-unfreeze removed directories are inconsistent",
        ));
    }
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read track-unfreeze transaction entries",
            error,
        )
    })? {
        let name = entry
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "read track-unfreeze transaction entry",
                    error,
                )
            })?
            .file_name()
            .to_string_lossy()
            .into_owned();
        if !expected_names.contains(&name) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-unfreeze transaction contains an unknown entry",
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
    require_canonical(&bytes, "track-freeze transaction")?;
    let tx: Transaction = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse track-freeze transaction",
            error,
        )
    })?;
    let expected_format = match tx.operation {
        TransactionOperation::Freeze => FORMAT,
        TransactionOperation::Unfreeze => UNFREEZE_FORMAT,
    };
    let rendered_sample_valid = match (&tx.operation, &tx.rendered_sample) {
        (TransactionOperation::Freeze, Some(sample)) => {
            sample.path == format!("assets/samples/{}.wav", tx.frozen_asset_id)
                && sample.byte_length != 0
                && decode_digest(&sample.sha256).is_ok()
        }
        (TransactionOperation::Unfreeze, None) => true,
        _ => false,
    };
    if tx.format != expected_format
        || tx.format_version != VERSION
        || tx.id != transaction_id(name)?
        || !is_valid_entity_id(&tx.track_id)
        || !is_valid_entity_id(&tx.frozen_clip_id)
        || !is_valid_entity_id(&tx.frozen_asset_id)
        || !is_valid_entity_id(&tx.freeze_state_id)
        || tx.entries.is_empty()
        || tx.entries.len() > MAX_ENTRIES
        || tx.before_project_revision.checked_add(1) != Some(tx.after_project_revision)
        || decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || !rendered_sample_valid
        || !tx.extensions.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction is invalid",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for entry in &tx.entries {
        if !is_valid_entity_id(&entry.id)
            || !ids.insert(entry.id.as_str())
            || !paths.insert(entry.path.as_str())
            || entry.before.is_none() && entry.after.is_none()
            || entry.before.as_ref().is_some_and(|before| {
                before.byte_length == 0 || decode_digest(&before.sha256).is_err()
            })
            || entry.after.as_ref().is_some_and(|after| {
                after.byte_length == 0 || decode_digest(&after.sha256).is_err()
            })
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "track-freeze transaction entries are invalid",
            ));
        }
    }
    Ok(tx)
}

fn optional_bytes(
    directory: &Dir,
    name: &str,
    maximum: u64,
) -> Result<Option<Vec<u8>>, ProjectResourcePublicationError> {
    match directory.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze recovery found a non-regular file",
        )),
        Ok(_) => usize::try_from(maximum)
            .map_err(|_| {
                failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "track-freeze recovery file length is not addressable",
                )
            })
            .and_then(|maximum| read_bounded(directory, name, maximum).map(Some)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "inspect track-freeze recovery resource",
            error,
        )),
    }
}

fn optional_sample(
    root: &Dir,
    sample: &TransactionFile,
) -> Result<(Option<Dir>, Option<Vec<u8>>), ProjectResourcePublicationError> {
    let assets = match root.open_dir_nofollow("assets") {
        Ok(assets) => assets,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "open track-freeze asset directory during recovery",
                error,
            ));
        }
    };
    let samples = match assets.open_dir_nofollow("samples") {
        Ok(samples) => samples,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((None, None)),
        Err(error) => {
            return Err(caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "open track-freeze sample directory during recovery",
                error,
            ));
        }
    };
    let bytes = optional_bytes(&samples, file_name(&sample.path)?, sample.byte_length)?;
    Ok((Some(samples), bytes))
}

fn optional_resource_parent(
    root: &Dir,
    entry: &TransactionEntry,
) -> Result<Option<Dir>, ProjectResourcePublicationError> {
    let parent = resource_parent(&entry.path)?;
    match root.open_dir_nofollow(parent) {
        Ok(parent) => Ok(Some(parent)),
        Err(error) if error.kind() == io::ErrorKind::NotFound && entry.created_parent_directory => {
            Ok(None)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze source parent disappeared during recovery",
        )),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open track-freeze resource parent during recovery",
            error,
        )),
    }
}

fn remove_optional_empty_directory(
    parent: &Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    match parent.remove_dir(name) {
        Ok(()) => sync_dir(parent),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove empty rolled-back track-freeze directory",
            error,
        )),
    }
}

fn restore_removed_directories(
    root: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for directory in &tx.removed_directories {
        match root.create_dir(directory) {
            Ok(()) => sync_dir(root)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = root.symlink_metadata(directory).map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "inspect restored track-unfreeze directory",
                        error,
                    )
                })?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "restored track-unfreeze directory is not real",
                    ));
                }
            }
            Err(error) => {
                return Err(caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "restore track-unfreeze directory",
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
    let operation = if name.starts_with(AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX) {
        TransactionOperation::Unfreeze
    } else {
        TransactionOperation::Freeze
    };
    let intent_format = match operation {
        TransactionOperation::Freeze => INTENT_FORMAT,
        TransactionOperation::Unfreeze => UNFREEZE_INTENT_FORMAT,
    };
    let count = if missing_file(transaction, INTENT_NAME)? {
        0
    } else {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "incomplete track-freeze intent")?;
        let intent: Intent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse incomplete track-freeze intent",
                error,
            )
        })?;
        if intent.format != intent_format
            || intent.format_version != VERSION
            || intent.id != transaction_id(name)?
            || intent.entry_count == 0
            || intent.entry_count > MAX_ENTRIES
            || !intent.extensions.is_empty()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-freeze intent is invalid",
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
    if operation == TransactionOperation::Freeze {
        expected.insert(SAMPLE_STAGING_NAME.to_owned());
    }
    for index in 0..count {
        expected.insert(previous_name(index));
        expected.insert(staged_name(index));
    }
    for item in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete track-freeze transaction",
            error,
        )
    })? {
        let item = item.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete track-freeze transaction entry",
                error,
            )
        })?;
        let Some(item) = item.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-freeze transaction contains a non-Unicode entry",
            ));
        };
        if !expected.contains(&item) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete track-freeze transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn cleanup_temporaries(
    root: &Dir,
    backups: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    for (index, entry) in tx.entries.iter().enumerate() {
        if let Some(after) = &entry.after {
            let Some(parent) = optional_resource_parent(root, entry)? else {
                continue;
            };
            remove_optional_matching_file(
                &parent,
                &format!(".{}-{index}-{}.json", transaction_label(tx), tx.id),
                decode_digest(&after.sha256)?,
            )?;
        }
    }
    remove_optional_matching_file(
        backups,
        &format!(".{}-project-{}.json", transaction_label(tx), tx.id),
        decode_digest(&tx.after_project_sha256)?,
    )
}

const fn transaction_label(tx: &Transaction) -> &'static str {
    match tx.operation {
        TransactionOperation::Freeze => "track-freeze",
        TransactionOperation::Unfreeze => "track-unfreeze",
    }
}

fn outcome(
    tx: &Transaction,
) -> Result<AudioTrackFreezePublicationOutcome, ProjectResourcePublicationError> {
    Ok(AudioTrackFreezePublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.track_id.clone(),
    })
}

fn unfreeze_outcome(
    tx: &Transaction,
) -> Result<AudioTrackUnfreezePublicationOutcome, ProjectResourcePublicationError> {
    Ok(AudioTrackUnfreezePublicationOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        track_id: tx.track_id.clone(),
    })
}

#[cfg(not(windows))]
fn clear_and_remove(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".track-freeze-cleanup-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine track-freeze transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined track-freeze transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined track-freeze transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "track-freeze transaction changed before cleanup",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove track-freeze transaction",
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
            "remove track-freeze transaction",
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
                "read track-freeze transaction for cleanup",
                error,
            )
        })?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read track-freeze transaction cleanup entry",
                error,
            )
        })?;
    names.sort();
    for name in names {
        transaction.remove_file(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "remove track-freeze transaction file",
                error,
            )
        })?;
    }
    sync_dir(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ProjectResourcePublicationRequest, SampleCleanupRequest, clean_unused_samples,
        publish_project_resource, recover_project_transactions, verify_project_package,
    };
    use hyper_audio_source::{DecodedAudio, encode_float_wav};
    use hyper_playback_json::{
        AudioTrackFreezeDeviceChain, LoaderLimits, MediaAssetFact, PlaybackJsonInput, load,
        serialize_audio_track_freeze, serialize_audio_track_unfreeze,
    };
    use serde_json::{Value, json};
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339001";
    const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339002";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339003";
    const SOURCE_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339004";
    const SOURCE_CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339005";
    const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339006";
    const SOURCE_PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339007";
    const FROZEN_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339008";
    const FROZEN_CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339009";
    const FROZEN_PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339010";
    const FREEZE_STATE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339011";
    const DEVICE_CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339012";
    const DEVICE_INSTANCE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339013";
    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: std::path::PathBuf,
        timeline: Vec<u8>,
        track: Vec<u8>,
        clip: Vec<u8>,
        arrangement: Vec<u8>,
        chain: Vec<u8>,
        sample: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-track-freeze-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed),
            ));
            for directory in [
                "timeline",
                "tracks",
                "clips",
                "arrangement",
                "devices",
                "assets/samples",
            ] {
                fs::create_dir_all(root.join(directory)).unwrap();
            }
            let timeline = canonical(json!({
                "format":"hyper-timeline","format_version":1,"id":TIMELINE_ID,
                "project_id":PROJECT_ID,"ticks_per_quarter":1920,
                "tempo_events":[{"tick":0,"microseconds_per_quarter":1_000_000}],
                "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
            }));
            let track = canonical(json!({
                "format":"hyper-track","format_version":1,"id":TRACK_ID,"project_id":PROJECT_ID,
                "name":"Lead","kind":"audio","order":0,"active":true,
                "clip_ids":[SOURCE_CLIP_ID],"extensions":{}
            }));
            let sample =
                encode_float_wav(&DecodedAudio::from_mono(48_000, vec![0.25; 50]).unwrap())
                    .unwrap();
            let clip = canonical(json!({
                "format":"hyper-audio-clip","format_version":1,"id":SOURCE_CLIP_ID,
                "project_id":PROJECT_ID,"track_id":TRACK_ID,"name":"Lead","enabled":true,
                "duration_ticks":2,"source":{"asset_id":SOURCE_ASSET_ID,"start_frame":0,"frame_count":50},
                "loop":{"enabled":false,"start_tick":0,"end_tick":2},
                "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
                "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
                "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":2,"source_frame_offset":50}],"extensions":{}
            }));
            let arrangement = canonical(json!({
                "format":"hyper-arrangement","format_version":1,"id":ARRANGEMENT_ID,"project_id":PROJECT_ID,
                "name":"Main","enabled":true,"placements":[{"id":SOURCE_PLACEMENT_ID,"track_id":TRACK_ID,
                    "clip_id":SOURCE_CLIP_ID,"start_tick":0,"clip_start_tick":0,"duration_ticks":2,"enabled":true}],
                "scene_launches":[],"extensions":{}
            }));
            let chain = canonical(json!({
                "format":"hyper-device-chain","format_version":1,"id":DEVICE_CHAIN_ID,
                "project_id":PROJECT_ID,"track_id":TRACK_ID,"devices":[{
                    "id":DEVICE_INSTANCE_ID,"device_id":"org.hyper-module.native-filter",
                    "state_version":1,"dsp_version":1,"enabled":true,
                    "state":{"mode":"low-pass","cutoff_millihertz":1_000_000,"resonance_millionths":707_107},
                    "extensions":{}
                }],"extensions":{}
            }));
            fs::write(root.join("timeline/timeline.json"), &timeline).unwrap();
            fs::write(root.join("tracks/lead.json"), &track).unwrap();
            fs::write(root.join("clips/lead.json"), &clip).unwrap();
            fs::write(root.join("arrangement/main.json"), &arrangement).unwrap();
            fs::write(root.join("devices/lead.json"), &chain).unwrap();
            fs::write(root.join("assets/samples/source.wav"), &sample).unwrap();
            let manifest = canonical(json!({
                "format":"hyper-project","format_version":1,"project_id":PROJECT_ID,"revision":1,
                "name":"Track freeze fixture","sample_rate_hz":48_000,
                "required_capabilities":{"format":"hyper-hardware-capabilities","format_version":1,
                    "audio_inputs":0,"audio_outputs":0,"cv_inputs":0,"cv_outputs":0,"gate_outputs":0,
                    "has_clock_input":false,"has_reset_input":false,"has_clock_output":false,
                    "usb_mode":"none","has_ethernet":false,"extensions":{}},
                "resources":[
                    descriptor(TIMELINE_ID,"timeline","timeline/timeline.json",&timeline),
                    descriptor(TRACK_ID,"track","tracks/lead.json",&track),
                    descriptor(SOURCE_CLIP_ID,"clip","clips/lead.json",&clip),
                    descriptor(ARRANGEMENT_ID,"arrangement","arrangement/main.json",&arrangement),
                    descriptor(DEVICE_CHAIN_ID,"device-chain","devices/lead.json",&chain)
                ],"assets":[{"id":SOURCE_ASSET_ID,"role":"sample","path":"assets/samples/source.wav",
                    "media_type":"audio/wav","byte_length":sample.len(),"sha256":hex_digest(digest(&sample)),"extensions":{}}],
                "extensions":{}
            }));
            fs::write(root.join(MANIFEST_NAME), manifest).unwrap();
            verify_project_package(&root).unwrap();
            Self {
                root,
                timeline,
                track,
                clip,
                arrangement,
                chain,
                sample,
            }
        }

        fn request(&self) -> ([u8; 32], AudioTrackFreezeDocuments) {
            let package = verify_project_package(&self.root).unwrap();
            let tracks = [self.track.as_slice()];
            let clips = [self.clip.as_slice()];
            let project = load(
                PlaybackJsonInput {
                    project_id: PROJECT_ID,
                    sample_rate_hz: 48_000,
                    timeline: &self.timeline,
                    tracks: &tracks,
                    clips: &clips,
                    scenes: &[],
                    arrangement: Some(&self.arrangement),
                },
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            let assets = [MediaAssetFact {
                id: SOURCE_ASSET_ID,
                sample_rate_hz: 48_000,
                frame_count: 50,
            }];
            let device_chain_sha256 = hex_digest(digest(&self.chain));
            let documents = serialize_audio_track_freeze(
                &project,
                &assets,
                &self.track,
                &clips,
                &self.arrangement,
                TRACK_ID,
                FROZEN_ASSET_ID,
                50,
                FROZEN_CLIP_ID,
                FROZEN_PLACEMENT_ID,
                FREEZE_STATE_ID,
                "Frozen Lead",
                Some(AudioTrackFreezeDeviceChain::new(
                    DEVICE_CHAIN_ID,
                    &device_chain_sha256,
                )),
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            (*package.manifest_sha256(), documents)
        }

        fn unfreeze_request(&self) -> ([u8; 32], AudioTrackUnfreezeDocuments) {
            let package = verify_project_package(&self.root).unwrap();
            let track = package.read_resource(TRACK_ID).unwrap();
            let clip = package.read_resource(FROZEN_CLIP_ID).unwrap();
            let arrangement = package.read_resource(ARRANGEMENT_ID).unwrap();
            let freeze_state = package.read_resource(FREEZE_STATE_ID).unwrap();
            let tracks = [track.as_slice()];
            let clips = [clip.as_slice()];
            let project = load(
                PlaybackJsonInput {
                    project_id: PROJECT_ID,
                    sample_rate_hz: 48_000,
                    timeline: &self.timeline,
                    tracks: &tracks,
                    clips: &clips,
                    scenes: &[],
                    arrangement: Some(&arrangement),
                },
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            let assets = [
                MediaAssetFact {
                    id: SOURCE_ASSET_ID,
                    sample_rate_hz: 48_000,
                    frame_count: 50,
                },
                MediaAssetFact {
                    id: FROZEN_ASSET_ID,
                    sample_rate_hz: 48_000,
                    frame_count: 50,
                },
            ];
            let documents = serialize_audio_track_unfreeze(
                &project,
                &assets,
                &track,
                &arrangement,
                &freeze_state,
                TRACK_ID,
                LoaderLimits::VERSION_1,
            )
            .unwrap();
            (*package.manifest_sha256(), documents)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[allow(
        clippy::needless_pass_by_value,
        reason = "fixtures consume concise JSON values before canonicalization"
    )]
    fn canonical(value: Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(&value).unwrap()
    }

    fn descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> Value {
        json!({"id":id,"role":role,"path":path,"media_type":"application/json",
            "byte_length":bytes.len(),"sha256":hex_digest(digest(bytes)),"extensions":{}})
    }

    #[test]
    fn publishes_complete_reversible_track_freeze() {
        let fixture = Fixture::new();
        let (identity, documents) = fixture.request();
        let outcome = publish_audio_track_freeze(
            &fixture.root,
            AudioTrackFreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
                rendered_sample: &fixture.sample,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(outcome.track_id(), TRACK_ID);
        assert_eq!(package.manifest().revision(), 2);
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        assert_eq!(package.read_resource(TRACK_ID).unwrap(), documents.track());
        assert_eq!(
            package.read_resource(FROZEN_CLIP_ID).unwrap(),
            documents.clip()
        );
        assert_eq!(
            package.read_resource(FREEZE_STATE_ID).unwrap(),
            documents.freeze_state()
        );
        assert!(
            package
                .manifest()
                .resources()
                .iter()
                .all(|entry| entry.id() != SOURCE_CLIP_ID)
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("assets/samples/{FROZEN_ASSET_ID}.wav"))
            )
            .unwrap(),
            fixture.sample
        );
    }

    #[test]
    fn rejects_generic_mutation_of_the_rendered_freeze_authority() {
        let fixture = Fixture::new();
        let (identity, freeze) = fixture.request();
        publish_audio_track_freeze(
            &fixture.root,
            AudioTrackFreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &freeze,
                rendered_sample: &fixture.sample,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        let frozen_identity = *package.manifest_sha256();
        let mut changed_arrangement: Value =
            serde_json::from_slice(&package.read_resource(ARRANGEMENT_ID).unwrap()).unwrap();
        changed_arrangement["placements"][0]["duration_ticks"] = json!(1);
        let changed_arrangement = canonical(changed_arrangement);

        let error = publish_project_resource(
            &fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: frozen_identity,
                resource_id: ARRANGEMENT_ID,
                expected_role: ResourceRole::Arrangement,
                resource_bytes: &changed_arrangement,
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Integrity);
        let still_frozen = verify_project_package(&fixture.root).unwrap();
        assert_eq!(*still_frozen.manifest_sha256(), frozen_identity);
        assert_eq!(
            still_frozen.read_resource(ARRANGEMENT_ID).unwrap(),
            freeze.arrangement()
        );
        let (unfreeze_identity, unfreeze) = fixture.unfreeze_request();
        publish_audio_track_unfreeze(
            &fixture.root,
            AudioTrackUnfreezePublicationRequest {
                expected_project_sha256: unfreeze_identity,
                documents: &unfreeze,
            },
        )
        .unwrap();
    }

    #[test]
    fn rejects_generic_mutation_of_a_frozen_tracks_device_chain() {
        let fixture = Fixture::new();
        let (identity, freeze) = fixture.request();
        publish_audio_track_freeze(
            &fixture.root,
            AudioTrackFreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &freeze,
                rendered_sample: &fixture.sample,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        let frozen_identity = *package.manifest_sha256();
        let mut changed_chain: Value =
            serde_json::from_slice(&package.read_resource(DEVICE_CHAIN_ID).unwrap()).unwrap();
        changed_chain["devices"][0]["enabled"] = json!(false);
        let changed_chain = canonical(changed_chain);

        let error = publish_project_resource(
            &fixture.root,
            ProjectResourcePublicationRequest {
                expected_project_sha256: frozen_identity,
                resource_id: DEVICE_CHAIN_ID,
                expected_role: ResourceRole::DeviceChain,
                resource_bytes: &changed_chain,
            },
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Integrity);
        let still_frozen = verify_project_package(&fixture.root).unwrap();
        assert_eq!(*still_frozen.manifest_sha256(), frozen_identity);
        assert_eq!(
            still_frozen.read_resource(DEVICE_CHAIN_ID).unwrap(),
            fixture.chain
        );
        let (unfreeze_identity, unfreeze) = fixture.unfreeze_request();
        publish_audio_track_unfreeze(
            &fixture.root,
            AudioTrackUnfreezePublicationRequest {
                expected_project_sha256: unfreeze_identity,
                documents: &unfreeze,
            },
        )
        .unwrap();
    }

    #[test]
    fn cleanup_refuses_a_source_asset_retained_by_a_frozen_track() {
        let fixture = Fixture::new();
        let (identity, freeze) = fixture.request();
        publish_audio_track_freeze(
            &fixture.root,
            AudioTrackFreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &freeze,
                rendered_sample: &fixture.sample,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        let retained_identity = *package.manifest_sha256();
        let requested = [SOURCE_ASSET_ID.to_owned()];

        let error = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: retained_identity,
                asset_ids: &requested,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(fixture.root.join("assets/samples/source.wav").is_file());
        assert_eq!(
            *verify_project_package(&fixture.root)
                .unwrap()
                .manifest_sha256(),
            retained_identity
        );
    }

    #[test]
    fn unfreezes_complete_rendered_track_without_deleting_its_sample_asset() {
        let fixture = Fixture::new();
        let (identity, freeze) = fixture.request();
        publish_audio_track_freeze(
            &fixture.root,
            AudioTrackFreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &freeze,
                rendered_sample: &fixture.sample,
            },
        )
        .unwrap();
        let (identity, documents) = fixture.unfreeze_request();
        let outcome = publish_audio_track_unfreeze(
            &fixture.root,
            AudioTrackUnfreezePublicationRequest {
                expected_project_sha256: identity,
                documents: &documents,
            },
        )
        .unwrap();
        let package = verify_project_package(&fixture.root).unwrap();
        assert_eq!(package.manifest().revision(), 3);
        assert_eq!(*package.manifest_sha256(), *outcome.project_identity());
        assert_eq!(package.read_resource(TRACK_ID).unwrap(), documents.track());
        assert_eq!(
            package.read_resource(SOURCE_CLIP_ID).unwrap(),
            documents.clips()[0].bytes()
        );
        assert!(
            package
                .manifest()
                .resources()
                .iter()
                .all(|entry| entry.id() != FROZEN_CLIP_ID && entry.id() != FREEZE_STATE_ID)
        );
        assert!(
            package
                .manifest()
                .assets()
                .iter()
                .any(|asset| asset.id() == FROZEN_ASSET_ID)
        );
        assert_eq!(
            fs::read(
                fixture
                    .root
                    .join(format!("assets/samples/{FROZEN_ASSET_ID}.wav"))
            )
            .unwrap(),
            fixture.sample
        );
    }

    #[test]
    fn crash_recovery_preserves_one_verified_track_unfreeze_side() {
        for phase in [
            PublicationPhase::Transaction,
            PublicationPhase::SourceMoved(0),
            PublicationPhase::ResourcePublished(0),
            PublicationPhase::ResourcePublished(1),
            PublicationPhase::SourceMoved(2),
            PublicationPhase::SourceMoved(3),
            PublicationPhase::SourceMoved(4),
            PublicationPhase::ResourcePublished(4),
            PublicationPhase::Manifest,
        ] {
            let fixture = Fixture::new();
            let (identity, freeze) = fixture.request();
            publish_audio_track_freeze(
                &fixture.root,
                AudioTrackFreezePublicationRequest {
                    expected_project_sha256: identity,
                    documents: &freeze,
                    rendered_sample: &fixture.sample,
                },
            )
            .unwrap();
            let (identity, documents) = fixture.unfreeze_request();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_unfreeze_observed(
                    &fixture.root,
                    AudioTrackUnfreezePublicationRequest {
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
            assert_eq!(report.audio_track_unfreezes().len(), 1);
            let expected = if phase == PublicationPhase::Manifest {
                AudioTrackUnfreezePublicationRecoveryDisposition::Committed
            } else {
                AudioTrackUnfreezePublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(report.audio_track_unfreezes()[0].disposition(), expected);
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackUnfreezePublicationRecoveryDisposition::Committed {
                assert_eq!(
                    package.read_resource(SOURCE_CLIP_ID).unwrap(),
                    documents.clips()[0].bytes()
                );
                assert!(
                    package
                        .manifest()
                        .resources()
                        .iter()
                        .all(|entry| entry.id() != FROZEN_CLIP_ID && entry.id() != FREEZE_STATE_ID)
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert_eq!(
                    package.read_resource(FROZEN_CLIP_ID).unwrap(),
                    freeze.clip()
                );
                assert_eq!(
                    package.read_resource(FREEZE_STATE_ID).unwrap(),
                    freeze.freeze_state()
                );
            }
        }
    }

    #[test]
    fn crash_recovery_preserves_one_verified_track_freeze_side() {
        for phase in [
            PublicationPhase::Transaction,
            PublicationPhase::SourceMoved(0),
            PublicationPhase::ResourcePublished(0),
            PublicationPhase::ResourcePublished(1),
            PublicationPhase::SourceMoved(2),
            PublicationPhase::ResourcePublished(3),
            PublicationPhase::SourceMoved(4),
            PublicationPhase::ResourcePublished(4),
            PublicationPhase::SamplePublished,
            PublicationPhase::Manifest,
        ] {
            let fixture = Fixture::new();
            let (identity, documents) = fixture.request();
            let result = catch_unwind(AssertUnwindSafe(|| {
                let _ = publish_observed(
                    &fixture.root,
                    AudioTrackFreezePublicationRequest {
                        expected_project_sha256: identity,
                        documents: &documents,
                        rendered_sample: &fixture.sample,
                    },
                    |current, _| {
                        assert_ne!(current, phase, "simulate process termination");
                        Ok(())
                    },
                );
            }));
            assert!(result.is_err());
            let report = recover_project_transactions(&fixture.root).unwrap();
            assert_eq!(report.audio_track_freezes().len(), 1);
            let expected = if phase == PublicationPhase::Manifest {
                AudioTrackFreezePublicationRecoveryDisposition::Committed
            } else {
                AudioTrackFreezePublicationRecoveryDisposition::RolledBack
            };
            assert_eq!(report.audio_track_freezes()[0].disposition(), expected);
            let package = verify_project_package(&fixture.root).unwrap();
            if expected == AudioTrackFreezePublicationRecoveryDisposition::Committed {
                assert_eq!(
                    package.read_resource(FROZEN_CLIP_ID).unwrap(),
                    documents.clip()
                );
            } else {
                assert_eq!(*package.manifest_sha256(), identity);
                assert_eq!(package.read_resource(SOURCE_CLIP_ID).unwrap(), fixture.clip);
                assert!(!fixture.root.join("freezes").exists());
            }
        }
    }
}
