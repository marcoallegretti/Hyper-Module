//! Runtime-side ownership of one explicit mixer-state handback.
//!
//! The controller stays with the sole Runtime playback owner. It waits for a
//! mixer fence and reads one coherent recovery snapshot before publishing a
//! complete, provenance-bound update to an explicitly configured directory.
//! It never changes the verified deployment or writes a project package.

use core::error::Error;
use core::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper_desktop_audio::{DesktopDirectControlError, DesktopPlaybackSession};
use hyper_mixer_runtime::{
    MixerBaseStateProjectionError, MixerParameterStateSnapshot, PlaybackRecoverySnapshot,
    mixer_base_state_from_snapshot,
};
use hyper_playback_json::{
    CompiledMixerGraph, LoaderLimits, MixerStateSerializationError, serialize_mixer_base_state,
};
use hyper_project_loader::PreparedProjectPlayback;
use hyper_project_model::ResourceRole;
use hyper_runtime_mixer_update::{
    RuntimeMixerUpdate, RuntimeMixerUpdateDirectory, RuntimeMixerUpdateError,
    RuntimeMixerUpdateSource,
};
use hyper_storage_engine::StorageError;
use uuid::Uuid;

/// One coherent mixer observation used to complete an explicit export.
///
/// The production adapter constructs this directly from a single recovery
/// snapshot. It deliberately keeps the fence and audible parameter state in
/// one value so a caller cannot accidentally acknowledge one observation and
/// serialize another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MixerRecovery {
    committed_mixer_fence: u64,
    mixer_parameter_state: Option<MixerParameterStateSnapshot>,
}

impl From<PlaybackRecoverySnapshot> for MixerRecovery {
    fn from(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            committed_mixer_fence: snapshot.committed_mixer_fence(),
            mixer_parameter_state: snapshot.mixer_parameter_state(),
        }
    }
}

/// Minimal Runtime session authority needed for an explicit mixer handback.
///
/// The production adapter keeps `DesktopPlaybackSession` as the only audio
/// owner. The narrow interface makes the fence/recovery boundary testable
/// without a native device.
pub(super) trait RuntimeMixerUpdateSession {
    fn mixer_recovery(&self) -> MixerRecovery;

    fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError>;
}

impl RuntimeMixerUpdateSession for DesktopPlaybackSession {
    fn mixer_recovery(&self) -> MixerRecovery {
        self.recovery_checkpoint().snapshot().into()
    }

    fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.try_send_mixer_fence()
    }
}

/// Immutable Runtime export facts prepared before native output starts.
pub(super) struct RuntimeMixerUpdateConfiguration {
    source: RuntimeMixerUpdateSource,
    mixer_id: String,
    source_mixer: Vec<u8>,
    mixer: CompiledMixerGraph,
    output_directory: Option<RuntimeMixerUpdateDirectory>,
}

impl RuntimeMixerUpdateConfiguration {
    /// Builds one export configuration from a structurally verified project.
    ///
    /// The retained source bytes and compiled graph originate from the same
    /// verified package. The controller later serializes only live base
    /// parameters onto this exact topology; routing and all other document
    /// fields remain untouched.
    pub(super) fn try_new(
        source: RuntimeMixerUpdateSource,
        project: &PreparedProjectPlayback,
        output_directory: Option<RuntimeMixerUpdateDirectory>,
    ) -> Result<Self, RuntimeMixerUpdateExportError> {
        let package = project.package();
        let mut mixers = package
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Mixer);
        let descriptor = mixers
            .next()
            .ok_or(RuntimeMixerUpdateExportError::MixerResourceMissing)?;
        if mixers.next().is_some() {
            return Err(RuntimeMixerUpdateExportError::MixerResourceAmbiguous);
        }
        let mixer_id = try_owned_mixer_identity(descriptor.id())?;
        let source_mixer = package
            .read_resource(descriptor.id())
            .map_err(RuntimeMixerUpdateExportError::SourceMixer)?;
        let (_, mixer, _, _) = project.session().parts();
        if mixer.project_id() != source.project_id() {
            return Err(RuntimeMixerUpdateExportError::ProjectCatalogUnavailable);
        }
        Ok(Self {
            source,
            mixer_id,
            source_mixer,
            mixer: mixer.clone(),
            output_directory,
        })
    }

    /// Attaches the immutable facts to the Runtime's sole playback owner.
    #[must_use]
    pub(super) fn attach(self) -> RuntimeMixerUpdateController {
        let state = if self.output_directory.is_some() {
            "Waiting for committed Runtime mixer recovery".to_owned()
        } else {
            "Mixer update unavailable: no external mixer-update directory".to_owned()
        };
        RuntimeMixerUpdateController {
            source: self.source,
            mixer_id: self.mixer_id,
            source_mixer: self.source_mixer,
            mixer: self.mixer,
            output_directory: self.output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeMixerUpdateObservation {
                state,
                export_enabled: false,
            })),
        }
    }
}

