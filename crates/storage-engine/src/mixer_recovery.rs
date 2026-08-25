use core::error::Error;
use core::fmt;
use std::io::{self, Write};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};
use hyper_fs_authority::{identity, rename_replace};
use hyper_project_model::ResourceRole;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::project_lock::{ProjectLock, ProjectLockError};
use super::stable_file::read_open_stable;
use super::{
    MAX_JSON_FILE_BYTES, StructurallyVerifiedProject, decode_sha256, verify_project_package,
};

const BACKUPS_DIRECTORY: &str = "backups";
const SNAPSHOT_DIRECTORY: &str = "mixer-recovery";
const RECORD_NAME: &str = "snapshot.json";
const LEGACY_MIXER_NAME: &str = "mixer.json";
const MIXER_SLOT_A: &str = "mixer-a.json";
const MIXER_SLOT_B: &str = "mixer-b.json";
const FORMAT: &str = "hyper-mixer-recovery-snapshot";
const LEGACY_FORMAT_VERSION: u8 = 1;
const FORMAT_VERSION: u8 = 2;
const MAX_SNAPSHOT_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug)]
pub struct MixerRecoverySnapshotRequest<'a> {
    pub expected_project_sha256: [u8; 32],
    pub mixer_id: &'a str,
    pub mixer_bytes: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MixerRecoverySnapshotErrorKind {
    ProjectChanged,
    SnapshotUnavailable,
    SnapshotInvalid,
    TransactionUnavailable,
    Io,
}

#[derive(Debug)]
pub struct MixerRecoverySnapshotError {
    kind: MixerRecoverySnapshotErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl MixerRecoverySnapshotError {
    #[must_use]
    pub const fn kind(&self) -> MixerRecoverySnapshotErrorKind {
        self.kind
    }
}

impl fmt::Display for MixerRecoverySnapshotError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for MixerRecoverySnapshotError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MixerRecoverySnapshot {
    project_id: String,
    manifest_sha256: [u8; 32],
    mixer_id: String,
    mixer_bytes: Vec<u8>,
}

impl MixerRecoverySnapshot {
    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub const fn manifest_sha256(&self) -> &[u8; 32] {
        &self.manifest_sha256
    }

    #[must_use]
    pub fn mixer_id(&self) -> &str {
        &self.mixer_id
    }

    #[must_use]
    pub fn mixer_bytes(&self) -> &[u8] {
        &self.mixer_bytes
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRecord {
    format: String,
    format_version: u8,
    project_id: String,
    manifest_sha256: String,
    mixer_id: String,
    mixer_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mixer_file: Option<String>,
}

/// Durably stores an exact, canonical live-mixer snapshot for one verified
/// project revision. The snapshot is local recovery state; it never changes
/// project resources or their manifest.
///
/// # Errors
///
/// Returns a classified error when the project changed, the mixer request does
/// not match its declared mixer resource, or the recovery state cannot be
/// atomically published.
pub fn store_mixer_recovery_snapshot(
    project_path: impl AsRef<Path>,
    request: MixerRecoverySnapshotRequest<'_>,
) -> Result<(), MixerRecoverySnapshotError> {
    validate_canonical_json(request.mixer_bytes)?;
    let verified = verify_project(project_path.as_ref())?;
    if request.expected_project_sha256 != *verified.manifest_sha256() {
        return Err(failure(
            MixerRecoverySnapshotErrorKind::ProjectChanged,
            "project changed before mixer recovery snapshot",
        ));
    }
    require_mixer_descriptor(&verified, request.mixer_id)?;
    let backups = open_or_create_dir(&verified.root_dir, BACKUPS_DIRECTORY)?;
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;
    verified.confirm_unchanged().map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::ProjectChanged,
            "project changed before mixer recovery snapshot publication",
            source,
        )
    })?;
    let snapshot_directory = open_or_create_dir(&backups, SNAPSHOT_DIRECTORY)?;
    let mixer_file = next_mixer_payload_name(&snapshot_directory)?;
    let record = SnapshotRecord {
        format: FORMAT.to_owned(),
        format_version: FORMAT_VERSION,
        project_id: verified.manifest().project_id().to_owned(),
        manifest_sha256: hex_digest(*verified.manifest_sha256()),
        mixer_id: request.mixer_id.to_owned(),
        mixer_sha256: hex_digest(digest(request.mixer_bytes)),
        mixer_file: Some(mixer_file.to_owned()),
    };
    let record_bytes = serde_json_canonicalizer::to_vec(&record).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "canonicalize mixer recovery snapshot record",
            source,
        )
    })?;
    publish_bytes(&snapshot_directory, mixer_file, request.mixer_bytes)?;
    publish_bytes(&snapshot_directory, RECORD_NAME, &record_bytes)
}

