#![doc = "Native project-package integrity and filesystem ownership verification."]
#![forbid(unsafe_code)]

mod mapping_publication;
mod recording_import;
mod stable_file;

pub use mapping_publication::{
    ControlMappingPublicationError, ControlMappingPublicationErrorKind,
    ControlMappingPublicationOutcome, ControlMappingPublicationRecovery,
    ControlMappingPublicationRecoveryDisposition, ControlMappingPublicationRequest,
    publish_control_mapping, recover_control_mapping_publications,
};
pub use recording_import::{
    RecordingImportError, RecordingImportOutcome, RecordingImportRecovery,
    RecordingImportRecoveryDisposition, RecordingImportRequest, import_finalized_recording_handoff,
    import_recording, recover_recording_imports,
};

use core::error::Error;
use core::fmt;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use hyper_project_model::{AssetDescriptor, ProjectManifest, ResourceDescriptor};
use same_file::Handle;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::stable_file::{is_link_like, read_open_stable, verify_open_stable};

const MANIFEST_NAME: &str = "project.json";
const MAX_JSON_BYTES: usize = 1_048_576;
const MAX_JSON_FILE_BYTES: u64 = 1_048_576;
const MAX_ENTRIES: usize = 32_768;
const MAX_DIRECTORIES: usize = 8_192;
const MAX_FILES: usize = 16_384;
const MAX_DEPTH: usize = 64;
const MAX_PATH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageErrorKind {
    Io,
    RootNotDirectory,
    LinkForbidden,
    UnsupportedEntry,
    UndeclaredEntry,
    LimitExceeded,
    PathEscape,
    ManifestInvalid,
    JsonInvalid,
    NonCanonicalJson,
    ByteLengthMismatch,
    Sha256Mismatch,
    ManagedTreeMismatch,
    ChangedDuringVerification,
    DescriptorNotFound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectPackageLimit {
    TotalResourceBytes {
        actual: u64,
        maximum: u64,
    },
    AssetCount {
        actual: usize,
        maximum: usize,
    },
    AssetFileBytes {
        asset_id: String,
        actual: u64,
        maximum: u64,
    },
}

/// Application-specific bounds checked against validated manifest declarations
/// before the owned tree or any payload file is scanned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectPackageVerificationLimits {
    pub max_total_resource_bytes: u64,
    pub max_assets: usize,
    pub max_asset_file_bytes: u64,
}

#[derive(Debug)]
pub struct StorageError {
    kind: StorageErrorKind,
    path: String,
    message: String,
    limit: Option<ProjectPackageLimit>,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl StorageError {
    #[must_use]
    pub const fn kind(&self) -> StorageErrorKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn limit(&self) -> Option<&ProjectPackageLimit> {
        self.limit.as_ref()
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.path, self.message)
    }
}

impl Error for StorageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug)]
pub struct StructurallyVerifiedProject {
    root: PathBuf,
    root_dir: Arc<Dir>,
    manifest: ProjectManifest,
    manifest_sha256: [u8; 32],
    verification_limits: Option<ProjectPackageVerificationLimits>,
}

impl StructurallyVerifiedProject {
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub const fn manifest(&self) -> &ProjectManifest {
        &self.manifest
    }

    #[must_use]
    pub const fn manifest_sha256(&self) -> &[u8; 32] {
        &self.manifest_sha256
    }

