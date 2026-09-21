//! Runtime recording preparation and finalized handback publication.
//!
//! The Runtime never writes its verified project. One explicitly configured
//! external directory owns the recoverable journal, published WAV, and the
//! provenance-bound update that the IDE may later import.

use core::error::Error;
use core::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_desktop_audio::{
    DesktopFinalizedRecording, DesktopPlaybackRecovery, DesktopRecordingSessionConfig,
};
use hyper_mixer_runtime::PlaybackRecoveryTransportState;
use hyper_playback_compiler::{FrameTimeline, FrameTimelineError};
use hyper_playback_json::CompiledMixerTrackKind;
use hyper_project_loader::PreparedProjectPlayback;
use hyper_recording_journal::JournalConfig;
use hyper_recording_metadata::{
    FinalizedRecordingHandoff, FinalizedRecordingHandoffParseError, RecordingCaptureKind,
    RecordingHandoffError, RecordingMetadataContext,
};
use hyper_recording_worker::RecordingWorkerConfig;
use hyper_runtime_recording_update::{
    RuntimeRecordingUpdate, RuntimeRecordingUpdateDirectory, RuntimeRecordingUpdateError,
    RuntimeRecordingUpdateErrorKind, RuntimeRecordingUpdateSource,
};
use uuid::Uuid;

const RECORDING_TRANSPORT_FRAMES: usize = super::MAXIMUM_BLOCK_FRAMES * 4;
const RECORDING_DRAIN_FRAMES: usize = super::MAXIMUM_BLOCK_FRAMES;
const RECORDING_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const RECORDING_MAXIMUM_FRAMES: u64 = 43_200_000;
const RECORDING_MAXIMUM_CHUNK_FRAMES: u32 = 4_096;

struct RuntimeRecordingPaths {
    journal: PathBuf,
    final_wav: PathBuf,
}

impl RuntimeRecordingPaths {
    fn for_id(root: &Path, recording_id: &str) -> Self {
        Self {
            journal: root.join(format!("{recording_id}.journal")),
            final_wav: root.join(format!("{recording_id}.wav")),
        }
    }
}

