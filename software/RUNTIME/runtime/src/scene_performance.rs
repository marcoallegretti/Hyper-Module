//! Runtime-side ownership of one bounded scene-performance handoff.
//!
//! The controller owns the committed-launch receiver after the audio session
//! transfers it. It never changes the verified Runtime bundle or its embedded
//! project: a completed capture becomes a provenance-bound, no-replace update
//! in an explicitly configured external directory.

use core::error::Error;
use core::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper_desktop_audio::{
    DesktopDirectControlError, DesktopPlaybackSession, DesktopSceneAvailability,
};
use hyper_mixer_runtime::{
    InteractiveSceneLaunchObservation, InteractiveSceneLaunchReceiver, PlaybackRecoverySnapshot,
    PlaybackRecoveryTransportState, TransportCommand,
};
use hyper_playback_compiler::scene::LaunchQuantization;
use hyper_playback_json::{LoadedPlaybackProject, LoaderLimits};
use hyper_scene_performance::{
    RuntimeScenePerformanceSource, RuntimeScenePerformanceUpdate,
    RuntimeScenePerformanceUpdateDirectory, RuntimeScenePerformanceUpdateError,
    ScenePerformanceCapture as SharedScenePerformanceCapture,
    ScenePerformanceCaptureError as SharedScenePerformanceCaptureError, ScenePerformanceLaunch,
    ScenePerformanceScene,
};
use uuid::Uuid;

/// One coherent transport observation used by capture finalization.
///
/// It intentionally contains only the facts the Runtime capture contract
/// needs. This lets the control state be tested against deterministic facts
/// while the production adapter continues to obtain them from the session's
/// coherent recovery checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CaptureRecovery {
    state: PlaybackRecoveryTransportState,
    committed_cycle_count: u64,
    committed_capture_start_sequence: u64,
    loop_enabled: bool,
}

impl From<PlaybackRecoverySnapshot> for CaptureRecovery {
    fn from(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            state: snapshot.state(),
            committed_cycle_count: snapshot.committed_cycle_count(),
            committed_capture_start_sequence: snapshot.committed_capture_start_sequence(),
            loop_enabled: snapshot.loop_enabled(),
        }
    }
}

/// Minimal Runtime session authority needed by the capture controller.
///
/// The production implementation remains the sole `DesktopPlaybackSession`
/// owner. Keeping the adapter narrow makes the capture lifecycle testable
/// without a native audio device or a second transport owner.
pub(super) trait RuntimeScenePerformanceSession {
    fn capture_recovery(&self) -> CaptureRecovery;

    fn scene_availability(&self, scene_index: usize) -> Option<DesktopSceneAvailability>;

    fn try_start_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError>;

    fn try_stop_transport(&mut self) -> Result<(), DesktopDirectControlError>;
}

impl RuntimeScenePerformanceSession for DesktopPlaybackSession {
    fn capture_recovery(&self) -> CaptureRecovery {
        self.recovery_checkpoint().snapshot().into()
    }

    fn scene_availability(&self, scene_index: usize) -> Option<DesktopSceneAvailability> {
        self.scene_availability(scene_index)
    }

    fn try_start_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError> {
        self.try_start_capture_transport()
    }

    fn try_stop_transport(&mut self) -> Result<(), DesktopDirectControlError> {
        self.try_send_transport_command(TransportCommand::Stop)
    }
}

/// Immutable Runtime capture configuration prepared before native output starts.
pub(super) struct RuntimeScenePerformanceCaptureConfiguration {
    source: RuntimeScenePerformanceSource,
    scenes: Vec<ScenePerformanceScene>,
    recordable_grid_scene_indices: Vec<usize>,
    maximum_launch_count: usize,
    output_directory: Option<RuntimeScenePerformanceUpdateDirectory>,
}