    /// Re-reads one declared resource as stable, digest-verified bytes.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the identifier is absent or the
    /// current file no longer matches this authority's manifest.
    pub fn read_resource(&self, resource_id: &str) -> Result<Vec<u8>, StorageError> {
        let descriptor = self
            .manifest
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == resource_id)
            .ok_or_else(|| descriptor_not_found(resource_id))?;
        self.read_descriptor(
            descriptor.path(),
            descriptor.byte_length(),
            descriptor.sha256(),
            MAX_JSON_FILE_BYTES,
        )
    }

    /// Re-reads one declared asset as stable, digest-verified bytes.
    ///
    /// The caller-supplied bound must be at least the declared file length and
    /// prevents an encoded asset from exhausting process memory.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection for an absent identifier, exceeded
    /// bound, path change, or content mismatch.
    pub fn read_asset(&self, asset_id: &str, maximum_bytes: u64) -> Result<Vec<u8>, StorageError> {
        let descriptor = self
            .manifest
            .assets()
            .iter()
            .find(|descriptor| descriptor.id() == asset_id)
            .ok_or_else(|| descriptor_not_found(asset_id))?;
        self.read_descriptor(
            descriptor.path(),
            descriptor.byte_length(),
            descriptor.sha256(),
            maximum_bytes,
        )
    }

    /// Confirms that the package still satisfies this structural authority.
    ///
    /// # Errors
    ///
    /// Returns a verification rejection when any owned file, manifest byte, or
    /// filesystem boundary has changed.
    pub fn confirm_unchanged(&self) -> Result<(), StorageError> {
        let current = verify_project_package_inner(&self.root, self.verification_limits)?;
        if !same_directory(&self.root_dir, &current.root_dir)?
            || current.manifest_sha256 != self.manifest_sha256
        {
            return Err(rejection(
                StorageErrorKind::ChangedDuringVerification,
                MANIFEST_NAME,
                "project manifest changed after structural verification",
            ));
        }
        Ok(())
    }

    fn read_descriptor(
        &self,
        relative: &str,
        expected_bytes: u64,
        expected_sha256: &str,
        maximum_bytes: u64,
    ) -> Result<Vec<u8>, StorageError> {
        if expected_bytes > maximum_bytes {
            return Err(rejection(
                StorageErrorKind::LimitExceeded,
                relative,
                "declared file exceeds the caller's byte limit",
            ));
        }
        let file = open_declared_file(&self.root_dir, relative)?;
        let verified = verify_open_stable(
            file,
            relative,
            expected_bytes,
            decode_sha256(expected_sha256, relative)?,
            true,
        )?;
        verified.bytes.ok_or_else(|| {
            rejection(
                StorageErrorKind::Io,
                relative,
                "verified file bytes are unavailable",
            )
        })
    }
}

/// Verifies one active project package's manifest, owned tree, canonical
/// resource bytes, declared lengths, and SHA-256 digests.
///
/// This boundary deliberately does not claim resource-schema, cross-resource,
/// or audio-container validity.
///
/// # Errors
///
/// Returns a classified rejection for I/O failures, links, path escapes,
/// undeclared entries, limits, malformed or noncanonical JSON, corruption, or
/// concurrent package changes.
pub fn verify_project_package(
    root: impl AsRef<Path>,
) -> Result<StructurallyVerifiedProject, StorageError> {
    verify_project_package_inner(root.as_ref(), None)
}

/// Verifies a project package with application-specific declaration bounds.
///
/// The limits are checked after the bounded manifest is parsed and before any
/// owned-tree traversal or payload hashing. The resulting authority retains
/// the limits and reapplies them during [`StructurallyVerifiedProject::confirm_unchanged`].
///
/// # Errors
///
/// Returns a classified rejection for invalid limits, an excessive manifest
/// declaration, or any error reported by [`verify_project_package`].
pub fn verify_project_package_with_limits(
    root: impl AsRef<Path>,
    limits: ProjectPackageVerificationLimits,
) -> Result<StructurallyVerifiedProject, StorageError> {
    verify_project_package_inner(root.as_ref(), Some(limits))
}

fn verify_project_package_inner(
    root: &Path,
    limits: Option<ProjectPackageVerificationLimits>,
) -> Result<StructurallyVerifiedProject, StorageError> {
    if let Some(limits) = limits
        && (limits.max_total_resource_bytes == 0
            || limits.max_assets == 0
            || limits.max_asset_file_bytes == 0)
    {
        return Err(rejection(
            StorageErrorKind::LimitExceeded,
            ".",
            "project verification limits must be positive",
        ));
    }
    let (canonical_root, root_dir) = open_stable_project_root(root)?;
    let manifest_bytes = read_open_stable(
        open_declared_file(&root_dir, MANIFEST_NAME)?,
        MANIFEST_NAME,
        MAX_JSON_FILE_BYTES,
    )?;
    assert_canonical_json(&manifest_bytes, MANIFEST_NAME)?;
    let manifest = ProjectManifest::parse(&manifest_bytes).map_err(|error| StorageError {
        kind: StorageErrorKind::ManifestInvalid,
        path: MANIFEST_NAME.to_owned(),
        message: "project manifest is invalid".to_owned(),
        limit: None,
        source: Some(Box::new(error)),
    })?;

    preflight_manifest(&manifest, limits)?;

    let policy = TreePolicy::new(&manifest);
    let initial_files = collect_owned_tree(root, &policy)?;
    require_managed_tree(&initial_files, &policy)?;
    verify_records(&root_dir, &manifest)?;

    let final_files = collect_owned_tree(root, &policy)?;
    if initial_files != final_files {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            ".",
            "project filesystem tree changed during verification",
        ));
    }
    require_managed_tree(&final_files, &policy)?;
    let final_manifest = read_open_stable(
        open_declared_file(&root_dir, MANIFEST_NAME)?,
        MANIFEST_NAME,
        MAX_JSON_FILE_BYTES,
    )?;
    if manifest_bytes != final_manifest {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            MANIFEST_NAME,
            "project manifest changed during verification",
        ));
    }

    Ok(StructurallyVerifiedProject {
        root: canonical_root,
        root_dir,
        manifest,
        manifest_sha256: Sha256::digest(&manifest_bytes).into(),
        verification_limits: limits,
    })
}

