use core::error::Error;
use core::fmt;
use std::path::Path;

use super::{
    ControlMappingPublicationError, ControlMappingPublicationRecovery, RecordingImportError,
    RecordingImportRecovery, recover_control_mapping_publications, recover_recording_imports,
};

/// Complete transaction-recovery evidence produced before an editable project
/// is loaded by the desktop IDE.
#[derive(Debug, Default)]
pub struct ProjectTransactionRecoveryReport {
    control_mappings: Vec<ControlMappingPublicationRecovery>,
    recordings: Vec<RecordingImportRecovery>,
}

impl ProjectTransactionRecoveryReport {
    #[must_use]
    pub fn control_mappings(&self) -> &[ControlMappingPublicationRecovery] {
        &self.control_mappings
    }

    #[must_use]
    pub fn recordings(&self) -> &[RecordingImportRecovery] {
        &self.recordings
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.control_mappings.is_empty() && self.recordings.is_empty()
    }
}

#[derive(Debug)]
pub enum ProjectTransactionRecoveryErrorSource {
    ControlMapping(ControlMappingPublicationError),
    Recording(RecordingImportError),
}

impl fmt::Display for ProjectTransactionRecoveryErrorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ControlMapping(source) => {
                write!(formatter, "control-mapping recovery failed: {source}")
            }
            Self::Recording(source) => write!(formatter, "recording recovery failed: {source}"),
        }
    }
}

impl Error for ProjectTransactionRecoveryErrorSource {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ControlMapping(source) => Some(source),
            Self::Recording(source) => Some(source),
        }
    }
}

/// Recovery failure that retains the outcomes already completed before the
/// failing transaction family was reached.
#[derive(Debug)]
pub struct ProjectTransactionRecoveryError {
    completed: ProjectTransactionRecoveryReport,
    source: ProjectTransactionRecoveryErrorSource,
}

impl ProjectTransactionRecoveryError {
    #[must_use]
    pub const fn completed(&self) -> &ProjectTransactionRecoveryReport {
        &self.completed
    }

    #[must_use]
    pub const fn recovery_source(&self) -> &ProjectTransactionRecoveryErrorSource {
        &self.source
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ProjectTransactionRecoveryReport,
        ProjectTransactionRecoveryErrorSource,
    ) {
        (self.completed, self.source)
    }
}

impl fmt::Display for ProjectTransactionRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for ProjectTransactionRecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Recovers every known editable-project transaction family in deterministic
/// order before normal project verification.
///
/// Mapping publication is recovered first because it may repair the manifest
/// and mapping pair needed by later package verification. Recording import is
/// then recovered through the same shared project lock.
///
/// Recovery is fail-closed when another project storage transaction owns that
/// lock. Callers may retry after the competing transaction has completed; this
/// function does not promise a consistent result during concurrent mutation.
///
/// # Errors
///
/// Returns the failing recovery authority together with all prior completed
/// outcomes. Unknown or unrelated filesystem state is never discarded.
pub fn recover_project_transactions(
    project_path: impl AsRef<Path>,
) -> Result<ProjectTransactionRecoveryReport, ProjectTransactionRecoveryError> {
    let project_path = project_path.as_ref();
    let control_mappings =
        recover_control_mapping_publications(project_path).map_err(|source| {
            ProjectTransactionRecoveryError {
                completed: ProjectTransactionRecoveryReport::default(),
                source: ProjectTransactionRecoveryErrorSource::ControlMapping(source),
            }
        })?;
    let report = ProjectTransactionRecoveryReport {
        control_mappings,
        recordings: Vec::new(),
    };
    match recover_recording_imports(project_path) {
        Ok(recordings) => Ok(ProjectTransactionRecoveryReport {
            control_mappings: report.control_mappings,
            recordings,
        }),
        Err(source) => Err(ProjectTransactionRecoveryError {
            completed: report,
            source: ProjectTransactionRecoveryErrorSource::Recording(source),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording_import::ProjectLock;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn transaction_free_recovery_is_read_only() {
        let root = std::env::temp_dir().join(format!(
            "hyper-project-recovery-empty-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();

        let report = recover_project_transactions(&root).unwrap();

        assert!(report.is_empty());
        assert!(!root.join("backups").exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn concurrent_project_mutation_fails_closed_and_can_be_retried() {
        let root = std::env::temp_dir().join(format!(
            "hyper-project-recovery-busy-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("backups")).unwrap();
        let backups =
            cap_std::fs::Dir::open_ambient_dir(root.join("backups"), cap_std::ambient_authority())
                .unwrap();
        let lock = ProjectLock::acquire(&backups).unwrap();

        let error = recover_project_transactions(&root).unwrap_err();

        assert!(error.completed().is_empty());
        assert!(matches!(
            error.recovery_source(),
            ProjectTransactionRecoveryErrorSource::ControlMapping(_)
        ));
        drop(lock);
        assert!(recover_project_transactions(&root).unwrap().is_empty());
        drop(backups);
        fs::remove_dir_all(root).unwrap();
    }
}
