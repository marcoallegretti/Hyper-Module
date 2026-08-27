use std::collections::BTreeSet;
use std::io;
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{AssetRole, ProjectManifest, ResourceRole, is_valid_entity_id};
use hyper_track_freeze::TrackFreezeState;
use same_file::Handle;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[cfg(not(windows))]
use super::same_directory;
use super::{
    INTENT_NAME, INTENT_STAGING_NAME, MANIFEST_NAME, MAX_JSON_BYTES, MAX_SAFE_INTEGER,
    PREVIOUS_PROJECT_NAME, PROJECT_STAGING_NAME, ProjectResourcePublicationError,
    ProjectResourcePublicationErrorKind, StructurallyVerifiedProject, TRANSACTION_NAME, caused,
    decode_digest, digest, failure, file_name, hex_digest, missing_file, open_or_create_dir,
    open_relative_parent, open_stable_project_root, publish_staged_file, read_bounded,
    require_canonical, sync_dir, verify_committed, verify_project_candidate,
    verify_project_package, write_exclusive_synced,
};
use crate::project_lock::{
    ProjectLock, ProjectTransactionFamily, SAMPLE_CLEANUP_TRANSACTION_PREFIX,
    find_unhandled_project_transaction,
};
use crate::stable_file::verify_open_stable;

const FORMAT: &str = "hyper-sample-cleanup-transaction";
const INTENT_FORMAT: &str = "hyper-sample-cleanup-intent";
const VERSION: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub struct SampleCleanupRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub asset_ids: &'a [String],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleCleanupOutcome {
    project_identity: [u8; 32],
    project_revision: u64,
    removed_assets: usize,
    removed_bytes: u64,
}

impl SampleCleanupOutcome {
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }
    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }
    #[must_use]
    pub const fn removed_assets(&self) -> usize {
        self.removed_assets
    }
    #[must_use]
    pub const fn removed_bytes(&self) -> u64 {
        self.removed_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleCleanupRecoveryDisposition {
    Committed,
    RolledBack,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SampleCleanupRecovery {
    transaction_id: String,
    disposition: SampleCleanupRecoveryDisposition,
}

impl SampleCleanupRecovery {
    #[must_use]
    pub fn transaction_id(&self) -> &str {
        &self.transaction_id
    }
    #[must_use]
    pub const fn disposition(&self) -> SampleCleanupRecoveryDisposition {
        self.disposition
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SampleCleanupPhase {
    Transaction,
    Asset(usize),
    SampleDirectory,
    AssetDirectory,
    Assets,
    Manifest,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    format: String,
    format_version: u32,
    id: String,
    extensions: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TransactionAsset {
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
    assets: Vec<TransactionAsset>,
    removed_sample_directory: bool,
    removed_asset_directory: bool,
    extensions: serde_json::Map<String, Value>,
}

/// Permanently removes the exact confirmed set of unreferenced sample assets.
///
/// # Errors
///
/// Returns a classified validation, concurrency, integrity, I/O, or recovery
/// failure. A published error retains whether the manifest commit occurred.
pub fn clean_unused_samples(
    project_path: impl AsRef<Path>,
    request: SampleCleanupRequest<'_>,
) -> Result<SampleCleanupOutcome, ProjectResourcePublicationError> {
    clean_unused_samples_observed(project_path.as_ref(), request, |_, _| Ok(()))
}

/// Recovers every durable sample-cleanup transaction in one project.
///
/// # Errors
///
/// Returns a fail-closed error for ambiguous, corrupt, inaccessible, or
/// concurrently owned recovery evidence.
pub fn recover_sample_cleanups(
    project_path: impl AsRef<Path>,
) -> Result<Vec<SampleCleanupRecovery>, ProjectResourcePublicationError> {
    let (root_path, root) = open_stable_project_root(project_path.as_ref()).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open project root for sample-cleanup recovery",
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

#[allow(
    clippy::too_many_lines,
    reason = "the retained authority, recovery gate, and transaction handoff stay visible in one coordinator"
)]
fn clean_unused_samples_observed(
    project_path: &Path,
    request: SampleCleanupRequest<'_>,
    mut observer: impl FnMut(SampleCleanupPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<SampleCleanupOutcome, ProjectResourcePublicationError> {
    if request.asset_ids.is_empty() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "sample cleanup requires at least one asset",
        ));
    }
    let verified = verify_project_package(project_path).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "project verification failed before sample cleanup",
            error,
        )
    })?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed after sample cleanup was confirmed",
        ));
    }
    let assets = validate_request(&verified, request.asset_ids)?;
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
            "a prior sample cleanup was recovered; reopen the current project revision",
        ));
    }
    verified.confirm_unchanged().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "project changed before sample cleanup",
            error,
        )
    })?;
    let active_project = read_bounded(&verified.root_dir, MANIFEST_NAME, MAX_JSON_BYTES)?;
    if digest(&active_project) != request.expected_project_sha256 {
        return Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "active project identity changed before sample cleanup staging",
        ));
    }
    let authorities = assets
        .iter()
        .map(|asset| verified_asset_authority(&verified.root_dir, asset))
        .collect::<Result<Vec<_>, _>>()?;
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
    let candidate_project = candidate_manifest(&active_project, &assets, next_revision)?;
    let removed_sample_directory = verified
        .manifest()
        .assets()
        .iter()
        .filter(|asset| asset.role() == AssetRole::Sample)
        .count()
        == assets.len();
    let removed_asset_directory = removed_sample_directory
        && verified.manifest().assets().len() == assets.len()
        && !verified
            .manifest()
            .resources()
            .iter()
            .any(|resource| resource.role() == ResourceRole::RecordingMetadata);
    let name = format!("{SAMPLE_CLEANUP_TRANSACTION_PREFIX}{}", Uuid::new_v4());
    backups.create_dir(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "create sample-cleanup transaction",
            error,
        )
    })?;
    sync_dir(&backups)?;
    let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Io,
            "open sample-cleanup transaction",
            error,
        )
    })?;
    let result = run_cleanup(
        &verified,
        &backups,
        &transaction,
        &name,
        &assets,
        &authorities,
        removed_sample_directory,
        removed_asset_directory,
        &active_project,
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

#[derive(Clone)]
struct SelectedAsset {
    id: String,
    path: String,
    byte_length: u64,
    sha256: String,
}

fn verified_asset_authority(
    root: &Dir,
    asset: &SelectedAsset,
) -> Result<Handle, ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &asset.path)?;
    verified_file_authority(
        &parent,
        file_name(&asset.path)?,
        asset.byte_length,
        &asset.sha256,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify sample asset before cleanup staging",
    )
}