pub(crate) fn open_stable_project_root(root: &Path) -> Result<(PathBuf, Arc<Dir>), StorageError> {
    let root_metadata =
        fs::symlink_metadata(root).map_err(|error| io_error(".", "inspect project root", error))?;
    if is_link_like(&root_metadata) {
        return Err(rejection(
            StorageErrorKind::LinkForbidden,
            ".",
            "project root cannot be a link or reparse point",
        ));
    }
    if !root_metadata.is_dir() {
        return Err(rejection(
            StorageErrorKind::RootNotDirectory,
            ".",
            "project root must be a real directory",
        ));
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|error| io_error(".", "resolve project root", error))?;
    let root_dir = Arc::new(
        Dir::open_ambient_dir(root, ambient_authority())
            .map_err(|error| io_error(".", "open project root", error))?,
    );
    let path_root_handle =
        Handle::from_path(root).map_err(|error| io_error(".", "reopen project root", error))?;
    let opened_root_handle = directory_handle(&root_dir)?;
    if opened_root_handle != path_root_handle {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            ".",
            "project root changed while it was being opened",
        ));
    }
    Ok((canonical_root, root_dir))
}

pub(crate) fn verify_project_candidate(
    root: &Path,
    root_dir: &Dir,
    manifest_bytes: &[u8],
) -> Result<ProjectManifest, StorageError> {
    assert_canonical_json(manifest_bytes, MANIFEST_NAME)?;
    let manifest = ProjectManifest::parse(manifest_bytes).map_err(|error| StorageError {
        kind: StorageErrorKind::ManifestInvalid,
        path: MANIFEST_NAME.to_owned(),
        message: "candidate project manifest is invalid".to_owned(),
        limit: None,
        source: Some(Box::new(error)),
    })?;
    preflight_manifest(&manifest, None)?;
    let policy = TreePolicy::new(&manifest);
    let initial_files = collect_owned_tree(root, &policy)?;
    require_managed_tree(&initial_files, &policy)?;
    verify_records(root_dir, &manifest)?;
    let final_files = collect_owned_tree(root, &policy)?;
    if initial_files != final_files {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            ".",
            "project filesystem tree changed during candidate verification",
        ));
    }
    Ok(manifest)
}

pub(crate) fn verify_retained_project_candidate(
    root_dir: &Dir,
    manifest_bytes: &[u8],
) -> Result<ProjectManifest, StorageError> {
    assert_canonical_json(manifest_bytes, MANIFEST_NAME)?;
    let manifest = ProjectManifest::parse(manifest_bytes).map_err(|error| StorageError {
        kind: StorageErrorKind::ManifestInvalid,
        path: MANIFEST_NAME.to_owned(),
        message: "retained project manifest is invalid".to_owned(),
        limit: None,
        source: Some(Box::new(error)),
    })?;
    preflight_manifest(&manifest, None)?;
    let policy = TreePolicy::new(&manifest);
    let initial_files = collect_owned_tree_retained(root_dir, &policy)?;
    require_managed_tree(&initial_files, &policy)?;
    verify_records(root_dir, &manifest)?;
    let final_files = collect_owned_tree_retained(root_dir, &policy)?;
    if initial_files != final_files {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            ".",
            "retained project tree changed during verification",
        ));
    }
    require_managed_tree(&final_files, &policy)?;
    let final_manifest = read_open_stable(
        open_declared_file(root_dir, MANIFEST_NAME)?,
        MANIFEST_NAME,
        MAX_JSON_FILE_BYTES,
    )?;
    if manifest_bytes != final_manifest {
        return Err(rejection(
            StorageErrorKind::ChangedDuringVerification,
            MANIFEST_NAME,
            "retained project manifest changed during verification",
        ));
    }
    Ok(manifest)
}

