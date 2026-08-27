use core::error::Error;
use core::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};

const LOCK_NAME: &str = "project-storage.lock";

pub(crate) const PROJECT_RESOURCE_TRANSACTION_PREFIX: &str = ".project-resource-publication-";
pub(crate) const AUDIO_TRACK_TRANSACTION_PREFIX: &str = ".audio-track-publication-";
pub(crate) const TRACK_ORDER_TRANSACTION_PREFIX: &str = ".track-order-publication-";
pub(crate) const AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX: &str = ".audio-track-duplication-";
pub(crate) const AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX: &str =
    ".audio-track-content-removal-";
pub(crate) const AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX: &str = ".audio-track-freeze-";
pub(crate) const AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX: &str = ".audio-track-unfreeze-";
pub(crate) const AUDIO_CLIP_TRANSACTION_PREFIX: &str = ".audio-clip-publication-";
pub(crate) const SAMPLE_CLEANUP_TRANSACTION_PREFIX: &str = ".sample-cleanup-";
pub(crate) const CONTROL_MAPPING_TRANSACTION_PREFIX: &str = ".control-mapping-publication-";
pub(crate) const RECORDING_IMPORT_TRANSACTION_PREFIX: &str = ".recording-import-";
pub(crate) const RECORDING_DISCARD_TRANSACTION_PREFIX: &str = ".recording-discard-";
pub(crate) const PROJECT_RENAME_TRANSACTION_PREFIX: &str = ".project-rename-";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectTransactionFamily {
    ProjectResource,
    AudioTrack,
    TrackOrder,
    AudioTrackDuplication,
    AudioTrackContentRemoval,
    AudioTrackFreeze,
    AudioTrackUnfreeze,
    AudioClip,
    SampleCleanup,
    ControlMapping,
    RecordingImport,
    RecordingDiscard,
    ProjectRename,
}

impl ProjectTransactionFamily {
    const ALL: [Self; 13] = [
        Self::ProjectResource,
        Self::AudioTrack,
        Self::TrackOrder,
        Self::AudioTrackDuplication,
        Self::AudioTrackContentRemoval,
        Self::AudioTrackFreeze,
        Self::AudioTrackUnfreeze,
        Self::AudioClip,
        Self::SampleCleanup,
        Self::ControlMapping,
        Self::RecordingImport,
        Self::RecordingDiscard,
        Self::ProjectRename,
    ];

    const fn prefix(self) -> &'static str {
        match self {
            Self::ProjectResource => PROJECT_RESOURCE_TRANSACTION_PREFIX,
            Self::AudioTrack => AUDIO_TRACK_TRANSACTION_PREFIX,
            Self::TrackOrder => TRACK_ORDER_TRANSACTION_PREFIX,
            Self::AudioTrackDuplication => AUDIO_TRACK_DUPLICATION_TRANSACTION_PREFIX,
            Self::AudioTrackContentRemoval => AUDIO_TRACK_CONTENT_REMOVAL_TRANSACTION_PREFIX,
            Self::AudioTrackFreeze => AUDIO_TRACK_FREEZE_TRANSACTION_PREFIX,
            Self::AudioTrackUnfreeze => AUDIO_TRACK_UNFREEZE_TRANSACTION_PREFIX,
            Self::AudioClip => AUDIO_CLIP_TRANSACTION_PREFIX,
            Self::SampleCleanup => SAMPLE_CLEANUP_TRANSACTION_PREFIX,
            Self::ControlMapping => CONTROL_MAPPING_TRANSACTION_PREFIX,
            Self::RecordingImport => RECORDING_IMPORT_TRANSACTION_PREFIX,
            Self::RecordingDiscard => RECORDING_DISCARD_TRANSACTION_PREFIX,
            Self::ProjectRename => PROJECT_RENAME_TRANSACTION_PREFIX,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProjectLockError {
    Active,
    Io {
        operation: &'static str,
        source: std::io::Error,
    },
}

impl fmt::Display for ProjectLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active => formatter.write_str("another project storage transaction is active"),
            Self::Io { operation, .. } => write!(formatter, "{operation} failed"),
        }
    }
}

impl Error for ProjectLockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Active => None,
            Self::Io { source, .. } => Some(source),
        }
    }
}

pub(crate) struct ProjectLock {
    _file: File,
}

impl ProjectLock {
    pub(crate) const fn file_name() -> &'static str {
        LOCK_NAME
    }

    pub(crate) fn acquire(backups: &Dir) -> Result<Self, ProjectLockError> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .follow(FollowSymlinks::No);
        let capability_file =
            backups
                .open_with(LOCK_NAME, &options)
                .map_err(|source| ProjectLockError::Io {
                    operation: "open project storage lock",
                    source,
                })?;
        let mut file = capability_file.into_std();
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(ProjectLockError::Active),
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(ProjectLockError::Io {
                    operation: "lock project storage transaction",
                    source,
                });
            }
        }
        file.set_len(0).map_err(|source| ProjectLockError::Io {
            operation: "reset project storage lock",
            source,
        })?;
        file.seek(SeekFrom::Start(0))
            .map_err(|source| ProjectLockError::Io {
                operation: "position project storage lock",
                source,
            })?;
        file.write_all(b"hyper-project-storage-lock-v2\n")
            .map_err(|source| ProjectLockError::Io {
                operation: "write project storage lock",
                source,
            })?;
        file.sync_all().map_err(|source| ProjectLockError::Io {
            operation: "sync project storage lock",
            source,
        })?;
        sync_dir(backups)?;
        Ok(Self { _file: file })
    }
}

pub(crate) fn find_unhandled_project_transaction(
    backups: &Dir,
    handled: &[ProjectTransactionFamily],
) -> Result<Option<ProjectTransactionFamily>, ProjectLockError> {
    let entries = backups.entries().map_err(|source| ProjectLockError::Io {
        operation: "read project transaction directory",
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ProjectLockError::Io {
            operation: "read project transaction entry",
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        for family in ProjectTransactionFamily::ALL {
            if name.starts_with(family.prefix()) && !handled.contains(&family) {
                return Ok(Some(family));
            }
        }
    }
    Ok(None)
}

fn sync_dir(directory: &Dir) -> Result<(), ProjectLockError> {
    let mut options = OpenOptions::new();
    options.read(true).maybe_dir(true);
    #[cfg(windows)]
    options.write(true);
    directory
        .open_with(".", &options)
        .map_err(|source| ProjectLockError::Io {
            operation: "open project storage directory for synchronization",
            source,
        })?
        .sync_all()
        .map_err(|source| ProjectLockError::Io {
            operation: "sync project storage directory",
            source,
        })
}
