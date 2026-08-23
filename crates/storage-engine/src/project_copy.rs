use core::error::Error;
use core::fmt;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_fs_authority::{identity, remove_directory_entry, rename_no_replace};
use hyper_project_model::{ProjectManifest, is_valid_project_name};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{ProjectLock, ProjectLockError};
use super::stable_file::copy_open_stable;
use super::{
    MANIFEST_NAME, MAX_JSON_BYTES, StructurallyVerifiedProject, decode_sha256, open_declared_file,
    open_stable_project_root, same_directory, verify_project_package,
};

const STAGING_PREFIX: &str = ".hyper-project-copy-";

#[derive(Clone, Copy, Debug)]
pub struct ProjectCopyRequest<'a> {
    pub source: &'a Path,
    pub destination_parent: &'a Path,
    pub directory_name: &'a str,
    pub project_name: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectCopyErrorKind {
    InvalidRequest,
    TargetConflict,
    SourceChanged,
    SourceInvalid,
    Serialization,
    Io,
    Verification,
    Cleanup,
}

#[derive(Debug)]
pub struct ProjectCopyError {
    kind: ProjectCopyErrorKind,
    path: String,
    message: String,
    published: bool,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectCopyError {
    #[must_use]
    pub const fn kind(&self) -> ProjectCopyErrorKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns whether the independently verified target was atomically
    /// published before this error occurred.
    #[must_use]
    pub const fn published(&self) -> bool {
        self.published
    }
}

impl fmt::Display for ProjectCopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path == "." {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for ProjectCopyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug)]
pub struct ProjectCopyOutcome {
    project: StructurallyVerifiedProject,
    source_project_id: String,
}

impl ProjectCopyOutcome {
    #[must_use]
    pub const fn project(&self) -> &StructurallyVerifiedProject {
        &self.project
    }

    #[must_use]
    pub fn source_project_id(&self) -> &str {
        &self.source_project_id
    }

    #[must_use]
    pub fn into_project(self) -> StructurallyVerifiedProject {
        self.project
    }
}

struct PreparedCopy {
    manifest: Vec<u8>,
    resources: Vec<(String, Vec<u8>)>,
}

struct LockedSourceCopy {
    project: StructurallyVerifiedProject,
    _lock: ProjectLock,
}

/// Copies one verified version-1 project into a new independent package.
///
/// All project-scoped JSON resources receive a fresh project identity, assets
/// are streamed as exact verified bytes, and the target appears only after the
/// complete staged candidate passes package verification.
///
/// # Errors
///
/// Returns a classified request, source, filesystem, serialization, candidate
/// verification, publication, or cleanup failure. Existing destinations are
/// never overwritten.
pub fn copy_project(
    request: ProjectCopyRequest<'_>,
) -> Result<ProjectCopyOutcome, ProjectCopyError> {
    validate_request(request)?;
    let locked_source = lock_source(request.source)?;
    let source = &locked_source.project;
    let source_project_id = source.manifest().project_id().to_owned();
    let destination_project_id = Uuid::new_v4().to_string();
    let prepared = prepare_copy(
        source,
        &source_project_id,
        &destination_project_id,
        request.project_name,
    )?;
    let (parent_path, parent_dir) =
        open_stable_project_root(request.destination_parent).map_err(|error| {
            caused(
                ProjectCopyErrorKind::Io,
                ".",
                "open destination parent",
                error,
            )
        })?;
    ensure_target_absent(&parent_dir, request.directory_name)?;
    let stage_name = format!("{STAGING_PREFIX}{}", Uuid::new_v4());
    parent_dir.create_dir(&stage_name).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            &stage_name,
            "create private project-copy stage",
            error,
        )
    })?;
    sync_directory(&parent_dir).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            ".",
            "sync destination parent after staging creation",
            error,
        )
    })?;
    let stage = parent_dir.open_dir_nofollow(&stage_name).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            &stage_name,
            "retain private project-copy stage",
            error,
        )
    })?;
    let stage_identity = identity(&stage.dir_metadata().map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            &stage_name,
            "inspect private project-copy stage",
            error,
        )
    })?);
    let stage_path = parent_path.join(&stage_name);
    let mut published = false;
    let result = stage_and_publish(
        source,
        &prepared,
        stage,
        &stage_path,
        &parent_dir,
        &parent_path,
        &stage_name,
        stage_identity,
        request.directory_name,
        &mut published,
    );
    match result {
        Ok(project) => Ok(ProjectCopyOutcome {
            project,
            source_project_id,
        }),
        Err(error) if published => Err(error),
        Err(error) => match remove_directory_entry(&parent_dir, &stage_name, stage_identity) {
            Ok(()) => {
                let _ = sync_directory(&parent_dir);
                Err(error)
            }
            Err(cleanup) => Err(caused(
                ProjectCopyErrorKind::Cleanup,
                &stage_name,
                format!("project copy failed and staging cleanup failed: {error}"),
                cleanup,
            )),
        },
    }
}