impl RuntimeScenePerformanceCaptureConfiguration {
    /// Builds immutable capture facts from a verified Runtime project.
    ///
    /// The project is still borrowed from the verified deployment here, before
    /// it becomes the native playback session's immutable source.
    pub(super) fn try_new(
        source: RuntimeScenePerformanceSource,
        project: &LoadedPlaybackProject,
        output_directory: Option<RuntimeScenePerformanceUpdateDirectory>,
    ) -> Result<Self, RuntimeScenePerformanceCaptureError> {
        let scene_count = project.scene_count();
        let mut scenes = Vec::new();
        scenes
            .try_reserve_exact(scene_count)
            .map_err(|_| RuntimeScenePerformanceCaptureError::CapacityUnavailable)?;
        let mut recordable_grid_scene_indices = Vec::new();
        recordable_grid_scene_indices
            .try_reserve_exact(scene_count)
            .map_err(|_| RuntimeScenePerformanceCaptureError::CapacityUnavailable)?;
        for scene_index in 0..scene_count {
            let scene_id = project
                .scene_id(scene_index)
                .ok_or(RuntimeScenePerformanceCaptureError::ProjectCatalogUnavailable)?;
            let quantization = project
                .scene_launch_quantization(scene_index)
                .ok_or(RuntimeScenePerformanceCaptureError::ProjectCatalogUnavailable)?;
            let enabled = project
                .scene_enabled(scene_index)
                .ok_or(RuntimeScenePerformanceCaptureError::ProjectCatalogUnavailable)?;
            let mut owned_scene_id = String::new();
            owned_scene_id
                .try_reserve_exact(scene_id.len())
                .map_err(|_| RuntimeScenePerformanceCaptureError::CapacityUnavailable)?;
            owned_scene_id.push_str(scene_id);
            let grid_quantized = matches!(quantization, LaunchQuantization::Grid { .. });
            if grid_quantized && enabled {
                recordable_grid_scene_indices.push(scene_index);
            }
            scenes.push(ScenePerformanceScene::new(owned_scene_id, grid_quantized));
        }
        Ok(Self {
            source,
            scenes,
            recordable_grid_scene_indices,
            maximum_launch_count: LoaderLimits::VERSION_1
                .max_scene_launches
                .saturating_sub(project.scene_launch_count()),
            output_directory,
        })
    }

    /// Attaches the one committed-launch receiver transferred by the session.
    #[must_use]
    pub(super) fn attach(
        self,
        receiver: InteractiveSceneLaunchReceiver,
    ) -> RuntimeScenePerformanceCaptureController {
        let initial_state = if self.output_directory.is_none() {
            "Capture unavailable: no external performance-update directory".to_owned()
        } else if self.maximum_launch_count == 0 {
            "Capture unavailable: arrangement scene-launch capacity is exhausted".to_owned()
        } else if self.recordable_grid_scene_indices.is_empty() {
            "Capture unavailable: no enabled grid-quantized scene".to_owned()
        } else {
            "Waiting for committed Runtime transport recovery".to_owned()
        };
        RuntimeScenePerformanceCaptureController {
            receiver,
            source: self.source,
            scenes: self.scenes,
            recordable_grid_scene_indices: self.recordable_grid_scene_indices,
            maximum_launch_count: self.maximum_launch_count,
            output_directory: self.output_directory,
            active: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeScenePerformanceCaptureObservation {
                state: initial_state,
                start_enabled: false,
                stop_enabled: false,
            })),
        }
    }
}

/// A classified Runtime capture refusal or terminal failure.
#[derive(Debug)]
pub(super) enum RuntimeScenePerformanceCaptureError {
    OutputDirectoryNotConfigured,
    AlreadyCapturing,
    NoActiveCapture,
    TransportMustBeStopped,
    LoopEnabled,
    NoRecordableGridScene,
    NoRemainingArrangementCapacity,
    ProjectCatalogUnavailable,
    CapacityUnavailable,
    EventOverflow {
        dropped_event_count: usize,
    },
    EventSourceDisconnected,
    Capture(SharedScenePerformanceCaptureError),
    Transport(DesktopDirectControlError),
    StopUnavailable {
        capture: Box<Self>,
        source: DesktopDirectControlError,
    },
    Update(RuntimeScenePerformanceUpdateError),
}

