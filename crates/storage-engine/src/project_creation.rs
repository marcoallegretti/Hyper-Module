use core::error::Error;
use core::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{ProjectLock, ProjectLockError};
use super::{StructurallyVerifiedProject, open_stable_project_root, same_directory};

const CREATION_RECORD: &str = "project-creation.json";
const MAX_CREATION_RECORD_BYTES: usize = 16 * 1024;
const FORMAT_VERSION: u8 = 1;

#[derive(Clone, Copy, Debug)]
pub struct ProjectCreationRequest<'a> {
    pub directory_name: &'a str,
    pub project_name: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectCreationErrorKind {
    InvalidRequest,
    TargetConflict,
    TransactionActive,
    Integrity,
    Io,
    Serialization,
    Verification,
}

#[derive(Debug)]
pub struct ProjectCreationError {
    kind: ProjectCreationErrorKind,
    path: String,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectCreationError {
    #[must_use]
    pub const fn kind(&self) -> ProjectCreationErrorKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for ProjectCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path == "." {
            formatter.write_str(&self.message)
        } else {
            write!(formatter, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for ProjectCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug)]
pub struct ProjectCreationOutcome {
    project: StructurallyVerifiedProject,
    resumed: bool,
}

impl ProjectCreationOutcome {
    #[must_use]
    pub const fn project(&self) -> &StructurallyVerifiedProject {
        &self.project
    }

    #[must_use]
    pub const fn resumed(&self) -> bool {
        self.resumed
    }

    #[must_use]
    pub fn into_project(self) -> StructurallyVerifiedProject {
        self.project
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectCreationRecord {
    format: String,
    format_version: u8,
    project_name: String,
    project_id: String,
    timeline_id: String,
    track_id: String,
    mixer_id: String,
}

struct ProjectCreationPlan {
    timeline: Vec<u8>,
    track: Vec<u8>,
    mixer: Vec<u8>,
    manifest: Vec<u8>,
}

/// Creates or resumes one manifest-last version-1 project transaction.
///
/// The target is one portable child directory below `parent`. A fresh project
/// contains a 120 BPM 4/4 timeline, one recordable audio track, and its
/// coherent mixer channel. Existing complete projects and unrelated partial
/// directories are never overwritten.
///
/// # Errors
///
/// Returns a classified validation, collision, transaction, filesystem,
/// serialization, integrity, or final verification failure.
pub fn create_project(
    parent: impl AsRef<Path>,
    request: ProjectCreationRequest<'_>,
) -> Result<ProjectCreationOutcome, ProjectCreationError> {
    validate_request(request)?;
    let (canonical_target, target_dir) =
        open_creation_target(parent.as_ref(), request.directory_name)?;
    let backups = open_or_create_directory(&target_dir, "backups")?;
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;

    let existing_record = read_optional_bounded(&backups, CREATION_RECORD)?;
    let (record, resumed) = if let Some(bytes) = existing_record {
        let record = parse_creation_record(&bytes)?;
        if record.project_name != request.project_name {
            return Err(failure(
                ProjectCreationErrorKind::TargetConflict,
                CREATION_RECORD,
                "incomplete project belongs to another requested name",
            ));
        }
        (record, true)
    } else {
        if !is_adoptable_empty_target(&target_dir, &backups)? {
            return Err(failure(
                ProjectCreationErrorKind::TargetConflict,
                request.directory_name,
                "project target already contains unrelated data",
            ));
        }
        let record = new_creation_record(request.project_name);
        let bytes = canonical(&record, CREATION_RECORD)?;
        write_exclusive_synced(&backups, CREATION_RECORD, &bytes)?;
        sync_dir(&backups, "backups")?;
        (record, false)
    };

    let plan = build_plan(&record)?;
    validate_resumable_tree(&target_dir, &backups, &record, &plan)?;
    publish_plan(&target_dir, &record, &plan)?;
    sync_dir(&target_dir, ".")?;

    let verified = super::verify_project_package(&canonical_target).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Verification,
            request.directory_name,
            "verify newly created project",
            source,
        )
    })?;
    if !same_directory(&target_dir, &verified.root_dir).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Integrity,
            request.directory_name,
            "compare created project directory identity",
            source,
        )
    })? {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            request.directory_name,
            "project directory changed during creation",
        ));
    }
    remove_creation_record(&backups)?;
    verified.confirm_unchanged().map_err(|source| {
        caused(
            ProjectCreationErrorKind::Verification,
            request.directory_name,
            "confirm created project after transaction cleanup",
            source,
        )
    })?;
    Ok(ProjectCreationOutcome {
        project: verified,
        resumed,
    })
}