/// Loads the current project's matching local mixer-recovery snapshot.
///
/// A snapshot recorded for a different project revision is deliberately
/// invisible: structural edits are already durable and must not be merged
/// with an older mixer layout.
///
/// # Errors
///
/// Returns `Ok(None)` when no matching snapshot exists. Returns a classified
/// error for malformed, linked, unstable, or otherwise untrusted recovery
/// state.
pub fn load_mixer_recovery_snapshot(
    project_path: impl AsRef<Path>,
) -> Result<Option<MixerRecoverySnapshot>, MixerRecoverySnapshotError> {
    let verified = verify_project(project_path.as_ref())?;
    let Some(backups) = open_optional_dir(&verified.root_dir, BACKUPS_DIRECTORY)? else {
        return Ok(None);
    };
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;
    let Some(snapshot_directory) = open_optional_dir(&backups, SNAPSHOT_DIRECTORY)? else {
        return Ok(None);
    };
    let record_bytes = match read_bounded(&snapshot_directory, RECORD_NAME) {
        Ok(bytes) => bytes,
        Err(error) if error.kind == MixerRecoverySnapshotErrorKind::SnapshotUnavailable => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let record = parse_record(&record_bytes)?;
    let mixer_file = mixer_payload_name(&record)?;
    let manifest_sha256 =
        decode_sha256(&record.manifest_sha256, RECORD_NAME).map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::SnapshotInvalid,
                "decode mixer recovery project digest",
                source,
            )
        })?;
    if record.project_id != verified.manifest().project_id()
        || manifest_sha256 != *verified.manifest_sha256()
    {
        return Ok(None);
    }
    require_mixer_descriptor(&verified, &record.mixer_id)?;
    let mixer_bytes = read_bounded(&snapshot_directory, mixer_file)?;
    validate_canonical_json(&mixer_bytes)?;
    let mixer_sha256 = decode_sha256(&record.mixer_sha256, mixer_file).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "decode mixer recovery digest",
            source,
        )
    })?;
    if digest(&mixer_bytes) != mixer_sha256 {
        return Err(failure(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "mixer recovery bytes do not match their record",
        ));
    }
    verified.confirm_unchanged().map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::ProjectChanged,
            "project changed while loading mixer recovery snapshot",
            source,
        )
    })?;
    Ok(Some(MixerRecoverySnapshot {
        project_id: record.project_id,
        manifest_sha256,
        mixer_id: record.mixer_id,
        mixer_bytes,
    }))
}

/// Invalidates the local mixer-recovery snapshot without changing the project.
///
/// # Errors
///
/// Returns a classified error when the local recovery state cannot be safely
/// removed.
pub fn clear_mixer_recovery_snapshot(
    project_path: impl AsRef<Path>,
) -> Result<(), MixerRecoverySnapshotError> {
    let verified = verify_project(project_path.as_ref())?;
    let Some(backups) = open_optional_dir(&verified.root_dir, BACKUPS_DIRECTORY)? else {
        return Ok(());
    };
    let _lock = ProjectLock::acquire(&backups).map_err(lock_error)?;
    let Some(snapshot_directory) = open_optional_dir(&backups, SNAPSHOT_DIRECTORY)? else {
        return Ok(());
    };
    remove_optional_file(&snapshot_directory, RECORD_NAME)?;
    remove_optional_file(&snapshot_directory, LEGACY_MIXER_NAME)?;
    remove_optional_file(&snapshot_directory, MIXER_SLOT_A)?;
    remove_optional_file(&snapshot_directory, MIXER_SLOT_B)?;
    sync_dir(&snapshot_directory)
}