impl fmt::Display for RuntimeScenePerformanceCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external scene-performance output directory",
            ),
            Self::AlreadyCapturing => {
                formatter.write_str("a scene-performance capture is already active")
            }
            Self::NoActiveCapture => formatter.write_str("no scene-performance capture is active"),
            Self::TransportMustBeStopped => {
                formatter.write_str("transport must be stopped before scene-performance capture")
            }
            Self::LoopEnabled => formatter
                .write_str("loop playback must be disabled before scene-performance capture"),
            Self::NoRecordableGridScene => formatter.write_str(
                "the verified project has no enabled, ready grid-quantized scene to capture",
            ),
            Self::NoRemainingArrangementCapacity => formatter.write_str(
                "the verified project has no remaining arrangement scene-launch capacity",
            ),
            Self::ProjectCatalogUnavailable => formatter
                .write_str("the verified Runtime project has inconsistent scene capture facts"),
            Self::CapacityUnavailable => {
                formatter.write_str("scene-performance capture capacity is unavailable")
            }
            Self::EventOverflow {
                dropped_event_count,
            } => write!(
                formatter,
                "scene-performance capture lost {dropped_event_count} committed launch event(s)"
            ),
            Self::EventSourceDisconnected => {
                formatter.write_str("scene-performance committed-launch source disconnected")
            }
            Self::Capture(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
            Self::StopUnavailable { capture, source } => write!(
                formatter,
                "scene-performance capture failed and transport could not stop it: {capture}; {source}"
            ),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeScenePerformanceCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Capture(source) => Some(source),
            Self::Transport(source) | Self::StopUnavailable { source, .. } => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::AlreadyCapturing
            | Self::NoActiveCapture
            | Self::TransportMustBeStopped
            | Self::LoopEnabled
            | Self::NoRecordableGridScene
            | Self::NoRemainingArrangementCapacity
            | Self::ProjectCatalogUnavailable
            | Self::CapacityUnavailable
            | Self::EventOverflow { .. }
            | Self::EventSourceDisconnected => None,
        }
    }
}

#[derive(Debug)]
enum RuntimeScenePerformanceCapturePhase {
    Capturing,
    Stopping {
        after_committed_cycle_count: u64,
        failure: Option<RuntimeScenePerformanceCaptureError>,
    },
}

/// One active capture that has a committed capture-start acknowledgement.
#[derive(Debug)]
struct ActiveRuntimeScenePerformanceCapture {
    capture: SharedScenePerformanceCapture,
    capture_start_sequence: u64,
    phase: RuntimeScenePerformanceCapturePhase,
}

impl ActiveRuntimeScenePerformanceCapture {
    const fn new(capture: SharedScenePerformanceCapture, capture_start_sequence: u64) -> Self {
        Self {
            capture,
            capture_start_sequence,
            phase: RuntimeScenePerformanceCapturePhase::Capturing,
        }
    }

    fn captured_launch_count(&self) -> usize {
        self.capture.captured_launch_count()
    }

    const fn is_capturing(&self) -> bool {
        matches!(self.phase, RuntimeScenePerformanceCapturePhase::Capturing)
    }

    fn stopping_after_committed_cycle_count(&self) -> Option<u64> {
        match self.phase {
            RuntimeScenePerformanceCapturePhase::Capturing => None,
            RuntimeScenePerformanceCapturePhase::Stopping {
                after_committed_cycle_count,
                ..
            } => Some(after_committed_cycle_count),
        }
    }

    fn begin_stopping(
        &mut self,
        after_committed_cycle_count: u64,
        failure: Option<RuntimeScenePerformanceCaptureError>,
    ) {
        self.phase = RuntimeScenePerformanceCapturePhase::Stopping {
            after_committed_cycle_count,
            failure,
        };
    }

    fn remember_failure(&mut self, failure: RuntimeScenePerformanceCaptureError) {
        if let RuntimeScenePerformanceCapturePhase::Stopping {
            failure: terminal_failure,
            ..
        } = &mut self.phase
            && terminal_failure.is_none()
        {
            *terminal_failure = Some(failure);
        }
    }

    fn take_terminal_failure(&mut self) -> Option<RuntimeScenePerformanceCaptureError> {
        match &mut self.phase {
            RuntimeScenePerformanceCapturePhase::Capturing => None,
            RuntimeScenePerformanceCapturePhase::Stopping { failure, .. } => failure.take(),
        }
    }

    fn stopped_after_start(&self, recovery: CaptureRecovery) -> bool {
        recovery.state == PlaybackRecoveryTransportState::Stopped
            && recovery.committed_capture_start_sequence >= self.capture_start_sequence
    }

    fn stopped_after_explicit_stop(&self, recovery: CaptureRecovery) -> bool {
        self.stopping_after_committed_cycle_count()
            .is_some_and(|after_committed_cycle_count| {
                recovery.state == PlaybackRecoveryTransportState::Stopped
                    && recovery.committed_cycle_count > after_committed_cycle_count
            })
    }

    fn observe(
        &mut self,
        observation: InteractiveSceneLaunchObservation,
    ) -> Result<(), RuntimeScenePerformanceCaptureError> {
        let launch = observation.launch();
        self.capture
            .observe(
                launch.scene_index(),
                launch.arrangement_tick(),
                launch.sequence(),
                observation.loop_enabled(),
            )
            .map_err(capture_error)
    }