fn lock_source(source_path: &Path) -> Result<LockedSourceCopy, ProjectCopyError> {
    let initially_verified = verify_project_package(source_path).map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceInvalid,
            ".",
            "verify source project",
            error,
        )
    })?;
    let source_backups = open_or_create_directory(&initially_verified.root_dir, "backups")?;
    let lock = ProjectLock::acquire(&source_backups).map_err(lock_error)?;
    let project = verify_project_package(source_path).map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceInvalid,
            ".",
            "reverify locked source project",
            error,
        )
    })?;
    if !same_directory(&initially_verified.root_dir, &project.root_dir).map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceChanged,
            ".",
            "compare locked source project root",
            error,
        )
    })? {
        return Err(failure(
            ProjectCopyErrorKind::SourceChanged,
            ".",
            "source project root changed before copy staging",
        ));
    }
    Ok(LockedSourceCopy {
        project,
        _lock: lock,
    })
}

#[allow(clippy::too_many_arguments)]
fn stage_and_publish(
    source: &StructurallyVerifiedProject,
    prepared: &PreparedCopy,
    stage: Dir,
    stage_path: &Path,
    parent: &Dir,
    parent_path: &Path,
    stage_name: &str,
    stage_identity: (u64, u64),
    destination_name: &str,
    published: &mut bool,
) -> Result<StructurallyVerifiedProject, ProjectCopyError> {
    for (path, bytes) in &prepared.resources {
        write_stage_bytes(&stage, path, bytes)?;
    }
    for asset in source.manifest().assets() {
        copy_stage_asset(
            source,
            &stage,
            asset.path(),
            asset.byte_length(),
            asset.sha256(),
        )?;
    }
    source.confirm_unchanged().map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceChanged,
            ".",
            "source project changed while staging its copy",
            error,
        )
    })?;
    write_stage_bytes(&stage, MANIFEST_NAME, &prepared.manifest)?;
    sync_directory(&stage)
        .map_err(|error| caused(ProjectCopyErrorKind::Io, ".", "sync staged project", error))?;
    verify_project_package(stage_path).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Verification,
            ".",
            "verify staged independent project",
            error,
        )
    })?;
    drop(stage);
    rename_no_replace(parent, stage_name, stage_identity, parent, destination_name).map_err(
        |error| {
            caused(
                ProjectCopyErrorKind::TargetConflict,
                destination_name,
                "publish staged project without overwriting an existing destination",
                error,
            )
        },
    )?;
    *published = true;
    sync_directory(parent)
        .map_err(|error| {
            caused(
                ProjectCopyErrorKind::Io,
                ".",
                "sync destination parent after project activation",
                error,
            )
        })
        .map_err(mark_published)?;
    verify_project_package(parent_path.join(destination_name))
        .map_err(|error| {
            caused(
                ProjectCopyErrorKind::Verification,
                destination_name,
                "verify published independent project",
                error,
            )
        })
        .map_err(mark_published)
}