pub(super) enum RuntimeRecordingPublication {
    Published(PathBuf),
    Uncertain {
        output_directory: PathBuf,
        source: RuntimeRecordingUpdateError,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeRecordingTarget {
    channel_index: usize,
    track_id: String,
    label: String,
}

/// Immutable recording facts derived from the verified deployment before
/// native audio starts.
pub(super) struct RuntimeRecordingConfiguration {
    source: RuntimeRecordingUpdateSource,
    targets: Vec<RuntimeRecordingTarget>,
    timeline: FrameTimeline,
    output_directory: Option<RuntimeRecordingUpdateDirectory>,
}

impl RuntimeRecordingConfiguration {
    pub(super) fn try_new(
        source: RuntimeRecordingUpdateSource,
        project: &PreparedProjectPlayback,
        output_directory: Option<RuntimeRecordingUpdateDirectory>,
    ) -> Result<Self, RuntimeRecordingError> {
        let manifest = project.package().manifest();
        if manifest.project_id() != source.project_id()
            || manifest.revision() != source.project_revision()
            || hyper_runtime_recording_update::sha256_hex(project.package().manifest_sha256())
                != source.project_sha256()
        {
            return Err(RuntimeRecordingError::ProjectChanged);
        }
        let (loaded, mixer, ..) = project.session().parts();
        let timeline = FrameTimeline::try_new(loaded.sample_rate_hz(), loaded.tempo_events())
            .map_err(RuntimeRecordingError::Timeline)?;
        let mut targets = Vec::new();
        targets
            .try_reserve_exact(mixer.slots().len())
            .map_err(|_| RuntimeRecordingError::CapacityUnavailable)?;
        for (channel_index, channel) in mixer.slots().iter().enumerate() {
            if channel.kind() != CompiledMixerTrackKind::Audio {
                continue;
            }
            let track_id = channel.track_id();
            if !loaded.is_audio_track(track_id) {
                return Err(RuntimeRecordingError::ProjectCatalogUnavailable);
            }
            let label = loaded
                .track_name(track_id)
                .ok_or(RuntimeRecordingError::ProjectCatalogUnavailable)?;
            targets.push(RuntimeRecordingTarget {
                channel_index,
                track_id: try_owned(track_id)?,
                label: try_owned(label)?,
            });
        }
        Ok(Self {
            source,
            targets,
            timeline,
            output_directory,
        })
    }

    pub(super) fn target_labels(&self) -> Result<Vec<String>, RuntimeRecordingError> {
        let mut labels = Vec::new();
        labels
            .try_reserve_exact(self.targets.len())
            .map_err(|_| RuntimeRecordingError::CapacityUnavailable)?;
        for target in &self.targets {
            labels.push(try_owned(&target.label)?);
        }
        Ok(labels)
    }

    pub(super) const fn output_configured(&self) -> bool {
        self.output_directory.is_some()
    }

    pub(super) const fn has_targets(&self) -> bool {
        !self.targets.is_empty()
    }

    pub(super) fn prepare(
        &self,
        target_index: usize,
        recovery: &DesktopPlaybackRecovery,
        captured_at_utc: String,
    ) -> Result<PreparedRuntimeRecording, RuntimeRecordingError> {
        let output = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeRecordingError::OutputDirectoryNotConfigured)?;
        let target = self
            .targets
            .get(target_index)
            .ok_or(RuntimeRecordingError::TargetUnavailable)?;
        let project = recovery
            .prepared_project()
            .ok_or(RuntimeRecordingError::ProjectSnapshotUnavailable)?;
        self.revalidate_project(project, target)?;
        let monitoring = recovery
            .monitoring_config()
            .ok_or(RuntimeRecordingError::InputUnavailable)?;
        let checkpoint = recovery.checkpoint();
        if checkpoint.state() != PlaybackRecoveryTransportState::Stopped {
            return Err(RuntimeRecordingError::TransportRolling);
        }
        if checkpoint.loop_enabled() {
            return Err(RuntimeRecordingError::LoopEnabled);
        }
        let start_tick = self
            .timeline
            .position_at_frame(checkpoint.position_frame())
            .and_then(|position| position.quantize_forward(1))
            .map_err(RuntimeRecordingError::Timeline)?;
        let recording_id = Uuid::new_v4().hyphenated().to_string();
        let asset_id = Uuid::new_v4().hyphenated().to_string();
        let update_id = Uuid::new_v4().hyphenated().to_string();
        let paths = RuntimeRecordingPaths::for_id(output.root_path(), &recording_id);
        let context = RecordingMetadataContext {
            id: recording_id,
            project_id: self.source.project_id().to_owned(),
            asset_id,
            track_id: target.track_id.clone(),
            capture_kind: RecordingCaptureKind::New,
            captured_at_utc,
            timeline_start_tick: start_tick,
            source_input_indices: vec![0, 1],
        };
        let session = recording_session_config(monitoring.input().clone(), paths);
        Ok(PreparedRuntimeRecording {
            session,
            context,
            update_id,
            target_label: target.label.clone(),
        })
    }

    fn revalidate_project(
        &self,
        project: &PreparedProjectPlayback,
        target: &RuntimeRecordingTarget,
    ) -> Result<(), RuntimeRecordingError> {
        let manifest = project.package().manifest();
        if manifest.project_id() != self.source.project_id()
            || manifest.revision() != self.source.project_revision()
            || hyper_runtime_recording_update::sha256_hex(project.package().manifest_sha256())
                != self.source.project_sha256()
        {
            return Err(RuntimeRecordingError::ProjectChanged);
        }
        let session = project.session();
        let (loaded, mixer, ..) = session.parts();
        if mixer
            .slots()
            .get(target.channel_index)
            .is_none_or(|channel| {
                channel.track_id() != target.track_id
                    || channel.kind() != CompiledMixerTrackKind::Audio
            })
            || session.mixer_channel_track_id_at(target.channel_index) != Some(&target.track_id)
            || !loaded.is_audio_track(&target.track_id)
        {
            return Err(RuntimeRecordingError::ProjectChanged);
        }
        Ok(())
    }

    pub(super) fn publish(
        &self,
        update_id: String,
        recording: &DesktopFinalizedRecording,
    ) -> Result<RuntimeRecordingPublication, RuntimeRecordingError> {
        let output = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeRecordingError::OutputDirectoryNotConfigured)?;
        let bytes = recording
            .to_handoff_json()
            .map_err(RuntimeRecordingError::Handoff)?;
        let handoff = FinalizedRecordingHandoff::parse(&bytes)
            .map_err(RuntimeRecordingError::HandoffParse)?;
        let update = RuntimeRecordingUpdate::try_new(update_id, self.source.clone(), handoff)
            .map_err(RuntimeRecordingError::Update)?;
        match output.publish(&update) {
            Ok(path) => Ok(RuntimeRecordingPublication::Published(path)),
            Err(source) if publication_outcome_is_uncertain(source.kind()) => {
                Ok(RuntimeRecordingPublication::Uncertain {
                    output_directory: output.root_path().to_owned(),
                    source,
                })
            }
            Err(source) => Err(RuntimeRecordingError::Update(source)),
        }
    }
}