    #[cfg(test)]
    fn observe_launch(
        &mut self,
        scene_index: usize,
        arrangement_tick: Option<u64>,
        sequence: u64,
        loop_enabled: bool,
    ) -> Result<(), RuntimeScenePerformanceCaptureError> {
        self.capture
            .observe(scene_index, arrangement_tick, sequence, loop_enabled)
            .map_err(capture_error)
    }

    fn into_launches(
        self,
    ) -> Result<Vec<ScenePerformanceLaunch>, RuntimeScenePerformanceCaptureError> {
        self.capture.into_launches().map_err(capture_error)
    }
}

fn capture_error(error: SharedScenePerformanceCaptureError) -> RuntimeScenePerformanceCaptureError {
    match error {
        SharedScenePerformanceCaptureError::InvalidMaximumLaunchCount => {
            RuntimeScenePerformanceCaptureError::NoRemainingArrangementCapacity
        }
        SharedScenePerformanceCaptureError::InvalidSceneIdentity { .. }
        | SharedScenePerformanceCaptureError::DuplicateSceneIdentity { .. } => {
            RuntimeScenePerformanceCaptureError::ProjectCatalogUnavailable
        }
        other => RuntimeScenePerformanceCaptureError::Capture(other),
    }
}

/// The Runtime owner turns this compact state into the operator-surface view.
///
/// The UI observes this value; it never advances capture state itself or
/// claims that a file exists before durable publication succeeds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeScenePerformanceCaptureObservation {
    pub(super) state: String,
    pub(super) start_enabled: bool,
    pub(super) stop_enabled: bool,
}

/// One durable Runtime update that became available to the IDE picker.
#[derive(Debug)]
pub(super) struct RuntimeScenePerformanceCaptureSuccess {
    pub(super) path: PathBuf,
    pub(super) launch_count: usize,
}

/// Terminal result reported by one Runtime owner turn.
#[derive(Debug)]
pub(super) enum RuntimeScenePerformanceCapturePoll {
    Saved(RuntimeScenePerformanceCaptureSuccess),
    Failed(RuntimeScenePerformanceCaptureError),
}

/// Runtime-side capture controller.
///
/// Its receiver is drained by the Runtime owner turn only. This makes a UI
/// timer observational: it never competes with audio transport ownership or
/// consumes capture evidence.
pub(super) struct RuntimeScenePerformanceCaptureController {
    receiver: InteractiveSceneLaunchReceiver,
    source: RuntimeScenePerformanceSource,
    scenes: Vec<ScenePerformanceScene>,
    recordable_grid_scene_indices: Vec<usize>,
    maximum_launch_count: usize,
    output_directory: Option<RuntimeScenePerformanceUpdateDirectory>,
    active: Option<ActiveRuntimeScenePerformanceCapture>,
    last_status: Option<String>,
    observation: Arc<Mutex<RuntimeScenePerformanceCaptureObservation>>,
}

impl RuntimeScenePerformanceCaptureController {
    /// Returns the Runtime-owned presentation snapshot for the operator UI.
    #[must_use]
    pub(super) fn observation(&self) -> Arc<Mutex<RuntimeScenePerformanceCaptureObservation>> {
        Arc::clone(&self.observation)
    }