fn open_creation_target(
    parent: &Path,
    directory_name: &str,
) -> Result<(std::path::PathBuf, Dir), ProjectCreationError> {
    let (parent_path, parent_dir) = open_stable_project_root(parent).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            ".",
            "open project parent directory",
            source,
        )
    })?;
    let target_path = parent_path.join(directory_name);
    match parent_dir.create_dir(directory_name) {
        Ok(()) => sync_dir(&parent_dir, ".")?,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(caused(
                ProjectCreationErrorKind::Io,
                directory_name,
                "create project directory",
                source,
            ));
        }
    }
    let retained_target = parent_dir
        .open_dir_nofollow(directory_name)
        .map_err(|source| {
            caused(
                ProjectCreationErrorKind::TargetConflict,
                directory_name,
                "open project target through its retained parent",
                source,
            )
        })?;
    let (canonical_target, path_target) =
        open_stable_project_root(&target_path).map_err(|source| {
            caused(
                ProjectCreationErrorKind::TargetConflict,
                directory_name,
                "project target is not a stable real directory",
                source,
            )
        })?;
    if !same_directory(&retained_target, &path_target).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Integrity,
            directory_name,
            "compare retained and path project directory identity",
            source,
        )
    })? {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            directory_name,
            "project parent changed while the target was being opened",
        ));
    }
    preflight_target(&retained_target)?;
    Ok((canonical_target, retained_target))
}

fn validate_request(request: ProjectCreationRequest<'_>) -> Result<(), ProjectCreationError> {
    let project_chars = request.project_name.chars().count();
    if !(1..=120).contains(&project_chars)
        || request.project_name.trim() != request.project_name
        || request.project_name.chars().any(char::is_control)
    {
        return Err(failure(
            ProjectCreationErrorKind::InvalidRequest,
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
            ProjectCreationErrorKind::InvalidRequest,
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

fn new_creation_record(project_name: &str) -> ProjectCreationRecord {
    ProjectCreationRecord {
        format: "hyper-project-creation".to_owned(),
        format_version: FORMAT_VERSION,
        project_name: project_name.to_owned(),
        project_id: Uuid::new_v4().to_string(),
        timeline_id: Uuid::new_v4().to_string(),
        track_id: Uuid::new_v4().to_string(),
        mixer_id: Uuid::new_v4().to_string(),
    }
}

fn parse_creation_record(bytes: &[u8]) -> Result<ProjectCreationRecord, ProjectCreationError> {
    let record: ProjectCreationRecord = serde_json::from_slice(bytes).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Integrity,
            CREATION_RECORD,
            "parse project creation record",
            source,
        )
    })?;
    if canonical(&record, CREATION_RECORD)? != bytes {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            CREATION_RECORD,
            "project creation record is not canonical",
        ));
    }
    if record.format != "hyper-project-creation"
        || record.format_version != FORMAT_VERSION
        || record.project_name.is_empty()
        || record.project_name.chars().count() > 120
        || [
            &record.project_id,
            &record.timeline_id,
            &record.track_id,
            &record.mixer_id,
        ]
        .into_iter()
        .any(|id| match Uuid::parse_str(id) {
            Ok(parsed) => parsed.to_string() != id.as_str(),
            Err(_) => true,
        })
    {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            CREATION_RECORD,
            "project creation record is invalid",
        ));
    }
    Ok(record)
}