fn verify_project(path: &Path) -> Result<StructurallyVerifiedProject, MixerRecoverySnapshotError> {
    verify_project_package(path).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::ProjectChanged,
            "verify project for mixer recovery",
            source,
        )
    })
}

fn require_mixer_descriptor(
    project: &StructurallyVerifiedProject,
    mixer_id: &str,
) -> Result<(), MixerRecoverySnapshotError> {
    if project
        .manifest()
        .resources()
        .iter()
        .any(|descriptor| descriptor.id() == mixer_id && descriptor.role() == ResourceRole::Mixer)
    {
        Ok(())
    } else {
        Err(failure(
            MixerRecoverySnapshotErrorKind::ProjectChanged,
            "mixer recovery snapshot does not match the current project mixer",
        ))
    }
}

fn next_mixer_payload_name(
    snapshot_directory: &Dir,
) -> Result<&'static str, MixerRecoverySnapshotError> {
    let record_bytes = match read_bounded(snapshot_directory, RECORD_NAME) {
        Ok(bytes) => bytes,
        Err(error) if error.kind == MixerRecoverySnapshotErrorKind::SnapshotUnavailable => {
            return Ok(MIXER_SLOT_A);
        }
        Err(error) => return Err(error),
    };
    let record = parse_record(&record_bytes)?;
    match mixer_payload_name(&record)? {
        MIXER_SLOT_A => Ok(MIXER_SLOT_B),
        MIXER_SLOT_B | LEGACY_MIXER_NAME => Ok(MIXER_SLOT_A),
        _ => unreachable!("mixer payload name was validated before slot selection"),
    }
}

fn mixer_payload_name(record: &SnapshotRecord) -> Result<&str, MixerRecoverySnapshotError> {
    if record.format != FORMAT {
        return Err(failure(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "mixer recovery snapshot format is unsupported",
        ));
    }
    match record.format_version {
        LEGACY_FORMAT_VERSION if record.mixer_file.is_none() => Ok(LEGACY_MIXER_NAME),
        FORMAT_VERSION => {
            let mixer_file = record.mixer_file.as_deref().ok_or_else(|| {
                failure(
                    MixerRecoverySnapshotErrorKind::SnapshotInvalid,
                    "mixer recovery snapshot payload name is missing",
                )
            })?;
            if matches!(mixer_file, MIXER_SLOT_A | MIXER_SLOT_B) {
                Ok(mixer_file)
            } else {
                Err(failure(
                    MixerRecoverySnapshotErrorKind::SnapshotInvalid,
                    "mixer recovery snapshot payload name is invalid",
                ))
            }
        }
        _ => Err(failure(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "mixer recovery snapshot version is unsupported",
        )),
    }
}

fn parse_record(bytes: &[u8]) -> Result<SnapshotRecord, MixerRecoverySnapshotError> {
    let value: serde_json::Value =
        hyper_strict_json::parse(bytes, MAX_SNAPSHOT_BYTES).map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::SnapshotInvalid,
                "parse mixer recovery snapshot record",
                source,
            )
        })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "canonicalize mixer recovery snapshot record",
            source,
        )
    })?;
    if canonical != bytes {
        return Err(failure(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "mixer recovery snapshot record is not canonical",
        ));
    }
    serde_json::from_value(value).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "decode mixer recovery snapshot record",
            source,
        )
    })
}

fn validate_canonical_json(bytes: &[u8]) -> Result<(), MixerRecoverySnapshotError> {
    let value: serde_json::Value =
        hyper_strict_json::parse(bytes, MAX_SNAPSHOT_BYTES).map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::SnapshotInvalid,
                "parse mixer recovery bytes",
                source,
            )
        })?;
    let canonical = serde_json_canonicalizer::to_vec(&value).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "canonicalize mixer recovery bytes",
            source,
        )
    })?;
    if canonical == bytes {
        Ok(())
    } else {
        Err(failure(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "mixer recovery bytes are not canonical",
        ))
    }
}

