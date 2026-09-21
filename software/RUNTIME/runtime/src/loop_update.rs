//! Runtime-side ownership of one explicit arrangement-loop handback.
//!
//! The controller stays with the sole Runtime playback owner. It waits for a
//! transport fence and reads one coherent recovery snapshot before publishing
//! a complete, provenance-bound update to an explicitly configured directory.
//! It never changes the verified deployment or writes a project package.

use core::error::Error;
use core::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper_desktop_audio::{DesktopDirectControlError, DesktopPlaybackSession};
use hyper_mixer_runtime::{PlaybackRecoverySnapshot, TransportLoopRange};
use hyper_project_loader::PreparedProjectPlayback;
use hyper_runtime_loop_update::{
    RuntimeLoopRange, RuntimeLoopUpdate, RuntimeLoopUpdateDirectory, RuntimeLoopUpdateError,
    RuntimeLoopUpdateSource,
};
use uuid::Uuid;

/// One coherent transport observation used to complete an explicit export.
///
/// The production adapter constructs this directly from one recovery snapshot.
/// It deliberately keeps the fence, range, and enabled state together so a
/// caller cannot acknowledge one transport observation and serialize another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TransportRecovery {
    committed_transport_fence: u64,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
}

impl From<PlaybackRecoverySnapshot> for TransportRecovery {
    fn from(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            committed_transport_fence: snapshot.committed_transport_fence(),
            loop_range: snapshot.loop_range(),
            loop_enabled: snapshot.loop_enabled(),
        }
    }
}

/// Minimal Runtime session authority needed for an explicit loop handback.
///
/// The production adapter keeps [`DesktopPlaybackSession`] as the only audio
/// owner. The narrow interface makes the fence/recovery boundary testable
/// without a native device.
pub(super) trait RuntimeLoopUpdateSession {
    fn transport_recovery(&self) -> TransportRecovery;

    fn try_send_transport_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError>;
}

impl RuntimeLoopUpdateSession for DesktopPlaybackSession {
    fn transport_recovery(&self) -> TransportRecovery {
        self.recovery_checkpoint().snapshot().into()
    }

    fn try_send_transport_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        self.try_send_transport_fence()
    }
}

/// Immutable Runtime export facts prepared before native output starts.
pub(super) struct RuntimeLoopUpdateConfiguration {
    source: RuntimeLoopUpdateSource,
    source_loop_range: Option<RuntimeLoopRange>,
    output_directory: Option<RuntimeLoopUpdateDirectory>,
}

impl RuntimeLoopUpdateConfiguration {
    /// Builds one export configuration from a structurally verified project.
    ///
    /// The source loop is read from the same prepared playback arrangement
    /// that initializes Runtime transport. It remains a comparison baseline;
    /// the controller later exports only a changed committed session state.
    pub(super) fn try_new(
        source: RuntimeLoopUpdateSource,
        project: &PreparedProjectPlayback,
        output_directory: Option<RuntimeLoopUpdateDirectory>,
    ) -> Result<Self, RuntimeLoopUpdateExportError> {
        if project.session().project_id() != source.project_id() {
            return Err(RuntimeLoopUpdateExportError::ProjectCatalogUnavailable);
        }
        let source_loop_range = project
            .session()
            .arrangement()
            .loop_range()
            .map(|range| {
                loop_range_from_frames(range.start_frame(), range.end_frame(), range.enabled())
            })
            .transpose()?;
        Ok(Self {
            source,
            source_loop_range,
            output_directory,
        })
    }

    /// Attaches immutable facts to the Runtime's sole playback owner.
    #[must_use]
    pub(super) fn attach(self) -> RuntimeLoopUpdateController {
        let state = if self.output_directory.is_some() {
            "Waiting for committed Runtime loop recovery".to_owned()
        } else {
            "Loop update unavailable: no external loop-update directory".to_owned()
        };
        RuntimeLoopUpdateController {
            source: self.source,
            source_loop_range: self.source_loop_range,
            output_directory: self.output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeLoopUpdateObservation {
                state,
                export_enabled: false,
            })),
        }
    }
}

/// A classified refusal or terminal error while exporting a loop update.
#[derive(Debug)]
pub(super) enum RuntimeLoopUpdateExportError {
    OutputDirectoryNotConfigured,
    ExportPending,
    TransportFenceUnavailable,
    ProjectCatalogUnavailable,
    FramePositionUnavailable,
    IncoherentLoopState,
    Transport(DesktopDirectControlError),
    Update(RuntimeLoopUpdateError),
}