fn build_plan(record: &ProjectCreationRecord) -> Result<ProjectCreationPlan, ProjectCreationError> {
    let timeline = canonical_value(
        &json!({
            "format": "hyper-timeline",
            "format_version": 1,
            "id": record.timeline_id,
            "project_id": record.project_id,
            "ticks_per_quarter": 1920,
            "tempo_events": [{"tick": 0, "microseconds_per_quarter": 500_000}],
            "meter_events": [{"tick": 0, "numerator": 4, "denominator": 4}],
            "extensions": {}
        }),
        "timeline/timeline.json",
    )?;
    let track = canonical_value(
        &json!({
            "format": "hyper-track",
            "format_version": 1,
            "id": record.track_id,
            "project_id": record.project_id,
            "name": "Audio 1",
            "kind": "audio",
            "order": 0,
            "active": true,
            "clip_ids": [],
            "extensions": {}
        }),
        "tracks",
    )?;
    let mixer = canonical_value(
        &json!({
            "format": "hyper-mixer",
            "format_version": 1,
            "id": record.mixer_id,
            "project_id": record.project_id,
            "channels": [{
                "track_id": record.track_id,
                "gain_millionths": 1_000_000,
                "pan_millionths": 0,
                "mute": false,
                "solo": false,
                "destination": {"kind": "main"},
                "sends": []
            }],
            "return_buses": [],
            "main": {"gain_millionths": 1_000_000, "mute": false},
            "cue": {"source": "main", "gain_millionths": 1_000_000, "mute": false},
            "extensions": {}
        }),
        "mixer/main.json",
    )?;
    let track_path = format!("tracks/{}.json", record.track_id);
    let manifest = canonical_value(
        &json!({
            "format": "hyper-project",
            "format_version": 1,
            "project_id": record.project_id,
            "revision": 1,
            "name": record.project_name,
            "sample_rate_hz": 48000,
            "required_capabilities": {
                "format": "hyper-hardware-capabilities",
                "format_version": 1,
                "audio_inputs": 2,
                "audio_outputs": 2,
                "cv_inputs": 0,
                "cv_outputs": 0,
                "gate_outputs": 0,
                "has_clock_input": false,
                "has_reset_input": false,
                "has_clock_output": false,
                "usb_mode": "none",
                "has_ethernet": false,
                "extensions": {}
            },
            "resources": [
                descriptor(&record.timeline_id, "timeline", "timeline/timeline.json", &timeline),
                descriptor(&record.track_id, "track", &track_path, &track),
                descriptor(&record.mixer_id, "mixer", "mixer/main.json", &mixer)
            ],
            "assets": [],
            "extensions": {}
        }),
        "project.json",
    )?;
    Ok(ProjectCreationPlan {
        timeline,
        track,
        mixer,
        manifest,
    })
}

fn descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> Value {
    json!({
        "id": id,
        "role": role,
        "path": path,
        "media_type": "application/json",
        "byte_length": bytes.len(),
        "sha256": hex_digest(Sha256::digest(bytes).into()),
        "extensions": {}
    })
}

fn publish_plan(
    root: &Dir,
    record: &ProjectCreationRecord,
    plan: &ProjectCreationPlan,
) -> Result<(), ProjectCreationError> {
    let timeline = open_or_create_directory(root, "timeline")?;
    write_idempotent_synced(&timeline, "timeline.json", &plan.timeline)?;
    sync_dir(&timeline, "timeline")?;

    let tracks = open_or_create_directory(root, "tracks")?;
    let track_name = format!("{}.json", record.track_id);
    write_idempotent_synced(&tracks, &track_name, &plan.track)?;
    sync_dir(&tracks, "tracks")?;

    let mixer = open_or_create_directory(root, "mixer")?;
    write_idempotent_synced(&mixer, "main.json", &plan.mixer)?;
    sync_dir(&mixer, "mixer")?;

    write_idempotent_synced(root, "project.json", &plan.manifest)
}

fn validate_resumable_tree(
    root: &Dir,
    backups: &Dir,
    record: &ProjectCreationRecord,
    plan: &ProjectCreationPlan,
) -> Result<(), ProjectCreationError> {
    validate_entries(
        root,
        &["backups", "timeline", "tracks", "mixer", "project.json"],
    )?;
    validate_entries(backups, &[ProjectLock::file_name(), CREATION_RECORD])?;
    validate_optional_directory(root, "timeline", &["timeline.json"])?;
    validate_optional_directory(root, "tracks", &[&format!("{}.json", record.track_id)])?;
    validate_optional_directory(root, "mixer", &["main.json"])?;
    validate_optional_bytes(root, "project.json", &plan.manifest)
}