fn verified_file_authority(
    directory: &Dir,
    name: &str,
    byte_length: u64,
    sha256: &str,
    kind: ProjectResourcePublicationErrorKind,
    operation: &'static str,
) -> Result<Handle, ProjectResourcePublicationError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory
        .open_with(name, &options)
        .map_err(|error| caused(kind, operation, error))?;
    let retained = file
        .try_clone()
        .map_err(|error| caused(kind, "retain sample file authority", error))?;
    let authority = Handle::from_file(retained.into_std())
        .map_err(|error| caused(kind, "identify sample file authority", error))?;
    verify_open_stable(
        file.into_std(),
        operation,
        byte_length,
        decode_digest(sha256)?,
        false,
    )
    .map_err(|error| caused(kind, operation, error))?;
    Ok(authority)
}

fn move_verified_asset(
    root: &Dir,
    transaction: &Dir,
    asset: &TransactionAsset,
    authority: &Handle,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &asset.path)?;
    let name = file_name(&asset.path)?;
    parent
        .rename(name, transaction, &asset.backup_name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "move unused sample into cleanup transaction",
                error,
            )
        })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    let moved = verified_file_authority(
        transaction,
        &asset.backup_name,
        asset.byte_length,
        &asset.sha256,
        ProjectResourcePublicationErrorKind::ProjectChanged,
        "verify moved sample cleanup asset",
    );
    if moved.as_ref().is_ok_and(|moved| moved == authority) {
        return Ok(());
    }
    transaction
        .rename(&asset.backup_name, &parent, name)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "restore changed sample after cleanup authority mismatch",
                error,
            )
        })?;
    sync_dir(&parent)?;
    sync_dir(transaction)?;
    match moved {
        Ok(_) => Err(failure(
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "sample asset path changed before cleanup removal",
        )),
        Err(error) => Err(error),
    }
}

fn verify_moved_assets(
    transaction: &Dir,
    assets: &[TransactionAsset],
    authorities: &[Handle],
) -> Result<(), ProjectResourcePublicationError> {
    if assets.len() != authorities.len() {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "sample cleanup retained authority count changed",
        ));
    }
    for (asset, authority) in assets.iter().zip(authorities) {
        let current = verified_file_authority(
            transaction,
            &asset.backup_name,
            asset.byte_length,
            &asset.sha256,
            ProjectResourcePublicationErrorKind::ProjectChanged,
            "revalidate moved sample cleanup asset",
        )?;
        if &current != authority {
            return Err(failure(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "moved sample cleanup asset authority changed",
            ));
        }
    }
    Ok(())
}

fn validate_request(
    verified: &StructurallyVerifiedProject,
    ids: &[String],
) -> Result<Vec<SelectedAsset>, ProjectResourcePublicationError> {
    let mut previous: Option<&str> = None;
    for id in ids {
        if !is_valid_entity_id(id) || previous.is_some_and(|value| value >= id.as_str()) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::InvalidResource,
                "sample cleanup asset identities must be canonical, unique, and sorted",
            ));
        }
        previous = Some(id);
    }
    let mut referenced = BTreeSet::new();
    for descriptor in verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| resource.role() == ResourceRole::Clip)
    {
        let bytes = verified.read_resource(descriptor.id()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "read audio clip while classifying sample cleanup",
                error,
            )
        })?;
        require_canonical(&bytes, "audio clip used for sample cleanup")?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "parse audio clip while classifying sample cleanup",
                error,
            )
        })?;
        let asset_id = value
            .pointer("/source/asset_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                failure(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "audio clip has no source asset identity",
                )
            })?;
        referenced.insert(asset_id.to_owned());
    }
    for descriptor in verified
        .manifest()
        .resources()
        .iter()
        .filter(|resource| resource.role() == ResourceRole::FreezeState)
    {
        let bytes = verified.read_resource(descriptor.id()).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "read track-freeze state while classifying sample cleanup",
                error,
            )
        })?;
        require_canonical(&bytes, "track-freeze state used for sample cleanup")?;
        let state = TrackFreezeState::parse(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Integrity,
                "parse track-freeze state while classifying sample cleanup",
                error,
            )
        })?;
        for original in state.original_clips() {
            referenced.insert(original.asset_id().to_owned());
        }
    }
    let mut selected = verified
        .manifest()
        .assets()
        .iter()
        .filter(|asset| asset.role() == AssetRole::Sample && !referenced.contains(asset.id()))
        .map(|asset| SelectedAsset {
            id: asset.id().to_owned(),
            path: asset.path().to_owned(),
            byte_length: asset.byte_length(),
            sha256: asset.sha256().to_owned(),
        })
        .collect::<Vec<_>>();
    selected.sort_unstable_by(|left, right| left.id.cmp(&right.id));
    if selected.len() != ids.len()
        || selected
            .iter()
            .zip(ids)
            .any(|(asset, requested)| asset.id != *requested)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "sample cleanup request does not match the complete unused sample set",
        ));
    }
    Ok(selected)
}

