#![doc = "Bounded, failure-atomic recent-project persistence for the Desktop IDE."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use directories::BaseDirs;
use hyper_fs_authority::{
    identity, open_syncable_directory_ambient, path_is_proven_absent, read_stable_file,
    rename_replace, sync_directory,
};
use hyper_project_model::is_valid_project_name;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const DOCUMENT_FORMAT: &str = "hyper-ide-project-history";
const DOCUMENT_VERSION: u8 = 1;
const HISTORY_FILE_NAME: &str = "recent-projects.json";
const LOCK_FILE_NAME: &str = "recent-projects.lock";
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_DOCUMENT_BYTES_U64: u64 = 64 * 1024;

pub const MAX_RECENT_PROJECTS: usize = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectHistoryErrorKind {
    ConfigDirectoryUnavailable,
    Io,
    Locked,
    DocumentLimit,
    InvalidJson,
    NonCanonicalJson,
    InvalidDocument,
    InvalidProject,
}

#[derive(Debug)]
pub struct ProjectHistoryError {
    kind: ProjectHistoryErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl ProjectHistoryError {
    #[must_use]
    pub const fn kind(&self) -> ProjectHistoryErrorKind {
        self.kind
    }
}

impl fmt::Display for ProjectHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ProjectHistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecentProject {
    name: String,
    path: PathBuf,
}

impl RecentProject {
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectHistory {
    document_path: PathBuf,
    lock_path: PathBuf,
}

impl ProjectHistory {
    /// Resolves the current platform's user configuration directory.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectHistoryErrorKind::ConfigDirectoryUnavailable`] when the
    /// platform cannot provide a user configuration directory.
    pub fn for_current_user() -> Result<Self, ProjectHistoryError> {
        let base = BaseDirs::new().ok_or_else(|| {
            rejection(
                ProjectHistoryErrorKind::ConfigDirectoryUnavailable,
                "user configuration directory is unavailable",
            )
        })?;
        Ok(Self::at(base.config_dir().join("hyper-module")))
    }

    #[must_use]
    pub fn at(config_directory: impl AsRef<Path>) -> Self {
        let config_directory = config_directory.as_ref();
        Self {
            document_path: config_directory.join(HISTORY_FILE_NAME),
            lock_path: config_directory.join(LOCK_FILE_NAME),
        }
    }

    #[must_use]
    pub fn document_path(&self) -> &Path {
        &self.document_path
    }

    /// Loads the last complete history document, or an empty list when no
    /// history has been written.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection for unreadable, oversized, linked,
    /// noncanonical, or structurally invalid history.
    pub fn load(&self) -> Result<Vec<RecentProject>, ProjectHistoryError> {
        let Some(directory) = self.open_existing_directory()? else {
            return Ok(Vec::new());
        };
        Self::load_from(&directory)
    }

    fn load_from(directory: &Dir) -> Result<Vec<RecentProject>, ProjectHistoryError> {
        let bytes = match read_stable_file(directory, HISTORY_FILE_NAME, MAX_DOCUMENT_BYTES_U64) {
            Ok(bytes) => bytes,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) if source.kind() == io::ErrorKind::InvalidInput => {
                return Err(error_with_source(
                    ProjectHistoryErrorKind::DocumentLimit,
                    "project history is linked, irregular, unstable, or exceeds its byte limit",
                    source,
                ));
            }
            Err(source) => return Err(io_error("read project history", source)),
        };
        decode_document(&bytes)
    }

    /// Moves a successfully opened project to the front of the bounded list.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when the project or history is invalid,
    /// another writer holds the history lock, or durable publication fails.
    pub fn remember(
        &self,
        name: &str,
        path: impl AsRef<Path>,
    ) -> Result<Vec<RecentProject>, ProjectHistoryError> {
        validate_name(name)?;
        let path = fs::canonicalize(path.as_ref()).map_err(|source| {
            error_with_source(
                ProjectHistoryErrorKind::InvalidProject,
                "canonicalize recent project",
                source,
            )
        })?;
        let metadata = fs::metadata(&path).map_err(|source| {
            error_with_source(
                ProjectHistoryErrorKind::InvalidProject,
                "inspect recent project",
                source,
            )
        })?;
        if !metadata.is_dir() || path.to_str().is_none() {
            return Err(rejection(
                ProjectHistoryErrorKind::InvalidProject,
                "recent project must be an absolute UTF-8 directory path",
            ));
        }
        self.with_exclusive_lock(|directory| {
            let mut projects = Self::load_from(directory)?;
            projects.retain(|project| project.path != path);
            projects.insert(
                0,
                RecentProject {
                    name: name.to_owned(),
                    path,
                },
            );
            projects.truncate(MAX_RECENT_PROJECTS);
            Self::publish(directory, &projects)?;
            Ok(projects)
        })
    }