fn preflight_target(root: &Dir) -> Result<(), ProjectCreationError> {
    let root_entries = entries(root)?;
    if root_entries.is_empty() {
        return Ok(());
    }
    validate_entries(
        root,
        &["backups", "timeline", "tracks", "mixer", "project.json"],
    )?;
    if !root_entries.iter().any(|name| name == "backups") {
        return Err(failure(
            ProjectCreationErrorKind::TargetConflict,
            ".",
            "existing project-like content has no durable creation record",
        ));
    }
    let backups = root.open_dir_nofollow("backups").map_err(|source| {
        caused(
            ProjectCreationErrorKind::TargetConflict,
            "backups",
            "existing project backup entry is not a real directory",
            source,
        )
    })?;
    validate_entries(&backups, &[ProjectLock::file_name(), CREATION_RECORD])?;
    let has_project_content = root_entries.iter().any(|name| name != "backups");
    if has_project_content && read_optional_bounded(&backups, CREATION_RECORD)?.is_none() {
        return Err(failure(
            ProjectCreationErrorKind::TargetConflict,
            CREATION_RECORD,
            "existing project-like content has no durable creation record",
        ));
    }
    Ok(())
}

fn is_adoptable_empty_target(root: &Dir, backups: &Dir) -> Result<bool, ProjectCreationError> {
    Ok(entries(root)?.iter().all(|name| name == "backups")
        && entries(backups)?
            .iter()
            .all(|name| name == ProjectLock::file_name()))
}

fn validate_optional_directory(
    root: &Dir,
    name: &str,
    allowed: &[&str],
) -> Result<(), ProjectCreationError> {
    match root.open_dir_nofollow(name) {
        Ok(directory) => validate_entries(&directory, allowed),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(caused(
            ProjectCreationErrorKind::Integrity,
            name,
            "partial project entry is not a real directory",
            source,
        )),
    }
}

fn validate_entries(directory: &Dir, allowed: &[&str]) -> Result<(), ProjectCreationError> {
    for name in entries(directory)? {
        if !allowed.iter().any(|allowed| *allowed == name) {
            return Err(failure(
                ProjectCreationErrorKind::TargetConflict,
                &name,
                "incomplete project contains an unrelated entry",
            ));
        }
    }
    Ok(())
}

fn entries(directory: &Dir) -> Result<Vec<String>, ProjectCreationError> {
    let mut names = Vec::new();
    for entry in directory.entries().map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            ".",
            "read project directory",
            source,
        )
    })? {
        let entry = entry.map_err(|source| {
            caused(
                ProjectCreationErrorKind::Io,
                ".",
                "read project directory entry",
                source,
            )
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            failure(
                ProjectCreationErrorKind::TargetConflict,
                ".",
                "project directory contains a non-Unicode entry",
            )
        })?;
        names.push(name);
    }
    Ok(names)
}

fn validate_optional_bytes(
    directory: &Dir,
    name: &str,
    expected: &[u8],
) -> Result<(), ProjectCreationError> {
    if let Some(bytes) = read_optional_bounded(directory, name)?
        && bytes != expected
    {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            name,
            "partial project file does not match its durable creation record",
        ));
    }
    Ok(())
}

fn open_or_create_directory(parent: &Dir, name: &str) -> Result<Dir, ProjectCreationError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent, ".")?,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(caused(
                ProjectCreationErrorKind::Io,
                name,
                "create project subdirectory",
                source,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Integrity,
            name,
            "open project subdirectory without following links",
            source,
        )
    })
}

fn write_exclusive_synced(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), ProjectCreationError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory.open_with(name, &options).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            name,
            "create project file",
            source,
        )
    })?;
    file.write_all(bytes).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            name,
            "write project file",
            source,
        )
    })?;
    file.sync_all().map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            name,
            "sync project file",
            source,
        )
    })
}

fn write_idempotent_synced(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), ProjectCreationError> {
    match write_exclusive_synced(directory, name, bytes) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .source()
                .and_then(|source| source.downcast_ref::<io::Error>())
                .is_some_and(|source| source.kind() == io::ErrorKind::AlreadyExists) =>
        {
            validate_optional_bytes(directory, name, bytes)
        }
        Err(error) => Err(error),
    }
}