fn publish_bytes(
    directory: &Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), MixerRecoverySnapshotError> {
    let temporary = format!(".{name}-{}.tmp", Uuid::new_v4());
    let result = (|| {
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = directory
            .open_with(&temporary, &options)
            .map_err(|source| {
                caused(
                    MixerRecoverySnapshotErrorKind::Io,
                    "create mixer recovery staging file",
                    source,
                )
            })?;
        file.write_all(bytes).map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "write mixer recovery staging file",
                source,
            )
        })?;
        file.sync_all().map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "sync mixer recovery staging file",
                source,
            )
        })?;
        let staged_identity = identity(&file.metadata().map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "inspect mixer recovery staging file",
                source,
            )
        })?);
        drop(file);
        rename_replace(directory, &temporary, staged_identity, directory, name).map_err(
            |source| {
                caused(
                    MixerRecoverySnapshotErrorKind::Io,
                    "publish mixer recovery snapshot",
                    source,
                )
            },
        )?;
        sync_dir(directory)
    })();
    if result.is_err() {
        let _ = directory.remove_file(&temporary);
    }
    result
}

fn read_bounded(directory: &Dir, name: &str) -> Result<Vec<u8>, MixerRecoverySnapshotError> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    let file = directory.open_with(name, &options).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            failure(
                MixerRecoverySnapshotErrorKind::SnapshotUnavailable,
                "mixer recovery snapshot is absent",
            )
        } else {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "open mixer recovery snapshot",
                source,
            )
        }
    })?;
    read_open_stable(file.into_std(), name, MAX_JSON_FILE_BYTES).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::SnapshotInvalid,
            "read mixer recovery snapshot",
            source,
        )
    })
}

fn open_or_create_dir(parent: &Dir, name: &str) -> Result<Dir, MixerRecoverySnapshotError> {
    match parent.create_dir(name) {
        Ok(()) => sync_dir(parent)?,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(caused(
                MixerRecoverySnapshotErrorKind::Io,
                "create mixer recovery directory",
                source,
            ));
        }
    }
    parent.open_dir_nofollow(name).map_err(|source| {
        caused(
            MixerRecoverySnapshotErrorKind::Io,
            "open mixer recovery directory",
            source,
        )
    })
}

fn open_optional_dir(parent: &Dir, name: &str) -> Result<Option<Dir>, MixerRecoverySnapshotError> {
    match parent.open_dir_nofollow(name) {
        Ok(directory) => Ok(Some(directory)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(caused(
            MixerRecoverySnapshotErrorKind::Io,
            "open mixer recovery directory",
            source,
        )),
    }
}

fn remove_optional_file(directory: &Dir, name: &str) -> Result<(), MixerRecoverySnapshotError> {
    match directory.remove_file(name) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(caused(
            MixerRecoverySnapshotErrorKind::Io,
            "remove mixer recovery snapshot",
            source,
        )),
    }
}

fn sync_dir(directory: &Dir) -> Result<(), MixerRecoverySnapshotError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "open mixer recovery directory for synchronization",
                source,
            )
        })?
        .sync_all()
        .map_err(|source| {
            caused(
                MixerRecoverySnapshotErrorKind::Io,
                "sync mixer recovery directory",
                source,
            )
        })
}