/// A classified refusal or terminal error while exporting a mixer update.
#[derive(Debug)]
pub(super) enum RuntimeMixerUpdateExportError {
    OutputDirectoryNotConfigured,
    ExportPending,
    MixerFenceUnavailable,
    MixerStateUnavailable,
    MixerResourceMissing,
    MixerResourceAmbiguous,
    ProjectCatalogUnavailable,
    CapacityUnavailable,
    SourceMixer(StorageError),
    Transport(DesktopDirectControlError),
    Projection(MixerBaseStateProjectionError),
    Serialization(MixerStateSerializationError),
    Update(RuntimeMixerUpdateError),
}

impl fmt::Display for RuntimeMixerUpdateExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external mixer-update output directory",
            ),
            Self::ExportPending => {
                formatter.write_str("a Runtime mixer-update export is already awaiting a fence")
            }
            Self::MixerFenceUnavailable => {
                formatter.write_str("Runtime cannot allocate a mixer fence for this export")
            }
            Self::MixerStateUnavailable => {
                formatter.write_str("committed Runtime mixer state is unavailable")
            }
            Self::MixerResourceMissing => {
                formatter.write_str("verified Runtime project has no mixer resource")
            }
            Self::MixerResourceAmbiguous => {
                formatter.write_str("verified Runtime project has multiple mixer resources")
            }
            Self::ProjectCatalogUnavailable => {
                formatter.write_str("verified Runtime project has inconsistent mixer export facts")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime mixer-update export capacity is unavailable")
            }
            Self::SourceMixer(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
            Self::Projection(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeMixerUpdateExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceMixer(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::ExportPending
            | Self::MixerFenceUnavailable
            | Self::MixerStateUnavailable
            | Self::MixerResourceMissing
            | Self::MixerResourceAmbiguous
            | Self::ProjectCatalogUnavailable
            | Self::CapacityUnavailable => None,
        }
    }
}

fn try_owned_mixer_identity(value: &str) -> Result<String, RuntimeMixerUpdateExportError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RuntimeMixerUpdateExportError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingRuntimeMixerUpdate {
    fence_sequence: u64,
}

/// Runtime-owned presentation state for the explicit operator control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeMixerUpdateObservation {
    pub(super) state: String,
    pub(super) export_enabled: bool,
}

/// One durable mixer update visible to the IDE's explicit import action.
#[derive(Debug)]
pub(super) struct RuntimeMixerUpdateExportSuccess {
    pub(super) path: PathBuf,
}

/// Terminal result reported by one Runtime owner turn.
#[derive(Debug)]
pub(super) enum RuntimeMixerUpdatePoll {
    Saved(RuntimeMixerUpdateExportSuccess),
    NoChanges,
    Failed(RuntimeMixerUpdateExportError),
}