fn prepare_copy(
    source: &StructurallyVerifiedProject,
    source_project_id: &str,
    destination_project_id: &str,
    destination_name: &str,
) -> Result<PreparedCopy, ProjectCopyError> {
    let source_manifest = source.read_manifest().map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceInvalid,
            MANIFEST_NAME,
            "read source manifest",
            error,
        )
    })?;
    let mut manifest: Value =
        hyper_strict_json::parse(&source_manifest, MAX_JSON_BYTES).map_err(|error| {
            caused(
                ProjectCopyErrorKind::SourceInvalid,
                MANIFEST_NAME,
                "parse source manifest",
                error,
            )
        })?;
    let manifest_object = manifest.as_object_mut().ok_or_else(|| {
        failure(
            ProjectCopyErrorKind::SourceInvalid,
            MANIFEST_NAME,
            "source manifest is not a JSON object",
        )
    })?;
    manifest_object.insert("project_id".to_owned(), destination_project_id.into());
    manifest_object.insert("revision".to_owned(), 1_u64.into());
    manifest_object.insert("name".to_owned(), destination_name.into());
    let descriptors = manifest_object
        .get_mut("resources")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            failure(
                ProjectCopyErrorKind::SourceInvalid,
                MANIFEST_NAME,
                "source manifest resources are invalid",
            )
        })?;
    let resources = rewrite_resources(
        source,
        source_project_id,
        destination_project_id,
        descriptors,
    )?;
    let manifest = serde_json_canonicalizer::to_vec(&manifest).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Serialization,
            MANIFEST_NAME,
            "canonicalize copied project manifest",
            error,
        )
    })?;
    let candidate = ProjectManifest::parse(&manifest).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Verification,
            MANIFEST_NAME,
            "validate copied project manifest",
            error,
        )
    })?;
    if candidate.project_id() != destination_project_id
        || candidate.name() != destination_name
        || candidate.revision() != 1
    {
        return Err(failure(
            ProjectCopyErrorKind::Verification,
            MANIFEST_NAME,
            "copied project manifest identity is inconsistent",
        ));
    }
    Ok(PreparedCopy {
        manifest,
        resources,
    })
}

fn rewrite_resources(
    source: &StructurallyVerifiedProject,
    source_project_id: &str,
    destination_project_id: &str,
    descriptors: &mut [Value],
) -> Result<Vec<(String, Vec<u8>)>, ProjectCopyError> {
    if descriptors.len() != source.manifest().resources().len() {
        return Err(failure(
            ProjectCopyErrorKind::SourceInvalid,
            MANIFEST_NAME,
            "source manifest resource count changed during copy preparation",
        ));
    }
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(source.manifest().resources().len())
        .map_err(|_| {
            failure(
                ProjectCopyErrorKind::Serialization,
                MANIFEST_NAME,
                "reserve copy resources",
            )
        })?;
    for (descriptor, raw_descriptor) in source.manifest().resources().iter().zip(descriptors) {
        if raw_descriptor.get("id").and_then(Value::as_str) != Some(descriptor.id()) {
            return Err(failure(
                ProjectCopyErrorKind::SourceInvalid,
                MANIFEST_NAME,
                "source manifest resource order is not authoritative",
            ));
        }
        let source_bytes = source.read_resource(descriptor.id()).map_err(|error| {
            caused(
                ProjectCopyErrorKind::SourceInvalid,
                descriptor.path(),
                "read verified source resource",
                error,
            )
        })?;
        let mut resource: Value =
            hyper_strict_json::parse(&source_bytes, MAX_JSON_BYTES).map_err(|error| {
                caused(
                    ProjectCopyErrorKind::SourceInvalid,
                    descriptor.path(),
                    "parse source resource",
                    error,
                )
            })?;
        let object = resource.as_object_mut().ok_or_else(|| {
            failure(
                ProjectCopyErrorKind::SourceInvalid,
                descriptor.path(),
                "source resource is not a JSON object",
            )
        })?;
        if object.get("project_id").and_then(Value::as_str) != Some(source_project_id) {
            return Err(failure(
                ProjectCopyErrorKind::SourceInvalid,
                descriptor.path(),
                "source resource does not belong to the source project",
            ));
        }
        object.insert("project_id".to_owned(), destination_project_id.into());
        let copied = serde_json_canonicalizer::to_vec(&resource).map_err(|error| {
            caused(
                ProjectCopyErrorKind::Serialization,
                descriptor.path(),
                "canonicalize copied project resource",
                error,
            )
        })?;
        let raw = raw_descriptor.as_object_mut().ok_or_else(|| {
            failure(
                ProjectCopyErrorKind::SourceInvalid,
                MANIFEST_NAME,
                "source manifest resource descriptor is not an object",
            )
        })?;
        raw.insert("byte_length".to_owned(), (copied.len() as u64).into());
        raw.insert(
            "sha256".to_owned(),
            hex_digest(Sha256::digest(&copied).into()).into(),
        );
        resources.push((descriptor.path().to_owned(), copied));
    }
    Ok(resources)
}