fn recording_session_config(
    input: hyper_desktop_audio::DesktopInputConfig,
    paths: RuntimeRecordingPaths,
) -> DesktopRecordingSessionConfig {
    DesktopRecordingSessionConfig {
        input,
        transport_capacity_frames: RECORDING_TRANSPORT_FRAMES,
        writer: RecordingWorkerConfig {
            journal_path: paths.journal,
            final_wav_path: paths.final_wav,
            journal: JournalConfig {
                sample_rate: super::SAMPLE_RATE_HZ,
                max_chunk_frames: RECORDING_MAXIMUM_CHUNK_FRAMES,
                max_total_frames: RECORDING_MAXIMUM_FRAMES,
            },
            drain_frames: RECORDING_DRAIN_FRAMES,
        },
        completion_timeout: RECORDING_COMPLETION_TIMEOUT,
    }
}

const fn publication_outcome_is_uncertain(kind: RuntimeRecordingUpdateErrorKind) -> bool {
    matches!(
        kind,
        RuntimeRecordingUpdateErrorKind::AuthorityChanged
            | RuntimeRecordingUpdateErrorKind::PublicationUncertain
    )
}

pub(super) struct PreparedRuntimeRecording {
    session: DesktopRecordingSessionConfig,
    context: RecordingMetadataContext,
    update_id: String,
    target_label: String,
}

impl PreparedRuntimeRecording {
    pub(super) const fn session(&self) -> &DesktopRecordingSessionConfig {
        &self.session
    }

    pub(super) fn into_active(self) -> ActiveRuntimeRecording {
        ActiveRuntimeRecording {
            context: self.context,
            update_id: self.update_id,
            target_label: self.target_label,
        }
    }

    pub(super) fn target_label(&self) -> &str {
        &self.target_label
    }
}

pub(super) struct ActiveRuntimeRecording {
    context: RecordingMetadataContext,
    update_id: String,
    target_label: String,
}

impl ActiveRuntimeRecording {
    pub(super) fn into_parts(self) -> (RecordingMetadataContext, String, String) {
        (self.context, self.update_id, self.target_label)
    }

    pub(super) fn target_label(&self) -> &str {
        &self.target_label
    }
}

#[derive(Debug)]
pub(super) enum RuntimeRecordingError {
    OutputDirectoryNotConfigured,
    InputUnavailable,
    TargetUnavailable,
    ProjectSnapshotUnavailable,
    ProjectCatalogUnavailable,
    ProjectChanged,
    TransportRolling,
    LoopEnabled,
    CapacityUnavailable,
    Timeline(FrameTimelineError),
    Handoff(RecordingHandoffError),
    HandoffParse(FinalizedRecordingHandoffParseError),
    Update(RuntimeRecordingUpdateError),
}