    /// Explicitly starts one bounded capture through the sole transport owner.
    pub(super) fn start<S: RuntimeScenePerformanceSession>(
        &mut self,
        session: &mut S,
    ) -> Result<usize, RuntimeScenePerformanceCaptureError> {
        if self.active.is_some() {
            return Err(RuntimeScenePerformanceCaptureError::AlreadyCapturing);
        }
        if self.output_directory.is_none() {
            return Err(RuntimeScenePerformanceCaptureError::OutputDirectoryNotConfigured);
        }
        let recovery = session.capture_recovery();
        if recovery.state != PlaybackRecoveryTransportState::Stopped {
            return Err(RuntimeScenePerformanceCaptureError::TransportMustBeStopped);
        }
        if recovery.loop_enabled {
            return Err(RuntimeScenePerformanceCaptureError::LoopEnabled);
        }
        if !self
            .recordable_grid_scene_indices
            .iter()
            .copied()
            .any(|scene_index| {
                session.scene_availability(scene_index) == Some(DesktopSceneAvailability::Ready)
            })
        {
            return Err(RuntimeScenePerformanceCaptureError::NoRecordableGridScene);
        }
        if self.maximum_launch_count == 0 {
            return Err(RuntimeScenePerformanceCaptureError::NoRemainingArrangementCapacity);
        }
        let capture =
            SharedScenePerformanceCapture::try_new(self.scenes.clone(), self.maximum_launch_count)
                .map_err(capture_error)?;

        // History before the capture-start command belongs to no capture.
        // It is safe to discard overflow from that history, but a missing
        // producer cannot be made lossless by starting a new capture.
        let discarded_history = self.receiver.drain(|_| {});
        if discarded_history.sender_disconnected() {
            return Err(RuntimeScenePerformanceCaptureError::EventSourceDisconnected);
        }
        let capture_start_sequence = session
            .try_start_capture_transport()
            .map_err(RuntimeScenePerformanceCaptureError::Transport)?;
        self.active = Some(ActiveRuntimeScenePerformanceCapture::new(
            capture,
            capture_start_sequence,
        ));
        self.last_status = None;
        self.refresh_observation(session);
        Ok(self.maximum_launch_count)
    }

    /// Requests the one stopped boundary used to finish an active capture.
    pub(super) fn stop<S: RuntimeScenePerformanceSession>(
        &mut self,
        session: &mut S,
    ) -> Result<usize, RuntimeScenePerformanceCaptureError> {
        let captured_launch_count = self
            .active
            .as_ref()
            .ok_or(RuntimeScenePerformanceCaptureError::NoActiveCapture)?
            .captured_launch_count();
        self.queue_stop(session, None)?;
        self.refresh_observation(session);
        Ok(captured_launch_count)
    }

    /// Drains committed launch evidence and finalizes only at a committed stop.
    pub(super) fn poll<S: RuntimeScenePerformanceSession>(
        &mut self,
        session: &mut S,
    ) -> Option<RuntimeScenePerformanceCapturePoll> {
        if self.active.is_none() {
            // Keep this transferred receiver bounded even while capture is
            // unavailable or idle. Its stale events must never become part of
            // a later explicit capture.
            let _ = self.receiver.drain(|_| {});
            self.refresh_observation(session);
            return None;
        }
        let recovery = session.capture_recovery();
        let (drain, mut observation_failure) = {
            let active = self
                .active
                .as_mut()
                .expect("active capture was checked before draining evidence");
            let mut observation_failure = None;
            let drain = self.receiver.drain(|observation| {
                if observation_failure.is_none() {
                    observation_failure = active.observe(observation).err();
                }
            });
            (drain, observation_failure)
        };
        if drain.dropped_event_count() > 0 {
            observation_failure = Some(RuntimeScenePerformanceCaptureError::EventOverflow {
                dropped_event_count: drain.dropped_event_count(),
            });
        }
        if drain.sender_disconnected() {
            observation_failure =
                Some(RuntimeScenePerformanceCaptureError::EventSourceDisconnected);
        }
        if self
            .active
            .as_ref()
            .is_some_and(ActiveRuntimeScenePerformanceCapture::is_capturing)
            && recovery.loop_enabled
        {
            observation_failure = Some(RuntimeScenePerformanceCaptureError::LoopEnabled);
        }
        if let Some(reason) = observation_failure {
            if self
                .active
                .as_ref()
                .is_some_and(ActiveRuntimeScenePerformanceCapture::is_capturing)
            {
                if let Err(error) = self.queue_stop(session, Some(reason)) {
                    self.active = None;
                    self.last_status = Some(format!("Capture failed: {error}"));
                    self.refresh_observation(session);
                    return Some(RuntimeScenePerformanceCapturePoll::Failed(error));
                }
            } else if let Some(active) = self.active.as_mut() {
                active.remember_failure(reason);
            }
        }

        let completed = self.active.as_ref().is_some_and(|active| {
            (active.is_capturing() && active.stopped_after_start(recovery))
                || active.stopped_after_explicit_stop(recovery)
        });
        if !completed {
            self.refresh_observation(session);
            return None;
        }
        Some(match self.finish() {
            Ok(saved) => {
                self.last_status = Some(format!(
                    "Update published: {} launch(es)",
                    saved.launch_count
                ));
                self.refresh_observation(session);
                RuntimeScenePerformanceCapturePoll::Saved(saved)
            }
            Err(error) => {
                self.last_status = Some(format!("Capture failed: {error}"));
                self.refresh_observation(session);
                RuntimeScenePerformanceCapturePoll::Failed(error)
            }
        })
    }