fn copy_stage_asset(
    source: &StructurallyVerifiedProject,
    stage: &Dir,
    path: &str,
    byte_length: u64,
    sha256: &str,
) -> Result<(), ProjectCopyError> {
    let parent = open_relative_parent(stage, path)?;
    let name = final_component(path)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut destination = parent.open_with(name, &options).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "create staged project asset",
            error,
        )
    })?;
    let source_file = open_declared_file(&source.root_dir, path).map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceInvalid,
            path,
            "open verified source asset",
            error,
        )
    })?;
    copy_open_stable(
        source_file,
        path,
        byte_length,
        decode_sha256(sha256, path).map_err(|error| {
            caused(
                ProjectCopyErrorKind::SourceInvalid,
                path,
                "decode source asset digest",
                error,
            )
        })?,
        &mut destination,
    )
    .map_err(|error| {
        caused(
            ProjectCopyErrorKind::SourceChanged,
            path,
            "copy verified source asset",
            error,
        )
    })?;
    destination.sync_all().map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "sync staged project asset",
            error,
        )
    })
}

fn write_stage_bytes(stage: &Dir, path: &str, bytes: &[u8]) -> Result<(), ProjectCopyError> {
    let parent = open_relative_parent(stage, path)?;
    let name = final_component(path)?;
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = parent.open_with(name, &options).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "create staged project file",
            error,
        )
    })?;
    file.write_all(bytes).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "write staged project file",
            error,
        )
    })?;
    file.sync_all().map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "sync staged project file",
            error,
        )
    })
}