    /// Removes one exact stored path from the bounded list.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when history cannot be locked, read, or
    /// durably replaced.
    pub fn forget(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<Vec<RecentProject>, ProjectHistoryError> {
        let path = path.as_ref();
        self.with_exclusive_lock(|directory| {
            let mut projects = Self::load_from(directory)?;
            let previous = projects.len();
            projects.retain(|project| project.path != path);
            if projects.len() != previous {
                Self::publish(directory, &projects)?;
            }
            Ok(projects)
        })
    }

    fn with_exclusive_lock<T>(
        &self,
        operation: impl FnOnce(&Dir) -> Result<T, ProjectHistoryError>,
    ) -> Result<T, ProjectHistoryError> {
        let parent = self.lock_path.parent().ok_or_else(|| {
            rejection(
                ProjectHistoryErrorKind::InvalidDocument,
                "project history lock path has no parent directory",
            )
        })?;
        fs::create_dir_all(parent)
            .map_err(|source| io_error("create project history directory", source))?;
        if path_is_proven_absent(parent).map_err(classification_error)? {
            return Err(rejection(
                ProjectHistoryErrorKind::InvalidDocument,
                "created project history directory is absent",
            ));
        }
        let directory = open_syncable_directory_ambient(parent)
            .map_err(|source| io_error("open project history directory", source))?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .follow(FollowSymlinks::No);
        let lock = directory
            .open_with(LOCK_FILE_NAME, &options)
            .map_err(|source| io_error("open project history lock", source))?;
        let lock = lock.into_std();
        lock.try_lock().map_err(|source| match source {
            std::fs::TryLockError::WouldBlock => rejection(
                ProjectHistoryErrorKind::Locked,
                "another project history writer is active",
            ),
            std::fs::TryLockError::Error(source) => io_error("lock project history", source),
        })?;
        operation(&directory)
    }

    fn open_existing_directory(&self) -> Result<Option<Dir>, ProjectHistoryError> {
        let parent = self.document_path.parent().ok_or_else(|| {
            rejection(
                ProjectHistoryErrorKind::InvalidDocument,
                "project history path has no parent directory",
            )
        })?;
        match path_is_proven_absent(parent) {
            Ok(true) => Ok(None),
            Ok(false) => open_syncable_directory_ambient(parent)
                .map(Some)
                .map_err(|source| io_error("open project history directory", source)),
            Err(source) => Err(classification_error(source)),
        }
    }

    fn publish(directory: &Dir, projects: &[RecentProject]) -> Result<(), ProjectHistoryError> {
        let document = StoredHistory {
            format: DOCUMENT_FORMAT,
            format_version: DOCUMENT_VERSION,
            projects: projects
                .iter()
                .map(|project| StoredProject {
                    name: project.name.as_str(),
                    path: project.path.to_str().unwrap_or_default(),
                })
                .collect(),
        };
        let bytes = serde_json_canonicalizer::to_vec(&document)
            .map_err(|source| serialization_error("canonicalize project history", source))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DOCUMENT_BYTES_U64 {
            return Err(rejection(
                ProjectHistoryErrorKind::DocumentLimit,
                "project history exceeds its byte limit",
            ));
        }
        let temporary_name = format!(".recent-projects-{}.tmp", Uuid::new_v4());
        if let Err(publication) = stage_and_publish(directory, &temporary_name, &bytes) {
            return match directory.remove_file(&temporary_name) {
                Ok(()) => Err(publication),
                Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => Err(publication),
                Err(cleanup) => Err(error_with_source(
                    ProjectHistoryErrorKind::Io,
                    &format!(
                        "clean up staged project history after publication failure: {publication}"
                    ),
                    cleanup,
                )),
            };
        }
        Ok(())
    }
}

fn stage_and_publish(
    directory: &Dir,
    temporary_name: &str,
    bytes: &[u8],
) -> Result<(), ProjectHistoryError> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let mut file = directory
        .open_with(temporary_name, &options)
        .map_err(|source| io_error("stage project history", source))?;
    file.write_all(bytes)
        .map_err(|source| io_error("write project history", source))?;
    file.sync_all()
        .map_err(|source| io_error("sync staged project history", source))?;
    let staged_identity = identity(
        &file
            .metadata()
            .map_err(|source| io_error("inspect staged project history", source))?,
    );
    drop(file);
    rename_replace(
        directory,
        temporary_name,
        staged_identity,
        directory,
        HISTORY_FILE_NAME,
    )
    .map_err(|source| io_error("publish project history", source))?;
    sync_directory(directory)
        .map_err(|source| io_error("synchronize project history directory", source))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHistory {
    format: String,
    format_version: u64,
    projects: Vec<RawProject>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct StoredHistory<'a> {
    format: &'static str,
    format_version: u8,
    projects: Vec<StoredProject<'a>>,
}

#[derive(Serialize)]
struct StoredProject<'a> {
    name: &'a str,
    path: &'a str,
}

fn decode_document(bytes: &[u8]) -> Result<Vec<RecentProject>, ProjectHistoryError> {
    let value = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES).map_err(|source| {
        error_with_source(
            ProjectHistoryErrorKind::InvalidJson,
            "project history is not strict JSON",
            source,
        )
    })?;
    let canonical = serde_json_canonicalizer::to_vec(&value)
        .map_err(|source| serialization_error("canonicalize loaded project history", source))?;
    if canonical != bytes {
        return Err(rejection(
            ProjectHistoryErrorKind::NonCanonicalJson,
            "project history JSON is not canonical",
        ));
    }
    let raw: RawHistory = serde_json::from_value(value).map_err(|source| {
        error_with_source(
            ProjectHistoryErrorKind::InvalidDocument,
            "project history shape is invalid",
            source,
        )
    })?;
    if raw.format != DOCUMENT_FORMAT || raw.format_version != u64::from(DOCUMENT_VERSION) {
        return Err(rejection(
            ProjectHistoryErrorKind::InvalidDocument,
            "project history format is unsupported",
        ));
    }
    if raw.projects.len() > MAX_RECENT_PROJECTS {
        return Err(rejection(
            ProjectHistoryErrorKind::DocumentLimit,
            "project history contains too many entries",
        ));
    }
    let mut projects = Vec::new();
    projects
        .try_reserve_exact(raw.projects.len())
        .map_err(|_| {
            rejection(
                ProjectHistoryErrorKind::DocumentLimit,
                "project history capacity is unavailable",
            )
        })?;
    for raw in raw.projects {
        validate_name(&raw.name)?;
        let path = validate_stored_path(&raw.path)?;
        let path_key = stored_path_key(&raw.path);
        if projects.iter().any(|project: &RecentProject| {
            stored_path_key(project.path.to_str().unwrap_or_default()) == path_key
        }) {
            return Err(rejection(
                ProjectHistoryErrorKind::InvalidDocument,
                "recent project paths must be unique",
            ));
        }
        projects.push(RecentProject {
            name: raw.name,
            path,
        });
    }
    Ok(projects)
}