/// Runtime-side controller for one-at-a-time explicit mixer exports.
pub(super) struct RuntimeMixerUpdateController {
    source: RuntimeMixerUpdateSource,
    mixer_id: String,
    source_mixer: Vec<u8>,
    mixer: CompiledMixerGraph,
    output_directory: Option<RuntimeMixerUpdateDirectory>,
    pending: Option<PendingRuntimeMixerUpdate>,
    last_status: Option<String>,
    observation: Arc<Mutex<RuntimeMixerUpdateObservation>>,
}

impl RuntimeMixerUpdateController {
    /// Returns the presentation snapshot observed by the operator UI.
    #[must_use]
    pub(super) fn observation(&self) -> Arc<Mutex<RuntimeMixerUpdateObservation>> {
        Arc::clone(&self.observation)
    }

    /// Queues one explicit boundary after all earlier accepted mixer commands.
    ///
    /// It neither reads state nor creates a file. Publication is deferred until
    /// a later owner turn observes the matching coherent recovery snapshot.
    pub(super) fn request_export<S: RuntimeMixerUpdateSession>(
        &mut self,
        session: &mut S,
    ) -> Result<u64, RuntimeMixerUpdateExportError> {
        if self.pending.is_some() {
            return Err(RuntimeMixerUpdateExportError::ExportPending);
        }
        if self.output_directory.is_none() {
            return Err(RuntimeMixerUpdateExportError::OutputDirectoryNotConfigured);
        }
        let fence_sequence = session
            .try_send_mixer_fence()
            .map_err(RuntimeMixerUpdateExportError::Transport)?
            .ok_or(RuntimeMixerUpdateExportError::MixerFenceUnavailable)?;
        self.pending = Some(PendingRuntimeMixerUpdate { fence_sequence });
        self.last_status = None;
        self.refresh_observation(session);
        Ok(fence_sequence)
    }

    /// Finalizes the pending export only after one coherent fence/state view.
    pub(super) fn poll<S: RuntimeMixerUpdateSession>(
        &mut self,
        session: &S,
    ) -> Option<RuntimeMixerUpdatePoll> {
        let snapshot = session.mixer_recovery();
        let Some(pending) = self.pending else {
            self.refresh_observation_from_snapshot(&snapshot);
            return None;
        };
        if snapshot.committed_mixer_fence < pending.fence_sequence {
            self.refresh_observation_from_snapshot(&snapshot);
            return None;
        }

        self.pending = None;
        let poll = match self.finish(&snapshot) {
            Ok(Some(saved)) => {
                self.last_status = Some("Mixer update published".to_owned());
                RuntimeMixerUpdatePoll::Saved(saved)
            }
            Ok(None) => {
                self.last_status = Some("No mixer changes to export".to_owned());
                RuntimeMixerUpdatePoll::NoChanges
            }
            Err(error) => {
                self.last_status = Some(format!("Mixer update failed: {error}"));
                RuntimeMixerUpdatePoll::Failed(error)
            }
        };
        self.refresh_observation_from_snapshot(&snapshot);
        Some(poll)
    }