fn read_optional_bounded(
    directory: &Dir,
    name: &str,
) -> Result<Option<Vec<u8>>, ProjectCreationError> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(caused(
                ProjectCreationErrorKind::Io,
                name,
                "inspect project file",
                source,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            name,
            "project entry is not a regular non-link file",
        ));
    }
    if metadata.len() > MAX_CREATION_RECORD_BYTES as u64 {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            name,
            "project creation file exceeds its size limit",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            name,
            "open project file",
            source,
        )
    })?;
    let mut bytes = Vec::new();
    file.take((MAX_CREATION_RECORD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| {
            caused(
                ProjectCreationErrorKind::Io,
                name,
                "read project file",
                source,
            )
        })?;
    if bytes.len() > MAX_CREATION_RECORD_BYTES {
        return Err(failure(
            ProjectCreationErrorKind::Integrity,
            name,
            "project creation file exceeds its size limit",
        ));
    }
    Ok(Some(bytes))
}

fn remove_creation_record(backups: &Dir) -> Result<(), ProjectCreationError> {
    backups.remove_file(CREATION_RECORD).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Io,
            CREATION_RECORD,
            "remove completed project creation record",
            source,
        )
    })?;
    sync_dir(backups, "backups")
}

fn sync_dir(directory: &Dir, path: &str) -> Result<(), ProjectCreationError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|source| {
            caused(
                ProjectCreationErrorKind::Io,
                path,
                "open directory for synchronization",
                source,
            )
        })?
        .sync_all()
        .map_err(|source| caused(ProjectCreationErrorKind::Io, path, "sync directory", source))
}

fn canonical<T: Serialize>(value: &T, path: &str) -> Result<Vec<u8>, ProjectCreationError> {
    serde_json_canonicalizer::to_vec(value).map_err(|source| {
        caused(
            ProjectCreationErrorKind::Serialization,
            path,
            "serialize canonical project creation data",
            source,
        )
    })
}

fn canonical_value(value: &Value, path: &str) -> Result<Vec<u8>, ProjectCreationError> {
    canonical(value, path)
}