fn validate_name(name: &str) -> Result<(), ProjectHistoryError> {
    if !is_valid_project_name(name) {
        return Err(rejection(
            ProjectHistoryErrorKind::InvalidProject,
            "recent project name is outside the supported domain",
        ));
    }
    Ok(())
}

fn validate_stored_path(raw: &str) -> Result<PathBuf, ProjectHistoryError> {
    let path = PathBuf::from(raw);
    let normalized: PathBuf = path.components().collect();
    let has_parent = path
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    #[cfg(windows)]
    let has_alternate_separator = raw.contains('/');
    #[cfg(not(windows))]
    let has_alternate_separator = false;
    if !path.is_absolute()
        || raw.contains('\0')
        || has_parent
        || has_alternate_separator
        || normalized != path
    {
        return Err(rejection(
            ProjectHistoryErrorKind::InvalidDocument,
            "recent project path is not in canonical absolute form",
        ));
    }
    Ok(path)
}

#[cfg(windows)]
fn stored_path_key(path: &str) -> String {
    path.to_lowercase()
}

#[cfg(not(windows))]
fn stored_path_key(path: &str) -> &str {
    path
}

fn classification_error(source: io::Error) -> ProjectHistoryError {
    if source.kind() == io::ErrorKind::InvalidInput {
        error_with_source(
            ProjectHistoryErrorKind::InvalidDocument,
            "project history directory path is linked or irregular",
            source,
        )
    } else {
        io_error("classify project history directory", source)
    }
}