fn preflight_manifest(
    manifest: &ProjectManifest,
    limits: Option<ProjectPackageVerificationLimits>,
) -> Result<(), StorageError> {
    let mut total_resource_bytes = 0_u64;
    for descriptor in manifest.resources() {
        if descriptor.byte_length() > MAX_JSON_FILE_BYTES {
            return Err(rejection(
                StorageErrorKind::LimitExceeded,
                descriptor.path(),
                "resource exceeds the JSON byte limit",
            ));
        }
        total_resource_bytes = total_resource_bytes
            .checked_add(descriptor.byte_length())
            .ok_or_else(|| {
                rejection(
                    StorageErrorKind::LimitExceeded,
                    MANIFEST_NAME,
                    "declared resource byte total overflowed",
                )
            })?;
    }

    let Some(limits) = limits else {
        return Ok(());
    };
    if total_resource_bytes > limits.max_total_resource_bytes {
        return Err(limit_rejection(
            MANIFEST_NAME,
            "declared resource bytes exceed the verification limit",
            ProjectPackageLimit::TotalResourceBytes {
                actual: total_resource_bytes,
                maximum: limits.max_total_resource_bytes,
            },
        ));
    }
    let asset_count = manifest.assets().len();
    if asset_count > limits.max_assets {
        return Err(limit_rejection(
            MANIFEST_NAME,
            "declared asset count exceeds the verification limit",
            ProjectPackageLimit::AssetCount {
                actual: asset_count,
                maximum: limits.max_assets,
            },
        ));
    }
    for descriptor in manifest.assets() {
        if descriptor.byte_length() > limits.max_asset_file_bytes {
            return Err(limit_rejection(
                descriptor.path(),
                "declared asset bytes exceed the verification limit",
                ProjectPackageLimit::AssetFileBytes {
                    asset_id: descriptor.id().to_owned(),
                    actual: descriptor.byte_length(),
                    maximum: limits.max_asset_file_bytes,
                },
            ));
        }
    }
    Ok(())
}

fn verify_records(root: &Dir, manifest: &ProjectManifest) -> Result<(), StorageError> {
    for descriptor in manifest.resources() {
        let verified = verify_open_stable(
            open_declared_file(root, descriptor.path())?,
            descriptor.path(),
            descriptor.byte_length(),
            decode_sha256(descriptor.sha256(), descriptor.path())?,
            true,
        )?;
        let bytes = verified.bytes.ok_or_else(|| {
            rejection(
                StorageErrorKind::Io,
                descriptor.path(),
                "verified resource bytes are unavailable",
            )
        })?;
        assert_canonical_json(&bytes, descriptor.path())?;
    }
    for descriptor in manifest.assets() {
        verify_open_stable(
            open_declared_file(root, descriptor.path())?,
            descriptor.path(),
            descriptor.byte_length(),
            decode_sha256(descriptor.sha256(), descriptor.path())?,
            false,
        )?;
    }
    Ok(())
}

fn assert_canonical_json(bytes: &[u8], path: &str) -> Result<(), StorageError> {
    let value: Value =
        hyper_strict_json::parse(bytes, MAX_JSON_BYTES).map_err(|error| StorageError {
            kind: StorageErrorKind::JsonInvalid,
            path: path.to_owned(),
            message: "JSON document is invalid".to_owned(),
            limit: None,
            source: Some(Box::new(error)),
        })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|error| StorageError {
        kind: StorageErrorKind::ManifestInvalid,
        path: path.to_owned(),
        message: "JSON document cannot be canonicalized".to_owned(),
        limit: None,
        source: Some(Box::new(error)),
    })?;
    if bytes != canonical {
        return Err(rejection(
            StorageErrorKind::NonCanonicalJson,
            path,
            "JSON document is not canonical RFC 8785 JSON",
        ));
    }
    Ok(())
}

