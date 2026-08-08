use core::error::Error;
use core::fmt;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt, OpenOptionsMaybeDirExt};
use cap_std::fs::{Dir, OpenOptions};

const LOCK_NAME: &str = "project-storage.lock";

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