fn rejection(kind: ProjectHistoryErrorKind, message: &str) -> ProjectHistoryError {
    ProjectHistoryError {
        kind,
        message: message.to_owned(),
        source: None,
    }
}

fn io_error(message: &str, source: io::Error) -> ProjectHistoryError {
    error_with_source(ProjectHistoryErrorKind::Io, message, source)
}

fn serialization_error(
    message: &str,
    source: impl Error + Send + Sync + 'static,
) -> ProjectHistoryError {
    error_with_source(ProjectHistoryErrorKind::InvalidDocument, message, source)
}

fn error_with_source(
    kind: ProjectHistoryErrorKind,
    message: &str,
    source: impl Error + Send + Sync + 'static,
) -> ProjectHistoryError {
    ProjectHistoryError {
        kind,
        message: message.to_owned(),
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use serde_json::json;

    use super::*;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        root: PathBuf,
        history: ProjectHistory,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "hyper-project-history-{}-{}",
                std::process::id(),
                NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self {
                history: ProjectHistory::at(root.join("config")),
                root,
            }
        }

        fn project(&self, index: usize) -> PathBuf {
            let path = self.root.join(format!("Project-{index}"));
            fs::create_dir(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn missing_history_is_empty_and_remembering_is_bounded_mru() {
        let fixture = Fixture::new();
        assert!(fixture.history.load().unwrap().is_empty());

        let mut paths = Vec::new();
        for index in 0..12 {
            let path = fixture.project(index);
            fixture
                .history
                .remember(&format!("Project {index}"), &path)
                .unwrap();
            paths.push(fs::canonicalize(path).unwrap());
        }
        let projects = fixture.history.load().unwrap();
        assert_eq!(projects.len(), MAX_RECENT_PROJECTS);
        assert_eq!(projects[0].path(), paths[11]);
        assert_eq!(projects[9].path(), paths[2]);

        let projects = fixture.history.remember("Renamed", &paths[5]).unwrap();
        assert_eq!(projects.len(), MAX_RECENT_PROJECTS);
        assert_eq!(projects[0].name(), "Renamed");
        assert_eq!(projects[0].path(), paths[5]);
        assert_eq!(
            projects
                .iter()
                .filter(|project| project.path() == paths[5])
                .count(),
            1
        );
    }

    #[test]
    fn locked_history_rejects_a_competing_writer_without_mutation() {
        let fixture = Fixture::new();
        let project = fixture.project(0);
        let lock_parent = fixture.history.lock_path.parent().unwrap();
        fs::create_dir_all(lock_parent).unwrap();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&fixture.history.lock_path)
            .unwrap();
        lock.try_lock().unwrap();

        let error = fixture.history.remember("Project", &project).unwrap_err();

        assert_eq!(error.kind(), ProjectHistoryErrorKind::Locked);
        assert!(!fixture.history.document_path().exists());

        drop(lock);
        fixture.history.remember("Project", &project).unwrap();
        let second = fixture.project(1);
        fixture.history.remember("Second", &second).unwrap();
        let projects = fixture.history.load().unwrap();
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].name(), "Second");
        assert_eq!(projects[1].name(), "Project");
    }

    #[test]
    fn rejects_noncanonical_relative_duplicate_and_excess_documents() {
        let fixture = Fixture::new();
        let config = fixture.history.document_path().parent().unwrap();
        fs::create_dir_all(config).unwrap();
        fs::write(
            fixture.history.document_path(),
            b"{\n  \"format\": \"hyper-ide-project-history\", \"format_version\": 1, \"projects\": []\n}",
        )
        .unwrap();
        assert_eq!(
            fixture.history.load().unwrap_err().kind(),
            ProjectHistoryErrorKind::NonCanonicalJson
        );

        for (document, expected) in [
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION,
                    "projects": [{"name": "Project", "path": "relative"}]
                }),
                ProjectHistoryErrorKind::InvalidDocument,
            ),
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION,
                    "projects": [
                        {"name": "One", "path": absolute_test_path("same")},
                        {"name": "Two", "path": absolute_test_path("same")}
                    ]
                }),
                ProjectHistoryErrorKind::InvalidDocument,
            ),
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION,
                    "projects": (0..=MAX_RECENT_PROJECTS)
                        .map(|index| json!({
                            "name": format!("Project {index}"),
                            "path": absolute_test_path(&index.to_string())
                        }))
                        .collect::<Vec<_>>()
                }),
                ProjectHistoryErrorKind::DocumentLimit,
            ),
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION + 1,
                    "projects": []
                }),
                ProjectHistoryErrorKind::InvalidDocument,
            ),
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION,
                    "projects": [],
                    "unknown": true
                }),
                ProjectHistoryErrorKind::InvalidDocument,
            ),
            (
                json!({
                    "format": DOCUMENT_FORMAT,
                    "format_version": DOCUMENT_VERSION,
                    "projects": [{
                        "name": "Alias",
                        "path": lexical_alias_test_path()
                    }]
                }),
                ProjectHistoryErrorKind::InvalidDocument,
            ),
        ] {
            fs::write(
                fixture.history.document_path(),
                serde_json_canonicalizer::to_vec(&document).unwrap(),
            )
            .unwrap();
            assert_eq!(fixture.history.load().unwrap_err().kind(), expected);
        }

        fs::write(
            fixture.history.document_path(),
            vec![b' '; MAX_DOCUMENT_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            fixture.history.load().unwrap_err().kind(),
            ProjectHistoryErrorKind::DocumentLimit
        );
    }

    #[test]
    fn config_parent_failures_are_not_reported_as_empty_history() {
        let fixture = Fixture::new();
        let config = fixture.root.join("not-a-directory");
        fs::write(&config, b"entry").unwrap();
        let history = ProjectHistory::at(&config);

        assert!(history.load().is_err());
    }

    #[test]
    fn history_accepts_every_version_one_manifest_name() {
        let fixture = Fixture::new();
        let project = fixture.project(0);

        let projects = fixture.history.remember(" Project\n", project).unwrap();

        assert_eq!(projects[0].name(), " Project\n");
        assert_eq!(fixture.history.load().unwrap()[0].name(), " Project\n");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_linked_history_document() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let config = fixture.history.document_path().parent().unwrap();
        fs::create_dir_all(config).unwrap();
        let target = fixture.root.join("target.json");
        fs::write(&target, b"{}").unwrap();
        symlink(target, fixture.history.document_path()).unwrap();

        assert_eq!(
            fixture.history.load().unwrap_err().kind(),
            ProjectHistoryErrorKind::DocumentLimit
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_linked_config_directory() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let target = fixture.root.join("target-config");
        let linked = fixture.root.join("linked-config");
        fs::create_dir(&target).unwrap();
        symlink(&target, &linked).unwrap();

        assert!(ProjectHistory::at(linked).load().is_err());
    }

    #[cfg(windows)]
    #[test]
    fn rejects_a_linked_config_directory() {
        let fixture = Fixture::new();
        let target = fixture.root.join("target-config");
        let linked = fixture.root.join("linked-config");
        fs::create_dir(&target).unwrap();
        junction::create(&target, &linked).unwrap();

        assert!(ProjectHistory::at(&linked).load().is_err());
        junction::delete(&linked).unwrap();
    }

    fn absolute_test_path(suffix: &str) -> String {
        std::env::temp_dir()
            .join(format!("history-{suffix}"))
            .to_string_lossy()
            .into_owned()
    }

    fn lexical_alias_test_path() -> String {
        std::env::temp_dir()
            .join("history-alias")
            .join("..")
            .join("history-target")
            .to_string_lossy()
            .into_owned()
    }
}