fn candidate_manifest(
    active: &[u8],
    assets: &[SelectedAsset],
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
    let records = value["assets"].as_array_mut().ok_or_else(|| {
        failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "project asset declarations are invalid",
        )
    })?;
    let ids = assets
        .iter()
        .map(|asset| asset.id.as_str())
        .collect::<BTreeSet<_>>();
    let before = records.len();
    records.retain(|record| record["id"].as_str().is_none_or(|id| !ids.contains(id)));
    if before.checked_sub(records.len()) != Some(assets.len()) {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "sample cleanup manifest did not remove exactly the selected assets",
        ));
    }
    let bytes = serde_json_canonicalizer::to_vec(&value).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Integrity,
            "serialize sample-cleanup manifest",
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
            "candidate sample-cleanup manifest is invalid",
            error,
        )
    })?;
    Ok(bytes)
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the durable move, candidate verification, and manifest-last order stay visible together"
)]
fn run_cleanup(
    verified: &StructurallyVerifiedProject,
    backups: &Dir,
    transaction: &Dir,
    name: &str,
    assets: &[SelectedAsset],
    authorities: &[Handle],
    removed_sample_directory: bool,
    removed_asset_directory: bool,
    previous_project: &[u8],
    candidate_project: &[u8],
    next_revision: u64,
    observer: &mut impl FnMut(SampleCleanupPhase, &Path) -> Result<(), ProjectResourcePublicationError>,
) -> Result<SampleCleanupOutcome, ProjectResourcePublicationError> {
    write_intent(transaction, name)?;
    write_exclusive_synced(transaction, PROJECT_STAGING_NAME, candidate_project)?;
    write_exclusive_synced(transaction, PREVIOUS_PROJECT_NAME, previous_project)?;
    let tx_assets = assets
        .iter()
        .enumerate()
        .map(|(index, asset)| TransactionAsset {
            id: asset.id.clone(),
            path: asset.path.clone(),
            byte_length: asset.byte_length,
            sha256: asset.sha256.clone(),
            backup_name: format!("asset-{index:05}.bin"),
        })
        .collect::<Vec<_>>();
    let tx = Transaction {
        format: FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        before_project_sha256: hex_digest(*verified.manifest_sha256()),
        after_project_sha256: hex_digest(digest(candidate_project)),
        before_project_revision: verified.manifest().revision(),
        after_project_revision: next_revision,
        assets: tx_assets,
        removed_sample_directory,
        removed_asset_directory,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&tx).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize sample-cleanup transaction",
            error,
        )
    })?;
    write_exclusive_synced(transaction, TRANSACTION_NAME, &bytes)?;
    sync_dir(transaction)?;
    sync_dir(backups)?;
    observer(SampleCleanupPhase::Transaction, verified.root())?;
    for (index, (asset, authority)) in tx.assets.iter().zip(authorities).enumerate() {
        move_verified_asset(&verified.root_dir, transaction, asset, authority)?;
        observer(SampleCleanupPhase::Asset(index), verified.root())?;
    }
    verify_moved_assets(transaction, &tx.assets, authorities)?;
    if removed_sample_directory {
        let assets_dir = verified
            .root_dir
            .open_dir_nofollow("assets")
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Io,
                    "open project asset directory",
                    error,
                )
            })?;
        assets_dir.remove_dir("samples").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty sample directory",
                error,
            )
        })?;
        sync_dir(&assets_dir)?;
        observer(SampleCleanupPhase::SampleDirectory, verified.root())?;
    }
    if removed_asset_directory {
        verified.root_dir.remove_dir("assets").map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::ProjectChanged,
                "remove empty asset directory",
                error,
            )
        })?;
        sync_dir(&verified.root_dir)?;
        observer(SampleCleanupPhase::AssetDirectory, verified.root())?;
    }
    observer(SampleCleanupPhase::Assets, verified.root())?;
    let candidate =
        verify_project_candidate(verified.root(), &verified.root_dir, candidate_project).map_err(
            |error| {
                caused(
                    ProjectResourcePublicationErrorKind::Integrity,
                    "sample-cleanup candidate does not verify",
                    error,
                )
            },
        )?;
    if candidate.revision() != next_revision {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Integrity,
            "sample-cleanup candidate revision changed",
        ));
    }
    verify_moved_assets(transaction, &tx.assets, authorities)?;
    publish_staged_file(
        transaction,
        PROJECT_STAGING_NAME,
        backups,
        &verified.root_dir,
        MANIFEST_NAME,
        &format!(".sample-cleanup-project-{}.json", tx.id),
        "sample-cleanup project manifest",
    )?;
    observer(SampleCleanupPhase::Manifest, verified.root())?;
    verify_committed(
        verified.root(),
        &verified.root_dir,
        digest(candidate_project),
    )?;
    outcome(&tx)
}