fn hex_digest(digest: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest {
        use fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

fn lock_error(error: ProjectLockError) -> ProjectCreationError {
    let kind = match error {
        ProjectLockError::Active => ProjectCreationErrorKind::TransactionActive,
        ProjectLockError::Io { .. } => ProjectCreationErrorKind::Io,
    };
    caused(kind, "backups", "acquire project creation lock", error)
}

fn failure(
    kind: ProjectCreationErrorKind,
    path: impl Into<String>,
    message: impl Into<String>,
) -> ProjectCreationError {
    ProjectCreationError {
        kind,
        path: path.into(),
        message: message.into(),
        source: None,
    }
}

fn caused(
    kind: ProjectCreationErrorKind,
    path: impl Into<String>,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> ProjectCreationError {
    ProjectCreationError {
        kind,
        path: path.into(),
        message: message.into(),
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    struct TempParent(PathBuf);

    impl TempParent {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-project-creation-{}-{}",
                std::process::id(),
                NEXT_TEST.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn request<'a>(
            directory_name: &'a str,
            project_name: &'a str,
        ) -> ProjectCreationRequest<'a> {
            ProjectCreationRequest {
                directory_name,
                project_name,
            }
        }
    }

    impl Drop for TempParent {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn creates_one_verified_recordable_default_project_manifest_last() {
        let parent = TempParent::new();
        let outcome =
            create_project(&parent.0, TempParent::request("Night Drive", "Night Drive")).unwrap();
        assert!(!outcome.resumed());
        assert_eq!(outcome.project().manifest().name(), "Night Drive");
        assert_eq!(outcome.project().manifest().revision(), 1);
        assert_eq!(outcome.project().manifest().sample_rate_hz(), 48_000);
        assert_eq!(outcome.project().manifest().resources().len(), 3);
        assert_eq!(
            outcome
                .project()
                .manifest()
                .required_capabilities()
                .audio_inputs(),
            2
        );
        assert_eq!(
            outcome
                .project()
                .manifest()
                .required_capabilities()
                .audio_outputs(),
            2
        );
        assert!(
            !outcome
                .project()
                .manifest()
                .required_capabilities()
                .has_ethernet()
        );
        assert!(
            !outcome
                .project()
                .root()
                .join("backups")
                .join(CREATION_RECORD)
                .exists()
        );
        outcome.project().confirm_unchanged().unwrap();
    }

    #[test]
    fn rejects_nonportable_names_before_creating_a_target() {
        let parent = TempParent::new();
        let oversized_utf8 = "🎛".repeat(61);
        for (directory_name, project_name) in [
            ("", "Project"),
            (".hidden", "Project"),
            ("CON", "Project"),
            ("COM¹", "Project"),
            ("LPT².mix", "Project"),
            (oversized_utf8.as_str(), "Project"),
            ("project.", "Project"),
            ("project/name", "Project"),
            ("Project", " Project"),
            ("Project", "Project\nName"),
        ] {
            let error =
                create_project(&parent.0, TempParent::request(directory_name, project_name))
                    .unwrap_err();
            assert_eq!(error.kind(), ProjectCreationErrorKind::InvalidRequest);
        }
        assert_eq!(fs::read_dir(&parent.0).unwrap().count(), 0);
    }

    #[test]
    fn refuses_existing_projects_and_unrelated_partial_directories() {
        let parent = TempParent::new();
        let occupied = parent.0.join("Occupied");
        fs::create_dir(&occupied).unwrap();
        fs::write(occupied.join("foreign.txt"), b"preserve").unwrap();
        let error =
            create_project(&parent.0, TempParent::request("Occupied", "Occupied")).unwrap_err();
        assert_eq!(error.kind(), ProjectCreationErrorKind::TargetConflict);
        assert_eq!(fs::read(occupied.join("foreign.txt")).unwrap(), b"preserve");
        assert!(!occupied.join("backups").exists());

        create_project(&parent.0, TempParent::request("Complete", "Complete")).unwrap();
        let error =
            create_project(&parent.0, TempParent::request("Complete", "Complete")).unwrap_err();
        assert_eq!(error.kind(), ProjectCreationErrorKind::TargetConflict);
        super::super::verify_project_package(parent.0.join("Complete")).unwrap();
    }

    #[test]
    fn resumes_only_bytes_bound_to_the_durable_creation_record() {
        let parent = TempParent::new();
        let root = parent.0.join("Recovered");
        let backups = root.join("backups");
        fs::create_dir_all(root.join("timeline")).unwrap();
        fs::create_dir(&backups).unwrap();
        let record = new_creation_record("Recovered");
        let record_bytes = canonical(&record, CREATION_RECORD).unwrap();
        fs::write(backups.join(CREATION_RECORD), record_bytes).unwrap();
        let plan = build_plan(&record).unwrap();
        fs::write(root.join("timeline/timeline.json"), &plan.timeline).unwrap();

        let outcome =
            create_project(&parent.0, TempParent::request("Recovered", "Recovered")).unwrap();
        assert!(outcome.resumed());
        assert_eq!(outcome.project().manifest().project_id(), record.project_id);
        assert!(!backups.join(CREATION_RECORD).exists());
        outcome.project().confirm_unchanged().unwrap();
    }

    #[test]
    fn refuses_tampered_resumption_without_replacing_the_file() {
        let parent = TempParent::new();
        let root = parent.0.join("Tampered");
        let backups = root.join("backups");
        fs::create_dir_all(root.join("timeline")).unwrap();
        fs::create_dir(&backups).unwrap();
        let record = new_creation_record("Tampered");
        fs::write(
            backups.join(CREATION_RECORD),
            canonical(&record, CREATION_RECORD).unwrap(),
        )
        .unwrap();
        fs::write(root.join("timeline/timeline.json"), b"foreign").unwrap();

        let error =
            create_project(&parent.0, TempParent::request("Tampered", "Tampered")).unwrap_err();
        assert_eq!(error.kind(), ProjectCreationErrorKind::Integrity);
        assert_eq!(
            fs::read(root.join("timeline/timeline.json")).unwrap(),
            b"foreign"
        );
        assert!(backups.join(CREATION_RECORD).exists());
    }

    #[test]
    fn concurrent_creation_observes_the_shared_project_transaction_lock() {
        let parent = TempParent::new();
        let root = parent.0.join("Locked");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("backups")).unwrap();
        let backups =
            Dir::open_ambient_dir(root.join("backups"), cap_std::ambient_authority()).unwrap();
        let _lock = ProjectLock::acquire(&backups).unwrap();

        let error = create_project(&parent.0, TempParent::request("Locked", "Locked")).unwrap_err();
        assert_eq!(error.kind(), ProjectCreationErrorKind::TransactionActive);
    }
}