    fn queue_stop<S: RuntimeScenePerformanceSession>(
        &mut self,
        session: &mut S,
        failure: Option<RuntimeScenePerformanceCaptureError>,
    ) -> Result<(), RuntimeScenePerformanceCaptureError> {
        if !self
            .active
            .as_ref()
            .is_some_and(ActiveRuntimeScenePerformanceCapture::is_capturing)
        {
            return Ok(());
        }
        let recovery = session.capture_recovery();
        match session.try_stop_transport() {
            Ok(()) => {
                self.active
                    .as_mut()
                    .expect("active capture cannot disappear while Runtime owns it")
                    .begin_stopping(recovery.committed_cycle_count, failure);
                Ok(())
            }
            Err(source) => match failure {
                Some(capture) => Err(RuntimeScenePerformanceCaptureError::StopUnavailable {
                    capture: Box::new(capture),
                    source,
                }),
                None => Err(RuntimeScenePerformanceCaptureError::Transport(source)),
            },
        }
    }

    fn finish(
        &mut self,
    ) -> Result<RuntimeScenePerformanceCaptureSuccess, RuntimeScenePerformanceCaptureError> {
        let mut active = self
            .active
            .take()
            .expect("only a completed active capture can be finalized");
        if let Some(error) = active.take_terminal_failure() {
            return Err(error);
        }
        let launches = active.into_launches()?;
        let launch_count = launches.len();
        let update = RuntimeScenePerformanceUpdate::try_new(
            Uuid::new_v4().to_string(),
            self.source.clone(),
            launches,
            self.maximum_launch_count,
        )
        .map_err(RuntimeScenePerformanceCaptureError::Update)?;
        let output_directory = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeScenePerformanceCaptureError::OutputDirectoryNotConfigured)?;
        let path = output_directory
            .publish(&update)
            .map_err(RuntimeScenePerformanceCaptureError::Update)?;
        Ok(RuntimeScenePerformanceCaptureSuccess { path, launch_count })
    }

    fn refresh_observation<S: RuntimeScenePerformanceSession>(&self, session: &S) {
        let recovery = session.capture_recovery();
        let (state, start_enabled, stop_enabled) = match self.active.as_ref() {
            Some(active) if active.is_capturing() => {
                let state =
                    if recovery.committed_capture_start_sequence >= active.capture_start_sequence {
                        format!(
                            "Capturing {} committed launch(es)",
                            active.captured_launch_count()
                        )
                    } else {
                        "Capture start queued; awaiting a committed audio cycle".to_owned()
                    };
                (state, false, true)
            }
            Some(active) => (
                format!(
                    "Capture stopping after {} committed launch(es)",
                    active.captured_launch_count()
                ),
                false,
                false,
            ),
            None => {
                let has_recordable_ready_scene = self.has_recordable_ready_scene(session);
                let start_enabled = self.output_directory.is_some()
                    && self.maximum_launch_count > 0
                    && has_recordable_ready_scene
                    && recovery.state == PlaybackRecoveryTransportState::Stopped
                    && !recovery.loop_enabled;
                let unavailable = if self.output_directory.is_none() {
                    Some("Capture unavailable: no external performance-update directory".to_owned())
                } else if self.maximum_launch_count == 0 {
                    Some(
                        "Capture unavailable: arrangement scene-launch capacity is exhausted"
                            .to_owned(),
                    )
                } else if !has_recordable_ready_scene {
                    Some("Capture unavailable: no enabled Ready grid scene".to_owned())
                } else if recovery.state != PlaybackRecoveryTransportState::Stopped {
                    Some("Capture requires stopped transport".to_owned())
                } else if recovery.loop_enabled {
                    Some("Capture requires loop playback to be disabled".to_owned())
                } else {
                    None
                };
                (
                    self.last_status
                        .clone()
                        .or(unavailable)
                        .unwrap_or_else(|| "Ready to capture a scene performance".to_owned()),
                    start_enabled,
                    false,
                )
            }
        };
        if let Ok(mut observation) = self.observation.lock() {
            *observation = RuntimeScenePerformanceCaptureObservation {
                state,
                start_enabled,
                stop_enabled,
            };
        }
    }

    fn has_recordable_ready_scene<S: RuntimeScenePerformanceSession>(&self, session: &S) -> bool {
        self.recordable_grid_scene_indices
            .iter()
            .copied()
            .any(|scene_index| {
                session.scene_availability(scene_index) == Some(DesktopSceneAvailability::Ready)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use hyper_mixer_runtime::interactive_scene_launch_channel;
    use hyper_scene_performance::read_runtime_scene_performance_update;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const FIRST_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const SECOND_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-scene-performance-{}",
                Uuid::new_v4()
            ));
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

    fn source() -> RuntimeScenePerformanceSource {
        RuntimeScenePerformanceSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn scene(id: &str, grid_quantized: bool) -> ScenePerformanceScene {
        ScenePerformanceScene::new(id.to_owned(), grid_quantized)
    }

    fn controller(
        output_directory: Option<RuntimeScenePerformanceUpdateDirectory>,
    ) -> (
        RuntimeScenePerformanceCaptureController,
        hyper_mixer_runtime::InteractiveSceneLaunchPublisher,
    ) {
        let (publisher, receiver) = interactive_scene_launch_channel();
        (
            RuntimeScenePerformanceCaptureController {
                receiver,
                source: source(),
                scenes: vec![scene(FIRST_SCENE_ID, true), scene(SECOND_SCENE_ID, false)],
                recordable_grid_scene_indices: vec![0],
                maximum_launch_count: 3,
                output_directory,
                active: None,
                last_status: None,
                observation: Arc::new(Mutex::new(RuntimeScenePerformanceCaptureObservation {
                    state: "fixture".to_owned(),
                    start_enabled: false,
                    stop_enabled: false,
                })),
            },
            publisher,
        )
    }

    fn active(capture_start_sequence: u64) -> ActiveRuntimeScenePerformanceCapture {
        ActiveRuntimeScenePerformanceCapture::new(
            SharedScenePerformanceCapture::try_new(
                vec![scene(FIRST_SCENE_ID, true), scene(SECOND_SCENE_ID, false)],
                3,
            )
            .unwrap(),
            capture_start_sequence,
        )
    }

    #[derive(Debug)]
    struct FakeSession {
        recovery: CaptureRecovery,
        availability: Vec<Option<DesktopSceneAvailability>>,
        start_sequence: u64,
        start_refused: bool,
        stop_refused: bool,
        start_count: usize,
        stop_count: usize,
    }

    impl FakeSession {
        fn stopped_ready() -> Self {
            Self {
                recovery: CaptureRecovery {
                    state: PlaybackRecoveryTransportState::Stopped,
                    committed_cycle_count: 4,
                    committed_capture_start_sequence: 0,
                    loop_enabled: false,
                },
                availability: vec![Some(DesktopSceneAvailability::Ready)],
                start_sequence: 5,
                start_refused: false,
                stop_refused: false,
                start_count: 0,
                stop_count: 0,
            }
        }
    }

    impl RuntimeScenePerformanceSession for FakeSession {
        fn capture_recovery(&self) -> CaptureRecovery {
            self.recovery
        }

        fn scene_availability(&self, scene_index: usize) -> Option<DesktopSceneAvailability> {
            self.availability.get(scene_index).copied().flatten()
        }

        fn try_start_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError> {
            self.start_count = self.start_count.saturating_add(1);
            if self.start_refused {
                Err(DesktopDirectControlError::CaptureStartSequenceExhausted)
            } else {
                Ok(self.start_sequence)
            }
        }

        fn try_stop_transport(&mut self) -> Result<(), DesktopDirectControlError> {
            self.stop_count = self.stop_count.saturating_add(1);
            if self.stop_refused {
                Err(DesktopDirectControlError::CaptureStartSequenceExhausted)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn start_requires_external_output_and_verified_stopped_ready_state() {
        let (mut disabled_controller, _publisher) = controller(None);
        let mut session = FakeSession::stopped_ready();

        assert!(matches!(
            disabled_controller.start(&mut session),
            Err(RuntimeScenePerformanceCaptureError::OutputDirectoryNotConfigured)
        ));
        assert_eq!(session.start_count, 0);

        let temporary = TemporaryDirectory::new();
        let output = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output));
        session.recovery.state = PlaybackRecoveryTransportState::Rolling;
        assert!(matches!(
            controller.start(&mut session),
            Err(RuntimeScenePerformanceCaptureError::TransportMustBeStopped)
        ));

        session.recovery.state = PlaybackRecoveryTransportState::Stopped;
        session.recovery.loop_enabled = true;
        assert!(matches!(
            controller.start(&mut session),
            Err(RuntimeScenePerformanceCaptureError::LoopEnabled)
        ));

        session.recovery.loop_enabled = false;
        session.availability[0] = Some(DesktopSceneAvailability::NotPrepared);
        assert!(matches!(
            controller.start(&mut session),
            Err(RuntimeScenePerformanceCaptureError::NoRecordableGridScene)
        ));
    }

    #[test]
    fn start_and_stop_use_one_capture_acknowledgement_and_stop_boundary() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output));
        let mut session = FakeSession::stopped_ready();

        assert_eq!(controller.start(&mut session).unwrap(), 3);
        assert_eq!(session.start_count, 1);
        let active = controller.active.as_ref().unwrap();
        assert!(!active.stopped_after_start(session.recovery));

        session.recovery.committed_capture_start_sequence = 5;
        assert!(active.stopped_after_start(session.recovery));
        assert_eq!(controller.stop(&mut session).unwrap(), 0);
        assert_eq!(session.stop_count, 1);
        let active = controller.active.as_ref().unwrap();
        assert!(!active.stopped_after_explicit_stop(session.recovery));
        session.recovery.committed_cycle_count = 5;
        assert!(active.stopped_after_explicit_stop(session.recovery));
    }

    #[test]
    fn presentation_waits_for_committed_runtime_boundaries_and_never_claims_empty_output() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output));
        let mut session = FakeSession::stopped_ready();

        controller.start(&mut session).unwrap();
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert_eq!(
            observation.state,
            "Capture start queued; awaiting a committed audio cycle"
        );
        assert!(
            observation.stop_enabled,
            "an explicit stop may cancel a start that has not yet been acknowledged"
        );
        drop(observation);

        session.recovery.state = PlaybackRecoveryTransportState::Rolling;
        session.recovery.committed_capture_start_sequence = session.start_sequence;
        assert!(controller.poll(&mut session).is_none());
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert_eq!(observation.state, "Capturing 0 committed launch(es)");
        assert!(!observation.start_enabled);
        assert!(observation.stop_enabled);
        drop(observation);

        controller.stop(&mut session).unwrap();
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert_eq!(
            observation.state,
            "Capture stopping after 0 committed launch(es)"
        );
        assert!(!observation.start_enabled);
        assert!(!observation.stop_enabled);
        drop(observation);

        session.recovery.state = PlaybackRecoveryTransportState::Stopped;
        session.recovery.committed_cycle_count = 5;
        assert!(matches!(
            controller.poll(&mut session),
            Some(RuntimeScenePerformanceCapturePoll::Failed(
                RuntimeScenePerformanceCaptureError::Capture(
                    SharedScenePerformanceCaptureError::NoLaunchesCaptured
                )
            ))
        ));
        let observation = controller.observation();
        let observation = observation.lock().unwrap();
        assert!(observation.state.starts_with("Capture failed:"));
        assert!(observation.start_enabled);
        assert!(!observation.stop_enabled);
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[test]
    fn finalization_publishes_one_provenance_bound_external_update() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output));
        let mut active = active(3);
        active.observe_launch(0, Some(960), 8, false).unwrap();
        active.observe_launch(0, Some(1_920), 9, false).unwrap();
        active.begin_stopping(4, None);
        controller.active = Some(active);

        let saved = controller.finish().unwrap();
        assert_eq!(saved.launch_count, 2);
        assert!(saved.path.starts_with(temporary.path()));
        let parsed = read_runtime_scene_performance_update(&saved.path, 3).unwrap();
        assert_eq!(parsed.source(), &source());
        assert_eq!(
            parsed.launches(),
            [
                ScenePerformanceLaunch::try_new(FIRST_SCENE_ID.to_owned(), 960).unwrap(),
                ScenePerformanceLaunch::try_new(FIRST_SCENE_ID.to_owned(), 1_920).unwrap(),
            ]
        );
    }

    #[test]
    fn terminal_capture_failure_never_publishes_a_partial_update() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeScenePerformanceUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output));
        let mut active = active(3);
        active.observe_launch(0, Some(960), 8, false).unwrap();
        active.begin_stopping(
            4,
            Some(RuntimeScenePerformanceCaptureError::EventOverflow {
                dropped_event_count: 1,
            }),
        );
        controller.active = Some(active);

        assert!(matches!(
            controller.finish(),
            Err(RuntimeScenePerformanceCaptureError::EventOverflow {
                dropped_event_count: 1
            })
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }
}