    fn finish(
        &self,
        snapshot: &MixerRecovery,
    ) -> Result<Option<RuntimeMixerUpdateExportSuccess>, RuntimeMixerUpdateExportError> {
        let live = snapshot
            .mixer_parameter_state
            .ok_or(RuntimeMixerUpdateExportError::MixerStateUnavailable)?;
        let state = mixer_base_state_from_snapshot(&self.mixer, &live)
            .map_err(RuntimeMixerUpdateExportError::Projection)?;
        let candidate = serialize_mixer_base_state(
            &self.source_mixer,
            self.source.project_id(),
            &self.mixer_id,
            &state,
            LoaderLimits::VERSION_1,
        )
        .map_err(RuntimeMixerUpdateExportError::Serialization)?;
        if candidate == self.source_mixer {
            return Ok(None);
        }
        let update = RuntimeMixerUpdate::try_new(
            Uuid::new_v4().to_string(),
            self.source.clone(),
            self.mixer_id.clone(),
            state,
        )
        .map_err(RuntimeMixerUpdateExportError::Update)?;
        let output_directory = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeMixerUpdateExportError::OutputDirectoryNotConfigured)?;
        let path = output_directory
            .publish(&update)
            .map_err(RuntimeMixerUpdateExportError::Update)?;
        Ok(Some(RuntimeMixerUpdateExportSuccess { path }))
    }

    fn refresh_observation<S: RuntimeMixerUpdateSession>(&self, session: &S) {
        self.refresh_observation_from_snapshot(&session.mixer_recovery());
    }

    fn refresh_observation_from_snapshot(&self, snapshot: &MixerRecovery) {
        let (state, export_enabled) = if let Some(pending) = self.pending {
            (
                format!(
                    "Mixer update queued at fence {}; awaiting a committed audio cycle",
                    pending.fence_sequence
                ),
                false,
            )
        } else if self.output_directory.is_none() {
            (
                "Mixer update unavailable: no external mixer-update directory".to_owned(),
                false,
            )
        } else if snapshot.mixer_parameter_state.is_none() {
            (
                "Mixer update unavailable: committed mixer state is unavailable".to_owned(),
                false,
            )
        } else {
            (
                self.last_status
                    .clone()
                    .unwrap_or_else(|| "Ready to export the committed mixer state".to_owned()),
                true,
            )
        };
        if let Ok(mut observation) = self.observation.lock() {
            *observation = RuntimeMixerUpdateObservation {
                state,
                export_enabled,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
    use hyper_mixer_runtime::{MixerChannelParameterState, MixerParameterStateSnapshot};
    use hyper_playback_json::{MediaAssetFact, PlaybackJsonInput, load};
    use hyper_runtime_mixer_update::read_runtime_mixer_update;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    const TIMELINE: &[u8] = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334458",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":500000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
    }"#;
    const TRACK: &[u8] = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334457",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","name":"Source","kind":"audio",
        "order":0,"active":true,"clip_ids":[],"extensions":{}
    }"#;
    const MIXER: &[u8] = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122334456",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122334455","channels":[{
        "track_id":"018f47a2-6b2d-7c11-8a44-001122334457","gain_millionths":1000000,
        "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}],
        "return_buses":[],"main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},"extensions":{}
    }"#;

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hyper-runtime-mixer-update-{}", Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn source() -> RuntimeMixerUpdateSource {
        RuntimeMixerUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn canonical_mixer() -> Vec<u8> {
        let value: serde_json::Value = serde_json::from_slice(MIXER).unwrap();
        serde_json_canonicalizer::to_vec(&value).unwrap()
    }

    fn compiled_mixer(source_mixer: &[u8]) -> CompiledMixerGraph {
        let tracks = [TRACK];
        let project = load(
            PlaybackJsonInput {
                project_id: PROJECT_ID,
                sample_rate_hz: 48_000,
                timeline: TIMELINE,
                tracks: &tracks,
                clips: &[],
                scenes: &[],
                arrangement: None,
            },
            LoaderLimits::VERSION_1,
        )
        .unwrap();
        let facts: [MediaAssetFact<'_>; 0] = [];
        project
            .load_mixer(source_mixer, &facts, LoaderLimits::VERSION_1)
            .unwrap()
            .compile_main_graph()
            .unwrap()
    }

    fn snapshot(channel_gain_millionths: u32) -> MixerParameterStateSnapshot {
        let channel = MixerChannelParameterState::for_test(
            ChannelParameters::new(
                LinearGain::from_millionths(channel_gain_millionths).unwrap(),
                Pan::CENTER,
                true,
            ),
            false,
            false,
            &[],
        );
        MixerParameterStateSnapshot::for_test(&[channel], LinearGain::UNITY, false)
    }

    fn controller(
        output_directory: Option<RuntimeMixerUpdateDirectory>,
    ) -> RuntimeMixerUpdateController {
        let source_mixer = canonical_mixer();
        RuntimeMixerUpdateController {
            source: source(),
            mixer_id: MIXER_ID.to_owned(),
            mixer: compiled_mixer(&source_mixer),
            source_mixer,
            output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeMixerUpdateObservation {
                state: "fixture".to_owned(),
                export_enabled: false,
            })),
        }
    }

    #[derive(Clone, Copy)]
    struct FakeSession {
        recovery: MixerRecovery,
        next_fence: Option<u64>,
        fence_request_count: usize,
    }

    impl FakeSession {
        fn with_snapshot(snapshot: &MixerParameterStateSnapshot) -> Self {
            Self {
                recovery: MixerRecovery {
                    committed_mixer_fence: 0,
                    mixer_parameter_state: Some(*snapshot),
                },
                next_fence: Some(7),
                fence_request_count: 0,
            }
        }
    }

    impl RuntimeMixerUpdateSession for FakeSession {
        fn mixer_recovery(&self) -> MixerRecovery {
            self.recovery
        }

        fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
            self.fence_request_count = self.fence_request_count.saturating_add(1);
            Ok(self.next_fence)
        }
    }

    #[test]
    fn export_waits_for_one_coherent_fence_then_publishes_one_complete_update() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeMixerUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output));
        let mut session = FakeSession::with_snapshot(&snapshot(750_000));

        assert_eq!(controller.request_export(&mut session).unwrap(), 7);
        assert_eq!(session.fence_request_count, 1);
        assert!(controller.poll(&session).is_none());
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert!(!observation.export_enabled);
        assert!(
            observation
                .state
                .contains("awaiting a committed audio cycle")
        );
        drop(observation);

        session.recovery.committed_mixer_fence = 7;
        let RuntimeMixerUpdatePoll::Saved(saved) = controller.poll(&session).unwrap() else {
            panic!("a coherent changed snapshot must publish exactly one update");
        };
        assert!(saved.path.starts_with(temporary.path()));
        let parsed = read_runtime_mixer_update(&saved.path).unwrap();
        assert_eq!(parsed.source(), &source());
        assert_eq!(parsed.mixer_id(), MIXER_ID);
        assert_eq!(parsed.state().channels[0].track_id, TRACK_ID);
        assert_eq!(
            parsed.state().channels[0].gain,
            LinearGain::from_millionths(750_000).unwrap()
        );
        assert!(controller.poll(&session).is_none());
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn unchanged_state_never_creates_an_artifact() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeMixerUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output));
        let mut session = FakeSession::with_snapshot(&snapshot(1_000_000));

        controller.request_export(&mut session).unwrap();
        session.recovery.committed_mixer_fence = 7;
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeMixerUpdatePoll::NoChanges)
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert_eq!(observation.state, "No mixer changes to export");
        assert!(observation.export_enabled);
    }

    #[test]
    fn unavailable_output_or_coherent_state_refuses_without_publication() {
        let mut disabled = controller(None);
        let mut disabled_session = FakeSession::with_snapshot(&snapshot(1_000_000));
        assert!(matches!(
            disabled.request_export(&mut disabled_session),
            Err(RuntimeMixerUpdateExportError::OutputDirectoryNotConfigured)
        ));
        assert_eq!(disabled_session.fence_request_count, 0);

        let temporary = TemporaryDirectory::new();
        let output = RuntimeMixerUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output));
        let mut session = FakeSession::with_snapshot(&snapshot(1_000_000));
        session.recovery.mixer_parameter_state = None;
        controller.request_export(&mut session).unwrap();
        session.recovery.committed_mixer_fence = 7;
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeMixerUpdatePoll::Failed(
                RuntimeMixerUpdateExportError::MixerStateUnavailable
            ))
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());

        session.recovery.mixer_parameter_state = Some(MixerParameterStateSnapshot::for_test(
            &[],
            LinearGain::UNITY,
            false,
        ));
        controller.request_export(&mut session).unwrap();
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeMixerUpdatePoll::Failed(
                RuntimeMixerUpdateExportError::Projection(
                    MixerBaseStateProjectionError::LayoutMismatch
                )
            ))
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }
}