fn finish_result(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: Dir,
    name: &str,
    result: Result<SampleCleanupOutcome, ProjectResourcePublicationError>,
) -> Result<SampleCleanupOutcome, ProjectResourcePublicationError> {
    match result {
        Ok(outcome) => {
            clear_and_remove(backups, transaction, name).map_err(|error| {
                ProjectResourcePublicationError {
                    kind: ProjectResourcePublicationErrorKind::Recovery,
                    message: "sample cleanup committed but transaction cleanup is incomplete"
                        .to_owned(),
                    published: true,
                    source: Some(Box::new(error)),
                }
            })?;
            Ok(outcome)
        }
        Err(operation) => {
            if missing_file(&transaction, TRANSACTION_NAME)? {
                clear_and_remove(backups, transaction, name)?;
                return Err(operation);
            }
            let tx = read_transaction(&transaction, name)?;
            match recover_one(root_path, root, backups, &transaction, &tx)? {
                SampleCleanupRecoveryDisposition::RolledBack => {
                    clear_and_remove(backups, transaction, name)?;
                    Err(operation)
                }
                SampleCleanupRecoveryDisposition::Committed => {
                    let outcome = outcome(&tx)?;
                    clear_and_remove(backups, transaction, name).map_err(|error| {
                        ProjectResourcePublicationError {
                            kind: ProjectResourcePublicationErrorKind::Recovery,
                            message:
                                "sample cleanup committed but transaction cleanup is incomplete"
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
) -> Result<Vec<SampleCleanupRecovery>, ProjectResourcePublicationError> {
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
        .filter(|name| name.starts_with(SAMPLE_CLEANUP_TRANSACTION_PREFIX))
        .collect::<Vec<_>>();
    names.sort_unstable();
    let mut recovered = Vec::new();
    recovered.try_reserve_exact(names.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup recovery capacity is unavailable",
        )
    })?;
    for name in names {
        let transaction = backups.open_dir_nofollow(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "open sample-cleanup transaction",
                error,
            )
        })?;
        if missing_file(&transaction, TRANSACTION_NAME)? {
            validate_incomplete(&transaction, &name)?;
            clear_and_remove(backups, transaction, &name)?;
            continue;
        }
        let tx = read_transaction(&transaction, &name)?;
        let disposition = recover_one(root_path, root, backups, &transaction, &tx)?;
        clear_and_remove(backups, transaction, &name)?;
        recovered.push(SampleCleanupRecovery {
            transaction_id: tx.id,
            disposition,
        });
    }
    Ok(recovered)
}

fn recover_one(
    root_path: &Path,
    root: &Dir,
    backups: &Dir,
    transaction: &Dir,
    tx: &Transaction,
) -> Result<SampleCleanupRecoveryDisposition, ProjectResourcePublicationError> {
    let active = read_bounded(root, MANIFEST_NAME, MAX_JSON_BYTES)?;
    let active_digest = digest(&active);
    let before = decode_digest(&tx.before_project_sha256)?;
    let after = decode_digest(&tx.after_project_sha256)?;
    if active_digest == before {
        let asset_states = preflight_rollback_assets(root, transaction, &tx.assets)?;
        if tx.removed_asset_directory {
            create_directory_if_missing(root, "assets", "restore asset directory")?;
        }
        if tx.removed_sample_directory {
            let assets_dir = root.open_dir_nofollow("assets").map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "open restored asset directory",
                    error,
                )
            })?;
            create_directory_if_missing(&assets_dir, "samples", "restore sample directory")?;
            sync_dir(&assets_dir)?;
        }
        for (asset, state) in tx.assets.iter().zip(&asset_states) {
            restore_asset(root, transaction, asset, state)?;
        }
        sync_dir(root)?;
        verify_committed(root_path, root, before)?;
        Ok(SampleCleanupRecoveryDisposition::RolledBack)
    } else if active_digest == after {
        let committed = (|| -> Result<_, ProjectResourcePublicationError> {
            validate_committed_residue(root, transaction, &tx.assets)?;
            verify_committed(root_path, root, after)?;
            Ok(SampleCleanupRecoveryDisposition::Committed)
        })();
        committed.map_err(|source| ProjectResourcePublicationError {
            kind: ProjectResourcePublicationErrorKind::Recovery,
            message: "sample cleanup committed but verification or recovery is incomplete"
                .to_owned(),
            published: true,
            source: Some(Box::new(source)),
        })
    } else {
        let _ = backups;
        Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup project manifest matches neither transaction side",
        ))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAssetSide {
    Active,
    Backup,
}

struct RecoveryAssetState {
    side: RecoveryAssetSide,
    authority: Handle,
}

fn preflight_rollback_assets(
    root: &Dir,
    transaction: &Dir,
    assets: &[TransactionAsset],
) -> Result<Vec<RecoveryAssetState>, ProjectResourcePublicationError> {
    let mut states = Vec::new();
    states.try_reserve_exact(assets.len()).map_err(|_| {
        failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup recovery capacity is unavailable",
        )
    })?;
    for asset in assets {
        let active = entry_present(root, &asset.path, "inspect active sample during recovery")?;
        let backup = entry_present(
            transaction,
            &asset.backup_name,
            "inspect backup sample during recovery",
        )?;
        let (side, authority) = match (active, backup) {
            (true, false) => {
                let parent = open_relative_parent(root, &asset.path)?;
                let authority = verified_file_authority(
                    &parent,
                    file_name(&asset.path)?,
                    asset.byte_length,
                    &asset.sha256,
                    ProjectResourcePublicationErrorKind::Recovery,
                    "preflight active sample during cleanup rollback",
                )?;
                (RecoveryAssetSide::Active, authority)
            }
            (false, true) => {
                let authority = verified_file_authority(
                    transaction,
                    &asset.backup_name,
                    asset.byte_length,
                    &asset.sha256,
                    ProjectResourcePublicationErrorKind::Recovery,
                    "preflight backup sample during cleanup rollback",
                )?;
                (RecoveryAssetSide::Backup, authority)
            }
            _ => {
                return Err(failure(
                    ProjectResourcePublicationErrorKind::Recovery,
                    "sample-cleanup asset exists on both or neither transaction side",
                ));
            }
        };
        states.push(RecoveryAssetState { side, authority });
    }
    Ok(states)
}

fn validate_committed_residue(
    root: &Dir,
    transaction: &Dir,
    assets: &[TransactionAsset],
) -> Result<(), ProjectResourcePublicationError> {
    for asset in assets {
        if entry_present(root, &asset.path, "inspect committed sample cleanup path")? {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "committed sample cleanup retained an active sample file",
            ));
        }
        if entry_present(
            transaction,
            &asset.backup_name,
            "inspect committed sample cleanup backup",
        )? {
            verified_file_authority(
                transaction,
                &asset.backup_name,
                asset.byte_length,
                &asset.sha256,
                ProjectResourcePublicationErrorKind::Recovery,
                "verify committed sample cleanup backup",
            )?;
        }
    }
    Ok(())
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

fn restore_asset(
    root: &Dir,
    transaction: &Dir,
    asset: &TransactionAsset,
    state: &RecoveryAssetState,
) -> Result<(), ProjectResourcePublicationError> {
    let parent = open_relative_parent(root, &asset.path)?;
    let name = file_name(&asset.path)?;
    let (directory, current_name) = match state.side {
        RecoveryAssetSide::Active => (&parent, name),
        RecoveryAssetSide::Backup => (transaction, asset.backup_name.as_str()),
    };
    let current = verified_file_authority(
        directory,
        current_name,
        asset.byte_length,
        &asset.sha256,
        ProjectResourcePublicationErrorKind::Recovery,
        "revalidate sample cleanup rollback authority",
    )?;
    if current != state.authority {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup asset authority changed during rollback",
        ));
    }
    match state.side {
        RecoveryAssetSide::Active => Ok(()),
        RecoveryAssetSide::Backup => {
            transaction
                .rename(&asset.backup_name, &parent, name)
                .map_err(|error| {
                    caused(
                        ProjectResourcePublicationErrorKind::Recovery,
                        "restore cleaned sample asset",
                        error,
                    )
                })?;
            sync_dir(&parent)?;
            sync_dir(transaction)
        }
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

fn write_intent(transaction: &Dir, name: &str) -> Result<(), ProjectResourcePublicationError> {
    let intent = Intent {
        format: INTENT_FORMAT.to_owned(),
        format_version: VERSION,
        id: transaction_id(name)?,
        extensions: serde_json::Map::new(),
    };
    let bytes = serde_json_canonicalizer::to_vec(&intent).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::InvalidResource,
            "serialize sample-cleanup intent",
            error,
        )
    })?;
    write_exclusive_synced(transaction, INTENT_STAGING_NAME, &bytes)?;
    transaction
        .rename(INTENT_STAGING_NAME, transaction, INTENT_NAME)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Io,
                "publish sample-cleanup intent",
                error,
            )
        })?;
    sync_dir(transaction)
}