impl fmt::Display for RuntimeRecordingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external recording-update directory",
            ),
            Self::InputUnavailable => {
                formatter.write_str("recording requires an explicitly selected monitored input")
            }
            Self::TargetUnavailable => {
                formatter.write_str("the selected Runtime recording target is unavailable")
            }
            Self::ProjectSnapshotUnavailable => {
                formatter.write_str("playback recovery exposes no verified project snapshot")
            }
            Self::ProjectCatalogUnavailable => {
                formatter.write_str("the verified Runtime project has inconsistent recording facts")
            }
            Self::ProjectChanged => {
                formatter.write_str("the verified project changed while recording was prepared")
            }
            Self::TransportRolling => {
                formatter.write_str("transport must be stopped before recording")
            }
            Self::LoopEnabled => {
                formatter.write_str("loop playback must be disabled before recording")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime recording preparation capacity is unavailable")
            }
            Self::Timeline(source) => source.fmt(formatter),
            Self::Handoff(source) => source.fmt(formatter),
            Self::HandoffParse(source) => source.fmt(formatter),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeRecordingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::Handoff(source) => Some(source),
            Self::HandoffParse(source) => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::InputUnavailable
            | Self::TargetUnavailable
            | Self::ProjectSnapshotUnavailable
            | Self::ProjectCatalogUnavailable
            | Self::ProjectChanged
            | Self::TransportRolling
            | Self::LoopEnabled
            | Self::CapacityUnavailable => None,
        }
    }
}

fn try_owned(value: &str) -> Result<String, RuntimeRecordingError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RuntimeRecordingError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::{
        RECORDING_COMPLETION_TIMEOUT, RECORDING_DRAIN_FRAMES, RECORDING_MAXIMUM_CHUNK_FRAMES,
        RECORDING_MAXIMUM_FRAMES, RECORDING_TRANSPORT_FRAMES, RuntimeRecordingPaths,
        publication_outcome_is_uncertain, recording_session_config,
    };
    use hyper_desktop_audio::DesktopInputConfig;
    use hyper_runtime_recording_update::RuntimeRecordingUpdateErrorKind;
    use std::path::Path;

    #[test]
    fn recording_paths_are_direct_children_with_distinct_suffixes() {
        let root = Path::new("C:/recordings");
        let id = "11111111-1111-4111-8111-111111111111";
        let paths = RuntimeRecordingPaths::for_id(root, id);

        assert_eq!(
            paths.journal,
            Path::new("C:/recordings/11111111-1111-4111-8111-111111111111.journal")
        );
        assert_eq!(
            paths.final_wav,
            Path::new("C:/recordings/11111111-1111-4111-8111-111111111111.wav")
        );
    }

    #[test]
    fn recording_session_uses_the_bounded_runtime_policy() {
        let input = DesktopInputConfig::try_new(super::super::SAMPLE_RATE_HZ, 256).unwrap();
        let paths = RuntimeRecordingPaths::for_id(
            Path::new("C:/recordings"),
            "11111111-1111-4111-8111-111111111111",
        );

        let session = recording_session_config(input.clone(), paths);

        assert_eq!(session.input, input);
        assert_eq!(
            session.transport_capacity_frames,
            RECORDING_TRANSPORT_FRAMES
        );
        assert_eq!(session.writer.drain_frames, RECORDING_DRAIN_FRAMES);
        assert_eq!(
            session.writer.journal.max_chunk_frames,
            RECORDING_MAXIMUM_CHUNK_FRAMES
        );
        assert_eq!(
            session.writer.journal.max_total_frames,
            RECORDING_MAXIMUM_FRAMES
        );
        assert_eq!(
            session.writer.journal.sample_rate,
            super::super::SAMPLE_RATE_HZ
        );
        assert_eq!(session.completion_timeout, RECORDING_COMPLETION_TIMEOUT);
    }

    #[test]
    fn only_indeterminate_publication_kinds_are_uncertain() {
        for kind in [
            RuntimeRecordingUpdateErrorKind::Io,
            RuntimeRecordingUpdateErrorKind::JsonInvalid,
            RuntimeRecordingUpdateErrorKind::NonCanonicalJson,
            RuntimeRecordingUpdateErrorKind::InvalidUpdate,
            RuntimeRecordingUpdateErrorKind::LimitExceeded,
            RuntimeRecordingUpdateErrorKind::OutputDirectoryInvalid,
            RuntimeRecordingUpdateErrorKind::PublicationConflict,
        ] {
            assert!(!publication_outcome_is_uncertain(kind), "{kind:?}");
        }
        for kind in [
            RuntimeRecordingUpdateErrorKind::AuthorityChanged,
            RuntimeRecordingUpdateErrorKind::PublicationUncertain,
        ] {
            assert!(publication_outcome_is_uncertain(kind), "{kind:?}");
        }
    }
}