fn open_declared_file(root: &Dir, relative: &str) -> Result<fs::File, StorageError> {
    let mut segments = relative.split('/').peekable();
    let mut directory = root
        .try_clone()
        .map_err(|error| io_error(relative, "clone project root handle", error))?;
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            let metadata = directory
                .symlink_metadata(segment)
                .map_err(|error| io_error(relative, "inspect declared file", error))?;
            if metadata.file_type().is_symlink() {
                return Err(rejection(
                    StorageErrorKind::LinkForbidden,
                    relative,
                    "declared file cannot be a link or reparse point",
                ));
            }
            if !metadata.is_file() {
                return Err(rejection(
                    StorageErrorKind::UnsupportedEntry,
                    relative,
                    "declared path must end at a regular file",
                ));
            }
            let mut options = OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            return directory
                .open_with(segment, &options)
                .map(cap_std::fs::File::into_std)
                .map_err(|error| io_error(relative, "open declared file", error));
        }
        let metadata = directory
            .symlink_metadata(segment)
            .map_err(|error| io_error(relative, "inspect declared directory", error))?;
        if metadata.file_type().is_symlink() {
            return Err(rejection(
                StorageErrorKind::LinkForbidden,
                relative,
                "declared path cannot traverse a link or reparse point",
            ));
        }
        if !metadata.is_dir() {
            return Err(rejection(
                StorageErrorKind::UnsupportedEntry,
                relative,
                "declared path ancestor must be a directory",
            ));
        }
        directory = directory
            .open_dir_nofollow(segment)
            .map_err(|error| io_error(relative, "open declared directory", error))?;
    }
    Err(rejection(
        StorageErrorKind::PathEscape,
        relative,
        "declared path has no file component",
    ))
}

fn directory_handle(directory: &Dir) -> Result<Handle, StorageError> {
    let file = directory
        .try_clone()
        .map_err(|error| io_error(".", "clone project root handle", error))?
        .into_std_file();
    Handle::from_file(file).map_err(|error| io_error(".", "inspect project root handle", error))
}

fn same_directory(left: &Dir, right: &Dir) -> Result<bool, StorageError> {
    Ok(directory_handle(left)? == directory_handle(right)?)
}

struct TreePolicy {
    declared_files: BTreeSet<String>,
    allowed_directories: BTreeSet<String>,
    managed_files: Vec<String>,
}

impl TreePolicy {
    fn new(manifest: &ProjectManifest) -> Self {
        let mut declared_files = BTreeSet::new();
        declared_files.insert(MANIFEST_NAME.to_owned());
        for path in manifest
            .resources()
            .iter()
            .map(ResourceDescriptor::path)
            .chain(manifest.assets().iter().map(AssetDescriptor::path))
        {
            declared_files.insert(path.to_owned());
        }
        let mut managed_files = declared_files
            .iter()
            .filter(|path| path.as_str() != MANIFEST_NAME)
            .cloned()
            .collect::<Vec<_>>();
        managed_files.sort();

        let mut allowed_directories = BTreeSet::new();
        for path in &declared_files {
            let segments = path.split('/').collect::<Vec<_>>();
            for length in 1..segments.len() {
                allowed_directories.insert(segments[..length].join("/"));
            }
        }
        Self {
            declared_files,
            allowed_directories,
            managed_files,
        }
    }

    fn allows_directory(&self, path: &str) -> bool {
        self.allowed_directories.contains(path) || local_only_directory(path)
    }

    fn allows_file(&self, path: &str) -> bool {
        self.declared_files.contains(path) || local_only_file(path)
    }
}