fn read_transaction(
    transaction: &Dir,
    name: &str,
) -> Result<Transaction, ProjectResourcePublicationError> {
    let bytes = read_bounded(transaction, TRANSACTION_NAME, MAX_JSON_BYTES)?;
    require_canonical(&bytes, "sample-cleanup transaction")?;
    let tx: Transaction = serde_json::from_slice(&bytes).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "parse sample-cleanup transaction",
            error,
        )
    })?;
    if tx.format != FORMAT
        || tx.format_version != VERSION
        || tx.id != transaction_id(name)?
        || tx.assets.is_empty()
        || !tx.extensions.is_empty()
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup transaction is invalid",
        ));
    }
    let mut ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut backups = BTreeSet::new();
    for (index, asset) in tx.assets.iter().enumerate() {
        if !is_valid_entity_id(&asset.id)
            || !asset.path.starts_with("assets/samples/")
            || asset.byte_length == 0
            || !ids.insert(&asset.id)
            || !paths.insert(&asset.path)
            || asset.backup_name != format!("asset-{index:05}.bin")
            || !backups.insert(&asset.backup_name)
            || decode_digest(&asset.sha256).is_err()
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "sample-cleanup asset record is invalid",
            ));
        }
    }
    if decode_digest(&tx.before_project_sha256).is_err()
        || decode_digest(&tx.after_project_sha256).is_err()
        || tx.after_project_revision != tx.before_project_revision.checked_add(1).unwrap_or(0)
        || tx.after_project_revision > MAX_SAFE_INTEGER
        || (tx.removed_asset_directory && !tx.removed_sample_directory)
    {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup transaction authority is invalid",
        ));
    }
    validate_transaction_entries(transaction, &tx)?;
    Ok(tx)
}

fn validate_transaction_entries(
    transaction: &Dir,
    tx: &Transaction,
) -> Result<(), ProjectResourcePublicationError> {
    let backup_names = tx
        .assets
        .iter()
        .map(|asset| asset.backup_name.as_str())
        .collect::<BTreeSet<_>>();
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read sample-cleanup transaction entries",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read sample-cleanup transaction entry",
                error,
            )
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "sample-cleanup transaction has a non-Unicode entry",
            ));
        };
        if !matches!(
            name.as_str(),
            INTENT_NAME | PROJECT_STAGING_NAME | PREVIOUS_PROJECT_NAME | TRANSACTION_NAME
        ) && !backup_names.contains(name.as_str())
        {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "sample-cleanup transaction contains an unknown entry",
            ));
        }
    }
    Ok(())
}

fn validate_incomplete(
    transaction: &Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    for entry in transaction.entries().map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "read incomplete sample-cleanup transaction",
            error,
        )
    })? {
        let entry = entry.map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read incomplete sample-cleanup entry",
                error,
            )
        })?;
        let Some(entry_name) = entry.file_name().to_str().map(str::to_owned) else {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete sample-cleanup transaction has a non-Unicode entry",
            ));
        };
        if !matches!(
            entry_name.as_str(),
            INTENT_STAGING_NAME | INTENT_NAME | PROJECT_STAGING_NAME | PREVIOUS_PROJECT_NAME
        ) {
            return Err(failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "incomplete sample-cleanup transaction contains an unknown entry",
            ));
        }
    }
    if !missing_file(transaction, INTENT_NAME)? {
        let bytes = read_bounded(transaction, INTENT_NAME, MAX_JSON_BYTES)?;
        require_canonical(&bytes, "sample-cleanup intent")?;
        let intent: Intent = serde_json::from_slice(&bytes).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "parse sample-cleanup intent",
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
                "sample-cleanup intent is invalid",
            ));
        }
    }
    Ok(())
}

fn outcome(tx: &Transaction) -> Result<SampleCleanupOutcome, ProjectResourcePublicationError> {
    let removed_bytes = tx
        .assets
        .iter()
        .try_fold(0_u64, |sum, asset| sum.checked_add(asset.byte_length))
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "sample-cleanup byte total overflowed",
            )
        })?;
    Ok(SampleCleanupOutcome {
        project_identity: decode_digest(&tx.after_project_sha256)?,
        project_revision: tx.after_project_revision,
        removed_assets: tx.assets.len(),
        removed_bytes,
    })
}

fn transaction_id(name: &str) -> Result<String, ProjectResourcePublicationError> {
    let id = name
        .strip_prefix(SAMPLE_CLEANUP_TRANSACTION_PREFIX)
        .ok_or_else(|| {
            failure(
                ProjectResourcePublicationErrorKind::Recovery,
                "sample-cleanup transaction name is invalid",
            )
        })?;
    let parsed = Uuid::parse_str(id).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup transaction identifier is invalid",
            error,
        )
    })?;
    if parsed.to_string() != id {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup transaction identifier is not canonical",
        ));
    }
    Ok(id.to_owned())
}

fn reject_unhandled(backups: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let unhandled =
        find_unhandled_project_transaction(backups, &[ProjectTransactionFamily::SampleCleanup])
            .map_err(|error| {
                caused(
                    ProjectResourcePublicationErrorKind::TransactionUnavailable,
                    "inspect project transactions before sample cleanup",
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

fn clear_transaction(transaction: &Dir) -> Result<(), ProjectResourcePublicationError> {
    let names = transaction
        .entries()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read sample-cleanup transaction for clearing",
                error,
            )
        })?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "read sample-cleanup transaction entry",
                error,
            )
        })?;
    for name in names {
        transaction.remove_file(&name).map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "remove sample-cleanup transaction file",
                error,
            )
        })?;
    }
    sync_dir(transaction)
}