impl fmt::Display for RuntimeLoopUpdateExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external loop-update output directory",
            ),
            Self::ExportPending => {
                formatter.write_str("a Runtime loop-update export is already awaiting a fence")
            }
            Self::TransportFenceUnavailable => {
                formatter.write_str("Runtime cannot allocate a transport fence for this export")
            }
            Self::ProjectCatalogUnavailable => {
                formatter.write_str("verified Runtime project has inconsistent loop export facts")
            }
            Self::FramePositionUnavailable => {
                formatter.write_str("Runtime loop frame position cannot be represented durably")
            }
            Self::IncoherentLoopState => formatter.write_str(
                "committed Runtime loop is enabled but has no range, so it cannot be persisted",
            ),
            Self::Transport(source) => source.fmt(formatter),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeLoopUpdateExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Transport(source) => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::ExportPending
            | Self::TransportFenceUnavailable
            | Self::ProjectCatalogUnavailable
            | Self::FramePositionUnavailable
            | Self::IncoherentLoopState => None,
        }
    }
}

fn loop_range_from_frames(
    start_frame: usize,
    end_frame: usize,
    enabled: bool,
) -> Result<RuntimeLoopRange, RuntimeLoopUpdateExportError> {
    let start_frame = u64::try_from(start_frame)
        .map_err(|_| RuntimeLoopUpdateExportError::FramePositionUnavailable)?;
    let end_frame = u64::try_from(end_frame)
        .map_err(|_| RuntimeLoopUpdateExportError::FramePositionUnavailable)?;
    RuntimeLoopRange::try_new(start_frame, end_frame, enabled)
        .map_err(RuntimeLoopUpdateExportError::Update)
}