fn collect_owned_tree(root: &Path, policy: &TreePolicy) -> Result<Vec<String>, StorageError> {
    struct Pending {
        path: PathBuf,
        prefix: String,
        depth: usize,
    }
    let mut files = Vec::new();
    let mut pending = vec![Pending {
        path: root.to_path_buf(),
        prefix: String::new(),
        depth: 0,
    }];
    let mut entries = 0_usize;
    let mut directories = 1_usize;
    let mut path_bytes = 0_usize;
    while let Some(current) = pending.pop() {
        let children = fs::read_dir(&current.path)
            .map_err(|error| io_error(".", "read project tree", error))?;
        for child in children {
            let child = child.map_err(|error| io_error(".", "read project entry", error))?;
            let name = child.file_name().into_string().map_err(|_| {
                rejection(
                    StorageErrorKind::UnsupportedEntry,
                    ".",
                    "project tree contains a non-Unicode entry name",
                )
            })?;
            let relative = if current.prefix.is_empty() {
                name
            } else {
                format!("{}/{}", current.prefix, name)
            };
            entries = entries.checked_add(1).ok_or_else(tree_limit)?;
            path_bytes = path_bytes
                .checked_add(relative.len())
                .ok_or_else(tree_limit)?;
            if entries > MAX_ENTRIES || path_bytes > MAX_PATH_BYTES {
                return Err(tree_limit());
            }
            let metadata = fs::symlink_metadata(child.path())
                .map_err(|error| io_error(&relative, "inspect project entry", error))?;
            if is_link_like(&metadata) {
                return Err(rejection(
                    StorageErrorKind::LinkForbidden,
                    &relative,
                    "symbolic links, junctions, and reparse points are forbidden",
                ));
            }
            if metadata.is_dir() {
                let depth = current.depth + 1;
                directories = directories.checked_add(1).ok_or_else(tree_limit)?;
                if depth > MAX_DEPTH || directories > MAX_DIRECTORIES {
                    return Err(tree_limit());
                }
                if !policy.allows_directory(&relative) {
                    return Err(rejection(
                        StorageErrorKind::UndeclaredEntry,
                        &relative,
                        "undeclared directory is forbidden",
                    ));
                }
                pending.push(Pending {
                    path: child.path(),
                    prefix: relative,
                    depth,
                });
            } else if metadata.is_file() {
                if !policy.allows_file(&relative) {
                    return Err(rejection(
                        StorageErrorKind::UndeclaredEntry,
                        &relative,
                        "undeclared file is forbidden",
                    ));
                }
                files.push(relative);
                if files.len() > MAX_FILES {
                    return Err(tree_limit());
                }
            } else {
                return Err(rejection(
                    StorageErrorKind::UnsupportedEntry,
                    &relative,
                    "unsupported filesystem entry is forbidden",
                ));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn collect_owned_tree_retained(
    root: &Dir,
    policy: &TreePolicy,
) -> Result<Vec<String>, StorageError> {
    struct Pending {
        dir: Dir,
        prefix: String,
        depth: usize,
    }
    let mut files = Vec::new();
    let mut pending = vec![Pending {
        dir: root
            .try_clone()
            .map_err(|error| io_error(".", "clone retained project root", error))?,
        prefix: String::new(),
        depth: 0,
    }];
    let mut entries = 0_usize;
    let mut directories = 1_usize;
    let mut path_bytes = 0_usize;
    while let Some(current) = pending.pop() {
        let children = current
            .dir
            .entries()
            .map_err(|error| io_error(".", "read retained project tree", error))?;
        for child in children {
            let child =
                child.map_err(|error| io_error(".", "read retained project entry", error))?;
            let name = child.file_name().into_string().map_err(|_| {
                rejection(
                    StorageErrorKind::UnsupportedEntry,
                    ".",
                    "retained project tree contains a non-Unicode entry name",
                )
            })?;
            let relative = if current.prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", current.prefix, name)
            };
            entries = entries.checked_add(1).ok_or_else(tree_limit)?;
            path_bytes = path_bytes
                .checked_add(relative.len())
                .ok_or_else(tree_limit)?;
            if entries > MAX_ENTRIES || path_bytes > MAX_PATH_BYTES {
                return Err(tree_limit());
            }
            let file_type = child
                .file_type()
                .map_err(|error| io_error(&relative, "inspect retained project entry", error))?;
            if file_type.is_symlink() {
                return Err(rejection(
                    StorageErrorKind::LinkForbidden,
                    &relative,
                    "symbolic links and reparse points are forbidden",
                ));
            }
            if file_type.is_dir() {
                let depth = current.depth + 1;
                directories = directories.checked_add(1).ok_or_else(tree_limit)?;
                if depth > MAX_DEPTH || directories > MAX_DIRECTORIES {
                    return Err(tree_limit());
                }
                if !policy.allows_directory(&relative) {
                    return Err(rejection(
                        StorageErrorKind::UndeclaredEntry,
                        &relative,
                        "undeclared directory is forbidden",
                    ));
                }
                let dir = current.dir.open_dir_nofollow(&name).map_err(|error| {
                    io_error(&relative, "open retained project directory", error)
                })?;
                pending.push(Pending {
                    dir,
                    prefix: relative,
                    depth,
                });
            } else if file_type.is_file() {
                if !policy.allows_file(&relative) {
                    return Err(rejection(
                        StorageErrorKind::UndeclaredEntry,
                        &relative,
                        "undeclared file is forbidden",
                    ));
                }
                files.push(relative);
                if files.len() > MAX_FILES {
                    return Err(tree_limit());
                }
            } else {
                return Err(rejection(
                    StorageErrorKind::UnsupportedEntry,
                    &relative,
                    "unsupported filesystem entry is forbidden",
                ));
            }
        }
    }
    files.sort();
    Ok(files)
}

fn require_managed_tree(files: &[String], policy: &TreePolicy) -> Result<(), StorageError> {
    let managed = files
        .iter()
        .filter(|path| path.as_str() != MANIFEST_NAME && !local_only_file(path))
        .cloned()
        .collect::<Vec<_>>();
    if managed != policy.managed_files {
        return Err(rejection(
            StorageErrorKind::ManagedTreeMismatch,
            ".",
            "managed project files do not exactly match the manifest",
        ));
    }
    Ok(())
}

fn local_only_directory(path: &str) -> bool {
    path == "cache"
        || path.starts_with("cache/")
        || path == "backups"
        || path.starts_with("backups/")
}

fn local_only_file(path: &str) -> bool {
    path.starts_with("cache/") || path.starts_with("backups/")
}

fn decode_sha256(value: &str, path: &str) -> Result<[u8; 32], StorageError> {
    if value.len() != 64 {
        return Err(invalid_sha256(path));
    }
    let mut output = [0_u8; 32];
    for (slot, pair) in output.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let high = hex_digit(pair[0]).ok_or_else(|| invalid_sha256(path))?;
        let low = hex_digit(pair[1]).ok_or_else(|| invalid_sha256(path))?;
        *slot = (high << 4) | low;
    }
    Ok(output)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn invalid_sha256(path: &str) -> StorageError {
    rejection(
        StorageErrorKind::ManifestInvalid,
        path,
        "descriptor SHA-256 is invalid",
    )
}

fn descriptor_not_found(identifier: &str) -> StorageError {
    rejection(
        StorageErrorKind::DescriptorNotFound,
        identifier,
        "project descriptor does not exist",
    )
}

fn tree_limit() -> StorageError {
    rejection(
        StorageErrorKind::LimitExceeded,
        ".",
        "project filesystem tree exceeds a verification limit",
    )
}

pub(crate) fn rejection(kind: StorageErrorKind, path: &str, message: &str) -> StorageError {
    StorageError {
        kind,
        path: path.to_owned(),
        message: message.to_owned(),
        limit: None,
        source: None,
    }
}

fn limit_rejection(path: &str, message: &str, limit: ProjectPackageLimit) -> StorageError {
    StorageError {
        kind: StorageErrorKind::LimitExceeded,
        path: path.to_owned(),
        message: message.to_owned(),
        limit: Some(limit),
        source: None,
    }
}

pub(crate) fn io_error(path: &str, action: &str, source: io::Error) -> StorageError {
    StorageError {
        kind: StorageErrorKind::Io,
        path: path.to_owned(),
        message: action.to_owned(),
        limit: None,
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    #[test]
    fn canonicalizer_matches_rfc_8785_primitive_vector() {
        let input = br#"{
            "numbers": [333333333.33333329, 1E30, 4.50,
                        2e-3, 0.000000000000000000000000001],
            "string": "\u20ac$\u000F\u000aA'\u0042\u0022\u005c\\\"\/",
            "literals": [null, true, false]
        }"#;
        let value: Value = serde_json::from_slice(input).expect("valid JSON");
        let actual =
            serde_json_canonicalizer::to_vec(&value).expect("RFC 8785 value is canonicalizable");
        let expected = r#"{"literals":[null,true,false],"numbers":[333333333.3333333,1e+30,4.5,0.002,1e-27],"string":"€$\u000f\nA'B\"\\\\\"/"}"#;

        assert_eq!(actual, expected.as_bytes());
    }
}