#[cfg(not(windows))]
fn clear_and_remove(
    backups: &Dir,
    transaction: Dir,
    name: &str,
) -> Result<(), ProjectResourcePublicationError> {
    let quarantine = format!(".sample-cleanup-disposal-{}", Uuid::new_v4());
    backups
        .rename(name, backups, &quarantine)
        .map_err(|error| {
            caused(
                ProjectResourcePublicationErrorKind::Recovery,
                "quarantine sample-cleanup transaction",
                error,
            )
        })?;
    let moved = backups.open_dir_nofollow(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "open quarantined sample-cleanup transaction",
            error,
        )
    })?;
    if !same_directory(&transaction, &moved).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "compare quarantined sample-cleanup transaction",
            error,
        )
    })? {
        return Err(failure(
            ProjectResourcePublicationErrorKind::Recovery,
            "sample-cleanup transaction changed before disposal",
        ));
    }
    drop(moved);
    clear_transaction(&transaction)?;
    drop(transaction);
    backups.remove_dir(&quarantine).map_err(|error| {
        caused(
            ProjectResourcePublicationErrorKind::Recovery,
            "remove sample-cleanup transaction",
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
            "remove sample-cleanup transaction",
            error,
        )
    })?;
    sync_dir(backups)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hyper_project_model::AssetRole;
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::{
        ProjectCreationRequest, create_project, recover_project_transactions,
        verify_project_package,
    };

    const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334478";
    const SECOND_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334480";
    const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334479";
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        parent: PathBuf,
        root: PathBuf,
        sample: Vec<u8>,
    }

    impl Fixture {
        fn new(referenced: bool) -> Self {
            let parent = std::env::temp_dir().join(format!(
                "hyper-sample-cleanup-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&parent).unwrap();
            let creation = create_project(
                &parent,
                ProjectCreationRequest {
                    directory_name: "Cleanup",
                    project_name: "Cleanup",
                },
            )
            .unwrap();
            let root = creation.project().root().to_owned();
            drop(creation);
            let sample = b"project-owned-sample-copy".to_vec();
            fs::create_dir_all(root.join("assets/samples")).unwrap();
            fs::write(root.join("assets/samples/unused.wav"), &sample).unwrap();
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(root.join(MANIFEST_NAME)).unwrap()).unwrap();
            manifest["assets"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": ASSET_ID,
                    "role": "sample",
                    "path": "assets/samples/unused.wav",
                    "media_type": "audio/wav",
                    "byte_length": sample.len(),
                    "sha256": sha256(&sample),
                    "extensions": {}
                }));
            if referenced {
                let clip = serde_json_canonicalizer::to_vec(&serde_json::json!({
                    "format": "hyper-audio-clip",
                    "format_version": 1,
                    "id": CLIP_ID,
                    "project_id": manifest["project_id"],
                    "track_id": "018f47a2-6b2d-7c11-8a44-001122334456",
                    "name": "Referenced",
                    "source": {"asset_id": ASSET_ID},
                    "extensions": {}
                }))
                .unwrap();
                fs::create_dir(root.join("clips")).unwrap();
                fs::write(root.join("clips/referenced.json"), &clip).unwrap();
                manifest["resources"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({
                        "id": CLIP_ID,
                        "role": "clip",
                        "path": "clips/referenced.json",
                        "media_type": "application/json",
                        "byte_length": clip.len(),
                        "sha256": sha256(&clip),
                        "extensions": {}
                    }));
            }
            fs::write(
                root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            verify_project_package(&root).unwrap();
            Self {
                parent,
                root,
                sample,
            }
        }

        fn request(&self) -> ([u8; 32], Vec<String>) {
            let verified = verify_project_package(&self.root).unwrap();
            (*verified.manifest_sha256(), vec![ASSET_ID.to_owned()])
        }

        fn add_unused_sample(&self) -> Vec<u8> {
            let sample = b"second-project-owned-sample-copy".to_vec();
            fs::write(self.root.join("assets/samples/second.wav"), &sample).unwrap();
            let mut manifest: Value =
                serde_json::from_slice(&fs::read(self.root.join(MANIFEST_NAME)).unwrap()).unwrap();
            manifest["assets"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "id": SECOND_ASSET_ID,
                    "role": "sample",
                    "path": "assets/samples/second.wav",
                    "media_type": "audio/wav",
                    "byte_length": sample.len(),
                    "sha256": sha256(&sample),
                    "extensions": {}
                }));
            fs::write(
                self.root.join(MANIFEST_NAME),
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            verify_project_package(&self.root).unwrap();
            sample
        }

        fn transaction_path(&self) -> PathBuf {
            fs::read_dir(self.root.join("backups"))
                .unwrap()
                .map(Result::unwrap)
                .find(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(SAMPLE_CLEANUP_TRANSACTION_PREFIX)
                })
                .unwrap()
                .path()
        }

        fn assert_no_cleanup_transaction(&self) {
            assert!(
                fs::read_dir(self.root.join("backups"))
                    .unwrap()
                    .map(Result::unwrap)
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(SAMPLE_CLEANUP_TRANSACTION_PREFIX))
            );
        }

        fn assert_previous(&self) {
            let verified = verify_project_package(&self.root).unwrap();
            assert_eq!(verified.manifest().assets().len(), 1);
            assert_eq!(
                fs::read(self.root.join("assets/samples/unused.wav")).unwrap(),
                self.sample
            );
        }

        fn assert_cleaned(&self) {
            let verified = verify_project_package(&self.root).unwrap();
            assert!(verified.manifest().assets().is_empty());
            assert!(!self.root.join("assets").exists());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.parent);
        }
    }

    #[test]
    fn removes_exact_unreferenced_samples_and_empty_asset_directories() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();

        let outcome = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
        )
        .unwrap();

        assert_eq!(outcome.removed_assets(), 1);
        assert_eq!(outcome.removed_bytes(), fixture.sample.len() as u64);
        assert_eq!(outcome.project_revision(), 2);
        fixture.assert_cleaned();
    }

    #[test]
    fn requires_the_complete_current_unused_sample_set() {
        for ids in [
            vec![ASSET_ID.to_owned()],
            vec![SECOND_ASSET_ID.to_owned()],
            vec![SECOND_ASSET_ID.to_owned(), ASSET_ID.to_owned()],
            vec![ASSET_ID.to_owned(), ASSET_ID.to_owned()],
            vec![ASSET_ID.to_owned(), CLIP_ID.to_owned()],
        ] {
            let fixture = Fixture::new(false);
            let second = fixture.add_unused_sample();
            let before = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
            let identity = digest(&before);

            let error = clean_unused_samples(
                &fixture.root,
                SampleCleanupRequest {
                    expected_project_sha256: identity,
                    asset_ids: &ids,
                },
            )
            .unwrap_err();

            assert_eq!(
                error.kind(),
                ProjectResourcePublicationErrorKind::InvalidResource
            );
            assert_eq!(fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(), before);
            let verified = verify_project_package(&fixture.root).unwrap();
            assert_eq!(verified.manifest().assets().len(), 2);
            assert_eq!(
                fs::read(fixture.root.join("assets/samples/unused.wav")).unwrap(),
                fixture.sample
            );
            assert_eq!(
                fs::read(fixture.root.join("assets/samples/second.wav")).unwrap(),
                second
            );
        }

        let fixture = Fixture::new(false);
        let second = fixture.add_unused_sample();
        let identity = digest(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap());
        let ids = vec![ASSET_ID.to_owned(), SECOND_ASSET_ID.to_owned()];
        let outcome = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
        )
        .unwrap();
        assert_eq!(outcome.removed_assets(), 2);
        assert_eq!(
            outcome.removed_bytes(),
            (fixture.sample.len() + second.len()) as u64
        );
        fixture.assert_cleaned();
    }

    #[test]
    fn refuses_changed_asset_bytes_without_committing_or_deleting_them() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();
        let replacement = vec![b'X'; fixture.sample.len()];

        let error = clean_unused_samples_observed(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
            |phase, root| {
                if phase == SampleCleanupPhase::Transaction {
                    fs::write(root.join("assets/samples/unused.wav"), &replacement).unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert!(!error.published());
        assert_eq!(
            fs::read(fixture.root.join("assets/samples/unused.wav")).unwrap(),
            replacement
        );
        let manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        assert_eq!(digest(&manifest), identity);
    }

    #[test]
    fn refuses_replaced_asset_object_without_committing_or_deleting_it() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();
        let active = fixture.root.join("assets/samples/unused.wav");
        let staged = fixture.root.join("assets/samples/replacement.wav");
        let replacement = vec![b'R'; fixture.sample.len()];

        let error = clean_unused_samples_observed(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
            |phase, _| {
                if phase == SampleCleanupPhase::Transaction {
                    fs::write(&staged, &replacement).unwrap();
                    fs::remove_file(&active).unwrap();
                    fs::rename(&staged, &active).unwrap();
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert!(!error.published());
        assert_eq!(fs::read(&active).unwrap(), replacement);
        assert!(!staged.exists());
        let manifest = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        assert_eq!(digest(&manifest), identity);
    }

    #[test]
    fn corrupt_rollback_backup_is_rejected_before_restore_and_retained() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            let _ = clean_unused_samples_observed(
                &fixture.root,
                SampleCleanupRequest {
                    expected_project_sha256: identity,
                    asset_ids: &ids,
                },
                |phase, _| {
                    assert_ne!(phase, SampleCleanupPhase::Asset(0));
                    Ok(())
                },
            );
        }));
        assert!(interrupted.is_err());
        let transaction = fixture.transaction_path();
        let backup = transaction.join("asset-00000.bin");
        let corrupt = vec![b'Y'; fixture.sample.len()];
        fs::write(&backup, &corrupt).unwrap();

        let error = recover_sample_cleanups(&fixture.root).unwrap_err();

        assert!(!error.published());
        assert_eq!(fs::read(&backup).unwrap(), corrupt);
        assert!(!fixture.root.join("assets/samples/unused.wav").exists());
        assert!(transaction.is_dir());
    }

    #[test]
    fn corrupt_committed_backup_remains_a_published_recovery_warning() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            let _ = clean_unused_samples_observed(
                &fixture.root,
                SampleCleanupRequest {
                    expected_project_sha256: identity,
                    asset_ids: &ids,
                },
                |phase, _| {
                    assert_ne!(phase, SampleCleanupPhase::Manifest);
                    Ok(())
                },
            );
        }));
        assert!(interrupted.is_err());
        let transaction = fixture.transaction_path();
        let backup = transaction.join("asset-00000.bin");
        let corrupt = vec![b'Z'; fixture.sample.len()];
        fs::write(&backup, &corrupt).unwrap();

        let error = recover_sample_cleanups(&fixture.root).unwrap_err();

        assert!(error.published());
        assert_eq!(fs::read(&backup).unwrap(), corrupt);
        assert!(transaction.is_dir());
        let verified = verify_project_package(&fixture.root).unwrap();
        assert!(verified.manifest().assets().is_empty());
    }

    #[test]
    fn refuses_a_still_referenced_sample_without_mutation() {
        let fixture = Fixture::new(true);
        let before = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let (identity, ids) = fixture.request();

        let error = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert_eq!(fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(), before);
        fixture.assert_previous();
    }

    #[test]
    fn every_cleanup_crash_phase_recovers_one_complete_revision() {
        for phase in [SampleCleanupPhase::Transaction, SampleCleanupPhase::Assets] {
            let fixture = Fixture::new(false);
            let (identity, ids) = fixture.request();
            let error = clean_unused_samples_observed(
                &fixture.root,
                SampleCleanupRequest {
                    expected_project_sha256: identity,
                    asset_ids: &ids,
                },
                |observed, _| {
                    if observed == phase {
                        Err(failure(
                            ProjectResourcePublicationErrorKind::Io,
                            "injected cleanup interruption",
                        ))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Io);
            fixture.assert_previous();
        }

        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();
        let outcome = clean_unused_samples_observed(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
            |observed, _| {
                if observed == SampleCleanupPhase::Manifest {
                    Err(failure(
                        ProjectResourcePublicationErrorKind::Io,
                        "injected cleanup interruption",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .unwrap();
        assert_eq!(outcome.removed_assets(), 1);
        fixture.assert_cleaned();
    }

    #[test]
    fn global_restart_recovery_covers_each_asset_and_directory_boundary() {
        for interrupted_phase in [
            SampleCleanupPhase::Transaction,
            SampleCleanupPhase::Asset(0),
            SampleCleanupPhase::Asset(1),
            SampleCleanupPhase::SampleDirectory,
            SampleCleanupPhase::AssetDirectory,
        ] {
            let fixture = Fixture::new(false);
            let second = fixture.add_unused_sample();
            let identity = digest(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap());
            let ids = vec![ASSET_ID.to_owned(), SECOND_ASSET_ID.to_owned()];
            let interrupted = catch_unwind(AssertUnwindSafe(|| {
                let _ = clean_unused_samples_observed(
                    &fixture.root,
                    SampleCleanupRequest {
                        expected_project_sha256: identity,
                        asset_ids: &ids,
                    },
                    |phase, _| {
                        assert_ne!(phase, interrupted_phase);
                        Ok(())
                    },
                );
            }));
            assert!(interrupted.is_err(), "{interrupted_phase:?}");

            let report = recover_project_transactions(&fixture.root).unwrap();

            assert_eq!(report.sample_cleanups().len(), 1, "{interrupted_phase:?}");
            assert_eq!(
                report.sample_cleanups()[0].disposition(),
                SampleCleanupRecoveryDisposition::RolledBack,
                "{interrupted_phase:?}"
            );
            let verified = verify_project_package(&fixture.root).unwrap();
            assert_eq!(verified.manifest().assets().len(), 2);
            assert_eq!(
                fs::read(fixture.root.join("assets/samples/unused.wav")).unwrap(),
                fixture.sample
            );
            assert_eq!(
                fs::read(fixture.root.join("assets/samples/second.wav")).unwrap(),
                second
            );
            fixture.assert_no_cleanup_transaction();
        }

        let fixture = Fixture::new(false);
        fixture.add_unused_sample();
        let identity = digest(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap());
        let ids = vec![ASSET_ID.to_owned(), SECOND_ASSET_ID.to_owned()];
        let interrupted = catch_unwind(AssertUnwindSafe(|| {
            let _ = clean_unused_samples_observed(
                &fixture.root,
                SampleCleanupRequest {
                    expected_project_sha256: identity,
                    asset_ids: &ids,
                },
                |phase, _| {
                    assert_ne!(phase, SampleCleanupPhase::Manifest);
                    Ok(())
                },
            );
        }));
        assert!(interrupted.is_err());

        let report = recover_project_transactions(&fixture.root).unwrap();

        assert_eq!(report.sample_cleanups().len(), 1);
        assert_eq!(
            report.sample_cleanups()[0].disposition(),
            SampleCleanupRecoveryDisposition::Committed
        );
        fixture.assert_cleaned();
        fixture.assert_no_cleanup_transaction();
    }

    #[test]
    fn committed_cleanup_reports_incomplete_recovery_as_published() {
        let fixture = Fixture::new(false);
        let (identity, ids) = fixture.request();

        let error = clean_unused_samples_observed(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
            |observed, root| {
                if observed == SampleCleanupPhase::Manifest {
                    fs::create_dir_all(root.join("assets/samples")).unwrap();
                    fs::write(root.join("assets/samples/undeclared.wav"), b"conflict").unwrap();
                    return Err(failure(
                        ProjectResourcePublicationErrorKind::Io,
                        "injected cleanup interruption",
                    ));
                }
                Ok(())
            },
        )
        .unwrap_err();

        assert!(error.published());
        assert_eq!(error.kind(), ProjectResourcePublicationErrorKind::Recovery);
        let manifest: Value =
            serde_json::from_slice(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap()).unwrap();
        assert!(manifest["assets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn recording_assets_are_never_cleanup_candidates() {
        let fixture = Fixture::new(false);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(fixture.root.join(MANIFEST_NAME)).unwrap()).unwrap();
        fs::create_dir(fixture.root.join("assets/recordings")).unwrap();
        fs::rename(
            fixture.root.join("assets/samples/unused.wav"),
            fixture.root.join("assets/recordings/unused.wav"),
        )
        .unwrap();
        fs::remove_dir(fixture.root.join("assets/samples")).unwrap();
        manifest["assets"][0]["role"] = "recording".into();
        manifest["assets"][0]["path"] = "assets/recordings/unused.wav".into();
        fs::write(
            fixture.root.join(MANIFEST_NAME),
            serde_json_canonicalizer::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let verified = verify_project_package(&fixture.root).unwrap();
        assert_eq!(verified.manifest().assets()[0].role(), AssetRole::Recording);
        let identity = *verified.manifest_sha256();
        let ids = vec![ASSET_ID.to_owned()];

        let error = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::InvalidResource
        );
        assert!(fixture.root.join("assets/recordings/unused.wav").is_file());
    }

    #[test]
    fn foreign_project_transaction_blocks_cleanup_before_mutation() {
        let fixture = Fixture::new(false);
        let before = fs::read(fixture.root.join(MANIFEST_NAME)).unwrap();
        let (identity, ids) = fixture.request();
        let foreign = fixture.root.join("backups").join(format!(
            "{}{}",
            crate::project_lock::PROJECT_RESOURCE_TRANSACTION_PREFIX,
            Uuid::new_v4()
        ));
        fs::create_dir(&foreign).unwrap();

        let error = clean_unused_samples(
            &fixture.root,
            SampleCleanupRequest {
                expected_project_sha256: identity,
                asset_ids: &ids,
            },
        )
        .unwrap_err();

        assert_eq!(
            error.kind(),
            ProjectResourcePublicationErrorKind::ProjectChanged
        );
        assert_eq!(fs::read(fixture.root.join(MANIFEST_NAME)).unwrap(), before);
        fixture.assert_previous();
        assert!(foreign.is_dir());
    }

    fn sha256(bytes: &[u8]) -> String {
        hex_digest(Sha256::digest(bytes).into())
    }
}