fn open_relative_parent(root: &Dir, path: &str) -> Result<Dir, ProjectCopyError> {
    let mut current = root.try_clone().map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            path,
            "retain project directory",
            error,
        )
    })?;
    let parts: Vec<&str> = path.split('/').collect();
    for component in &parts[..parts.len().saturating_sub(1)] {
        match current.create_dir(component) {
            Ok(()) => sync_directory(&current).map_err(|error| {
                caused(
                    ProjectCopyErrorKind::Io,
                    path,
                    "sync created project directory",
                    error,
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(caused(
                    ProjectCopyErrorKind::Io,
                    path,
                    "create project payload directory",
                    error,
                ));
            }
        }
        current = current.open_dir_nofollow(component).map_err(|error| {
            caused(
                ProjectCopyErrorKind::Io,
                path,
                "open project payload directory",
                error,
            )
        })?;
    }
    Ok(current)
}

fn final_component(path: &str) -> Result<&str, ProjectCopyError> {
    path.rsplit('/')
        .next()
        .filter(|component| !component.is_empty())
        .ok_or_else(|| {
            failure(
                ProjectCopyErrorKind::InvalidRequest,
                path,
                "project payload path is invalid",
            )
        })
}

fn ensure_target_absent(parent: &Dir, name: &str) -> Result<(), ProjectCopyError> {
    match parent.symlink_metadata(name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(failure(
            ProjectCopyErrorKind::TargetConflict,
            name,
            "project-copy destination already exists",
        )),
        Err(error) => Err(caused(
            ProjectCopyErrorKind::Io,
            name,
            "inspect project-copy destination",
            error,
        )),
    }
}

fn open_or_create_directory(parent: &Dir, name: &str) -> Result<Dir, ProjectCopyError> {
    match parent.create_dir(name) {
        Ok(()) => sync_directory(parent).map_err(|error| {
            caused(
                ProjectCopyErrorKind::Io,
                name,
                "sync created project directory",
                error,
            )
        })?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(caused(
                ProjectCopyErrorKind::Io,
                name,
                "create project directory",
                error,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|error| {
        caused(
            ProjectCopyErrorKind::Io,
            name,
            "open project directory without links",
            error,
        )
    })
}

fn validate_request(request: ProjectCopyRequest<'_>) -> Result<(), ProjectCopyError> {
    if !is_valid_project_name(request.project_name)
        || request.project_name.trim() != request.project_name
        || request.project_name.chars().any(char::is_control)
    {
        return Err(failure(
            ProjectCopyErrorKind::InvalidRequest,
            "project_name",
            "project name must contain 1 to 120 characters without outer whitespace",
        ));
    }
    let name = request.directory_name;
    let name_chars = name.chars().count();
    let forbidden = name.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
            )
    });
    let reserved_stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(reserved_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || reserved_stem
            .strip_prefix("COM")
            .is_some_and(reserved_port_suffix)
        || reserved_stem
            .strip_prefix("LPT")
            .is_some_and(reserved_port_suffix);
    if !(1..=120).contains(&name_chars)
        || name.len() > 240
        || name == "."
        || name == ".."
        || name.starts_with('.')
        || name.ends_with([' ', '.'])
        || name.trim() != name
        || forbidden
        || reserved
    {
        return Err(failure(
            ProjectCopyErrorKind::InvalidRequest,
            "directory_name",
            "project directory name is not portable across supported desktop filesystems",
        ));
    }
    Ok(())
}

fn reserved_port_suffix(suffix: &str) -> bool {
    matches!(
        suffix,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

fn lock_error(error: ProjectLockError) -> ProjectCopyError {
    let kind = match error {
        ProjectLockError::Active => ProjectCopyErrorKind::SourceChanged,
        ProjectLockError::Io { .. } => ProjectCopyErrorKind::Io,
    };
    caused(kind, "backups", "acquire source project copy lock", error)
}

fn sync_directory(directory: &Dir) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory.open_with(".", &options)?.sync_all()
}

fn hex_digest(digest: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn failure(
    kind: ProjectCopyErrorKind,
    path: impl Into<String>,
    message: impl Into<String>,
) -> ProjectCopyError {
    ProjectCopyError {
        kind,
        path: path.into(),
        message: message.into(),
        published: false,
        source: None,
    }
}

fn caused(
    kind: ProjectCopyErrorKind,
    path: impl Into<String>,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> ProjectCopyError {
    ProjectCopyError {
        kind,
        path: path.into(),
        message: message.into(),
        published: false,
        source: Some(Box::new(source)),
    }
}

fn mark_published(mut error: ProjectCopyError) -> ProjectCopyError {
    error.published = true;
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use crate::{ProjectCreationRequest, create_project};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TemporaryParent(PathBuf);

    impl TemporaryParent {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-project-copy-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn source(&self) -> PathBuf {
            self.0.join("Source")
        }
    }

    impl Drop for TemporaryParent {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn create_source(parent: &TemporaryParent) {
        create_project(
            &parent.0,
            ProjectCreationRequest {
                directory_name: "Source",
                project_name: "Source",
            },
        )
        .unwrap();
    }

    fn canonical(value: &Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(value).unwrap()
    }

    fn append_sample_asset(source: &Path) -> Vec<u8> {
        let asset = b"RIFFverified-copy-asset".to_vec();
        fs::create_dir_all(source.join("assets/samples")).unwrap();
        fs::write(source.join("assets/samples/sample.wav"), &asset).unwrap();
        let manifest_path = source.join(MANIFEST_NAME);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest["assets"].as_array_mut().unwrap().push(json!({
            "id": "018f47a2-6b2d-7c11-8a44-00112233a001",
            "role": "sample",
            "path": "assets/samples/sample.wav",
            "media_type": "audio/wav",
            "byte_length": asset.len(),
            "sha256": hex_digest(Sha256::digest(&asset).into()),
            "extensions": {}
        }));
        fs::write(&manifest_path, canonical(&manifest)).unwrap();
        asset
    }

    #[test]
    fn copies_a_verified_project_as_an_independent_package_with_exact_assets() {
        let parent = TemporaryParent::new();
        create_source(&parent);
        let source = parent.source();
        let source_before = fs::read(source.join(MANIFEST_NAME)).unwrap();
        let asset = append_sample_asset(&source);
        let source_manifest = fs::read(source.join(MANIFEST_NAME)).unwrap();
        let copied = copy_project(ProjectCopyRequest {
            source: &source,
            destination_parent: &parent.0,
            directory_name: "Independent",
            project_name: "Independent",
        })
        .unwrap();
        let destination = parent.0.join("Independent");
        let source_verified = verify_project_package(&source).unwrap();
        let destination_verified = verify_project_package(&destination).unwrap();
        assert_ne!(
            source_verified.manifest().project_id(),
            destination_verified.manifest().project_id()
        );
        assert_eq!(destination_verified.manifest().name(), "Independent");
        assert_eq!(destination_verified.manifest().revision(), 1);
        assert_eq!(
            copied.source_project_id(),
            source_verified.manifest().project_id()
        );
        assert_eq!(
            fs::read(source.join(MANIFEST_NAME)).unwrap(),
            source_manifest
        );
        assert_ne!(source_before, source_manifest);
        assert_eq!(
            fs::read(destination.join("assets/samples/sample.wav")).unwrap(),
            asset
        );
        for (source_resource, destination_resource) in source_verified
            .manifest()
            .resources()
            .iter()
            .zip(destination_verified.manifest().resources())
        {
            assert_eq!(source_resource.id(), destination_resource.id());
            assert_eq!(source_resource.path(), destination_resource.path());
            let source_document: Value = serde_json::from_slice(
                &source_verified.read_resource(source_resource.id()).unwrap(),
            )
            .unwrap();
            let destination_document: Value = serde_json::from_slice(
                &destination_verified
                    .read_resource(destination_resource.id())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                source_document["project_id"],
                source_verified.manifest().project_id()
            );
            assert_eq!(
                destination_document["project_id"],
                destination_verified.manifest().project_id()
            );
        }
        assert!(fs::read_dir(&parent.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
        }));
    }

    #[test]
    fn refuses_a_collision_without_changing_the_existing_destination() {
        let parent = TemporaryParent::new();
        create_source(&parent);
        let destination = parent.0.join("Independent");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("operator-note"), b"do not replace").unwrap();
        let error = copy_project(ProjectCopyRequest {
            source: &parent.source(),
            destination_parent: &parent.0,
            directory_name: "Independent",
            project_name: "Independent",
        })
        .unwrap_err();
        assert_eq!(error.kind(), ProjectCopyErrorKind::TargetConflict);
        assert_eq!(
            fs::read(destination.join("operator-note")).unwrap(),
            b"do not replace"
        );
    }

    #[test]
    fn refuses_an_active_source_storage_transaction_without_creating_a_destination() {
        let parent = TemporaryParent::new();
        create_source(&parent);
        let source = parent.source();
        let verified = verify_project_package(&source).unwrap();
        let backups = open_or_create_directory(&verified.root_dir, "backups").unwrap();
        let _lock = ProjectLock::acquire(&backups).unwrap();
        let error = copy_project(ProjectCopyRequest {
            source: &source,
            destination_parent: &parent.0,
            directory_name: "Independent",
            project_name: "Independent",
        })
        .unwrap_err();
        assert_eq!(error.kind(), ProjectCopyErrorKind::SourceChanged);
        assert!(!parent.0.join("Independent").exists());
    }

    #[test]
    fn rejects_a_foreign_resource_before_exposing_a_destination() {
        let parent = TemporaryParent::new();
        create_source(&parent);
        let source = parent.source();
        let manifest_path = source.join(MANIFEST_NAME);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let resource_path = manifest["resources"][0]["path"]
            .as_str()
            .unwrap()
            .to_owned();
        let document_path = source.join(&resource_path);
        let mut document: Value =
            serde_json::from_slice(&fs::read(&document_path).unwrap()).unwrap();
        document["project_id"] = "018f47a2-6b2d-7c11-8a44-00112233a099".into();
        let document = canonical(&document);
        fs::write(&document_path, &document).unwrap();
        manifest["resources"][0]["byte_length"] = (document.len() as u64).into();
        manifest["resources"][0]["sha256"] = hex_digest(Sha256::digest(&document).into()).into();
        fs::write(&manifest_path, canonical(&manifest)).unwrap();
        verify_project_package(&source).unwrap();
        let error = copy_project(ProjectCopyRequest {
            source: &source,
            destination_parent: &parent.0,
            directory_name: "Independent",
            project_name: "Independent",
        })
        .unwrap_err();
        assert_eq!(error.kind(), ProjectCopyErrorKind::SourceInvalid);
        assert!(!parent.0.join("Independent").exists());
        assert!(fs::read_dir(&parent.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(STAGING_PREFIX)
        }));
    }
}
