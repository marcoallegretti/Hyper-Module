use core::error::Error;
use core::fmt;
use std::path::Path;

use super::{
    AudioTrackPublicationRecovery, ControlMappingPublicationError,
    ControlMappingPublicationRecovery, ProjectResourcePublicationError,
    ProjectResourcePublicationRecovery, RecordingImportError, RecordingImportRecovery,
    recover_audio_track_publications, recover_control_mapping_publications,
    recover_project_resource_publications, recover_recording_imports,
};

/// Complete transaction-recovery evidence produced before an editable project
/// is loaded by the desktop IDE.
#[derive(Debug, Default)]
pub struct ProjectTransactionRecoveryReport {
    resources: Vec<ProjectResourcePublicationRecovery>,
    audio_tracks: Vec<AudioTrackPublicationRecovery>,
    control_mappings: Vec<ControlMappingPublicationRecovery>,
    recordings: Vec<RecordingImportRecovery>,
}

impl ProjectTransactionRecoveryReport {
    #[must_use]
    pub fn resources(&self) -> &[ProjectResourcePublicationRecovery] {
        &self.resources
    }

    #[must_use]
    pub fn audio_tracks(&self) -> &[AudioTrackPublicationRecovery] {
        &self.audio_tracks
    }

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
        self.resources.is_empty()
            && self.audio_tracks.is_empty()
            && self.control_mappings.is_empty()
            && self.recordings.is_empty()
    }
}

#[derive(Debug)]
pub enum ProjectTransactionRecoveryErrorSource {
    Resource(ProjectResourcePublicationError),
    AudioTrack(ProjectResourcePublicationError),
    ControlMapping(ControlMappingPublicationError),
    Recording(RecordingImportError),
}

impl fmt::Display for ProjectTransactionRecoveryErrorSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resource(source) => write!(formatter, "resource recovery failed: {source}"),
            Self::AudioTrack(source) => write!(formatter, "audio-track recovery failed: {source}"),
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
            Self::Resource(source) | Self::AudioTrack(source) => Some(source),
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
    source: Box<ProjectTransactionRecoveryErrorSource>,
}

impl ProjectTransactionRecoveryError {
    #[must_use]
    pub const fn completed(&self) -> &ProjectTransactionRecoveryReport {
        &self.completed
    }

    #[must_use]
    pub fn recovery_source(&self) -> &ProjectTransactionRecoveryErrorSource {
        self.source.as_ref()
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        ProjectTransactionRecoveryReport,
        ProjectTransactionRecoveryErrorSource,
    ) {
        (self.completed, *self.source)
    }
}

impl fmt::Display for ProjectTransactionRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for ProjectTransactionRecoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Recovers every known editable-project transaction family in deterministic
/// order before normal project verification.
///
/// Existing-resource publication is recovered first because it may repair a
/// manifest/resource pair needed by later package verification. Audio-track,
/// mapping, and recording transactions then follow in that order through the
/// same shared lock.
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
    let resources = recover_project_resource_publications(project_path).map_err(|source| {
        ProjectTransactionRecoveryError {
            completed: ProjectTransactionRecoveryReport::default(),
            source: Box::new(ProjectTransactionRecoveryErrorSource::Resource(source)),
        }
    })?;
    let audio_tracks = match recover_audio_track_publications(project_path) {
        Ok(audio_tracks) => audio_tracks,
        Err(source) => {
            return Err(ProjectTransactionRecoveryError {
                completed: ProjectTransactionRecoveryReport {
                    resources,
                    ..ProjectTransactionRecoveryReport::default()
                },
                source: Box::new(ProjectTransactionRecoveryErrorSource::AudioTrack(source)),
            });
        }
    };
    let control_mappings = match recover_control_mapping_publications(project_path) {
        Ok(control_mappings) => control_mappings,
        Err(source) => {
            return Err(ProjectTransactionRecoveryError {
                completed: ProjectTransactionRecoveryReport {
                    resources,
                    audio_tracks,
                    ..ProjectTransactionRecoveryReport::default()
                },
                source: Box::new(ProjectTransactionRecoveryErrorSource::ControlMapping(
                    source,
                )),
            });
        }
    };
    let report = ProjectTransactionRecoveryReport {
        resources,
        audio_tracks,
        control_mappings,
        recordings: Vec::new(),
    };
    match recover_recording_imports(project_path) {
        Ok(recordings) => Ok(ProjectTransactionRecoveryReport {
            resources: report.resources,
            audio_tracks: report.audio_tracks,
            control_mappings: report.control_mappings,
            recordings,
        }),
        Err(source) => Err(ProjectTransactionRecoveryError {
            completed: report,
            source: Box::new(ProjectTransactionRecoveryErrorSource::Recording(source)),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_lock::ProjectLock;
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
            ProjectTransactionRecoveryErrorSource::Resource(_)
        ));
        drop(lock);
        assert!(recover_project_transactions(&root).unwrap().is_empty());
        drop(backups);
        fs::remove_dir_all(root).unwrap();
    }
}