fn loop_state_from_recovery(
    snapshot: &TransportRecovery,
) -> Result<Option<RuntimeLoopRange>, RuntimeLoopUpdateExportError> {
    match (snapshot.loop_range, snapshot.loop_enabled) {
        (None, false) => Ok(None),
        (None, true) => Err(RuntimeLoopUpdateExportError::IncoherentLoopState),
        (Some(range), enabled) => {
            loop_range_from_frames(range.start_frame(), range.end_frame(), enabled).map(Some)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingRuntimeLoopUpdate {
    fence_sequence: u64,
}

/// Runtime-owned presentation state for the explicit operator control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeLoopUpdateObservation {
    pub(super) state: String,
    pub(super) export_enabled: bool,
}

/// One durable loop update visible to the IDE's explicit import action.
#[derive(Debug)]
pub(super) struct RuntimeLoopUpdateExportSuccess {
    pub(super) path: PathBuf,
}

/// Terminal result reported by one Runtime owner turn.
#[derive(Debug)]
pub(super) enum RuntimeLoopUpdatePoll {
    Saved(RuntimeLoopUpdateExportSuccess),
    NoChanges,
    Failed(RuntimeLoopUpdateExportError),
}

/// Runtime-side controller for one-at-a-time explicit loop exports.
pub(super) struct RuntimeLoopUpdateController {
    source: RuntimeLoopUpdateSource,
    source_loop_range: Option<RuntimeLoopRange>,
    output_directory: Option<RuntimeLoopUpdateDirectory>,
    pending: Option<PendingRuntimeLoopUpdate>,
    last_status: Option<String>,
    observation: Arc<Mutex<RuntimeLoopUpdateObservation>>,
}

impl RuntimeLoopUpdateController {
    /// Returns the presentation snapshot observed by the operator UI.
    #[must_use]
    pub(super) fn observation(&self) -> Arc<Mutex<RuntimeLoopUpdateObservation>> {
        Arc::clone(&self.observation)
    }

    /// Returns whether playback-owner replacement can occur without losing a
    /// requested transport fence or its eventual publication outcome.
    pub(super) const fn is_idle(&self) -> bool {
        self.pending.is_none()
    }

    /// Queues one explicit boundary after all earlier accepted transport commands.
    ///
    /// It neither reads state nor creates a file. Publication is deferred until
    /// a later owner turn observes the matching coherent recovery snapshot.
    pub(super) fn request_export<S: RuntimeLoopUpdateSession>(
        &mut self,
        session: &mut S,
    ) -> Result<u64, RuntimeLoopUpdateExportError> {
        if self.pending.is_some() {
            return Err(RuntimeLoopUpdateExportError::ExportPending);
        }
        if self.output_directory.is_none() {
            return Err(RuntimeLoopUpdateExportError::OutputDirectoryNotConfigured);
        }
        let fence_sequence = session
            .try_send_transport_fence()
            .map_err(RuntimeLoopUpdateExportError::Transport)?
            .ok_or(RuntimeLoopUpdateExportError::TransportFenceUnavailable)?;
        self.pending = Some(PendingRuntimeLoopUpdate { fence_sequence });
        self.last_status = None;
        self.refresh_observation(session);
        Ok(fence_sequence)
    }

    /// Finalizes the pending export only after one coherent fence/state view.
    pub(super) fn poll<S: RuntimeLoopUpdateSession>(
        &mut self,
        session: &S,
    ) -> Option<RuntimeLoopUpdatePoll> {
        let snapshot = session.transport_recovery();
        let Some(pending) = self.pending else {
            self.refresh_observation_from_snapshot(&snapshot);
            return None;
        };
        if snapshot.committed_transport_fence < pending.fence_sequence {
            self.refresh_observation_from_snapshot(&snapshot);
            return None;
        }

        self.pending = None;
        let poll = match self.finish(&snapshot) {
            Ok(Some(saved)) => {
                self.last_status = Some("Loop update published".to_owned());
                RuntimeLoopUpdatePoll::Saved(saved)
            }
            Ok(None) => {
                self.last_status = Some("No loop changes to export".to_owned());
                RuntimeLoopUpdatePoll::NoChanges
            }
            Err(error) => {
                self.last_status = Some(format!("Loop update failed: {error}"));
                RuntimeLoopUpdatePoll::Failed(error)
            }
        };
        self.refresh_observation_from_snapshot(&snapshot);
        Some(poll)
    }

    fn finish(
        &self,
        snapshot: &TransportRecovery,
    ) -> Result<Option<RuntimeLoopUpdateExportSuccess>, RuntimeLoopUpdateExportError> {
        let loop_range = loop_state_from_recovery(snapshot)?;
        if loop_range == self.source_loop_range {
            return Ok(None);
        }
        let update =
            RuntimeLoopUpdate::try_new(Uuid::new_v4().to_string(), self.source.clone(), loop_range)
                .map_err(RuntimeLoopUpdateExportError::Update)?;
        let output_directory = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeLoopUpdateExportError::OutputDirectoryNotConfigured)?;
        let path = output_directory
            .publish(&update)
            .map_err(RuntimeLoopUpdateExportError::Update)?;
        Ok(Some(RuntimeLoopUpdateExportSuccess { path }))
    }

    fn refresh_observation<S: RuntimeLoopUpdateSession>(&self, session: &S) {
        self.refresh_observation_from_snapshot(&session.transport_recovery());
    }

    fn refresh_observation_from_snapshot(&self, snapshot: &TransportRecovery) {
        let live_state = loop_state_from_recovery(snapshot);
        let (state, export_enabled) = if let Some(pending) = self.pending {
            (
                format!(
                    "Loop update queued at transport fence {}; awaiting a committed audio cycle",
                    pending.fence_sequence
                ),
                false,
            )
        } else if self.output_directory.is_none() {
            (
                "Loop update unavailable: no external loop-update directory".to_owned(),
                false,
            )
        } else if let Err(error) = live_state {
            (format!("Loop update unavailable: {error}"), false)
        } else {
            (
                self.last_status
                    .clone()
                    .unwrap_or_else(|| "Ready to export the committed loop state".to_owned()),
                true,
            )
        };
        if let Ok(mut observation) = self.observation.lock() {
            *observation = RuntimeLoopUpdateObservation {
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

    use hyper_runtime_loop_update::read_runtime_loop_update;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("hyper-runtime-loop-update-{}", Uuid::new_v4()));
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

    fn source() -> RuntimeLoopUpdateSource {
        RuntimeLoopUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn range(start_frame: usize, end_frame: usize) -> TransportLoopRange {
        TransportLoopRange::new(start_frame, end_frame).unwrap()
    }

    fn persisted_range(start_frame: usize, end_frame: usize, enabled: bool) -> RuntimeLoopRange {
        loop_range_from_frames(start_frame, end_frame, enabled).unwrap()
    }

    fn controller(
        output_directory: Option<RuntimeLoopUpdateDirectory>,
        source_loop_range: Option<RuntimeLoopRange>,
    ) -> RuntimeLoopUpdateController {
        RuntimeLoopUpdateController {
            source: source(),
            source_loop_range,
            output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeLoopUpdateObservation {
                state: "fixture".to_owned(),
                export_enabled: false,
            })),
        }
    }

    #[derive(Clone, Copy)]
    struct FakeSession {
        recovery: TransportRecovery,
        next_fence: Option<u64>,
        fence_request_count: usize,
    }

    impl FakeSession {
        fn with_loop(range: Option<TransportLoopRange>, enabled: bool) -> Self {
            Self {
                recovery: TransportRecovery {
                    committed_transport_fence: 0,
                    loop_range: range,
                    loop_enabled: enabled,
                },
                next_fence: Some(7),
                fence_request_count: 0,
            }
        }
    }

    impl RuntimeLoopUpdateSession for FakeSession {
        fn transport_recovery(&self) -> TransportRecovery {
            self.recovery
        }

        fn try_send_transport_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
            self.fence_request_count = self.fence_request_count.saturating_add(1);
            Ok(self.next_fence)
        }
    }

    #[test]
    fn export_waits_for_one_coherent_fence_then_publishes_the_changed_loop() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output), Some(persisted_range(48_000, 192_000, true)));
        let mut session = FakeSession::with_loop(Some(range(96_000, 288_000)), false);

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

        session.recovery.committed_transport_fence = 7;
        let RuntimeLoopUpdatePoll::Saved(saved) = controller.poll(&session).unwrap() else {
            panic!("a coherent changed snapshot must publish exactly one update");
        };
        assert!(saved.path.starts_with(temporary.path()));
        let parsed = read_runtime_loop_update(&saved.path).unwrap();
        assert_eq!(parsed.source(), &source());
        let loop_range = parsed.loop_range().unwrap();
        assert_eq!(loop_range.start_frame(), 96_000);
        assert_eq!(loop_range.end_frame(), 288_000);
        assert!(!loop_range.enabled());
        assert!(controller.poll(&session).is_none());
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn unchanged_state_never_creates_an_artifact() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let source_range = Some(persisted_range(48_000, 192_000, false));
        let mut controller = controller(Some(output), source_range);
        let mut session = FakeSession::with_loop(Some(range(48_000, 192_000)), false);

        controller.request_export(&mut session).unwrap();
        session.recovery.committed_transport_fence = 7;
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeLoopUpdatePoll::NoChanges)
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert_eq!(observation.state, "No loop changes to export");
        assert!(observation.export_enabled);
    }

    #[test]
    fn cleared_loop_publishes_an_explicit_clear() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output), Some(persisted_range(48_000, 192_000, true)));
        let mut session = FakeSession::with_loop(None, false);

        controller.request_export(&mut session).unwrap();
        session.recovery.committed_transport_fence = 7;
        let RuntimeLoopUpdatePoll::Saved(saved) = controller.poll(&session).unwrap() else {
            panic!("a cleared loop changed from the persisted range and must publish");
        };
        assert_eq!(
            read_runtime_loop_update(&saved.path).unwrap().loop_range(),
            None
        );
    }

    #[test]
    fn unavailable_output_or_nonpersistable_live_state_refuses_without_publication() {
        let mut disabled = controller(None, None);
        let mut disabled_session = FakeSession::with_loop(None, false);
        assert!(matches!(
            disabled.request_export(&mut disabled_session),
            Err(RuntimeLoopUpdateExportError::OutputDirectoryNotConfigured)
        ));
        assert_eq!(disabled_session.fence_request_count, 0);

        let temporary = TemporaryDirectory::new();
        let output = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output), None);
        let mut session = FakeSession::with_loop(None, true);
        controller.request_export(&mut session).unwrap();
        session.recovery.committed_transport_fence = 7;
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeLoopUpdatePoll::Failed(
                RuntimeLoopUpdateExportError::IncoherentLoopState
            ))
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert!(!observation.export_enabled);
        assert!(observation.state.contains("enabled but has no range"));
    }

    #[test]
    fn export_does_not_queue_a_second_fence_while_one_is_pending() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeLoopUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output), None);
        let mut session = FakeSession::with_loop(None, false);

        controller.request_export(&mut session).unwrap();
        assert!(matches!(
            controller.request_export(&mut session),
            Err(RuntimeLoopUpdateExportError::ExportPending)
        ));
        assert_eq!(session.fence_request_count, 1);
    }
}
