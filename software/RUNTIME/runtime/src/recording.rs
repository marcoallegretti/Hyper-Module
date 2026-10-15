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
const RECORDING_MAXIMUM_FRAMES: u64 = super::MAX_AUDIO_FRAMES;
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
    available_inputs: u8,
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
            available_inputs: manifest.required_capabilities().audio_inputs(),
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

    pub(super) const fn stereo_input_available(&self) -> bool {
        self.available_inputs >= 2
    }

    pub(super) fn validate_source_inputs(&self) -> Result<(), RuntimeRecordingError> {
        if self.stereo_input_available() {
            Ok(())
        } else {
            Err(RuntimeRecordingError::SourceInputUnavailable {
                available_inputs: self.available_inputs,
            })
        }
    }

    pub(super) fn validate_target_index(
        &self,
        target_index: usize,
    ) -> Result<(), RuntimeRecordingError> {
        self.targets
            .get(target_index)
            .map(|_| ())
            .ok_or(RuntimeRecordingError::TargetUnavailable)
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
        self.validate_source_inputs()?;
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
    SourceInputUnavailable { available_inputs: u8 },
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
            Self::SourceInputUnavailable { available_inputs } => write!(
                formatter,
                "stereo recording requires two declared project audio inputs; project declares {available_inputs}"
            ),
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
            | Self::SourceInputUnavailable { .. }
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
        RECORDING_TRANSPORT_FRAMES, RuntimeRecordingPaths, RuntimeRecordingPublication,
        publication_outcome_is_uncertain, recording_session_config,
    };
    use hyper_desktop_audio::DesktopInputConfig;
    use hyper_recording_journal::{JournalConfig, JournalWriter, publish_wav};
    use hyper_recording_metadata::{
        FinalizedRecordingMetadata, RecordingCaptureKind, RecordingMetadataContext,
    };
    use hyper_runtime_recording_update::{
        RuntimeRecordingUpdateDirectory, RuntimeRecordingUpdateErrorKind,
        RuntimeRecordingUpdateSource, read_runtime_recording_update, sha256_hex,
    };
    use hyper_storage_engine::{ProjectCreationRequest, create_project};
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    struct TemporaryProjectParent {
        path: PathBuf,
    }

    impl TemporaryProjectParent {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-recording-policy-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TemporaryProjectParent {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn assert_duplicate_publication_refused(
        configuration: &super::RuntimeRecordingConfiguration,
        update_id: String,
        finalized: &FinalizedRecordingMetadata,
        update_path: &Path,
        output_path: &Path,
    ) {
        let published_bytes = fs::read(update_path).unwrap();
        let duplicate = match configuration.publish(update_id, finalized) {
            Err(super::RuntimeRecordingError::Update(duplicate)) => duplicate,
            Err(error) => panic!("duplicate Runtime update was misclassified: {error}"),
            Ok(_) => panic!("a duplicate Runtime update identity must not be published"),
        };
        assert_eq!(
            duplicate.kind(),
            RuntimeRecordingUpdateErrorKind::PublicationConflict
        );
        assert_eq!(fs::read(update_path).unwrap(), published_bytes);
        assert!(fs::read_dir(output_path).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".pending")
        }));
    }

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
            super::super::MAX_AUDIO_FRAMES
        );
        assert_eq!(
            session.writer.journal.sample_rate,
            super::super::SAMPLE_RATE_HZ
        );
        assert_eq!(session.completion_timeout, RECORDING_COMPLETION_TIMEOUT);
    }

    #[test]
    fn configuration_catalogs_the_verified_audio_target_without_mutating_the_project() {
        let temporary = TemporaryProjectParent::new();
        let creation = create_project(
            &temporary.path,
            ProjectCreationRequest {
                directory_name: "Runtime Capture",
                project_name: "Runtime Capture",
            },
        )
        .unwrap();
        let prepared = hyper_project_loader::prepare_project_playback(
            creation.project().root(),
            super::super::limits().unwrap(),
        )
        .unwrap();
        let manifest = prepared.package().manifest();
        let source = RuntimeRecordingUpdateSource::try_new(
            manifest.project_id().to_owned(),
            manifest.revision(),
            sha256_hex(prepared.package().manifest_sha256()),
            "a".repeat(64),
            7,
        )
        .unwrap();

        let configuration =
            super::RuntimeRecordingConfiguration::try_new(source, &prepared, None).unwrap();

        assert_eq!(configuration.target_labels().unwrap(), ["Audio 1"]);
        assert!(configuration.has_targets());
        assert!(configuration.stereo_input_available());
        assert!(configuration.validate_source_inputs().is_ok());
        assert!(!configuration.output_configured());
        assert!(configuration.validate_target_index(0).is_ok());
        assert!(matches!(
            configuration.validate_target_index(1),
            Err(super::RuntimeRecordingError::TargetUnavailable)
        ));
        assert!(matches!(
            configuration.validate_target_index(usize::MAX),
            Err(super::RuntimeRecordingError::TargetUnavailable)
        ));
        prepared.package().confirm_unchanged().unwrap();
    }

    #[test]
    fn stereo_recording_refuses_projects_with_fewer_than_two_declared_inputs() {
        for available_inputs in [0_u8, 1] {
            let temporary = TemporaryProjectParent::new();
            let creation = create_project(
                &temporary.path,
                ProjectCreationRequest {
                    directory_name: "Runtime Capture",
                    project_name: "Runtime Capture",
                },
            )
            .unwrap();
            let project_root = creation.project().root().to_path_buf();
            drop(creation);
            let manifest_path = project_root.join("project.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
            manifest["required_capabilities"]["audio_inputs"] = serde_json::json!(available_inputs);
            fs::write(
                &manifest_path,
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let prepared = hyper_project_loader::prepare_project_playback(
                &project_root,
                super::super::limits().unwrap(),
            )
            .unwrap();
            let manifest = prepared.package().manifest();
            let source = RuntimeRecordingUpdateSource::try_new(
                manifest.project_id().to_owned(),
                manifest.revision(),
                sha256_hex(prepared.package().manifest_sha256()),
                "a".repeat(64),
                7,
            )
            .unwrap();
            let output_path = temporary.path.join("recording-updates");
            fs::create_dir(&output_path).unwrap();
            let output = RuntimeRecordingUpdateDirectory::open(&output_path).unwrap();
            let configuration =
                super::RuntimeRecordingConfiguration::try_new(source, &prepared, Some(output))
                    .unwrap();

            assert!(configuration.has_targets());
            assert!(configuration.output_configured());
            assert!(!configuration.stereo_input_available());
            assert!(matches!(
                configuration.validate_source_inputs(),
                Err(super::RuntimeRecordingError::SourceInputUnavailable {
                    available_inputs: declared
                }) if declared == available_inputs
            ));
            assert!(
                super::super::initial_runtime_recording_state(&configuration, true)
                    .contains("stereo recording requires two declared project audio inputs")
            );
            prepared.package().confirm_unchanged().unwrap();
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    #[ignore = "requires a live PipeWire daemon with the hyper-ci-sink and hyper-ci-source virtual nodes"]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear live owner transition keeps verified project, native graph, and publication evidence together"
    )]
    fn live_runtime_owner_records_publishes_and_restores_verified_playback() {
        use hyper_desktop_audio::{
            DesktopMonitoringConfig, DesktopOutputConfig, DesktopPlaybackSession,
            discover_desktop_input_targets, discover_desktop_output_targets,
        };
        use hyper_runtime_recording_update::read_runtime_recording_update;
        use std::thread;
        use std::time::{Duration, Instant};

        let temporary = TemporaryProjectParent::new();
        let creation = create_project(
            &temporary.path,
            ProjectCreationRequest {
                directory_name: "Runtime Capture",
                project_name: "Runtime Capture",
            },
        )
        .unwrap();
        let prepared = hyper_project_loader::prepare_project_playback(
            creation.project().root(),
            super::super::limits().unwrap(),
        )
        .unwrap();
        let manifest = prepared.package().manifest();
        let project_id = manifest.project_id().to_owned();
        let source = RuntimeRecordingUpdateSource::try_new(
            project_id.clone(),
            manifest.revision(),
            sha256_hex(prepared.package().manifest_sha256()),
            "a".repeat(64),
            7,
        )
        .unwrap();
        let output_path = temporary.path.join("recording-updates");
        fs::create_dir(&output_path).unwrap();
        let output = RuntimeRecordingUpdateDirectory::open(&output_path).unwrap();
        let recording =
            super::RuntimeRecordingConfiguration::try_new(source.clone(), &prepared, Some(output))
                .unwrap();
        let track_id = recording.targets[0].track_id.clone();

        let outputs = discover_desktop_output_targets().unwrap();
        let output_target = outputs
            .targets()
            .iter()
            .find(|target| target.display_name() == "Hyper CI Sink")
            .expect("CI null sink was not discovered");
        let inputs = discover_desktop_input_targets().unwrap();
        let input_target = inputs
            .targets()
            .iter()
            .find(|target| target.display_name() == "Hyper CI Source")
            .expect("CI virtual source was not discovered");
        let output_config = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_output_target(output_target)
            .unwrap();
        let input_config = DesktopInputConfig::try_new(48_000, 256)
            .unwrap()
            .with_input_target(input_target)
            .unwrap();
        let monitoring =
            DesktopMonitoringConfig::try_new(input_config, output_config, 2_048, 256, 64).unwrap();
        let playback = DesktopPlaybackSession::start_project_monitored_with_monitor_state(
            monitoring, prepared, 1.0, true,
        )
        .unwrap();
        let mut owner = super::super::RuntimePlaybackOwner::new(playback, recording, true);
        assert!(owner.recording_observation().lock().unwrap().start_enabled);

        let started = owner.start_recording(0).unwrap();
        assert!(!started.terminal_output_failure, "{:?}", started.message);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !owner.is_recording() && Instant::now() < deadline {
            let poll = owner.poll();
            assert!(!poll.terminal_output_failure, "{:?}", poll.message);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            owner.is_recording(),
            "Runtime did not acquire its recording owner"
        );
        assert!(owner.recording_observation().lock().unwrap().stop_enabled);
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(
            owner.state.as_ref(),
            Some(super::super::RuntimePlaybackOwnerState::Recording { session, .. })
                if session.recording_snapshot().accepted_frame_count == 0
        ) && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        let Some(super::super::RuntimePlaybackOwnerState::Recording { session, .. }) =
            owner.state.as_ref()
        else {
            panic!("Runtime recording owner was lost before stop");
        };
        assert!(session.recording_snapshot().accepted_frame_count > 0);

        let stopped = owner.stop_recording().unwrap();
        assert!(!stopped.terminal_output_failure, "{:?}", stopped.message);
        assert!(
            stopped.message.as_deref().is_some_and(
                |message| message.starts_with("Runtime recording for Audio 1 published to ")
            ),
            "{:?}",
            stopped.message
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !matches!(
            owner.state.as_ref(),
            Some(super::super::RuntimePlaybackOwnerState::Playback(_))
        ) && Instant::now() < deadline
        {
            let poll = owner.poll();
            assert!(!poll.terminal_output_failure, "{:?}", poll.message);
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            owner.state.as_ref(),
            Some(super::super::RuntimePlaybackOwnerState::Playback(_))
        ));
        assert!(owner.recording_observation().lock().unwrap().start_enabled);

        let updates: Vec<_> = fs::read_dir(&output_path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        assert_eq!(updates.len(), 1, "{:?}", stopped.message);
        let update = read_runtime_recording_update(&updates[0]).unwrap();
        assert_eq!(update.source(), &source);
        assert_eq!(update.handoff().metadata().project_id(), project_id);
        assert_eq!(update.handoff().metadata().track_id(), track_id);
        assert!(
            fs::metadata(update.handoff().recording_path())
                .unwrap()
                .len()
                > 44
        );
        let Some(super::super::RuntimePlaybackOwnerState::Playback(playback)) = owner.state.take()
        else {
            panic!("restored playback owner was unavailable for clean native shutdown");
        };
        (*playback).stop().unwrap();
    }

    #[test]
    fn configuration_rejects_source_facts_from_another_project_revision() {
        let temporary = TemporaryProjectParent::new();
        let creation = create_project(
            &temporary.path,
            ProjectCreationRequest {
                directory_name: "Runtime Capture",
                project_name: "Runtime Capture",
            },
        )
        .unwrap();
        let prepared = hyper_project_loader::prepare_project_playback(
            creation.project().root(),
            super::super::limits().unwrap(),
        )
        .unwrap();
        let manifest = prepared.package().manifest();
        let source = RuntimeRecordingUpdateSource::try_new(
            manifest.project_id().to_owned(),
            manifest.revision() + 1,
            sha256_hex(prepared.package().manifest_sha256()),
            "a".repeat(64),
            7,
        )
        .unwrap();

        assert!(matches!(
            super::RuntimeRecordingConfiguration::try_new(source, &prepared, None),
            Err(super::RuntimeRecordingError::ProjectChanged)
        ));
        prepared.package().confirm_unchanged().unwrap();
    }

    #[test]
    fn finalized_runtime_publication_round_trips_verified_wav_and_project_provenance() {
        let temporary = TemporaryProjectParent::new();
        let creation = create_project(
            &temporary.path,
            ProjectCreationRequest {
                directory_name: "Runtime Capture",
                project_name: "Runtime Capture",
            },
        )
        .unwrap();
        let prepared = hyper_project_loader::prepare_project_playback(
            creation.project().root(),
            super::super::limits().unwrap(),
        )
        .unwrap();
        let manifest = prepared.package().manifest();
        let source = RuntimeRecordingUpdateSource::try_new(
            manifest.project_id().to_owned(),
            manifest.revision(),
            sha256_hex(prepared.package().manifest_sha256()),
            "a".repeat(64),
            7,
        )
        .unwrap();
        let output_path = temporary.path.join("recording-updates");
        fs::create_dir(&output_path).unwrap();
        let output = RuntimeRecordingUpdateDirectory::open(&output_path).unwrap();
        let configuration =
            super::RuntimeRecordingConfiguration::try_new(source.clone(), &prepared, Some(output))
                .unwrap();

        let journal_path = output_path.join("verified-take.journal");
        let wav_path = output_path.join("verified-take.wav");
        let mut writer = JournalWriter::create(
            &journal_path,
            JournalConfig {
                sample_rate: super::super::SAMPLE_RATE_HZ,
                max_chunk_frames: 3,
                max_total_frames: 3,
            },
        )
        .unwrap();
        writer
            .append(&[0.25, -0.25, 0.5, -0.5, 0.75, -0.75])
            .unwrap();
        drop(writer);
        let publication = publish_wav(&journal_path, &wav_path).unwrap();
        let publication_sha256 = *publication.wav_sha256();
        let publication_bytes = publication.byte_length();
        let recording_id = Uuid::new_v4().hyphenated().to_string();
        let track_id = configuration.targets[0].track_id.clone();
        let finalized = FinalizedRecordingMetadata::try_new(
            publication,
            RecordingMetadataContext {
                id: recording_id.clone(),
                project_id: source.project_id().to_owned(),
                asset_id: Uuid::new_v4().hyphenated().to_string(),
                track_id: track_id.clone(),
                capture_kind: RecordingCaptureKind::New,
                captured_at_utc: "2026-09-21T12:00:00.000Z".to_owned(),
                timeline_start_tick: 0,
                source_input_indices: vec![0, 1],
            },
        )
        .unwrap();
        let update_id = Uuid::new_v4().hyphenated().to_string();

        let RuntimeRecordingPublication::Published(update_path) = configuration
            .publish(update_id.clone(), &finalized)
            .unwrap()
        else {
            panic!("a stable local directory must publish a determinate update");
        };
        let update = read_runtime_recording_update(&update_path).unwrap();

        assert_eq!(update.id(), update_id);
        assert_eq!(update.source(), &source);
        assert_eq!(update.handoff().recording_path(), wav_path);
        assert_eq!(update.handoff().recording_byte_length(), publication_bytes);
        assert_eq!(update.handoff().recording_sha256(), &publication_sha256);
        assert_eq!(update.handoff().metadata().id(), recording_id);
        assert_eq!(
            update.handoff().metadata().project_id(),
            source.project_id()
        );
        assert_eq!(update.handoff().metadata().track_id(), track_id);
        assert_eq!(update.handoff().metadata().frame_count(), 3);

        assert_duplicate_publication_refused(
            &configuration,
            update_id,
            &finalized,
            &update_path,
            &output_path,
        );
        prepared.package().confirm_unchanged().unwrap();
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