fn lock_error(source: ProjectLockError) -> MixerRecoverySnapshotError {
    let kind = if matches!(source, ProjectLockError::Active) {
        MixerRecoverySnapshotErrorKind::TransactionUnavailable
    } else {
        MixerRecoverySnapshotErrorKind::Io
    };
    caused(
        kind,
        "acquire project storage lock for mixer recovery",
        source,
    )
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn hex_digest(value: [u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn failure(
    kind: MixerRecoverySnapshotErrorKind,
    message: impl Into<String>,
) -> MixerRecoverySnapshotError {
    MixerRecoverySnapshotError {
        kind,
        message: message.into(),
        source: None,
    }
}

fn caused(
    kind: MixerRecoverySnapshotErrorKind,
    message: impl Into<String>,
    source: impl Error + Send + Sync + 'static,
) -> MixerRecoverySnapshotError {
    MixerRecoverySnapshotError {
        kind,
        message: message.into(),
        source: Some(Box::new(source)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use hyper_project_model::ResourceRole;

    use super::*;
    use crate::{
        ProjectCreationRequest, ProjectRenameRequest, create_project, rename_project,
        verify_project_package,
    };

    struct Fixture {
        parent: std::path::PathBuf,
        project: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent =
                std::env::temp_dir().join(format!("hyper-mixer-recovery-{}", Uuid::new_v4()));
            fs::create_dir(&parent).unwrap();
            let created = create_project(
                &parent,
                ProjectCreationRequest {
                    directory_name: "Project",
                    project_name: "Project",
                },
            )
            .unwrap();
            Self {
                parent,
                project: created.project().root().to_path_buf(),
            }
        }

        fn mixer_snapshot_parts(&self) -> ([u8; 32], String, Vec<u8>) {
            let verified = verify_project_package(&self.project).unwrap();
            let mixer_id = verified
                .manifest()
                .resources()
                .iter()
                .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
                .unwrap()
                .id()
                .to_owned();
            let mixer = verified.read_resource(&mixer_id).unwrap();
            (*verified.manifest_sha256(), mixer_id, mixer)
        }

        fn snapshot_directory(&self) -> std::path::PathBuf {
            self.project
                .join(BACKUPS_DIRECTORY)
                .join(SNAPSHOT_DIRECTORY)
        }

        fn rewrite_record(&self, mutate: impl FnOnce(&mut serde_json::Value)) {
            let path = self.snapshot_directory().join(RECORD_NAME);
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            mutate(&mut record);
            fs::write(path, serde_json_canonicalizer::to_vec(&record).unwrap()).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.parent).unwrap();
        }
    }

    #[test]
    fn snapshot_round_trips_without_mutating_the_project_and_can_be_cleared() {
        let fixture = Fixture::new();
        let original = verify_project_package(&fixture.project).unwrap();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        let request = MixerRecoverySnapshotRequest {
            expected_project_sha256,
            mixer_id: &mixer_id,
            mixer_bytes: &mixer_bytes,
        };

        store_mixer_recovery_snapshot(&fixture.project, request).unwrap();

        let snapshot = load_mixer_recovery_snapshot(&fixture.project)
            .unwrap()
            .expect("the matching snapshot is recoverable");
        assert_eq!(snapshot.project_id(), original.manifest().project_id());
        assert_eq!(snapshot.manifest_sha256(), original.manifest_sha256());
        assert_eq!(snapshot.mixer_id(), request.mixer_id);
        assert_eq!(snapshot.mixer_bytes(), request.mixer_bytes);
        let after = verify_project_package(&fixture.project).unwrap();
        assert_eq!(after.manifest_sha256(), original.manifest_sha256());

        clear_mixer_recovery_snapshot(&fixture.project).unwrap();
        assert_eq!(
            load_mixer_recovery_snapshot(&fixture.project).unwrap(),
            None
        );
    }

    #[test]
    fn interrupted_payload_replacement_retains_the_last_recorded_snapshot() {
        let fixture = Fixture::new();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        store_mixer_recovery_snapshot(
            &fixture.project,
            MixerRecoverySnapshotRequest {
                expected_project_sha256,
                mixer_id: &mixer_id,
                mixer_bytes: &mixer_bytes,
            },
        )
        .unwrap();
        let mut updated_value: serde_json::Value = serde_json::from_slice(&mixer_bytes).unwrap();
        updated_value["main"]["mute"] = serde_json::Value::Bool(true);
        let updated_mixer = serde_json_canonicalizer::to_vec(&updated_value).unwrap();
        assert_ne!(updated_mixer, mixer_bytes);

        fs::write(
            fixture.snapshot_directory().join(MIXER_SLOT_B),
            &updated_mixer,
        )
        .unwrap();
        let interrupted = load_mixer_recovery_snapshot(&fixture.project)
            .unwrap()
            .expect("the old record remains authoritative until replacement is committed");
        assert_eq!(interrupted.mixer_bytes(), mixer_bytes);

        store_mixer_recovery_snapshot(
            &fixture.project,
            MixerRecoverySnapshotRequest {
                expected_project_sha256,
                mixer_id: &mixer_id,
                mixer_bytes: &updated_mixer,
            },
        )
        .unwrap();
        let committed = load_mixer_recovery_snapshot(&fixture.project)
            .unwrap()
            .expect("the replacement record addresses the newly published payload");
        assert_eq!(committed.mixer_bytes(), updated_mixer);
    }

    #[test]
    fn legacy_version_one_snapshot_remains_recoverable() {
        let fixture = Fixture::new();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        let directory = fixture.snapshot_directory();
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(LEGACY_MIXER_NAME), &mixer_bytes).unwrap();
        let project = verify_project_package(&fixture.project).unwrap();
        let record = SnapshotRecord {
            format: FORMAT.to_owned(),
            format_version: LEGACY_FORMAT_VERSION,
            project_id: project.manifest().project_id().to_owned(),
            manifest_sha256: hex_digest(expected_project_sha256),
            mixer_id: mixer_id.clone(),
            mixer_sha256: hex_digest(digest(&mixer_bytes)),
            mixer_file: None,
        };
        fs::write(
            directory.join(RECORD_NAME),
            serde_json_canonicalizer::to_vec(&record).unwrap(),
        )
        .unwrap();

        let snapshot = load_mixer_recovery_snapshot(&fixture.project)
            .unwrap()
            .expect("a valid snapshot created by version one remains available");
        assert_eq!(snapshot.mixer_id(), mixer_id);
        assert_eq!(snapshot.mixer_bytes(), mixer_bytes);
    }

    #[test]
    fn unsupported_record_format_and_version_are_rejected() {
        let fixture = Fixture::new();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        let request = MixerRecoverySnapshotRequest {
            expected_project_sha256,
            mixer_id: &mixer_id,
            mixer_bytes: &mixer_bytes,
        };
        store_mixer_recovery_snapshot(&fixture.project, request).unwrap();
        fixture.rewrite_record(|record| {
            record["format"] = serde_json::Value::String("foreign-recovery".to_owned());
        });
        assert_eq!(
            load_mixer_recovery_snapshot(&fixture.project)
                .unwrap_err()
                .kind(),
            MixerRecoverySnapshotErrorKind::SnapshotInvalid
        );

        store_mixer_recovery_snapshot(&fixture.project, request).unwrap_err();
        fixture.rewrite_record(|record| {
            record["format"] = serde_json::Value::String(FORMAT.to_owned());
            record["format_version"] = serde_json::Value::from(99_u8);
        });
        assert_eq!(
            load_mixer_recovery_snapshot(&fixture.project)
                .unwrap_err()
                .kind(),
            MixerRecoverySnapshotErrorKind::SnapshotInvalid
        );
    }

    #[test]
    fn snapshot_is_ignored_after_a_durable_project_revision_change() {
        let fixture = Fixture::new();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        let request = MixerRecoverySnapshotRequest {
            expected_project_sha256,
            mixer_id: &mixer_id,
            mixer_bytes: &mixer_bytes,
        };
        store_mixer_recovery_snapshot(&fixture.project, request).unwrap();

        rename_project(
            &fixture.project,
            ProjectRenameRequest {
                expected_project_sha256: request.expected_project_sha256,
                project_name: "Renamed",
            },
        )
        .unwrap();

        assert_eq!(
            load_mixer_recovery_snapshot(&fixture.project).unwrap(),
            None
        );
    }

    #[test]
    fn a_corrupted_snapshot_is_rejected_before_it_can_be_restored() {
        let fixture = Fixture::new();
        let (expected_project_sha256, mixer_id, mixer_bytes) = fixture.mixer_snapshot_parts();
        let request = MixerRecoverySnapshotRequest {
            expected_project_sha256,
            mixer_id: &mixer_id,
            mixer_bytes: &mixer_bytes,
        };
        store_mixer_recovery_snapshot(&fixture.project, request).unwrap();
        let record =
            parse_record(&fs::read(fixture.snapshot_directory().join(RECORD_NAME)).unwrap())
                .unwrap();
        fs::write(
            fixture
                .snapshot_directory()
                .join(mixer_payload_name(&record).unwrap()),
            b"{}",
        )
        .unwrap();

        let error = load_mixer_recovery_snapshot(&fixture.project).unwrap_err();
        assert_eq!(
            error.kind(),
            MixerRecoverySnapshotErrorKind::SnapshotInvalid
        );
    }
}
