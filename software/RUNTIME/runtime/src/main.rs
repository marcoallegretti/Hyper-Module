//! The runtime process: hosts one verified bundle and serves one panel.

#![deny(unsafe_code)]

mod automation_update;
mod input;
mod loop_update;
mod mixer_update;
mod native_device_update;
mod output;
mod recording;
mod scene_performance;

use std::cell::{Cell, RefCell};
use std::io::BufRead;
use std::net::TcpListener;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use hyper_audio_engine::{LinearGain, LoudnessMeasurementCommand, LoudnessMeasurementState, Pan};
use hyper_audio_source::DecodeLimits;
use hyper_control_runtime::{
    ControlBindingAvailability, ControlBindingUnavailableReason, PanelBinding, PanelConnectError,
    PanelConnection, PanelDispatchError, PanelDispatchOutcome, PanelDisplay, PanelReleaseFailure,
    UnboundReason,
};
use hyper_desktop_audio::{
    CaptureStreamCounters, DesktopControlSinkError, DesktopMixerSendTap, DesktopMonitoringConfig,
    DesktopMonitoringControl, DesktopNativeDevice, DesktopOutputStopError,
    DesktopPlaybackRecordingNativeSettlement, DesktopPlaybackRecordingSession,
    DesktopPlaybackRecordingStartRecovery, DesktopPlaybackRecordingStartRecoveryWait,
    DesktopPlaybackRecordingStartSettlement, DesktopPlaybackRecovery,
    DesktopPlaybackRecoveryPendingStart, DesktopPlaybackRecoveryPendingStop,
    DesktopPlaybackRecoveryPendingStopFailure, DesktopPlaybackRecoveryPendingStopWait,
    DesktopPlaybackRecoveryPendingWait, DesktopPlaybackRecoveryStopError, DesktopPlaybackSession,
    DesktopRecordingWriterSettlement, DesktopSceneAvailability, InputMeterReceiver,
    OutputStreamCounters, OutputStreamSnapshot, OutputStreamState, OutputStreamStatus, StreamState,
    StreamStatus,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{
    ChannelSamplePeakBallistics, ChannelSamplePeakDisplay, EbuLoudnessPresentationConfig,
    LocateState, LoudnessPresentationSnapshot, LoudnessScaleRegion, MaximumTruePeakDisplay,
    MetronomeGain, MixerChannelHandle, MixerParameterCommand, MixerParameterStateSnapshot,
    MixerSendHandle, MixerStripSamplePeakBallistics, MixerStripSamplePeakBallisticsSnapshot,
    NativeDeviceCommand, NativeDeviceHandle, NativeDeviceStateSnapshot, OutputMeterReceiver,
    PlaybackCycleObservation, PlaybackRecoveryCheckpoint, PlaybackRecoverySnapshot,
    PlaybackRecoveryTransportState, SamplePeakBallisticsConfig, ScaledLoudnessValue,
    StereoSamplePeakBallistics, StereoSamplePeakBallisticsSnapshot,
    StereoSamplePeakDisplayBallistics, StereoSamplePeakDisplayBallisticsSnapshot, TransportCommand,
    TransportLoopRange,
};
use hyper_native_device::{
    NativeCompressorState, NativeDeviceState, NativeFilterMode, NativeFilterState,
};
use hyper_panel_protocol::{LinkConfig, Message, PanelDescriptor};
use hyper_playback_compiler::{FrameTimeline, MeterEvent, TempoEvent, musical_position};
use hyper_playback_json::{LoadedTimelineMarker, LoaderLimits};
use hyper_playback_session::{PlaybackExtent, PlaybackSessionAssemblyLimits};
use hyper_project_loader::{PreparedProjectPlayback, ProjectPlaybackLimits};
use hyper_recording_worker::RecordingWorkerCompletion;
use hyper_runtime_automation_update::{
    RuntimeAutomationUpdateDirectory, RuntimeAutomationUpdateSource,
};
use hyper_runtime_deployment::resolve_runtime_deployment;
use hyper_runtime_host::{PanelHost, RuntimeArguments, ServedPanel, serve_panels_with_idle};
use hyper_runtime_loop_update::{RuntimeLoopUpdateDirectory, RuntimeLoopUpdateSource};
use hyper_runtime_mixer_update::{RuntimeMixerUpdateDirectory, RuntimeMixerUpdateSource};
use hyper_runtime_native_device_update::{
    RuntimeNativeDeviceUpdateDirectory, RuntimeNativeDeviceUpdateSource,
};
use hyper_runtime_recording_update::{
    RuntimeRecordingUpdateDirectory, RuntimeRecordingUpdateSource,
};
use hyper_scene_performance::{
    RuntimeScenePerformanceSource, RuntimeScenePerformanceUpdateDirectory, sha256_hex,
};
use slint::{
    CloseRequestResponse, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel,
};

use automation_update::{
    RuntimeAutomationCaptureConfiguration, RuntimeAutomationCaptureController,
    RuntimeAutomationCaptureObservation, RuntimeAutomationCapturePoll,
};
use input::{RuntimeInputRoute, discover_runtime_input_targets, prepare_runtime_input};
use loop_update::{
    RuntimeLoopUpdateConfiguration, RuntimeLoopUpdateController, RuntimeLoopUpdateObservation,
    RuntimeLoopUpdatePoll,
};
use mixer_update::{
    RuntimeMixerUpdateConfiguration, RuntimeMixerUpdateController, RuntimeMixerUpdateObservation,
    RuntimeMixerUpdatePoll,
};
use native_device_update::{
    RuntimeNativeDeviceUpdateConfiguration, RuntimeNativeDeviceUpdateController,
    RuntimeNativeDeviceUpdateObservation, RuntimeNativeDeviceUpdatePoll,
};
use output::{RuntimeOutputRoute, discover_runtime_output_targets, prepare_runtime_output};
use recording::{
    ActiveRuntimeRecording, RuntimeRecordingConfiguration, RuntimeRecordingError,
    RuntimeRecordingPublication,
};
use scene_performance::{
    RuntimeScenePerformanceCaptureConfiguration, RuntimeScenePerformanceCaptureController,
    RuntimeScenePerformanceCaptureObservation, RuntimeScenePerformanceCapturePoll,
};

slint::include_modules!();

/// The runtime's own hosting policy. Stated here rather than borrowed from the
/// IDE: these are the limits this process accepts, and they are software
/// limits, not evidence of embedded hardware capacity.
const MAXIMUM_BLOCK_FRAMES: usize = 1_024;
const SAMPLE_RATE_HZ: u32 = 48_000;
const MAX_AUDIO_FILE_BYTES: u64 = 512 * 1_024 * 1_024;
const MAX_AUDIO_FRAMES: u64 = 43_200_000;
const MAX_AUDIO_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_RESOURCE_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_RUNTIME_BUNDLE_BYTES: u64 = 1_024 * 1_024 * 1_024;
const OUTPUT_METER_FLOOR_DBFS: f32 = -90.0;
const OUTPUT_METER_FALLOFF_DB_PER_SECOND: f32 = 13.3;
const INPUT_MONITORING_TRANSPORT_FRAMES: usize = MAXIMUM_BLOCK_FRAMES * 2;
const INPUT_MONITORING_TARGET_FRAMES: usize = 256;
const INPUT_MONITORING_CHUNK_FRAMES: usize = 64;

const HEARTBEAT_INTERVAL_MS: u32 = 50;
const HEARTBEAT_TIMEOUT_MS: u32 = 500;

/// The runtime's control owner. Every method forwards; the session owns the
/// sole control runtime, so it is the only thing that can answer these.
struct RuntimePlaybackOwner {
    state: Option<RuntimePlaybackOwnerState>,
    recording: RuntimeRecordingConfiguration,
    recording_observation: Arc<Mutex<RuntimeRecordingObservation>>,
    playback_available: Arc<AtomicBool>,
    output_pinned: bool,
}

impl RuntimePlaybackOwner {
    fn new(
        playback: DesktopPlaybackSession,
        recording: RuntimeRecordingConfiguration,
        output_pinned: bool,
    ) -> Self {
        let recording_observation = Arc::new(Mutex::new(RuntimeRecordingObservation {
            state: initial_runtime_recording_state(&recording, output_pinned),
            start_enabled: false,
            stop_enabled: false,
        }));
        let playback_available = Arc::new(AtomicBool::new(true));
        let mut owner = Self {
            state: Some(RuntimePlaybackOwnerState::Playback(Box::new(playback))),
            recording,
            recording_observation,
            playback_available,
            output_pinned,
        };
        owner.refresh_recording_observation();
        owner
    }

    fn playback(&self) -> Option<&DesktopPlaybackSession> {
        match self.state.as_ref()? {
            RuntimePlaybackOwnerState::Playback(playback) => Some(playback),
            RuntimePlaybackOwnerState::Recording { session, .. } => Some(session.playback()),
            RuntimePlaybackOwnerState::PendingStop { .. }
            | RuntimePlaybackOwnerState::Recovery { .. }
            | RuntimePlaybackOwnerState::PendingStart(_)
            | RuntimePlaybackOwnerState::PendingRecordingStart(_)
            | RuntimePlaybackOwnerState::StopError { .. }
            | RuntimePlaybackOwnerState::ShutdownPlayback(_)
            | RuntimePlaybackOwnerState::ShutdownRecovery(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
            | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
            | RuntimePlaybackOwnerState::ShutdownStopError(_) => None,
        }
    }

    fn playback_mut(&mut self) -> Option<&mut DesktopPlaybackSession> {
        match self.state.as_mut()? {
            RuntimePlaybackOwnerState::Playback(playback) => Some(playback),
            RuntimePlaybackOwnerState::Recording { session, .. } => Some(session.playback_mut()),
            RuntimePlaybackOwnerState::PendingStop { .. }
            | RuntimePlaybackOwnerState::Recovery { .. }
            | RuntimePlaybackOwnerState::PendingStart(_)
            | RuntimePlaybackOwnerState::PendingRecordingStart(_)
            | RuntimePlaybackOwnerState::StopError { .. }
            | RuntimePlaybackOwnerState::ShutdownPlayback(_)
            | RuntimePlaybackOwnerState::ShutdownRecovery(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
            | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
            | RuntimePlaybackOwnerState::ShutdownStopError(_) => None,
        }
    }

    fn recording_observation(&self) -> Arc<Mutex<RuntimeRecordingObservation>> {
        Arc::clone(&self.recording_observation)
    }

    fn playback_availability(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.playback_available)
    }

    fn is_recording(&self) -> bool {
        matches!(
            self.state,
            Some(RuntimePlaybackOwnerState::Recording { .. })
        )
    }

    fn transition_pending(&self) -> bool {
        matches!(
            self.state,
            Some(
                RuntimePlaybackOwnerState::PendingStop { .. }
                    | RuntimePlaybackOwnerState::Recovery { .. }
                    | RuntimePlaybackOwnerState::PendingStart(_)
                    | RuntimePlaybackOwnerState::PendingRecordingStart(_)
                    | RuntimePlaybackOwnerState::StopError { .. }
                    | RuntimePlaybackOwnerState::ShutdownPlayback(_)
                    | RuntimePlaybackOwnerState::ShutdownRecovery(_)
                    | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
                    | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
                    | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
                    | RuntimePlaybackOwnerState::ShutdownStopError(_)
            )
        )
    }

    fn start_recording(&mut self, target_index: usize) -> Result<RuntimePlaybackOwnerPoll, String> {
        if !self.output_pinned {
            return Err("recording requires an explicitly pinned audio output".to_owned());
        }
        if !self.recording.output_configured() {
            return Err(RuntimeRecordingError::OutputDirectoryNotConfigured.to_string());
        }
        if !self.recording.has_targets() {
            return Err("the verified project has no audio-track recording target".to_owned());
        }
        let state = self
            .state
            .take()
            .ok_or_else(|| "Runtime playback ownership is unavailable".to_owned())?;
        let RuntimePlaybackOwnerState::Playback(playback) = state else {
            self.state = Some(state);
            return Err(if self.is_recording() {
                "a Runtime recording is already active".to_owned()
            } else {
                runtime_playback_unavailable()
            });
        };
        let recovery_snapshot = playback.recovery_checkpoint().snapshot();
        if recovery_snapshot.state() != PlaybackRecoveryTransportState::Stopped {
            self.state = Some(RuntimePlaybackOwnerState::Playback(playback));
            self.refresh_recording_observation();
            return Err("transport must be stopped before recording".to_owned());
        }
        if recovery_snapshot.loop_enabled() {
            self.state = Some(RuntimePlaybackOwnerState::Playback(playback));
            self.refresh_recording_observation();
            return Err("loop playback must be disabled before recording".to_owned());
        }
        if playback
            .monitoring_input_status()
            .is_none_or(|status| status.state() != StreamState::Running)
        {
            self.state = Some(RuntimePlaybackOwnerState::Playback(playback));
            self.refresh_recording_observation();
            return Err("recording input must be running before recording".to_owned());
        }
        self.playback_available.store(false, Ordering::Release);
        let result = match (*playback).into_recovery() {
            Ok(recovery) => self.advance_recovery(
                recovery,
                RuntimePlaybackRecoveryAction::StartRecording { target_index },
                None,
            ),
            Err(error) => {
                let warning = format!("playback shutdown before recording reported: {error}");
                if terminal_playback_output_stop_failure(error.output.as_ref()) {
                    self.retain_terminal_output_stop_error(error);
                    RuntimePlaybackOwnerPoll {
                        message: Some(format!(
                            "{warning}; terminal output failure prevented recording startup"
                        )),
                        playback_replaced: false,
                        terminal_output_failure: true,
                    }
                } else {
                    match error.into_recovery() {
                        Ok(recovery) => self.advance_recovery(
                            recovery,
                            RuntimePlaybackRecoveryAction::StartRecording { target_index },
                            Some(warning),
                        ),
                        Err(error) => {
                            let action =
                                RuntimePlaybackRecoveryAction::StartRecording { target_index };
                            self.retain_stop_error(error, action);
                            RuntimePlaybackOwnerPoll {
                                message: Some(format!(
                                    "{warning}; waiting for native shutdown before recording"
                                )),
                                playback_replaced: false,
                                terminal_output_failure: false,
                            }
                        }
                    }
                }
            }
        };
        self.refresh_recording_observation();
        Ok(result)
    }

    fn stop_recording(&mut self) -> Result<RuntimePlaybackOwnerPoll, String> {
        let state = self
            .state
            .take()
            .ok_or_else(|| "Runtime playback ownership is unavailable".to_owned())?;
        let RuntimePlaybackOwnerState::Recording { session, capture } = state else {
            self.state = Some(state);
            return Err(if self.transition_pending() {
                runtime_playback_unavailable()
            } else {
                "no Runtime recording is active".to_owned()
            });
        };
        self.playback_available.store(false, Ordering::Release);
        let result = self.finalize_recording(session, capture, None);
        self.refresh_recording_observation();
        Ok(result)
    }

    fn finalize_recording(
        &mut self,
        session: Box<DesktopPlaybackRecordingSession>,
        capture: ActiveRuntimeRecording,
        interruption: Option<RuntimeRecordingInterruption>,
    ) -> RuntimePlaybackOwnerPoll {
        let (context, update_id, target_label) = capture.into_parts();
        let prefix = interruption.map(RuntimeRecordingInterruption::message);
        match (*session).finalize(context) {
            Ok(finalized) => {
                let (recording, recovery) = finalized.into_parts();
                let publication = self.recording.publish(update_id, &recording);
                let message = match publication {
                    Ok(RuntimeRecordingPublication::Published(path)) => format!(
                        "Runtime recording for {target_label} published to {}",
                        path.display()
                    ),
                    Ok(RuntimeRecordingPublication::Uncertain {
                        output_directory,
                        source,
                    }) => format!(
                        "Runtime recording for {target_label} finalized at {}; update publication durability is uncertain and an update may exist in {}: {source}",
                        recording.publication().final_wav_path().display(),
                        output_directory.display()
                    ),
                    Err(error) => format!(
                        "Runtime recording for {target_label} finalized at {} but its update was not published: {error}",
                        recording.publication().final_wav_path().display()
                    ),
                };
                self.accept_finalized_recording_recovery(
                    recovery,
                    join_runtime_messages(prefix.map(str::to_owned), message),
                    interruption,
                )
            }
            Err(error) => {
                let message = format!(
                    "Runtime recording for {target_label} failed during finalization: {error}"
                );
                self.accept_finalized_recording_recovery(
                    error.into_recovery(),
                    join_runtime_messages(prefix.map(str::to_owned), message),
                    interruption,
                )
            }
        }
    }

    fn abort_recording_for_shutdown(&mut self) -> Result<Option<String>, String> {
        let Some(state) = self.state.take() else {
            return Err("Runtime playback ownership is unavailable during shutdown".to_owned());
        };
        self.playback_available.store(false, Ordering::Release);
        let action = runtime_playback_shutdown_action(state.phase());
        let result = match (action, state) {
            (
                RuntimePlaybackShutdownAction::RetainPlayback,
                RuntimePlaybackOwnerState::Playback(playback),
            ) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownPlayback(playback));
                Ok(None)
            }
            (
                RuntimePlaybackShutdownAction::AbortRecording,
                RuntimePlaybackOwnerState::Recording { session, capture },
            ) => self.abort_active_recording_for_shutdown(session, &capture),
            (
                RuntimePlaybackShutdownAction::RetainPendingStop,
                RuntimePlaybackOwnerState::PendingStop { pending, .. },
            ) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownPendingStop(pending));
                Ok(Some(
                    "Runtime shutdown retained pending native teardown; no replacement owner started"
                        .to_owned(),
                ))
            }
            (
                RuntimePlaybackShutdownAction::RetainRecovery,
                RuntimePlaybackOwnerState::Recovery { recovery, .. },
            ) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(recovery));
                Ok(Some(
                    "Runtime shutdown retained stopped playback recovery; no replacement owner started"
                        .to_owned(),
                ))
            }
            (
                RuntimePlaybackShutdownAction::RetainPendingStart,
                RuntimePlaybackOwnerState::PendingStart(pending),
            ) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownPendingStart(pending));
                Ok(Some(
                    "Runtime shutdown retained cancelled playback startup; no replacement owner started"
                        .to_owned(),
                ))
            }
            (
                RuntimePlaybackShutdownAction::RetainPendingStart,
                RuntimePlaybackOwnerState::PendingRecordingStart(pending),
            ) => self.settle_failed_recording_start_for_shutdown(*pending),
            (
                RuntimePlaybackShutdownAction::RetainStopError,
                RuntimePlaybackOwnerState::StopError { error, .. },
            ) => {
                let message =
                    format!("Runtime shutdown retained unresolved playback stop evidence: {error}");
                self.state = Some(RuntimePlaybackOwnerState::ShutdownStopError(error));
                Err(message)
            }
            (
                RuntimePlaybackShutdownAction::AlreadyRetained,
                state @ (RuntimePlaybackOwnerState::ShutdownPlayback(_)
                | RuntimePlaybackOwnerState::ShutdownRecovery(_)
                | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
                | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
                | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
                | RuntimePlaybackOwnerState::ShutdownStopError(_)),
            ) => {
                self.state = Some(state);
                Ok(None)
            }
            _ => unreachable!("Runtime playback shutdown phase and owner state diverged"),
        };
        self.refresh_recording_observation();
        result
    }

    fn settle_failed_recording_start_for_shutdown(
        &mut self,
        pending: DesktopPlaybackRecordingStartRecovery,
    ) -> Result<Option<String>, String> {
        match pending.wait(Duration::from_secs(1)) {
            DesktopPlaybackRecordingStartRecoveryWait::Settled(settlement) => {
                let failed = recording_start_settlement_failed(&settlement);
                let message = format!(
                    "Runtime shutdown settled failed recording startup: {}",
                    describe_recording_start_settlement(&settlement)
                );
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(Box::new(
                    settlement.recovery,
                )));
                if failed {
                    Err(message)
                } else {
                    Ok(Some(message))
                }
            }
            DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) => {
                let journal = pending.journal_path().map_or_else(
                    || "not created".to_owned(),
                    |path| path.display().to_string(),
                );
                self.state = Some(RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(
                    Box::new(pending),
                ));
                Err(format!(
                    "Runtime shutdown could not settle failed recording startup within one second; writer/native outcome unresolved; journal location (not proven recoverable): {journal}; no replacement owner started"
                ))
            }
        }
    }

    fn abort_active_recording_for_shutdown(
        &mut self,
        session: Box<DesktopPlaybackRecordingSession>,
        capture: &ActiveRuntimeRecording,
    ) -> Result<Option<String>, String> {
        let target_label = capture.target_label().to_owned();
        match (*session).abort() {
            Ok(aborted) => {
                let (completion, recovery) = aborted.into_parts();
                let (message, failed) = match completion {
                    RecordingWorkerCompletion::Aborted {
                        frames,
                        journal_path,
                    } => (
                        format!(
                            "Runtime shutdown aborted recording for {target_label} after {frames} frame(s); recovery journal: {}",
                            journal_path.display()
                        ),
                        false,
                    ),
                    RecordingWorkerCompletion::Failed {
                        frames,
                        journal_path,
                        error,
                    } => {
                        let journal = journal_path.map_or_else(
                            || "unavailable".to_owned(),
                            |path| path.display().to_string(),
                        );
                        (
                            format!(
                                "Runtime shutdown settled a failed recording for {target_label} after {frames} frame(s); recovery journal: {journal}: {error}"
                            ),
                            true,
                        )
                    }
                    RecordingWorkerCompletion::Published(recording) => (
                        format!(
                            "Runtime shutdown unexpectedly found an already published recording for {target_label} at {}",
                            recording.final_wav_path().display()
                        ),
                        true,
                    ),
                };
                self.retain_shutdown_recovery(recovery);
                if failed {
                    Err(message)
                } else {
                    Ok(Some(message))
                }
            }
            Err(error) => {
                let message = format!(
                    "Runtime shutdown could not cleanly abort recording for {target_label}: {error}"
                );
                let recovery = error.into_recovery();
                self.retain_shutdown_recovery(recovery);
                Err(message)
            }
        }
    }

    fn retain_shutdown_recovery(
        &mut self,
        recovery: hyper_desktop_audio::DesktopPlaybackRecordingRecovery,
    ) {
        match recovery.into_recovery() {
            Ok(recovery) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(Box::new(
                    recovery,
                )));
            }
            Err(error) => match error.into_pending() {
                Ok(pending) => {
                    self.state = Some(RuntimePlaybackOwnerState::ShutdownPendingStop(Box::new(
                        pending,
                    )));
                }
                Err(error) => {
                    self.state = Some(RuntimePlaybackOwnerState::ShutdownStopError(Box::new(
                        error,
                    )));
                }
            },
        }
    }

    fn poll(&mut self) -> RuntimePlaybackOwnerPoll {
        let Some(state) = self.state.take() else {
            return RuntimePlaybackOwnerPoll {
                message: Some("Runtime playback ownership is unavailable".to_owned()),
                playback_replaced: false,
                terminal_output_failure: false,
            };
        };
        let result = match state {
            state @ (RuntimePlaybackOwnerState::Playback(_)
            | RuntimePlaybackOwnerState::ShutdownPlayback(_)
            | RuntimePlaybackOwnerState::ShutdownRecovery(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
            | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
            | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
            | RuntimePlaybackOwnerState::ShutdownStopError(_)) => {
                self.retain_without_progress(state)
            }
            RuntimePlaybackOwnerState::Recording { session, capture } => {
                match runtime_recording_interruption(&session) {
                    Some(interruption) => {
                        self.playback_available.store(false, Ordering::Release);
                        self.finalize_recording(session, capture, Some(interruption))
                    }
                    None => self.retain_without_progress(RuntimePlaybackOwnerState::Recording {
                        session,
                        capture,
                    }),
                }
            }
            RuntimePlaybackOwnerState::PendingStop { pending, action } => {
                self.poll_pending_stop(pending, action)
            }
            RuntimePlaybackOwnerState::Recovery { recovery, retry_at } => {
                if Instant::now() < retry_at {
                    self.state = Some(RuntimePlaybackOwnerState::Recovery { recovery, retry_at });
                    RuntimePlaybackOwnerPoll {
                        message: None,
                        playback_replaced: false,
                        terminal_output_failure: false,
                    }
                } else {
                    self.restart_playback(*recovery, None)
                }
            }
            RuntimePlaybackOwnerState::PendingStart(pending) => self.poll_pending_start(pending),
            RuntimePlaybackOwnerState::PendingRecordingStart(pending) => {
                self.poll_pending_recording_start(*pending)
            }
            RuntimePlaybackOwnerState::StopError { error, action } => {
                match (*error).into_recovery() {
                    Ok(recovery) => self.advance_recovery(recovery, action, None),
                    Err(error) => {
                        self.retain_stop_error(error, action);
                        RuntimePlaybackOwnerPoll {
                            message: None,
                            playback_replaced: false,
                            terminal_output_failure: false,
                        }
                    }
                }
            }
        };
        self.refresh_recording_observation();
        result
    }

    fn retain_without_progress(
        &mut self,
        state: RuntimePlaybackOwnerState,
    ) -> RuntimePlaybackOwnerPoll {
        self.state = Some(state);
        RuntimePlaybackOwnerPoll {
            message: None,
            playback_replaced: false,
            terminal_output_failure: false,
        }
    }

    fn poll_pending_stop(
        &mut self,
        pending: Box<DesktopPlaybackRecoveryPendingStop>,
        action: RuntimePlaybackRecoveryAction,
    ) -> RuntimePlaybackOwnerPoll {
        match (*pending).wait(Duration::ZERO) {
            DesktopPlaybackRecoveryPendingStopWait::TimedOut(pending) => self
                .retain_without_progress(RuntimePlaybackOwnerState::PendingStop {
                    pending: Box::new(pending),
                    action,
                }),
            DesktopPlaybackRecoveryPendingStopWait::Completed(recovery) => {
                self.advance_recovery(recovery, action, None)
            }
            DesktopPlaybackRecoveryPendingStopWait::Failed(failure) => self
                .settle_pending_stop_failure(
                    failure,
                    action,
                    "native shutdown failed before the playback-owner transition",
                ),
            DesktopPlaybackRecoveryPendingStopWait::Panicked(failure) => self
                .settle_pending_stop_failure(
                    failure,
                    action,
                    "native shutdown panicked before the playback-owner transition",
                ),
        }
    }

    fn settle_pending_stop_failure(
        &mut self,
        failure: DesktopPlaybackRecoveryPendingStopFailure,
        action: RuntimePlaybackRecoveryAction,
        message: &'static str,
    ) -> RuntimePlaybackOwnerPoll {
        let output_shutdown_failed = failure.output_shutdown_failed();
        let recovery = failure.into_recovery();
        match route_pending_stop_recovery(recovery, action, output_shutdown_failed) {
            RuntimePendingStopRecoveryRoute::RetainForShutdown(recovery) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(Box::new(
                    recovery,
                )));
                RuntimePlaybackOwnerPoll {
                    message: Some(format!(
                        "{message}; Runtime shutdown retained playback recovery"
                    )),
                    playback_replaced: false,
                    terminal_output_failure: true,
                }
            }
            RuntimePendingStopRecoveryRoute::Continue { recovery, action } => {
                self.advance_recovery(recovery, action, Some(message.to_owned()))
            }
        }
    }

    fn poll_pending_start(
        &mut self,
        pending: Box<DesktopPlaybackRecoveryPendingStart>,
    ) -> RuntimePlaybackOwnerPoll {
        match (*pending).wait(Duration::ZERO) {
            DesktopPlaybackRecoveryPendingWait::TimedOut(pending) => self.retain_without_progress(
                RuntimePlaybackOwnerState::PendingStart(Box::new(pending)),
            ),
            DesktopPlaybackRecoveryPendingWait::Completed(recovery) => {
                self.restart_playback(recovery, None)
            }
            DesktopPlaybackRecoveryPendingWait::Failed(recovery) => self.restart_playback(
                recovery,
                Some("cancelled playback restart ended with a native failure".to_owned()),
            ),
            DesktopPlaybackRecoveryPendingWait::Panicked(recovery) => self.restart_playback(
                recovery,
                Some("cancelled playback restart ended with a native panic".to_owned()),
            ),
        }
    }

    fn poll_pending_recording_start(
        &mut self,
        pending: DesktopPlaybackRecordingStartRecovery,
    ) -> RuntimePlaybackOwnerPoll {
        self.settle_recording_start(pending.wait(Duration::ZERO), None)
    }

    fn settle_recording_start(
        &mut self,
        outcome: DesktopPlaybackRecordingStartRecoveryWait,
        prefix: Option<String>,
    ) -> RuntimePlaybackOwnerPoll {
        match outcome {
            DesktopPlaybackRecordingStartRecoveryWait::TimedOut(pending) => {
                self.state = Some(RuntimePlaybackOwnerState::PendingRecordingStart(Box::new(
                    pending,
                )));
                RuntimePlaybackOwnerPoll {
                    message: prefix.map(|prefix| format!(
                        "{prefix}; waiting for failed recording startup to release native and writer ownership"
                    )),
                    playback_replaced: false,
                    terminal_output_failure: false,
                }
            }
            DesktopPlaybackRecordingStartRecoveryWait::Settled(settlement) => {
                self.restart_settled_recording_start(settlement, prefix)
            }
        }
    }

    fn restart_settled_recording_start(
        &mut self,
        settlement: DesktopPlaybackRecordingStartSettlement,
        prefix: Option<String>,
    ) -> RuntimePlaybackOwnerPoll {
        let output_failed = settlement.output != DesktopPlaybackRecordingNativeSettlement::Clean;
        let message =
            join_runtime_messages(prefix, describe_recording_start_settlement(&settlement));
        match route_pending_stop_recovery(
            settlement.recovery,
            RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
            output_failed,
        ) {
            RuntimePendingStopRecoveryRoute::RetainForShutdown(recovery) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(Box::new(
                    recovery,
                )));
                RuntimePlaybackOwnerPoll {
                    message: Some(format!(
                        "{message}; terminal output failure prevented playback restart"
                    )),
                    playback_replaced: false,
                    terminal_output_failure: true,
                }
            }
            RuntimePendingStopRecoveryRoute::Continue { recovery, .. } => {
                self.restart_playback(recovery, Some(message))
            }
        }
    }

    fn advance_recovery(
        &mut self,
        recovery: DesktopPlaybackRecovery,
        action: RuntimePlaybackRecoveryAction,
        prefix: Option<String>,
    ) -> RuntimePlaybackOwnerPoll {
        match action {
            RuntimePlaybackRecoveryAction::StartRecording { target_index } => {
                let captured_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                let prepared = match self.recording.prepare(target_index, &recovery, captured_at) {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        return self.restart_playback(
                            recovery,
                            Some(join_runtime_messages(
                                prefix,
                                format!("Runtime recording preparation failed: {error}"),
                            )),
                        );
                    }
                };
                let target_label = prepared.target_label().to_owned();
                match recovery.start_recording(prepared.session()) {
                    Ok(session) => {
                        self.state = Some(RuntimePlaybackOwnerState::Recording {
                            session: Box::new(session),
                            capture: prepared.into_active(),
                        });
                        self.playback_available.store(true, Ordering::Release);
                        RuntimePlaybackOwnerPoll {
                            message: Some(join_runtime_messages(
                                prefix,
                                format!("Runtime recording started for {target_label}"),
                            )),
                            playback_replaced: true,
                            terminal_output_failure: false,
                        }
                    }
                    Err(error) => {
                        let message = join_runtime_messages(
                            prefix,
                            format!("Runtime recording start failed: {error}"),
                        );
                        self.settle_recording_start(
                            error.into_recovery().wait(Duration::ZERO),
                            Some(message),
                        )
                    }
                }
            }
            RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback => {
                self.restart_playback(recovery, prefix)
            }
        }
    }

    fn accept_recording_recovery(
        &mut self,
        recovery: hyper_desktop_audio::DesktopPlaybackRecordingRecovery,
        message: String,
    ) -> RuntimePlaybackOwnerPoll {
        match recovery.into_recovery() {
            Ok(recovery) => self.restart_playback(recovery, Some(message)),
            Err(error) => match error.into_pending() {
                Ok(pending) => {
                    self.state = Some(RuntimePlaybackOwnerState::PendingStop {
                        pending: Box::new(pending),
                        action: RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
                    });
                    RuntimePlaybackOwnerPoll {
                        message: Some(format!(
                            "{message}; waiting for native shutdown before restoring playback"
                        )),
                        playback_replaced: false,
                        terminal_output_failure: false,
                    }
                }
                Err(error) => {
                    self.state = Some(RuntimePlaybackOwnerState::StopError {
                        error: Box::new(error),
                        action: RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
                    });
                    RuntimePlaybackOwnerPoll {
                        message: Some(format!(
                            "{message}; playback recovery remains retained after an inconsistent stop outcome"
                        )),
                        playback_replaced: false,
                        terminal_output_failure: false,
                    }
                }
            },
        }
    }

    fn accept_finalized_recording_recovery(
        &mut self,
        recovery: hyper_desktop_audio::DesktopPlaybackRecordingRecovery,
        message: String,
        interruption: Option<RuntimeRecordingInterruption>,
    ) -> RuntimePlaybackOwnerPoll {
        let output_shutdown_failed = recovery.output_shutdown_failed();
        let terminal_output_failure = output_shutdown_failed
            || interruption.is_some_and(RuntimeRecordingInterruption::output_failed);
        match route_finalized_recording_recovery(recovery, interruption, output_shutdown_failed) {
            RuntimeFinalizedRecordingRecoveryRoute::RetainForShutdown(recovery) => {
                self.retain_shutdown_recovery(recovery);
                RuntimePlaybackOwnerPoll {
                    message: Some(message),
                    playback_replaced: false,
                    terminal_output_failure,
                }
            }
            RuntimeFinalizedRecordingRecoveryRoute::RestorePlayback(recovery) => {
                self.accept_recording_recovery(recovery, message)
            }
        }
    }

    fn retain_stop_error(
        &mut self,
        error: DesktopPlaybackRecoveryStopError,
        action: RuntimePlaybackRecoveryAction,
    ) {
        match error.into_pending() {
            Ok(pending) => {
                self.state = Some(RuntimePlaybackOwnerState::PendingStop {
                    pending: Box::new(pending),
                    action,
                });
            }
            Err(error) => {
                self.state = Some(RuntimePlaybackOwnerState::StopError {
                    error: Box::new(error),
                    action,
                });
            }
        }
    }

    fn retain_terminal_output_stop_error(&mut self, error: DesktopPlaybackRecoveryStopError) {
        match error.into_recovery() {
            Ok(recovery) => {
                self.state = Some(RuntimePlaybackOwnerState::ShutdownRecovery(Box::new(
                    recovery,
                )));
            }
            Err(error) => match error.into_pending() {
                Ok(pending) => {
                    self.state = Some(RuntimePlaybackOwnerState::ShutdownPendingStop(Box::new(
                        pending,
                    )));
                }
                Err(error) => {
                    self.state = Some(RuntimePlaybackOwnerState::ShutdownStopError(Box::new(
                        error,
                    )));
                }
            },
        }
    }

    fn restart_playback(
        &mut self,
        mut recovery: DesktopPlaybackRecovery,
        prefix: Option<String>,
    ) -> RuntimePlaybackOwnerPoll {
        match recovery.restart() {
            Ok(playback) => {
                self.state = Some(RuntimePlaybackOwnerState::Playback(Box::new(playback)));
                self.playback_available.store(true, Ordering::Release);
                RuntimePlaybackOwnerPoll {
                    message: Some(join_runtime_messages(
                        prefix,
                        "Runtime playback restored".to_owned(),
                    )),
                    playback_replaced: true,
                    terminal_output_failure: false,
                }
            }
            Err(error) => {
                let message = join_runtime_messages(
                    prefix,
                    format!("Runtime playback restart failed: {error}"),
                );
                if let Some(pending) = error.into_pending() {
                    self.state = Some(RuntimePlaybackOwnerState::PendingStart(Box::new(pending)));
                } else {
                    self.state = Some(RuntimePlaybackOwnerState::Recovery {
                        recovery: Box::new(recovery),
                        retry_at: Instant::now() + Duration::from_secs(1),
                    });
                }
                RuntimePlaybackOwnerPoll {
                    message: Some(message),
                    playback_replaced: false,
                    terminal_output_failure: false,
                }
            }
        }
    }

    fn refresh_recording_observation(&mut self) {
        let (state, start_enabled, stop_enabled) = match self.state.as_ref() {
            Some(RuntimePlaybackOwnerState::Playback(playback)) => {
                let checkpoint = playback.recovery_checkpoint().snapshot();
                let input_running = playback
                    .monitoring_input_status()
                    .is_some_and(|status| status.state() == StreamState::Running);
                let ready = self.output_pinned
                    && self.recording.output_configured()
                    && self.recording.has_targets()
                    && input_running
                    && checkpoint.state() == PlaybackRecoveryTransportState::Stopped
                    && !checkpoint.loop_enabled();
                let state = if ready {
                    "Ready to record a verified audio track".to_owned()
                } else if !self.recording.output_configured() {
                    initial_runtime_recording_state(&self.recording, self.output_pinned)
                } else if !self.output_pinned {
                    "Recording unavailable: audio output is not explicitly pinned".to_owned()
                } else if !self.recording.has_targets() {
                    "Recording unavailable: project has no audio track".to_owned()
                } else if !input_running {
                    "Recording unavailable: monitored input is not running".to_owned()
                } else if checkpoint.loop_enabled() {
                    "Recording unavailable: loop playback is enabled".to_owned()
                } else {
                    "Stop transport to begin recording".to_owned()
                };
                (state, ready, false)
            }
            Some(RuntimePlaybackOwnerState::Recording { session, capture }) => {
                let frames = session.recording_snapshot().accepted_frame_count;
                (
                    format!(
                        "Recording {} · {frames} accepted frame(s)",
                        capture.target_label()
                    ),
                    false,
                    true,
                )
            }
            Some(RuntimePlaybackOwnerState::PendingStop { .. }) => (
                "Waiting for native shutdown; no replacement owner has started".to_owned(),
                false,
                false,
            ),
            Some(RuntimePlaybackOwnerState::Recovery { .. }) => (
                "Recording stopped; retrying playback recovery".to_owned(),
                false,
                false,
            ),
            Some(RuntimePlaybackOwnerState::PendingStart(_)) => (
                "Waiting for cancelled playback restart to release native ownership".to_owned(),
                false,
                false,
            ),
            Some(RuntimePlaybackOwnerState::PendingRecordingStart(_)) => (
                "Waiting for failed recording startup to settle native and writer ownership"
                    .to_owned(),
                false,
                false,
            ),
            Some(RuntimePlaybackOwnerState::StopError { .. }) => (
                "Playback shutdown authority is retained; no replacement owner has started"
                    .to_owned(),
                false,
                false,
            ),
            Some(
                state @ (RuntimePlaybackOwnerState::ShutdownPlayback(_)
                | RuntimePlaybackOwnerState::ShutdownRecovery(_)
                | RuntimePlaybackOwnerState::ShutdownPendingStop(_)
                | RuntimePlaybackOwnerState::ShutdownPendingStart(_)
                | RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(_)
                | RuntimePlaybackOwnerState::ShutdownStopError(_)),
            ) => (runtime_shutdown_recording_observation(state), false, false),
            None => (
                "Runtime playback ownership is unavailable".to_owned(),
                false,
                false,
            ),
        };
        if let Ok(mut observation) = self.recording_observation.lock() {
            *observation = RuntimeRecordingObservation {
                state,
                start_enabled,
                stop_enabled,
            };
        }
    }
}

fn runtime_shutdown_recording_observation(state: &RuntimePlaybackOwnerState) -> String {
    match state {
        RuntimePlaybackOwnerState::ShutdownPlayback(playback) => format!(
            "Runtime shutdown retains the final playback owner at frame {} until release",
            playback.recovery_checkpoint().snapshot().position_frame()
        ),
        RuntimePlaybackOwnerState::ShutdownRecovery(recovery) => format!(
            "Runtime recording stopped for shutdown at frame {}",
            recovery.checkpoint().position_frame()
        ),
        RuntimePlaybackOwnerState::ShutdownPendingStop(pending) => format!(
            "Runtime shutdown retains pending native ownership (input={:?}, output={:?})",
            pending.input_status().map(|status| status.state()),
            pending.output_status().map(|status| status.state())
        ),
        RuntimePlaybackOwnerState::ShutdownPendingStart(pending) => format!(
            "Runtime shutdown retains cancelled native startup (input={:?}, output={:?})",
            pending.input_status().map(|status| status.state()),
            pending.output_status().map(|status| status.state())
        ),
        RuntimePlaybackOwnerState::ShutdownPendingRecordingStart(pending) => format!(
            "Runtime shutdown retains failed recording startup and writer ownership; journal: {}",
            pending.journal_path().map_or_else(
                || "not created".to_owned(),
                |path| path.display().to_string()
            )
        ),
        RuntimePlaybackOwnerState::ShutdownStopError(error) => {
            format!("Runtime shutdown retains playback stop evidence: {error}")
        }
        RuntimePlaybackOwnerState::Playback(_)
        | RuntimePlaybackOwnerState::Recording { .. }
        | RuntimePlaybackOwnerState::PendingStop { .. }
        | RuntimePlaybackOwnerState::Recovery { .. }
        | RuntimePlaybackOwnerState::PendingStart(_)
        | RuntimePlaybackOwnerState::PendingRecordingStart(_)
        | RuntimePlaybackOwnerState::StopError { .. } => {
            unreachable!("only shutdown-retained owners have shutdown observations")
        }
    }
}

fn initial_runtime_recording_state(
    recording: &RuntimeRecordingConfiguration,
    output_pinned: bool,
) -> String {
    if !recording.output_configured() {
        "Recording unavailable: no external recording-update directory".to_owned()
    } else if !output_pinned {
        "Recording unavailable: audio output is not explicitly pinned".to_owned()
    } else if !recording.has_targets() {
        "Recording unavailable: project has no audio track".to_owned()
    } else {
        "Waiting for committed Runtime playback recovery".to_owned()
    }
}

fn describe_recording_start_settlement(
    settlement: &DesktopPlaybackRecordingStartSettlement,
) -> String {
    let writer = match settlement.writer.as_ref() {
        Some(DesktopRecordingWriterSettlement::Completed(completion)) => {
            match completion.as_ref() {
                RecordingWorkerCompletion::Aborted {
                    frames,
                    journal_path,
                } => format!(
                    "recording writer aborted after {frames} frame(s); journal: {}",
                    journal_path.display()
                ),
                RecordingWorkerCompletion::Failed {
                    frames,
                    journal_path,
                    error,
                } => format!(
                    "recording writer failed after {frames} frame(s); journal: {}; {error}",
                    journal_path.as_ref().map_or_else(
                        || "unavailable".to_owned(),
                        |path| path.display().to_string()
                    )
                ),
                RecordingWorkerCompletion::Published(recording) => format!(
                    "recording writer unexpectedly published {}",
                    recording.final_wav_path().display()
                ),
            }
        }
        Some(DesktopRecordingWriterSettlement::Failed(error)) => format!(
            "recording writer settlement failed for {}: {error}",
            settlement.journal_path.as_ref().map_or_else(
                || "unknown journal".to_owned(),
                |path| path.display().to_string()
            )
        ),
        Some(DesktopRecordingWriterSettlement::Pending(_)) => {
            unreachable!("settled recording startup still owns a pending writer")
        }
        None => "recording writer was not started".to_owned(),
    };
    let input = match settlement.input {
        DesktopPlaybackRecordingNativeSettlement::Clean => "input teardown settled",
        DesktopPlaybackRecordingNativeSettlement::Failed => "input teardown failed",
        DesktopPlaybackRecordingNativeSettlement::Panicked => "input teardown panicked",
    };
    let output = match settlement.output {
        DesktopPlaybackRecordingNativeSettlement::Clean => "output teardown settled",
        DesktopPlaybackRecordingNativeSettlement::Failed => "output teardown failed",
        DesktopPlaybackRecordingNativeSettlement::Panicked => "output teardown panicked",
    };
    format!("{writer}; {input}; {output}")
}

fn recording_start_settlement_failed(settlement: &DesktopPlaybackRecordingStartSettlement) -> bool {
    let writer_failed = match settlement.writer.as_ref() {
        Some(DesktopRecordingWriterSettlement::Failed(_)) => true,
        Some(DesktopRecordingWriterSettlement::Completed(completion)) => matches!(
            completion.as_ref(),
            RecordingWorkerCompletion::Failed { .. } | RecordingWorkerCompletion::Published(_)
        ),
        Some(DesktopRecordingWriterSettlement::Pending(_)) | None => false,
    };
    settlement.input != DesktopPlaybackRecordingNativeSettlement::Clean
        || settlement.output != DesktopPlaybackRecordingNativeSettlement::Clean
        || writer_failed
}

fn join_runtime_messages(prefix: Option<String>, message: String) -> String {
    match prefix {
        Some(prefix) => format!("{prefix}; {message}"),
        None => message,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeRecordingInputInterruption {
    Unavailable,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeRecordingInterruption {
    input: Option<RuntimeRecordingInputInterruption>,
    output_failed: bool,
}

impl RuntimeRecordingInterruption {
    fn message(self) -> &'static str {
        match (self.input, self.output_failed) {
            (Some(RuntimeRecordingInputInterruption::Unavailable), false) => {
                "Runtime recording stopped automatically because input ownership became unavailable"
            }
            (Some(RuntimeRecordingInputInterruption::Failed), false) => {
                "Runtime recording stopped automatically because native input ended unexpectedly"
            }
            (None, true) => {
                "Runtime recording stopped automatically because native output ended unexpectedly"
            }
            (Some(RuntimeRecordingInputInterruption::Unavailable), true) => {
                "Runtime recording stopped automatically because input ownership became unavailable and native output ended unexpectedly"
            }
            (Some(RuntimeRecordingInputInterruption::Failed), true) => {
                "Runtime recording stopped automatically because native input and output ended unexpectedly"
            }
            (None, false) => unreachable!("an interruption requires terminal native evidence"),
        }
    }

    const fn output_failed(self) -> bool {
        self.output_failed
    }
}

fn classify_runtime_recording_interruption(
    input: Option<StreamState>,
    output: OutputStreamState,
) -> Option<RuntimeRecordingInterruption> {
    let input = match input {
        None => Some(RuntimeRecordingInputInterruption::Unavailable),
        Some(StreamState::Failed) => Some(RuntimeRecordingInputInterruption::Failed),
        Some(_) => None,
    };
    let output_failed = output == OutputStreamState::Failed;
    if input.is_none() && !output_failed {
        None
    } else {
        Some(RuntimeRecordingInterruption {
            input,
            output_failed,
        })
    }
}

fn runtime_recording_interruption(
    session: &DesktopPlaybackRecordingSession,
) -> Option<RuntimeRecordingInterruption> {
    classify_runtime_recording_interruption(
        session
            .playback()
            .monitoring_input_status()
            .map(|status| status.state()),
        session.playback().output_status().state(),
    )
}

fn terminal_playback_output_stop_failure(output: Option<&DesktopOutputStopError>) -> bool {
    matches!(
        output,
        Some(DesktopOutputStopError::RuntimeFailed | DesktopOutputStopError::ThreadPanicked)
    )
}

enum RuntimeFinalizedRecordingRecoveryRoute<T> {
    RestorePlayback(T),
    RetainForShutdown(T),
}

enum RuntimePendingStopRecoveryRoute<T> {
    Continue {
        recovery: T,
        action: RuntimePlaybackRecoveryAction,
    },
    RetainForShutdown(T),
}

fn route_finalized_recording_recovery<T>(
    recovery: T,
    interruption: Option<RuntimeRecordingInterruption>,
    output_shutdown_failed: bool,
) -> RuntimeFinalizedRecordingRecoveryRoute<T> {
    if output_shutdown_failed
        || interruption.is_some_and(RuntimeRecordingInterruption::output_failed)
    {
        RuntimeFinalizedRecordingRecoveryRoute::RetainForShutdown(recovery)
    } else {
        RuntimeFinalizedRecordingRecoveryRoute::RestorePlayback(recovery)
    }
}

fn route_pending_stop_recovery<T>(
    recovery: T,
    action: RuntimePlaybackRecoveryAction,
    output_shutdown_failed: bool,
) -> RuntimePendingStopRecoveryRoute<T> {
    if output_shutdown_failed {
        RuntimePendingStopRecoveryRoute::RetainForShutdown(recovery)
    } else {
        RuntimePendingStopRecoveryRoute::Continue { recovery, action }
    }
}

enum RuntimePlaybackOwnerState {
    Playback(Box<DesktopPlaybackSession>),
    Recording {
        session: Box<DesktopPlaybackRecordingSession>,
        capture: ActiveRuntimeRecording,
    },
    PendingStop {
        pending: Box<DesktopPlaybackRecoveryPendingStop>,
        action: RuntimePlaybackRecoveryAction,
    },
    Recovery {
        recovery: Box<DesktopPlaybackRecovery>,
        retry_at: Instant,
    },
    PendingStart(Box<DesktopPlaybackRecoveryPendingStart>),
    PendingRecordingStart(Box<DesktopPlaybackRecordingStartRecovery>),
    StopError {
        error: Box<DesktopPlaybackRecoveryStopError>,
        action: RuntimePlaybackRecoveryAction,
    },
    ShutdownPlayback(Box<DesktopPlaybackSession>),
    ShutdownRecovery(Box<DesktopPlaybackRecovery>),
    ShutdownPendingStop(Box<DesktopPlaybackRecoveryPendingStop>),
    ShutdownPendingStart(Box<DesktopPlaybackRecoveryPendingStart>),
    ShutdownPendingRecordingStart(Box<DesktopPlaybackRecordingStartRecovery>),
    ShutdownStopError(Box<DesktopPlaybackRecoveryStopError>),
}

impl RuntimePlaybackOwnerState {
    const fn phase(&self) -> RuntimePlaybackOwnerPhase {
        match self {
            Self::Playback(_) => RuntimePlaybackOwnerPhase::Playback,
            Self::Recording { .. } => RuntimePlaybackOwnerPhase::Recording,
            Self::PendingStop { .. } => RuntimePlaybackOwnerPhase::PendingStop,
            Self::Recovery { .. } => RuntimePlaybackOwnerPhase::Recovery,
            Self::PendingStart(_) | Self::PendingRecordingStart(_) => {
                RuntimePlaybackOwnerPhase::PendingStart
            }
            Self::StopError { .. } => RuntimePlaybackOwnerPhase::StopError,
            Self::ShutdownPlayback(_)
            | Self::ShutdownRecovery(_)
            | Self::ShutdownPendingStop(_)
            | Self::ShutdownPendingStart(_)
            | Self::ShutdownPendingRecordingStart(_)
            | Self::ShutdownStopError(_) => RuntimePlaybackOwnerPhase::ShutdownRetained,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimePlaybackOwnerPhase {
    Playback,
    Recording,
    PendingStop,
    Recovery,
    PendingStart,
    StopError,
    ShutdownRetained,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimePlaybackShutdownAction {
    RetainPlayback,
    AbortRecording,
    RetainPendingStop,
    RetainRecovery,
    RetainPendingStart,
    RetainStopError,
    AlreadyRetained,
}

const fn runtime_playback_shutdown_action(
    phase: RuntimePlaybackOwnerPhase,
) -> RuntimePlaybackShutdownAction {
    match phase {
        RuntimePlaybackOwnerPhase::Playback => RuntimePlaybackShutdownAction::RetainPlayback,
        RuntimePlaybackOwnerPhase::Recording => RuntimePlaybackShutdownAction::AbortRecording,
        RuntimePlaybackOwnerPhase::PendingStop => RuntimePlaybackShutdownAction::RetainPendingStop,
        RuntimePlaybackOwnerPhase::Recovery => RuntimePlaybackShutdownAction::RetainRecovery,
        RuntimePlaybackOwnerPhase::PendingStart => {
            RuntimePlaybackShutdownAction::RetainPendingStart
        }
        RuntimePlaybackOwnerPhase::StopError => RuntimePlaybackShutdownAction::RetainStopError,
        RuntimePlaybackOwnerPhase::ShutdownRetained => {
            RuntimePlaybackShutdownAction::AlreadyRetained
        }
    }
}

#[derive(Clone, Copy)]
enum RuntimePlaybackRecoveryAction {
    StartRecording { target_index: usize },
    RestoreFinalizedRecordingPlayback,
}

#[derive(Debug)]
struct RuntimePlaybackOwnerPoll {
    message: Option<String>,
    playback_replaced: bool,
    terminal_output_failure: bool,
}

struct RuntimeRecordingControlOutcome {
    message: Option<String>,
    terminal_output_failure: bool,
    terminal_owner_failure: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeRecordingObservation {
    state: String,
    start_enabled: bool,
    stop_enabled: bool,
}

struct SessionHost {
    owner: RuntimePlaybackOwner,
    mixer_channel_count: usize,
    audio_observers: RuntimeAudioObservers,
    meter_receivers: RuntimeMeterReceivers,
    automation_capture: RuntimeAutomationCaptureController,
    mixer_update: RuntimeMixerUpdateController,
    native_device_update: RuntimeNativeDeviceUpdateController,
    loop_update: RuntimeLoopUpdateController,
    scene_performance: RuntimeScenePerformanceCaptureController,
    panel_connected: Arc<AtomicBool>,
}

impl PanelHost for SessionHost {
    type Error = DesktopControlSinkError;

    fn mixer_channel_count(&self) -> usize {
        self.mixer_channel_count
    }

    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
        if self.owner.is_recording() || self.owner.transition_pending() {
            return Err(PanelConnectError::CapacityUnavailable);
        }
        let connection = self
            .owner
            .playback_mut()
            .ok_or(PanelConnectError::CapacityUnavailable)?
            .attach_panel(descriptor, bindings)?;
        self.panel_connected.store(true, Ordering::Release);
        Ok(connection)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        let Some(playback) = self.owner.playback_mut() else {
            self.panel_connected.store(false, Ordering::Release);
            return Vec::new();
        };
        let failures = playback.detach_panel();
        self.panel_connected
            .store(playback.has_panel(), Ordering::Release);
        failures
    }

    fn panel_connected(&self) -> bool {
        self.owner
            .playback()
            .is_some_and(DesktopPlaybackSession::has_panel)
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        let Some(playback) = self.owner.playback_mut() else {
            return Ok(PanelDispatchOutcome::Unbound(
                UnboundReason::NoPanelConnected,
            ));
        };
        playback.deliver_panel_message(message)
    }

    fn panel_display(&mut self, out: &mut Vec<PanelDisplay>) {
        if let Some(playback) = self.owner.playback_mut() {
            playback.panel_display(out);
        }
    }
}

impl SessionHost {
    fn refresh_audio_observers(&self) {
        if let Some(playback) = self.owner.playback() {
            self.audio_observers.rebind(playback);
        }
    }

    fn refresh_meter_receivers(&mut self) -> Result<(), &'static str> {
        let playback = self
            .owner
            .playback_mut()
            .ok_or("Runtime playback authority is temporarily unavailable")?;
        let output = playback
            .take_meter_receiver()
            .ok_or("Runtime output meter authority was already transferred")?;
        let input = playback.take_monitoring_input_meter_receiver();
        self.meter_receivers.replace(output, input);
        Ok(())
    }

    fn refresh_playback_generation(&mut self) -> Result<(), String> {
        let playback = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        let scene_receiver = playback.take_scene_launch_receiver().ok_or_else(|| {
            "replacement playback exposed no committed scene-launch receiver".to_owned()
        })?;
        self.scene_performance
            .rebind_receiver(scene_receiver)
            .map_err(|error| format!("scene-performance receiver rebind failed: {error}"))?;
        let parameter_receiver = playback.take_mixer_parameter_receiver().ok_or_else(|| {
            "replacement playback exposed no committed mixer-parameter receiver".to_owned()
        })?;
        self.automation_capture
            .rebind_receiver(parameter_receiver)
            .map_err(|error| format!("automation receiver rebind failed: {error}"))?;
        self.refresh_audio_observers();
        self.refresh_meter_receivers().map_err(str::to_owned)
    }

    fn finish_owner_poll(
        &mut self,
        poll: RuntimePlaybackOwnerPoll,
    ) -> RuntimeRecordingControlOutcome {
        let message = poll.message.clone();
        let terminal_output_failure = poll.terminal_output_failure;
        match finish_runtime_playback_owner_poll(poll, || self.refresh_playback_generation()) {
            Ok(poll) => RuntimeRecordingControlOutcome {
                message: poll.message,
                terminal_output_failure: poll.terminal_output_failure,
                terminal_owner_failure: None,
            },
            Err(error) => RuntimeRecordingControlOutcome {
                message,
                terminal_output_failure,
                terminal_owner_failure: Some(format!(
                    "Runtime playback receiver transfer failed: {error}"
                )),
            },
        }
    }

    fn refresh_recording_blocker_observation(&self) {
        if self.owner.is_recording() || self.owner.transition_pending() {
            return;
        }
        let Some(blocker) = self.recording_transition_blocker() else {
            return;
        };
        if let Ok(mut observation) = self.owner.recording_observation.lock()
            && observation.start_enabled
        {
            observation.state = format!("Recording transition blocked by {blocker}");
            observation.start_enabled = false;
        }
    }

    /// Returns the first owner whose in-flight work prevents a playback-owner
    /// transition. The caller must run this before native teardown because a
    /// replacement session cannot complete any fence or capture owned by the
    /// current playback generation.
    fn recording_transition_blocker(&self) -> Option<&'static str> {
        if self.owner.transition_pending() {
            Some("a playback-owner transition is already pending")
        } else if self
            .owner
            .playback()
            .is_some_and(DesktopPlaybackSession::has_panel)
        {
            Some("an external panel is connected")
        } else if !self.scene_performance.is_idle() {
            Some("scene-performance capture is active or stopping")
        } else if !self.automation_capture.is_idle() {
            Some("automation capture is active or stopping")
        } else if !self.mixer_update.is_idle() {
            Some("a mixer-update export is pending")
        } else if !self.native_device_update.is_idle() {
            Some("a native-device update export is pending")
        } else if !self.loop_update.is_idle() {
            Some("a loop-update export is pending")
        } else {
            None
        }
    }
}

fn finish_runtime_playback_owner_poll(
    poll: RuntimePlaybackOwnerPoll,
    refresh_playback_generation: impl FnOnce() -> Result<(), String>,
) -> Result<RuntimePlaybackOwnerPoll, String> {
    if poll.playback_replaced {
        refresh_playback_generation()?;
    }
    Ok(poll)
}

impl RuntimeRecordingControl for SessionHost {
    fn start_recording(
        &mut self,
        target_index: usize,
    ) -> Result<RuntimeRecordingControlOutcome, String> {
        if let Some(blocker) = self.recording_transition_blocker() {
            return Err(format!("recording transition is blocked by {blocker}"));
        }
        let poll = self.owner.start_recording(target_index)?;
        let outcome = self.finish_owner_poll(poll);
        if outcome.message.is_none() && outcome.terminal_owner_failure.is_none() {
            return Err("recording transition produced no operator outcome".to_owned());
        }
        Ok(outcome)
    }

    fn stop_recording(&mut self) -> Result<RuntimeRecordingControlOutcome, String> {
        if let Some(blocker) = self.recording_transition_blocker() {
            return Err(format!("recording transition is blocked by {blocker}"));
        }
        let poll = self.owner.stop_recording()?;
        let outcome = self.finish_owner_poll(poll);
        if outcome.message.is_none() && outcome.terminal_owner_failure.is_none() {
            return Err("recording transition produced no operator outcome".to_owned());
        }
        Ok(outcome)
    }

    fn poll_recording(&mut self) -> RuntimeRecordingControlOutcome {
        let poll = self.owner.poll();
        let result = self.finish_owner_poll(poll);
        self.refresh_recording_blocker_observation();
        result
    }

    fn abort_recording_for_shutdown(&mut self) -> Result<Option<String>, String> {
        self.owner.abort_recording_for_shutdown()
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hyper-runtime: {message}");
            ExitCode::FAILURE
        }
    }
}

const RUNTIME_HELP: &str = r"Hyper Module Runtime

Usage:
  hyper-runtime -h | --help
  hyper-runtime -V | --version
  hyper-runtime --list-audio-outputs
  hyper-runtime --list-audio-inputs
  hyper-runtime <DEPLOYMENT_DIRECTORY> [OPTIONS]

Arguments:
  DEPLOYMENT_DIRECTORY                 Verified Runtime deployment target

Options:
  -h, --help                         Show this help and exit
  -V, --version                      Show the package version and exit
  --list-audio-outputs               List current desktop output routes and exit
  --list-audio-inputs                List current desktop input routes and exit
  --audio-output ROUTING_ID          Pin one exact discovered output route
  --audio-input ROUTING_ID           Select one exact input for monitor/record
  --panel-listen ADDRESS             Listen for one external control panel
  --recording-updates DIRECTORY      Publish recording handoffs outside deployment
  --performance-updates DIRECTORY    Publish scene-performance handoffs
  --automation-updates DIRECTORY     Publish automation handoffs
  --mixer-updates DIRECTORY          Publish live mixer-state handoffs
  --native-device-updates DIRECTORY  Publish live native-device handoffs
  --loop-updates DIRECTORY           Publish live loop-range handoffs
  --operator-ui                      Open the desktop operator window";

const RUNTIME_VERSION: &str = concat!("hyper-runtime ", env!("CARGO_PKG_VERSION"));

fn run() -> Result<(), String> {
    match parse_runtime_invocation(std::env::args().skip(1))
        .map_err(|error| format!("{error}\n\n{RUNTIME_HELP}"))?
    {
        RuntimeInvocation::Help => {
            println!("{RUNTIME_HELP}");
            Ok(())
        }
        RuntimeInvocation::Version => {
            println!("{RUNTIME_VERSION}");
            Ok(())
        }
        RuntimeInvocation::ListAudioOutputs => list_runtime_audio_outputs(),
        RuntimeInvocation::ListAudioInputs => list_runtime_audio_inputs(),
        RuntimeInvocation::Host(arguments) => {
            let stop = install_runtime_shutdown_signal()?;
            if arguments.operator_ui() {
                run_operator_ui(*arguments, &stop)
            } else {
                run_headless(&arguments, &stop)
            }
        }
    }
}

type RuntimeShutdownHandler = Box<dyn FnMut() + Send>;

fn install_runtime_shutdown_signal_with<E>(
    install: impl FnOnce(RuntimeShutdownHandler) -> Result<(), E>,
) -> Result<Arc<AtomicBool>, E> {
    let stop = Arc::new(AtomicBool::new(false));
    let signal_stop = Arc::clone(&stop);
    install(Box::new(move || {
        signal_stop.store(true, Ordering::Release);
    }))?;
    Ok(stop)
}

fn install_runtime_shutdown_signal() -> Result<Arc<AtomicBool>, String> {
    install_runtime_shutdown_signal_with(|handler| {
        ctrlc::try_set_handler(handler)
            .map_err(|error| format!("cannot install Runtime shutdown signal handler: {error}"))
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RuntimeInvocation {
    Help,
    Version,
    ListAudioOutputs,
    ListAudioInputs,
    Host(Box<RuntimeArguments>),
}

fn parse_runtime_invocation<I>(arguments: I) -> Result<RuntimeInvocation, String>
where
    I: IntoIterator<Item = String>,
{
    let arguments: Vec<_> = arguments.into_iter().collect();
    if matches!(arguments.as_slice(), [option] if option == "--help" || option == "-h") {
        return Ok(RuntimeInvocation::Help);
    }
    if matches!(arguments.as_slice(), [option] if option == "--version" || option == "-V") {
        return Ok(RuntimeInvocation::Version);
    }
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return Err("help and version options must be used by themselves".to_owned());
    }
    if matches!(arguments.as_slice(), [option] if option == "--list-audio-outputs") {
        return Ok(RuntimeInvocation::ListAudioOutputs);
    }
    if matches!(arguments.as_slice(), [option] if option == "--list-audio-inputs") {
        return Ok(RuntimeInvocation::ListAudioInputs);
    }
    if arguments
        .iter()
        .any(|argument| argument == "--list-audio-outputs" || argument == "--list-audio-inputs")
    {
        return Err("audio target listing options must be used by themselves".to_owned());
    }
    RuntimeArguments::parse(arguments)
        .map(|arguments| RuntimeInvocation::Host(Box::new(arguments)))
        .map_err(|error| error.to_string())
}

fn list_runtime_audio_outputs() -> Result<(), String> {
    let targets = discover_runtime_output_targets()
        .map_err(|error| format!("desktop audio-output discovery failed: {error}"))?;
    if targets.is_empty() {
        println!("No routable desktop audio outputs found.");
        return Ok(());
    }
    println!("ROUTING ID\tOUTPUT");
    for target in targets {
        println!("{}\t{}", target.routing_id(), target.display_name());
    }
    Ok(())
}

fn list_runtime_audio_inputs() -> Result<(), String> {
    let targets = discover_runtime_input_targets()
        .map_err(|error| format!("desktop audio-input discovery failed: {error}"))?;
    if targets.is_empty() {
        println!("no routable desktop audio inputs found");
        return Ok(());
    }
    println!("ROUTING ID\tINPUT");
    for target in targets {
        println!("{}\t{}", target.routing_id(), target.display_name());
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct RuntimeFacts {
    bundle_identity: String,
    deployment_generation: u64,
    recovered_from: Option<String>,
    project_name: String,
    project_id: String,
    project_revision: u64,
    output_route: RuntimeOutputRoute,
    input_route: RuntimeInputRoute,
    playback_extent: PlaybackExtent,
    musical_timeline: RuntimeMusicalTimeline,
    timeline_markers: Arc<[OperatorTimelineMarker]>,
}

/// Immutable conversion authority for the Runtime's display-only musical
/// transport position. It is built from a verified project before native
/// output starts, then shared with the UI observer without touching the audio
/// owner.
#[derive(Clone, Debug)]
struct RuntimeMusicalTimeline {
    frames: Arc<FrameTimeline>,
    meter_events: Arc<[MeterEvent]>,
}

impl RuntimeMusicalTimeline {
    fn try_new(
        sample_rate_hz: u32,
        tempo_events: &[TempoEvent],
        meter_events: &[MeterEvent],
    ) -> Result<Self, String> {
        let frames = FrameTimeline::try_new(sample_rate_hz, tempo_events)
            .map_err(|error| format!("frame-to-tick conversion is invalid: {error}"))?;
        let meter_events: Arc<[MeterEvent]> = meter_events.into();
        musical_position(meter_events.as_ref(), 0)
            .map_err(|error| format!("meter-map conversion is invalid: {error}"))?;
        Ok(Self {
            frames: Arc::new(frames),
            meter_events,
        })
    }

    fn format_position(&self, frame: usize) -> Option<String> {
        let tick = self.frames.position_at_frame(frame).ok()?.whole_tick();
        self.format_tick(tick)
    }

    fn format_tick(&self, tick: u64) -> Option<String> {
        let position = musical_position(self.meter_events.as_ref(), tick).ok()?;
        Some(format!(
            "{}|{}|{:03}",
            position.bar(),
            position.beat(),
            position.tick()
        ))
    }

    fn tick_to_frame_ceil(&self, tick: u64) -> Option<usize> {
        self.frames.tick_to_frame_ceil(tick).ok()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorTimelineMarker {
    id: String,
    name: String,
    tick: u64,
    musical_position: Option<String>,
    frame: Option<usize>,
}

#[derive(Clone, Debug)]
struct RuntimeObservers {
    facts: RuntimeFacts,
    audio: RuntimeAudioObservers,
    meter_receivers: RuntimeMeterReceivers,
    panel_connected: Arc<AtomicBool>,
    recording: Arc<Mutex<RuntimeRecordingObservation>>,
    playback_available: Arc<AtomicBool>,
    automation_capture: Arc<Mutex<RuntimeAutomationCaptureObservation>>,
    mixer_update: Arc<Mutex<RuntimeMixerUpdateObservation>>,
    native_device_update: Arc<Mutex<RuntimeNativeDeviceUpdateObservation>>,
    loop_update: Arc<Mutex<RuntimeLoopUpdateObservation>>,
    scene_performance: Arc<Mutex<RuntimeScenePerformanceCaptureObservation>>,
}

#[derive(Clone, Debug)]
struct RuntimeAudioObservationHandles {
    output_status: OutputStreamStatus,
    output_counters: OutputStreamCounters,
    input_status: Option<StreamStatus>,
    input_counters: Option<CaptureStreamCounters>,
    input_control: Option<DesktopMonitoringControl>,
    recovery: PlaybackRecoveryCheckpoint,
}

#[derive(Clone, Debug)]
struct RuntimeAudioObservers {
    handles: Arc<Mutex<RuntimeAudioObservationHandles>>,
}

#[derive(Debug)]
struct RuntimeMeterReceiverGeneration {
    output: OutputMeterReceiver,
    input: Option<InputMeterReceiver>,
}

#[derive(Clone, Debug, Default)]
struct RuntimeMeterReceivers {
    pending: Arc<Mutex<Option<RuntimeMeterReceiverGeneration>>>,
}

impl RuntimeMeterReceivers {
    fn replace(&self, output: OutputMeterReceiver, input: Option<InputMeterReceiver>) {
        *self
            .pending
            .lock()
            .expect("Runtime meter-receiver lock is not poisoned") =
            Some(RuntimeMeterReceiverGeneration { output, input });
    }

    fn take(&self) -> Option<RuntimeMeterReceiverGeneration> {
        self.pending
            .lock()
            .expect("Runtime meter-receiver lock is not poisoned")
            .take()
    }
}

impl RuntimeAudioObservers {
    fn new(session: &DesktopPlaybackSession) -> Self {
        Self {
            handles: Arc::new(Mutex::new(RuntimeAudioObservationHandles::from_session(
                session,
            ))),
        }
    }

    fn snapshot(&self) -> RuntimeAudioObservationHandles {
        self.handles
            .lock()
            .expect("Runtime audio-observer lock is not poisoned")
            .clone()
    }

    fn rebind(&self, session: &DesktopPlaybackSession) {
        self.replace(RuntimeAudioObservationHandles::from_session(session));
    }

    fn replace(&self, handles: RuntimeAudioObservationHandles) {
        *self
            .handles
            .lock()
            .expect("Runtime audio-observer lock is not poisoned") = handles;
    }
}

impl RuntimeAudioObservationHandles {
    fn from_session(session: &DesktopPlaybackSession) -> Self {
        Self {
            output_status: session.output_status(),
            output_counters: session.counters(),
            input_status: session.monitoring_input_status(),
            input_counters: session.monitoring_input_counters(),
            input_control: session.monitoring_control(),
            recovery: session.recovery_checkpoint(),
        }
    }
}

struct OperatorReady {
    observers: RuntimeObservers,
    control_mapping: OperatorControlMappingSummary,
    scenes: Vec<OperatorScene>,
    mixer_channels: Vec<OperatorMixerChannel>,
    mixer_sends: Vec<OperatorMixerSend>,
    native_devices: Vec<OperatorNativeDevice>,
    native_device_chains: Vec<OperatorNativeDeviceChain>,
    automation_targets: Vec<String>,
    recording_targets: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OperatorControlMappingSummary {
    live: usize,
    cue_output_unavailable: usize,
    scene_disabled: usize,
    scene_consolidation_required: usize,
    scene_audio_not_prepared: usize,
}

impl OperatorControlMappingSummary {
    const fn unavailable(self) -> usize {
        self.cue_output_unavailable
            .saturating_add(self.scene_disabled)
            .saturating_add(self.scene_consolidation_required)
            .saturating_add(self.scene_audio_not_prepared)
    }

    const fn enabled(self) -> usize {
        self.live.saturating_add(self.unavailable())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorControlMappingPresentation {
    state: String,
    detail: String,
    warning: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorScene {
    index: usize,
    name: String,
    availability: DesktopSceneAvailability,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorMixerChannel {
    index: usize,
    name: String,
    handle: MixerChannelHandle,
    solo_available: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorMixerSend {
    channel_index: usize,
    send_index: usize,
    source_name: String,
    target_name: String,
    tap: DesktopMixerSendTap,
    handle: MixerSendHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorNativeDevice {
    track_name: String,
    kind: String,
    handle: NativeDeviceHandle,
}

/// Display-only facts for one immutable native-device-chain export target.
///
/// The selection position is assigned by the verified Runtime catalog. It is
/// deliberately not a device handle, track identity, or label supplied by the
/// UI; each of those has different lifetime or uniqueness semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
struct OperatorNativeDeviceChain {
    track_label: String,
    device_count: i32,
}

struct OperatorWorker {
    ready: Receiver<Result<OperatorReady, String>>,
    completion: Receiver<Result<(), String>>,
    handle: std::thread::JoinHandle<Result<(), String>>,
}

struct OperatorMeterObserver {
    receiver: OutputMeterReceiver,
    ballistics: StereoSamplePeakBallistics,
    latest: Option<StereoSamplePeakBallisticsSnapshot>,
    latest_loudness: Option<LoudnessPresentationSnapshot>,
    strip_ballistics: MixerStripSamplePeakBallistics,
    latest_strip_ballistics: Option<MixerStripSamplePeakBallisticsSnapshot>,
    peak_hold_available: bool,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
}

/// Presentation-only owner for raw accepted-input meter evidence. It has no
/// capture, monitor, output, or transport authority.
struct OperatorInputMeterObserver {
    receiver: InputMeterReceiver,
    ballistics: StereoSamplePeakDisplayBallistics,
    latest: Option<StereoSamplePeakDisplayBallisticsSnapshot>,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
}

impl OperatorInputMeterObserver {
    fn new(receiver: InputMeterReceiver, config: SamplePeakBallisticsConfig) -> Self {
        Self {
            receiver,
            ballistics: StereoSamplePeakDisplayBallistics::new(config),
            latest: None,
            coalesced_snapshot_count: 0,
            dropped_snapshot_count: 0,
            sender_disconnected: false,
        }
    }

    fn observe(&mut self) -> OperatorInputMeterPresentation {
        let read = self.receiver.read_display(&mut self.ballistics);
        let latest = read.latest();
        self.coalesced_snapshot_count = self
            .coalesced_snapshot_count
            .saturating_add(u64::try_from(latest.coalesced_snapshot_count()).unwrap_or(u64::MAX));
        self.dropped_snapshot_count = self
            .dropped_snapshot_count
            .saturating_add(u64::try_from(latest.dropped_snapshot_count()).unwrap_or(u64::MAX));
        self.sender_disconnected |= latest.sender_disconnected();
        if self.sender_disconnected {
            self.ballistics.reset();
            self.latest = None;
        } else if let Some(snapshot) = read.display() {
            self.latest = Some(snapshot);
        }
        operator_input_meter_presentation(
            self.latest,
            self.coalesced_snapshot_count,
            self.dropped_snapshot_count,
            self.sender_disconnected,
        )
    }
}

impl OperatorMeterObserver {
    fn new(receiver: OutputMeterReceiver, config: SamplePeakBallisticsConfig) -> Self {
        Self {
            receiver,
            ballistics: StereoSamplePeakBallistics::new(config),
            strip_ballistics: MixerStripSamplePeakBallistics::new(config),
            latest: None,
            latest_loudness: None,
            latest_strip_ballistics: None,
            peak_hold_available: false,
            coalesced_snapshot_count: 0,
            dropped_snapshot_count: 0,
            sender_disconnected: false,
        }
    }

    fn observe(&mut self) -> OperatorMeterObservation {
        let read = self
            .receiver
            .read_ballistics_with_strips(&mut self.ballistics, &mut self.strip_ballistics);
        let latest = read.latest();
        self.coalesced_snapshot_count = self
            .coalesced_snapshot_count
            .saturating_add(u64::try_from(latest.coalesced_snapshot_count()).unwrap_or(u64::MAX));
        self.dropped_snapshot_count = self
            .dropped_snapshot_count
            .saturating_add(u64::try_from(latest.dropped_snapshot_count()).unwrap_or(u64::MAX));
        self.sender_disconnected |= latest.sender_disconnected();
        if let Some(snapshot) = latest.snapshot() {
            self.latest_loudness =
                Some(snapshot.present_loudness(EbuLoudnessPresentationConfig::default()));
        }
        if let Some(snapshot) = read.strip_ballistics() {
            self.latest_strip_ballistics = Some(snapshot);
        }
        if let Some(snapshot) = read.ballistics() {
            self.latest = Some(snapshot);
            self.peak_hold_available = true;
        }
        OperatorMeterObservation {
            presentation: operator_meter_presentation(
                self.latest,
                self.peak_hold_available,
                self.coalesced_snapshot_count,
                self.dropped_snapshot_count,
                self.sender_disconnected,
                self.latest_loudness,
            ),
            strip_meters: self.latest_strip_ballistics,
        }
    }

    fn reset_peak_hold(&mut self) {
        self.ballistics.reset_peak_hold();
        self.peak_hold_available = false;
    }
}

struct StartedRuntime {
    host: SessionHost,
    observers: RuntimeObservers,
    control_mapping: OperatorControlMappingSummary,
    native_device_chains: Vec<OperatorNativeDeviceChain>,
    automation_targets: Vec<String>,
    recording_targets: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum OperatorCommand {
    Start,
    Stop,
    StartRecording {
        target_index: usize,
    },
    StopRecording,
    ExportMixerUpdate,
    ExportNativeDeviceUpdate {
        chain_index: usize,
    },
    ExportLoopUpdate,
    StartAutomationCapture {
        target_index: usize,
        name: String,
    },
    StopAutomationCapture,
    StartScenePerformanceCapture,
    StopScenePerformanceCapture,
    ReturnToStart,
    Locate {
        frame: usize,
        state: LocateState,
    },
    LaunchScene {
        scene_index: usize,
    },
    StopInteractiveScenes,
    SetLoopEnabled {
        enabled: bool,
    },
    SetLoopRange {
        range: Option<TransportLoopRange>,
    },
    SetMetronomeEnabled {
        enabled: bool,
    },
    SetMetronomeGain {
        gain: MetronomeGain,
    },
    SetInputMonitorGain {
        gain_millionths: u32,
    },
    SetInputMonitorMuted {
        muted: bool,
    },
    SetLoudnessState {
        state: LoudnessMeasurementState,
    },
    ResetLoudness,
    SetMixerChannelGain {
        channel: MixerChannelHandle,
        gain_millionths: u32,
    },
    SetMixerChannelPan {
        channel: MixerChannelHandle,
        pan_millionths: i32,
    },
    SetMixerChannelMute {
        channel: MixerChannelHandle,
        muted: bool,
    },
    SetMixerChannelSolo {
        channel: MixerChannelHandle,
        solo: bool,
    },
    SetMixerSendGain {
        send: MixerSendHandle,
        gain_millionths: u32,
    },
    SetMixerSendPan {
        send: MixerSendHandle,
        pan_millionths: i32,
    },
    SetMixerSendEnabled {
        send: MixerSendHandle,
        enabled: bool,
    },
    SetNativeDeviceEnabled {
        device: NativeDeviceHandle,
        enabled: bool,
    },
    SetNativeDeviceState {
        device: NativeDeviceHandle,
        state: NativeDeviceState,
    },
    SetMainMute {
        muted: bool,
    },
    SetMainGain {
        gain_millionths: u32,
    },
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorTransportAction {
    Start,
    Stop,
    Toggle,
    ReturnToStart,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperatorLoopAuthority {
    range: Option<TransportLoopRange>,
    enabled: bool,
    position_frame: usize,
    extent: PlaybackExtent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperatorMetronomeAuthority {
    available: bool,
    enabled: bool,
    gain: MetronomeGain,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OperatorMixerChannelAuthority {
    handle: MixerChannelHandle,
    gain: LinearGain,
    pan: Pan,
    muted: bool,
    solo: bool,
    solo_available: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct OperatorMixerAuthority {
    channels: Vec<OperatorMixerChannelAuthority>,
    sends: Vec<OperatorMixerSendAuthority>,
    main_gain: LinearGain,
    main_muted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct OperatorMixerSendAuthority {
    handle: MixerSendHandle,
    gain: LinearGain,
    pan: Pan,
    enabled: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OperatorNativeDeviceAuthority {
    handle: NativeDeviceHandle,
    enabled: bool,
    state: NativeDeviceState,
}

const OPERATOR_COMMAND_CAPACITY: usize = 8;

trait RuntimeOperatorControl {
    fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String>;
    fn queue_loudness(&mut self, command: LoudnessMeasurementCommand) -> Result<(), String>;
    fn queue_mixer(&mut self, command: MixerParameterCommand) -> Result<(), String>;
    fn queue_native_device(&mut self, command: NativeDeviceCommand) -> Result<(), String>;

    fn set_input_monitor_gain(&mut self, gain: f32) -> Result<(), String>;

    fn set_input_monitor_muted(&mut self, muted: bool) -> Result<(), String>;
}

trait RuntimeRecordingControl {
    fn start_recording(
        &mut self,
        target_index: usize,
    ) -> Result<RuntimeRecordingControlOutcome, String>;

    fn stop_recording(&mut self) -> Result<RuntimeRecordingControlOutcome, String>;

    fn poll_recording(&mut self) -> RuntimeRecordingControlOutcome;

    fn abort_recording_for_shutdown(&mut self) -> Result<Option<String>, String>;
}

/// Runtime scene-performance commands remain with the same owner as transport.
trait RuntimeScenePerformanceControl {
    fn start_scene_performance_capture(&mut self) -> Result<String, String>;

    fn stop_scene_performance_capture(&mut self) -> Result<String, String>;

    fn poll_scene_performance_capture(&mut self) -> Option<String>;
}

/// Runtime automation capture remains with the session that owns transport and
/// the sole committed mixer-parameter receiver.
trait RuntimeAutomationCaptureControl {
    fn start_automation_capture(
        &mut self,
        target_index: usize,
        name: String,
    ) -> Result<String, String>;

    fn stop_automation_capture(&mut self) -> Result<String, String>;

    fn poll_automation_capture(&mut self) -> Option<String>;
}

/// Mixer-update export remains with the same owner as live mixer commands.
trait RuntimeMixerUpdateControl {
    fn export_mixer_update(&mut self) -> Result<String, String>;

    fn poll_mixer_update_export(&mut self) -> Option<String>;
}

/// Native-device update export remains with the Runtime session that owns the
/// audio-cycle recovery checkpoint.
trait RuntimeNativeDeviceUpdateControl {
    fn export_native_device_update(&mut self, chain_index: usize) -> Result<String, String>;

    fn poll_native_device_update_export(&mut self) -> Option<String>;
}

/// Loop-update export remains with the same owner as live transport commands.
trait RuntimeLoopUpdateControl {
    fn export_loop_update(&mut self) -> Result<String, String>;

    fn poll_loop_update_export(&mut self) -> Option<String>;
}

trait RuntimeOutputObservation {
    fn output_state(&self) -> OutputStreamState;
}

impl RuntimeOutputObservation for OutputStreamStatus {
    fn output_state(&self) -> OutputStreamState {
        self.state()
    }
}

impl RuntimeOutputObservation for RuntimeAudioObservers {
    fn output_state(&self) -> OutputStreamState {
        self.snapshot().output_status.state()
    }
}

impl RuntimeOperatorControl for SessionHost {
    fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String> {
        self.owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?
            .try_send_transport_command(command)
            .map_err(|error| error.to_string())
    }

    fn queue_loudness(&mut self, command: LoudnessMeasurementCommand) -> Result<(), String> {
        self.owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?
            .loudness_command_sender()
            .try_send(command)
            .map_err(|error| error.to_string())
    }

    fn queue_mixer(&mut self, command: MixerParameterCommand) -> Result<(), String> {
        self.owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?
            .try_send_mixer_parameter_command(command)
            .map_err(|error| error.to_string())
    }

    fn queue_native_device(&mut self, command: NativeDeviceCommand) -> Result<(), String> {
        self.owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?
            .try_send_native_device_command(command)
            .map_err(|error| error.to_string())
    }

    fn set_input_monitor_gain(&mut self, gain: f32) -> Result<(), String> {
        running_input_monitor_control(
            self.owner
                .playback()
                .ok_or_else(runtime_playback_unavailable)?,
        )?
        .set_gain_linear(gain)
        .map_err(|error| error.to_string())
    }

    fn set_input_monitor_muted(&mut self, muted: bool) -> Result<(), String> {
        let control = running_input_monitor_control(
            self.owner
                .playback()
                .ok_or_else(runtime_playback_unavailable)?,
        )?;
        control.set_muted(muted);
        Ok(())
    }
}

fn running_input_monitor_control(
    playback: &DesktopPlaybackSession,
) -> Result<DesktopMonitoringControl, String> {
    if !runtime_input_monitor_running(
        playback
            .monitoring_input_status()
            .map(|status| status.state()),
    ) {
        return Err("Runtime input monitor capture is not running".to_owned());
    }
    playback
        .monitoring_control()
        .ok_or_else(|| "Runtime input monitor is unavailable".to_owned())
}

fn runtime_input_monitor_running(state: Option<StreamState>) -> bool {
    matches!(state, Some(StreamState::Running))
}

impl RuntimeScenePerformanceControl for SessionHost {
    fn start_scene_performance_capture(&mut self) -> Result<String, String> {
        if self.owner.is_recording() {
            return Err("scene-performance capture is unavailable while recording".to_owned());
        }
        let capture = &mut self.scene_performance;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        capture.start(session).map_or_else(
            |error| Err(error.to_string()),
            |maximum_launch_count| {
                Ok(format!(
                    "scene-performance capture started with capacity for {maximum_launch_count} arrangement scene launch(es)"
                ))
            },
        )
    }

    fn stop_scene_performance_capture(&mut self) -> Result<String, String> {
        let capture = &mut self.scene_performance;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        capture.stop(session).map_or_else(
            |error| Err(error.to_string()),
            |captured_launch_count| {
                Ok(format!(
                    "scene-performance capture stop requested after {captured_launch_count} committed launch(es)"
                ))
            },
        )
    }

    fn poll_scene_performance_capture(&mut self) -> Option<String> {
        let capture = &mut self.scene_performance;
        let session = self.owner.playback_mut()?;
        match capture.poll(session) {
            Some(RuntimeScenePerformanceCapturePoll::Saved(saved)) => Some(format!(
                "scene-performance capture published {} launch(es) to {}",
                saved.launch_count,
                saved.path.display()
            )),
            Some(RuntimeScenePerformanceCapturePoll::Failed(error)) => {
                Some(format!("scene-performance capture failed: {error}"))
            }
            None => None,
        }
    }
}

impl RuntimeAutomationCaptureControl for SessionHost {
    fn start_automation_capture(
        &mut self,
        target_index: usize,
        name: String,
    ) -> Result<String, String> {
        if self.owner.is_recording() {
            return Err("automation capture is unavailable while recording".to_owned());
        }
        let capture = &mut self.automation_capture;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        capture.start(session, target_index, name).map_or_else(
            |error| Err(error.to_string()),
            |maximum_point_count| {
                Ok(format!(
                    "Runtime automation capture started with capacity for {maximum_point_count} committed parameter change(s)"
                ))
            },
        )
    }

    fn stop_automation_capture(&mut self) -> Result<String, String> {
        let capture = &mut self.automation_capture;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        capture.stop(session).map_or_else(
            |error| Err(error.to_string()),
            |captured_point_count| {
                Ok(format!(
                    "Runtime automation capture stop requested after {captured_point_count} committed parameter change(s)"
                ))
            },
        )
    }

    fn poll_automation_capture(&mut self) -> Option<String> {
        let capture = &mut self.automation_capture;
        let session = self.owner.playback_mut()?;
        match capture.poll(session) {
            Some(RuntimeAutomationCapturePoll::Saved(saved)) => Some(format!(
                "Runtime automation capture published {} parameter change(s) to {}",
                saved.point_count,
                saved.path.display()
            )),
            Some(RuntimeAutomationCapturePoll::Failed(error)) => {
                Some(format!("Runtime automation capture failed: {error}"))
            }
            None => None,
        }
    }
}

impl RuntimeMixerUpdateControl for SessionHost {
    fn export_mixer_update(&mut self) -> Result<String, String> {
        if self.owner.is_recording() {
            return Err("mixer-update export is unavailable while recording".to_owned());
        }
        let update = &mut self.mixer_update;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        update.request_export(session).map_or_else(
            |error| Err(error.to_string()),
            |fence_sequence| {
                Ok(format!(
                    "mixer-update export queued at committed mixer fence {fence_sequence}"
                ))
            },
        )
    }

    fn poll_mixer_update_export(&mut self) -> Option<String> {
        let update = &mut self.mixer_update;
        let session = self.owner.playback_mut()?;
        match update.poll(session) {
            Some(RuntimeMixerUpdatePoll::Saved(saved)) => Some(format!(
                "mixer update published to {}",
                saved.path.display()
            )),
            Some(RuntimeMixerUpdatePoll::NoChanges) => {
                Some("mixer update not published: no committed mixer changes".to_owned())
            }
            Some(RuntimeMixerUpdatePoll::Failed(error)) => {
                Some(format!("mixer-update export failed: {error}"))
            }
            None => None,
        }
    }
}

impl RuntimeNativeDeviceUpdateControl for SessionHost {
    fn export_native_device_update(&mut self, chain_index: usize) -> Result<String, String> {
        if self.owner.is_recording() {
            return Err("native-device update export is unavailable while recording".to_owned());
        }
        let update = &mut self.native_device_update;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        update.request_export(session, chain_index).map_or_else(
            |error| Err(error.to_string()),
            |fence_sequence| {
                Ok(format!(
                    "native-device update export queued at committed native-device fence {fence_sequence}"
                ))
            },
        )
    }

    fn poll_native_device_update_export(&mut self) -> Option<String> {
        let update = &mut self.native_device_update;
        let session = self.owner.playback_mut()?;
        match update.poll(session) {
            Some(RuntimeNativeDeviceUpdatePoll::Saved(saved)) => Some(format!(
                "native-device update published to {}",
                saved.path.display()
            )),
            Some(RuntimeNativeDeviceUpdatePoll::NoChanges) => Some(
                "native-device update not published: no committed native-device changes".to_owned(),
            ),
            Some(RuntimeNativeDeviceUpdatePoll::Failed(error)) => {
                Some(format!("native-device update export failed: {error}"))
            }
            None => None,
        }
    }
}

impl RuntimeLoopUpdateControl for SessionHost {
    fn export_loop_update(&mut self) -> Result<String, String> {
        if self.owner.is_recording() {
            return Err("loop-update export is unavailable while recording".to_owned());
        }
        let update = &mut self.loop_update;
        let session = self
            .owner
            .playback_mut()
            .ok_or_else(runtime_playback_unavailable)?;
        update.request_export(session).map_or_else(
            |error| Err(error.to_string()),
            |fence_sequence| {
                Ok(format!(
                    "loop-update export queued at committed transport fence {fence_sequence}"
                ))
            },
        )
    }

    fn poll_loop_update_export(&mut self) -> Option<String> {
        let update = &mut self.loop_update;
        let session = self.owner.playback_mut()?;
        match update.poll(session) {
            Some(RuntimeLoopUpdatePoll::Saved(saved)) => {
                Some(format!("loop update published to {}", saved.path.display()))
            }
            Some(RuntimeLoopUpdatePoll::NoChanges) => {
                Some("loop update not published: no committed loop changes".to_owned())
            }
            Some(RuntimeLoopUpdatePoll::Failed(error)) => {
                Some(format!("loop-update export failed: {error}"))
            }
            None => None,
        }
    }
}

fn runtime_playback_unavailable() -> String {
    "Runtime playback owner is transitioning".to_owned()
}

fn operator_timeline_marker_catalog(
    markers: &[LoadedTimelineMarker],
    timeline: &RuntimeMusicalTimeline,
) -> Result<Vec<OperatorTimelineMarker>, String> {
    let mut catalog = Vec::new();
    catalog
        .try_reserve_exact(markers.len())
        .map_err(|_| "operator timeline-marker catalog capacity is unavailable".to_owned())?;
    for marker in markers {
        catalog.push(operator_timeline_marker(
            marker.id().to_owned(),
            marker.name().to_owned(),
            marker.tick(),
            timeline,
        ));
    }
    Ok(catalog)
}

fn operator_timeline_marker(
    id: String,
    name: String,
    tick: u64,
    timeline: &RuntimeMusicalTimeline,
) -> OperatorTimelineMarker {
    OperatorTimelineMarker {
        id,
        name,
        tick,
        musical_position: timeline.format_tick(tick),
        frame: timeline.tick_to_frame_ceil(tick),
    }
}

struct PreparedRuntimeStartup {
    facts: RuntimeFacts,
    project: PreparedProjectPlayback,
    recording: RuntimeRecordingConfiguration,
    recording_targets: Vec<String>,
    automation_capture: RuntimeAutomationCaptureConfiguration,
    automation_targets: Vec<String>,
    mixer_update: RuntimeMixerUpdateConfiguration,
    native_device_update: RuntimeNativeDeviceUpdateConfiguration,
    native_device_chains: Vec<OperatorNativeDeviceChain>,
    loop_update: RuntimeLoopUpdateConfiguration,
    scene_performance: RuntimeScenePerformanceCaptureConfiguration,
}

/// Explicit external authorities available for Runtime handback artifacts.
///
/// Each directory is opened before native audio starts, and each controller
/// receives only its own retained authority from this aggregate.
struct RuntimeExternalUpdateDirectories {
    scene_performance: Option<RuntimeScenePerformanceUpdateDirectory>,
    recording: Option<RuntimeRecordingUpdateDirectory>,
    automation: Option<RuntimeAutomationUpdateDirectory>,
    mixer: Option<RuntimeMixerUpdateDirectory>,
    native_device: Option<RuntimeNativeDeviceUpdateDirectory>,
    loop_update: Option<RuntimeLoopUpdateDirectory>,
}

/// Complete audio authority preflighted before either native stream is opened.
struct RuntimeAudioConfiguration {
    output: hyper_desktop_audio::DesktopOutputConfig,
    output_route: RuntimeOutputRoute,
    input_route: RuntimeInputRoute,
    monitoring: Option<DesktopMonitoringConfig>,
}

/// Controllers attached to the sole native-audio session after all artifact
/// and audio preflight work has completed.
struct RuntimeHostAssembly {
    facts: RuntimeFacts,
    session: DesktopPlaybackSession,
    recording: RuntimeRecordingConfiguration,
    recording_targets: Vec<String>,
    automation_capture: RuntimeAutomationCaptureController,
    automation_capture_observation: Arc<Mutex<RuntimeAutomationCaptureObservation>>,
    mixer_update: RuntimeMixerUpdateController,
    mixer_update_observation: Arc<Mutex<RuntimeMixerUpdateObservation>>,
    native_device_update: RuntimeNativeDeviceUpdateController,
    native_device_update_observation: Arc<Mutex<RuntimeNativeDeviceUpdateObservation>>,
    loop_update: RuntimeLoopUpdateController,
    loop_update_observation: Arc<Mutex<RuntimeLoopUpdateObservation>>,
    scene_performance: RuntimeScenePerformanceCaptureController,
    scene_performance_observation: Arc<Mutex<RuntimeScenePerformanceCaptureObservation>>,
    native_device_chains: Vec<OperatorNativeDeviceChain>,
    automation_targets: Vec<String>,
}

fn prepare_runtime_audio(
    arguments: &RuntimeArguments,
) -> Result<RuntimeAudioConfiguration, String> {
    let output = prepare_runtime_output(
        arguments.audio_output(),
        SAMPLE_RATE_HZ,
        MAXIMUM_BLOCK_FRAMES,
    )
    .map_err(|error| format!("Runtime audio-output preflight failed: {error}"))?;
    let input = prepare_runtime_input(
        arguments.audio_input(),
        SAMPLE_RATE_HZ,
        MAXIMUM_BLOCK_FRAMES,
    )
    .map_err(|error| format!("Runtime audio-input preflight failed: {error}"))?;
    let (output, output_route) = output.into_parts();
    let (input, input_route) = input.into_parts();
    configure_runtime_audio(output, output_route, input, input_route)
}

fn configure_runtime_audio(
    output: hyper_desktop_audio::DesktopOutputConfig,
    output_route: RuntimeOutputRoute,
    input: Option<hyper_desktop_audio::DesktopInputConfig>,
    input_route: RuntimeInputRoute,
) -> Result<RuntimeAudioConfiguration, String> {
    let monitoring = input
        .map(|input| {
            DesktopMonitoringConfig::try_new(
                input,
                output.clone(),
                INPUT_MONITORING_TRANSPORT_FRAMES,
                INPUT_MONITORING_TARGET_FRAMES,
                INPUT_MONITORING_CHUNK_FRAMES,
            )
        })
        .transpose()
        .map_err(|error| format!("Runtime input-monitor policy is invalid: {error}"))?;
    Ok(RuntimeAudioConfiguration {
        output,
        output_route,
        input_route,
        monitoring,
    })
}

/// Prepares the immutable provenance and source-loop baseline for one Runtime
/// handback before the native audio owner starts.
fn prepare_runtime_loop_update_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeLoopUpdateDirectory>,
) -> Result<RuntimeLoopUpdateConfiguration, String> {
    let manifest = project.package().manifest();
    let source = RuntimeLoopUpdateSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime loop-update source could not be prepared: {error}")
    })?;
    RuntimeLoopUpdateConfiguration::try_new(source, project, output_directory).map_err(|error| {
        format!("verified Runtime loop-update configuration could not be prepared: {error}")
    })
}

fn prepare_runtime_recording_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeRecordingUpdateDirectory>,
) -> Result<(RuntimeRecordingConfiguration, Vec<String>), String> {
    let manifest = project.package().manifest();
    let source = RuntimeRecordingUpdateSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        hyper_runtime_recording_update::sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime recording-update source could not be prepared: {error}")
    })?;
    let recording = RuntimeRecordingConfiguration::try_new(source, project, output_directory)
        .map_err(|error| {
            format!("verified Runtime recording configuration could not be prepared: {error}")
        })?;
    let targets = recording.target_labels().map_err(|error| {
        format!("verified Runtime recording target catalog could not be prepared: {error}")
    })?;
    Ok((recording, targets))
}

/// Prepares immutable provenance, stable target identities, and bounded
/// capture capacity before the native audio owner starts.
fn prepare_runtime_automation_capture_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeAutomationUpdateDirectory>,
) -> Result<RuntimeAutomationCaptureConfiguration, String> {
    let manifest = project.package().manifest();
    let source = RuntimeAutomationUpdateSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime automation-update source could not be prepared: {error}")
    })?;
    RuntimeAutomationCaptureConfiguration::try_new(source, project, output_directory).map_err(
        |error| {
            format!(
                "verified Runtime automation-capture configuration could not be prepared: {error}"
            )
        },
    )
}

fn prepare_runtime_scene_performance_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeScenePerformanceUpdateDirectory>,
) -> Result<RuntimeScenePerformanceCaptureConfiguration, String> {
    let manifest = project.package().manifest();
    let (loaded_project, ..) = project.session().parts();
    let source = RuntimeScenePerformanceSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime scene-performance source could not be prepared: {error}")
    })?;
    RuntimeScenePerformanceCaptureConfiguration::try_new(source, loaded_project, output_directory)
        .map_err(|error| {
            format!(
                "verified Runtime scene-performance configuration could not be prepared: {error}"
            )
        })
}

/// Prepares immutable provenance and chain-selection facts before the native
/// audio owner starts. Live state is intentionally absent here: it is read
/// only after the controller's explicit native-device fence commits.
fn prepare_runtime_native_device_update_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeNativeDeviceUpdateDirectory>,
) -> Result<RuntimeNativeDeviceUpdateConfiguration, String> {
    let manifest = project.package().manifest();
    let source = RuntimeNativeDeviceUpdateSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime native-device-update source could not be prepared: {error}")
    })?;
    RuntimeNativeDeviceUpdateConfiguration::try_new(source, project, output_directory).map_err(
        |error| {
            format!(
                "verified Runtime native-device-update configuration could not be prepared: {error}"
            )
        },
    )
}

fn prepare_runtime_mixer_update_configuration(
    project: &PreparedProjectPlayback,
    bundle_identity: &str,
    deployment_generation: u64,
    output_directory: Option<RuntimeMixerUpdateDirectory>,
) -> Result<RuntimeMixerUpdateConfiguration, String> {
    let manifest = project.package().manifest();
    let source = RuntimeMixerUpdateSource::try_new(
        manifest.project_id().to_owned(),
        manifest.revision(),
        sha256_hex(project.package().manifest_sha256()),
        bundle_identity.to_owned(),
        deployment_generation,
    )
    .map_err(|error| {
        format!("verified Runtime mixer-update source could not be prepared: {error}")
    })?;
    RuntimeMixerUpdateConfiguration::try_new(source, project, output_directory).map_err(|error| {
        format!("verified Runtime mixer-update configuration could not be prepared: {error}")
    })
}

fn prepare_runtime_startup(arguments: &RuntimeArguments) -> Result<PreparedRuntimeStartup, String> {
    let deployment = resolve_runtime_deployment(
        arguments.deployment_root(),
        env!("CARGO_PKG_VERSION"),
        MAX_RUNTIME_BUNDLE_BYTES,
        limits()?,
    )
    .map_err(|error| {
        format!(
            "deployment target {} could not be resolved: {}",
            arguments.deployment_root().display(),
            format_error_chain(&error)
        )
    })?;
    let outputs = open_runtime_external_update_directories(arguments)?;
    let prepared_project = deployment.bundle().project();
    let manifest = prepared_project.package().manifest();
    let (project, ..) = prepared_project.session().parts();
    let musical_timeline = RuntimeMusicalTimeline::try_new(
        project.sample_rate_hz(),
        project.tempo_events(),
        project.meter_events(),
    )
    .map_err(|error| format!("verified project musical timing could not be prepared: {error}"))?;
    let timeline_markers =
        operator_timeline_marker_catalog(project.timeline_markers(), &musical_timeline)?;
    let bundle_identity = sha256_hex(deployment.bundle().bundle_sha256());
    let (recording, recording_targets) = prepare_runtime_recording_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.recording,
    )?;
    let scene_performance = prepare_runtime_scene_performance_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.scene_performance,
    )?;
    let automation_capture = prepare_runtime_automation_capture_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.automation,
    )?;
    let automation_targets = automation_capture.target_labels().map_err(|error| {
        format!("verified Runtime automation target catalog could not be prepared: {error}")
    })?;
    let mixer_update = prepare_runtime_mixer_update_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.mixer,
    )?;
    let native_device_update = prepare_runtime_native_device_update_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.native_device,
    )?;
    let native_device_chains =
        operator_native_device_chain_catalog(prepared_project, &native_device_update)?;
    let loop_update = prepare_runtime_loop_update_configuration(
        prepared_project,
        &bundle_identity,
        deployment.generation(),
        outputs.loop_update,
    )?;
    let facts = RuntimeFacts {
        bundle_identity,
        deployment_generation: deployment.generation(),
        recovered_from: deployment.recovered_from_bundle_sha256().map(sha256_hex),
        project_name: manifest.name().to_owned(),
        project_id: manifest.project_id().to_owned(),
        project_revision: manifest.revision(),
        output_route: RuntimeOutputRoute::SystemDefault,
        input_route: RuntimeInputRoute::Disabled,
        playback_extent: deployment.bundle().project().session().extent(),
        musical_timeline,
        timeline_markers: timeline_markers.into(),
    };
    Ok(PreparedRuntimeStartup {
        facts,
        project: deployment.into_bundle().into_project(),
        recording,
        recording_targets,
        automation_capture,
        automation_targets,
        mixer_update,
        native_device_update,
        native_device_chains,
        loop_update,
        scene_performance,
    })
}

fn start_runtime(arguments: &RuntimeArguments) -> Result<StartedRuntime, String> {
    let RuntimeHostAssembly {
        facts,
        session,
        recording,
        recording_targets,
        automation_capture,
        automation_capture_observation,
        mixer_update,
        mixer_update_observation,
        native_device_update,
        native_device_update_observation,
        loop_update,
        loop_update_observation,
        scene_performance,
        scene_performance_observation,
        native_device_chains,
        automation_targets,
    } = prepare_output_and_host(
        || prepare_runtime_startup(arguments),
        || prepare_runtime_audio(arguments),
        attach_runtime_host,
    )?;
    let control_mapping = operator_control_mapping_summary(
        session
            .control_mapping_statuses()
            .iter()
            .map(|status| status.availability()),
    );
    let panel_connected = Arc::new(AtomicBool::new(false));
    let audio_observers = RuntimeAudioObservers::new(&session);
    let meter_receivers = RuntimeMeterReceivers::default();
    let mixer_channel_count = session.mixer_channel_count();
    let output_pinned = matches!(facts.output_route, RuntimeOutputRoute::Pinned { .. });
    let owner = RuntimePlaybackOwner::new(session, recording, output_pinned);
    let recording_observation = owner.recording_observation();
    let playback_available = owner.playback_availability();
    let observers = RuntimeObservers {
        facts,
        audio: audio_observers.clone(),
        meter_receivers: meter_receivers.clone(),
        panel_connected: Arc::clone(&panel_connected),
        recording: recording_observation,
        playback_available,
        automation_capture: automation_capture_observation,
        mixer_update: mixer_update_observation,
        native_device_update: native_device_update_observation,
        loop_update: loop_update_observation,
        scene_performance: scene_performance_observation,
    };
    let mut host = SessionHost {
        owner,
        mixer_channel_count,
        audio_observers,
        meter_receivers,
        automation_capture,
        mixer_update,
        native_device_update,
        loop_update,
        scene_performance,
        panel_connected,
    };
    if let Some(blocker) = host.recording_transition_blocker() {
        return Err(format!(
            "verified Runtime host unexpectedly started with a recording-transition blocker: {blocker}"
        ));
    }
    host.refresh_audio_observers();
    host.refresh_meter_receivers().map_err(|error| {
        format!("verified Runtime host cannot publish meter authority: {error}")
    })?;
    Ok(StartedRuntime {
        host,
        observers,
        control_mapping,
        native_device_chains,
        automation_targets,
        recording_targets,
    })
}

fn attach_runtime_host(
    audio: RuntimeAudioConfiguration,
    startup: PreparedRuntimeStartup,
) -> Result<RuntimeHostAssembly, String> {
    let RuntimeAudioConfiguration {
        output,
        output_route,
        input_route,
        monitoring,
    } = audio;
    let mut facts = startup.facts;
    facts.output_route = output_route;
    facts.input_route = input_route;
    let mut session = match monitoring {
        Some(monitoring) => DesktopPlaybackSession::start_project_monitored_with_monitor_state(
            monitoring,
            startup.project,
            1.0,
            true,
        ),
        None => DesktopPlaybackSession::start_project(output, startup.project),
    }
    .map_err(|error| format!("verified bundle could not be hosted: {error}"))?;
    let scene_receiver = session.take_scene_launch_receiver().ok_or_else(|| {
        "verified bundle could not transfer committed scene-launch authority".to_owned()
    })?;
    let scene_performance = startup.scene_performance.attach(scene_receiver);
    let scene_performance_observation = scene_performance.observation();
    let parameter_receiver = session.take_mixer_parameter_receiver().ok_or_else(|| {
        "verified bundle could not transfer committed mixer-parameter authority".to_owned()
    })?;
    let automation_capture = startup.automation_capture.attach(parameter_receiver);
    let automation_capture_observation = automation_capture.observation();
    let mixer_update = startup.mixer_update.attach();
    let mixer_update_observation = mixer_update.observation();
    let native_device_update = startup.native_device_update.attach();
    let native_device_update_observation = native_device_update.observation();
    let loop_update = startup.loop_update.attach();
    let loop_update_observation = loop_update.observation();
    Ok(RuntimeHostAssembly {
        facts,
        session,
        recording: startup.recording,
        recording_targets: startup.recording_targets,
        automation_capture,
        automation_capture_observation,
        mixer_update,
        mixer_update_observation,
        native_device_update,
        native_device_update_observation,
        loop_update,
        loop_update_observation,
        scene_performance,
        scene_performance_observation,
        native_device_chains: startup.native_device_chains,
        automation_targets: startup.automation_targets,
    })
}

/// Opens an explicit Runtime recording-update directory only when it is
/// outside the verified deployment tree. Journals, WAVs, and IDE handback
/// artifacts must never modify the hosted deployment.
fn open_runtime_recording_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeRecordingUpdateDirectory>, String> {
    let Some(path) = arguments.recording_updates() else {
        return Ok(None);
    };
    let output = RuntimeRecordingUpdateDirectory::open(path).map_err(|error| {
        format!(
            "recording-update output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for recording-update output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "recording-update output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "recording-update output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

/// Opens an explicit Runtime output directory only when it is outside the
/// verified deployment tree. Runtime artifacts must never change the
/// deployment, content-addressed bundle, or embedded project package.
fn open_runtime_scene_performance_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeScenePerformanceUpdateDirectory>, String> {
    let Some(path) = arguments.performance_updates() else {
        return Ok(None);
    };
    let output = RuntimeScenePerformanceUpdateDirectory::open(path).map_err(|error| {
        format!(
            "scene-performance output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for scene-performance output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "scene-performance output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "scene-performance output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

/// Opens an explicit Runtime automation-update directory only when it is
/// outside the verified deployment tree. Runtime captures are handbacks for
/// the IDE, never mutations of the deployment, bundle, or embedded project.
fn open_runtime_automation_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeAutomationUpdateDirectory>, String> {
    let Some(path) = arguments.automation_updates() else {
        return Ok(None);
    };
    let output = RuntimeAutomationUpdateDirectory::open(path).map_err(|error| {
        format!(
            "automation-update output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for automation-update output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "automation-update output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "automation-update output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

/// Opens an explicit Runtime mixer-update directory only when it is outside
/// the verified deployment tree. The Runtime never writes an update into its
/// deployment, bundle, or embedded project package.
fn open_runtime_mixer_update_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeMixerUpdateDirectory>, String> {
    let Some(path) = arguments.mixer_updates() else {
        return Ok(None);
    };
    let output = RuntimeMixerUpdateDirectory::open(path).map_err(|error| {
        format!(
            "mixer-update output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for mixer-update output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "mixer-update output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "mixer-update output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

/// Opens an explicit Runtime native-device-update directory only when it is
/// outside the verified deployment tree. Native-device handback artifacts
/// must never modify the hosted deployment or its embedded project package.
fn open_runtime_native_device_update_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeNativeDeviceUpdateDirectory>, String> {
    let Some(path) = arguments.native_device_updates() else {
        return Ok(None);
    };
    let output = RuntimeNativeDeviceUpdateDirectory::open(path).map_err(|error| {
        format!(
            "native-device-update output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for native-device-update output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "native-device-update output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "native-device-update output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

/// Opens an explicit Runtime loop-update directory only when it is outside the
/// verified deployment tree. The Runtime never writes an update into its
/// deployment, bundle, or embedded project package.
fn open_runtime_loop_update_output(
    arguments: &RuntimeArguments,
) -> Result<Option<RuntimeLoopUpdateDirectory>, String> {
    let Some(path) = arguments.loop_updates() else {
        return Ok(None);
    };
    let output = RuntimeLoopUpdateDirectory::open(path).map_err(|error| {
        format!(
            "loop-update output directory {} is invalid: {error}",
            path.display()
        )
    })?;
    let deployment_root = std::fs::canonicalize(arguments.deployment_root()).map_err(|error| {
        format!(
            "verified deployment root {} cannot be canonicalized for loop-update output isolation: {error}",
            arguments.deployment_root().display()
        )
    })?;
    let output_root = std::fs::canonicalize(output.root_path()).map_err(|error| {
        format!(
            "loop-update output directory {} cannot be canonicalized: {error}",
            output.root_path().display()
        )
    })?;
    if output_root.starts_with(&deployment_root) {
        return Err(format!(
            "loop-update output directory {} must be outside deployment target {}",
            output.root_path().display(),
            arguments.deployment_root().display()
        ));
    }
    Ok(Some(output))
}

fn open_runtime_external_update_directories(
    arguments: &RuntimeArguments,
) -> Result<RuntimeExternalUpdateDirectories, String> {
    Ok(RuntimeExternalUpdateDirectories {
        scene_performance: open_runtime_scene_performance_output(arguments)?,
        recording: open_runtime_recording_output(arguments)?,
        automation: open_runtime_automation_output(arguments)?,
        mixer: open_runtime_mixer_update_output(arguments)?,
        native_device: open_runtime_native_device_update_output(arguments)?,
        loop_update: open_runtime_loop_update_output(arguments)?,
    })
}

fn announce_runtime(arguments: &RuntimeArguments, runtime: &StartedRuntime) {
    println!(
        "hosting deployment generation {} bundle {} from {} with {} mixer channels via {} output and {} input",
        runtime.observers.facts.deployment_generation,
        runtime.observers.facts.bundle_identity,
        arguments.deployment_root().display(),
        runtime.host.mixer_channel_count(),
        runtime.observers.facts.output_route,
        runtime.observers.facts.input_route,
    );
    if let Some(recovered) = &runtime.observers.facts.recovered_from {
        println!("recovered from rejected bundle {recovered}");
    }
    println!(
        "{}",
        runtime_control_mapping_announcement(runtime.control_mapping)
    );
}

fn runtime_control_mapping_announcement(summary: OperatorControlMappingSummary) -> String {
    let presentation = operator_control_mapping_presentation(summary, true, false);
    format!(
        "control mapping: {} — {}",
        presentation.state, presentation.detail
    )
}

fn run_headless(arguments: &RuntimeArguments, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let mut runtime = start_runtime(arguments)?;
    announce_runtime(arguments, &runtime);
    watch_for_quit(Arc::clone(stop));
    let output_status = runtime.observers.audio.clone();
    host_runtime(
        arguments,
        &mut runtime.host,
        &output_status,
        stop,
        None,
        &|message| {
            println!("{message}");
        },
    )?;
    println!("stopping");
    Ok(())
}

fn run_runtime_owner_turn<H, O>(
    host: &mut H,
    output_status: &O,
    terminal_output_failure: &Cell<bool>,
    terminal_owner_failure: &RefCell<Option<String>>,
    stop: &AtomicBool,
    commands: Option<&Receiver<OperatorCommand>>,
    diagnostic: &dyn Fn(String),
) where
    H: RuntimeAutomationCaptureControl
        + RuntimeMixerUpdateControl
        + RuntimeNativeDeviceUpdateControl
        + RuntimeLoopUpdateControl
        + RuntimeOperatorControl
        + RuntimeRecordingControl
        + RuntimeScenePerformanceControl,
    O: RuntimeOutputObservation,
{
    if stop.load(Ordering::Acquire) {
        return;
    }
    let output_failed = output_status.output_state() == OutputStreamState::Failed;
    let recording = host.poll_recording();
    if let Some(message) = recording.message {
        diagnostic(message);
    }
    if let Some(error) = recording.terminal_owner_failure {
        if output_failed || recording.terminal_output_failure {
            terminal_output_failure.set(true);
        }
        diagnostic(error.clone());
        *terminal_owner_failure.borrow_mut() = Some(error);
        stop.store(true, Ordering::Release);
        return;
    }
    if output_failed || recording.terminal_output_failure {
        terminal_output_failure.set(true);
        stop.store(true, Ordering::Release);
        return;
    }
    if let Some(commands) = commands {
        match drain_operator_commands(host, commands, stop, diagnostic) {
            Some(RuntimeOperatorDrainFailure::Output) => {
                terminal_output_failure.set(true);
                stop.store(true, Ordering::Release);
                return;
            }
            Some(RuntimeOperatorDrainFailure::Owner {
                error,
                output_failed,
            }) => {
                if output_failed {
                    terminal_output_failure.set(true);
                }
                diagnostic(error.clone());
                *terminal_owner_failure.borrow_mut() = Some(error);
                stop.store(true, Ordering::Release);
                return;
            }
            None => {}
        }
    }
    if stop.load(Ordering::Acquire) {
        return;
    }
    if let Some(message) = host.poll_scene_performance_capture() {
        diagnostic(message);
    }
    if let Some(message) = host.poll_automation_capture() {
        diagnostic(message);
    }
    if let Some(message) = host.poll_mixer_update_export() {
        diagnostic(message);
    }
    if let Some(message) = host.poll_native_device_update_export() {
        diagnostic(message);
    }
    if let Some(message) = host.poll_loop_update_export() {
        diagnostic(message);
    }
}

fn host_runtime<H, O>(
    arguments: &RuntimeArguments,
    host: &mut H,
    output_status: &O,
    stop: &AtomicBool,
    commands: Option<&Receiver<OperatorCommand>>,
    diagnostic: &dyn Fn(String),
) -> Result<(), String>
where
    H: PanelHost
        + RuntimeAutomationCaptureControl
        + RuntimeMixerUpdateControl
        + RuntimeNativeDeviceUpdateControl
        + RuntimeLoopUpdateControl
        + RuntimeOperatorControl
        + RuntimeRecordingControl
        + RuntimeScenePerformanceControl,
    O: RuntimeOutputObservation,
{
    let terminal_output_failure = Cell::new(false);
    let terminal_owner_failure = RefCell::new(None);
    let mut owner_turn = |host: &mut H| {
        run_runtime_owner_turn(
            host,
            output_status,
            &terminal_output_failure,
            &terminal_owner_failure,
            stop,
            commands,
            diagnostic,
        );
    };

    let Some(address) = arguments.panel_listen() else {
        diagnostic("no panel link requested".to_owned());
        while !stop.load(Ordering::Acquire) {
            owner_turn(host);
            std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL);
        }
        return finish_runtime_host(
            host,
            output_status,
            terminal_output_failure.get(),
            terminal_owner_failure.borrow().clone(),
            diagnostic,
        );
    };

    let listener = match prepare_panel_listener(address) {
        Ok(listener) => listener,
        Err(error) => {
            contain_panel_service_failure(host, stop, diagnostic, error, &mut owner_turn);
            return finish_runtime_host(
                host,
                output_status,
                terminal_output_failure.get(),
                terminal_owner_failure.borrow().clone(),
                diagnostic,
            );
        }
    };
    diagnostic(format!(
        "panel link listening on {}",
        listener.local_addr().unwrap_or(address)
    ));

    let config = match LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS) {
        Ok(config) => config,
        Err(error) => {
            drop(listener);
            contain_panel_service_failure(
                host,
                stop,
                diagnostic,
                format!("panel link configuration is invalid: {error:?}"),
                &mut owner_turn,
            );
            return finish_runtime_host(
                host,
                output_status,
                terminal_output_failure.get(),
                terminal_owner_failure.borrow().clone(),
                diagnostic,
            );
        }
    };
    let started = Instant::now();
    let service = serve_panels_with_idle(
        &listener,
        host,
        config,
        stop,
        || millis_since(started),
        |served| report_to(served, diagnostic),
        &mut owner_turn,
    );
    drop(listener);
    if let Err(error) = service {
        contain_panel_service_failure(
            host,
            stop,
            diagnostic,
            format!("panel service ended: {error}"),
            &mut owner_turn,
        );
    }
    finish_runtime_host(
        host,
        output_status,
        terminal_output_failure.get(),
        terminal_owner_failure.borrow().clone(),
        diagnostic,
    )
}

fn finish_runtime_host<H: RuntimeRecordingControl, O: RuntimeOutputObservation>(
    host: &mut H,
    output_status: &O,
    terminal_output_failure: bool,
    terminal_owner_failure: Option<String>,
    diagnostic: &dyn Fn(String),
) -> Result<(), String> {
    let output = finish_runtime_output(output_status, terminal_output_failure, diagnostic);
    let recording = host.abort_recording_for_shutdown();
    if let Ok(Some(message)) = &recording {
        diagnostic(message.clone());
    }
    let result = match (output, recording) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
        (Err(output), Err(recording)) => Err(format!(
            "{output}; recording shutdown also failed: {recording}"
        )),
    };
    match (result, terminal_owner_failure) {
        (Ok(()), None) => Ok(()),
        (Ok(()), Some(error)) | (Err(error), None) => Err(error),
        (Err(error), Some(owner)) => Err(format!("{error}; {owner}")),
    }
}

fn prepare_panel_listener(address: std::net::SocketAddr) -> Result<TcpListener, String> {
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot poll {address}: {error}"))?;
    Ok(listener)
}

fn contain_panel_service_failure<H, I>(
    host: &mut H,
    stop: &AtomicBool,
    diagnostic: &dyn Fn(String),
    error: String,
    mut owner_turn: I,
) where
    H: PanelHost,
    I: FnMut(&mut H),
{
    diagnostic(error);
    for failure in host.disconnect_panel() {
        diagnostic(format!(
            "{:?} could not be released and stays held",
            failure.control()
        ));
    }
    diagnostic("the project stays hosted; request shutdown to stop it".to_owned());
    while !stop.load(Ordering::Acquire) {
        owner_turn(host);
        std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL);
    }
}

fn finish_runtime_output<O: RuntimeOutputObservation>(
    output_status: &O,
    terminal_output_failure: bool,
    diagnostic: &dyn Fn(String),
) -> Result<(), String> {
    if terminal_output_failure || output_status.output_state() == OutputStreamState::Failed {
        let message = "native audio ownership ended unexpectedly".to_owned();
        diagnostic(message.clone());
        Err(message)
    } else {
        Ok(())
    }
}

enum RuntimeOperatorDrainFailure {
    Output,
    Owner { error: String, output_failed: bool },
}

#[allow(
    clippy::too_many_lines,
    reason = "the bounded operator command dispatcher keeps every queue outcome explicit"
)]
fn drain_operator_commands<
    H: RuntimeLoopUpdateControl
        + RuntimeAutomationCaptureControl
        + RuntimeMixerUpdateControl
        + RuntimeNativeDeviceUpdateControl
        + RuntimeOperatorControl
        + RuntimeRecordingControl
        + RuntimeScenePerformanceControl,
>(
    host: &mut H,
    commands: &Receiver<OperatorCommand>,
    stop: &AtomicBool,
    diagnostic: &dyn Fn(String),
) -> Option<RuntimeOperatorDrainFailure> {
    for _ in 0..OPERATOR_COMMAND_CAPACITY {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let command = match commands.try_recv() {
            Ok(command) => command,
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                stop.store(true, Ordering::Release);
                break;
            }
        };
        match command {
            OperatorCommand::Start => match host.queue_transport(TransportCommand::Start) {
                Ok(()) => diagnostic("transport start queued".to_owned()),
                Err(error) => diagnostic(format!("transport start refused: {error}")),
            },
            OperatorCommand::Stop => match host.queue_transport(TransportCommand::Stop) {
                Ok(()) => diagnostic("transport stop queued".to_owned()),
                Err(error) => diagnostic(format!("transport stop refused: {error}")),
            },
            OperatorCommand::StartRecording { target_index } => {
                match host.start_recording(target_index) {
                    Ok(outcome) => {
                        if let Some(message) = outcome.message {
                            diagnostic(message);
                        }
                        if let Some(error) = outcome.terminal_owner_failure {
                            return Some(RuntimeOperatorDrainFailure::Owner {
                                error,
                                output_failed: outcome.terminal_output_failure,
                            });
                        }
                        if outcome.terminal_output_failure {
                            return Some(RuntimeOperatorDrainFailure::Output);
                        }
                    }
                    Err(error) => diagnostic(format!("Runtime recording start refused: {error}")),
                }
            }
            OperatorCommand::StopRecording => match host.stop_recording() {
                Ok(outcome) => {
                    if let Some(message) = outcome.message {
                        diagnostic(message);
                    }
                    if let Some(error) = outcome.terminal_owner_failure {
                        return Some(RuntimeOperatorDrainFailure::Owner {
                            error,
                            output_failed: outcome.terminal_output_failure,
                        });
                    }
                    if outcome.terminal_output_failure {
                        return Some(RuntimeOperatorDrainFailure::Output);
                    }
                }
                Err(error) => diagnostic(format!("Runtime recording stop refused: {error}")),
            },
            OperatorCommand::ExportMixerUpdate => match host.export_mixer_update() {
                Ok(message) => diagnostic(message),
                Err(error) => diagnostic(format!("mixer-update export refused: {error}")),
            },
            OperatorCommand::ExportNativeDeviceUpdate { chain_index } => {
                match host.export_native_device_update(chain_index) {
                    Ok(message) => diagnostic(message),
                    Err(error) => {
                        diagnostic(format!("native-device update export refused: {error}"));
                    }
                }
            }
            OperatorCommand::ExportLoopUpdate => match host.export_loop_update() {
                Ok(message) => diagnostic(message),
                Err(error) => diagnostic(format!("loop-update export refused: {error}")),
            },
            OperatorCommand::StartAutomationCapture { target_index, name } => {
                match host.start_automation_capture(target_index, name) {
                    Ok(message) => diagnostic(message),
                    Err(error) => {
                        diagnostic(format!("Runtime automation capture start refused: {error}"));
                    }
                }
            }
            OperatorCommand::StopAutomationCapture => match host.stop_automation_capture() {
                Ok(message) => diagnostic(message),
                Err(error) => {
                    diagnostic(format!("Runtime automation capture stop refused: {error}"));
                }
            },
            OperatorCommand::StartScenePerformanceCapture => {
                match host.start_scene_performance_capture() {
                    Ok(message) => diagnostic(message),
                    Err(error) => {
                        diagnostic(format!("scene-performance capture start refused: {error}"));
                    }
                }
            }
            OperatorCommand::StopScenePerformanceCapture => {
                match host.stop_scene_performance_capture() {
                    Ok(message) => diagnostic(message),
                    Err(error) => {
                        diagnostic(format!("scene-performance capture stop refused: {error}"));
                    }
                }
            }
            OperatorCommand::ReturnToStart => match host.queue_transport(TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            }) {
                Ok(()) => diagnostic("transport return to start queued".to_owned()),
                Err(error) => diagnostic(format!("transport return to start refused: {error}")),
            },
            OperatorCommand::Locate { frame, state } => {
                let action = match state {
                    LocateState::Preserve => "transport locate",
                    LocateState::Stopped => "transport locate-and-stop",
                    LocateState::Rolling => "transport play",
                };
                match host.queue_transport(TransportCommand::Locate { frame, state }) {
                    Ok(()) => diagnostic(format!("{action} to frame {frame} queued")),
                    Err(error) => diagnostic(format!("{action} refused: {error}")),
                }
            }
            OperatorCommand::LaunchScene { scene_index } => {
                match host.queue_transport(TransportCommand::LaunchScene { scene_index }) {
                    Ok(()) => diagnostic(format!("scene {} launch queued", scene_index + 1)),
                    Err(error) => {
                        diagnostic(format!("scene {} launch refused: {error}", scene_index + 1));
                    }
                }
            }
            OperatorCommand::StopInteractiveScenes => queue_operator_scene_stop(host, diagnostic),
            OperatorCommand::SetLoopEnabled { enabled } => {
                match host.queue_transport(TransportCommand::SetLoopEnabled(enabled)) {
                    Ok(()) => diagnostic(if enabled {
                        "session loop enable queued".to_owned()
                    } else {
                        "session loop disable queued".to_owned()
                    }),
                    Err(error) => diagnostic(format!("session loop toggle refused: {error}")),
                }
            }
            OperatorCommand::SetLoopRange { range } => {
                queue_operator_loop_range(host, range, diagnostic);
            }
            OperatorCommand::SetMetronomeEnabled { enabled } => {
                match host.queue_transport(TransportCommand::SetMetronomeEnabled(enabled)) {
                    Ok(()) => diagnostic(if enabled {
                        "metronome enable queued".to_owned()
                    } else {
                        "metronome disable queued".to_owned()
                    }),
                    Err(error) => diagnostic(format!("metronome toggle refused: {error}")),
                }
            }
            OperatorCommand::SetMetronomeGain { gain } => {
                match host.queue_transport(TransportCommand::SetMetronomeGain(gain)) {
                    Ok(()) => diagnostic("metronome level queued".to_owned()),
                    Err(error) => diagnostic(format!("metronome level refused: {error}")),
                }
            }
            OperatorCommand::SetInputMonitorGain { gain_millionths } => {
                let Some(gain) = operator_input_monitor_gain_from_millionths(gain_millionths)
                else {
                    diagnostic("input monitor level refused: value is outside 0..=100%".to_owned());
                    continue;
                };
                match host.set_input_monitor_gain(gain) {
                    Ok(()) => diagnostic("input monitor level applied".to_owned()),
                    Err(error) => diagnostic(format!("input monitor level refused: {error}")),
                }
            }
            OperatorCommand::SetInputMonitorMuted { muted } => {
                match host.set_input_monitor_muted(muted) {
                    Ok(()) => diagnostic(if muted {
                        "input monitor mute applied".to_owned()
                    } else {
                        "input monitor unmute applied".to_owned()
                    }),
                    Err(error) => diagnostic(format!("input monitor mute refused: {error}")),
                }
            }
            OperatorCommand::SetLoudnessState { state } => {
                match host.queue_loudness(LoudnessMeasurementCommand::SetState(state)) {
                    Ok(()) => diagnostic(match state {
                        LoudnessMeasurementState::Running => {
                            "programme loudness run queued".to_owned()
                        }
                        LoudnessMeasurementState::Standby => {
                            "programme loudness standby queued".to_owned()
                        }
                    }),
                    Err(error) => {
                        diagnostic(format!("programme loudness state refused: {error}"));
                    }
                }
            }
            OperatorCommand::ResetLoudness => {
                match host.queue_loudness(LoudnessMeasurementCommand::Reset) {
                    Ok(()) => diagnostic("programme loudness reset queued".to_owned()),
                    Err(error) => {
                        diagnostic(format!("programme loudness reset refused: {error}"));
                    }
                }
            }
            command @ (OperatorCommand::SetMixerChannelGain { .. }
            | OperatorCommand::SetMixerChannelPan { .. }
            | OperatorCommand::SetMixerSendGain { .. }
            | OperatorCommand::SetMixerSendPan { .. }
            | OperatorCommand::SetMainGain { .. }) => {
                drain_operator_mixer_value_command(host, &command, diagnostic);
            }
            command @ (OperatorCommand::SetMixerChannelMute { .. }
            | OperatorCommand::SetMixerChannelSolo { .. }
            | OperatorCommand::SetMixerSendEnabled { .. }
            | OperatorCommand::SetMainMute { .. }) => {
                drain_operator_mixer_boolean_command(host, &command, diagnostic);
            }
            OperatorCommand::SetNativeDeviceEnabled { device, enabled } => {
                match host.queue_native_device(NativeDeviceCommand::SetEnabled { device, enabled })
                {
                    Ok(()) => diagnostic(if enabled {
                        "native device enable queued".to_owned()
                    } else {
                        "native device bypass queued".to_owned()
                    }),
                    Err(error) => diagnostic(format!("native device bypass refused: {error}")),
                }
            }
            OperatorCommand::SetNativeDeviceState { device, state } => {
                match host.queue_native_device(NativeDeviceCommand::SetState { device, state }) {
                    Ok(()) => diagnostic("native device parameter update queued".to_owned()),
                    Err(error) => {
                        diagnostic(format!("native device parameter update refused: {error}"));
                    }
                }
            }
            OperatorCommand::Quit => {
                stop.store(true, Ordering::Release);
                break;
            }
        }
    }
    None
}

fn queue_operator_loop_range<H: RuntimeOperatorControl>(
    host: &mut H,
    range: Option<TransportLoopRange>,
    diagnostic: &dyn Fn(String),
) {
    match host.queue_transport(TransportCommand::SetLoopRange(range)) {
        Ok(()) => diagnostic(range.map_or_else(
            || "session loop range clear queued".to_owned(),
            |range| {
                format!(
                    "session loop range {}–{} queued",
                    range.start_frame(),
                    range.end_frame()
                )
            },
        )),
        Err(error) => diagnostic(format!("session loop range refused: {error}")),
    }
}

fn queue_operator_scene_stop<H: RuntimeOperatorControl>(host: &mut H, diagnostic: &dyn Fn(String)) {
    match host.queue_transport(TransportCommand::StopInteractiveScenes) {
        Ok(()) => diagnostic("interactive scene stop queued".to_owned()),
        Err(error) => diagnostic(format!("interactive scene stop refused: {error}")),
    }
}

fn drain_operator_mixer_boolean_command<H: RuntimeOperatorControl>(
    host: &mut H,
    command: &OperatorCommand,
    diagnostic: &dyn Fn(String),
) {
    let (command, accepted, refused) = match command {
        OperatorCommand::SetMixerChannelMute { channel, muted } => (
            MixerParameterCommand::ChannelMute {
                channel: *channel,
                muted: *muted,
            },
            if *muted {
                "mixer channel mute queued"
            } else {
                "mixer channel unmute queued"
            },
            "mixer channel mute refused",
        ),
        OperatorCommand::SetMixerChannelSolo { channel, solo } => (
            MixerParameterCommand::ChannelSolo {
                channel: *channel,
                solo: *solo,
            },
            if *solo {
                "mixer channel solo queued"
            } else {
                "mixer channel unsolo queued"
            },
            "mixer channel solo refused",
        ),
        OperatorCommand::SetMixerSendEnabled { send, enabled } => (
            MixerParameterCommand::SendEnabled {
                send: *send,
                enabled: *enabled,
            },
            if *enabled {
                "mixer send enable queued"
            } else {
                "mixer send disable queued"
            },
            "mixer send toggle refused",
        ),
        OperatorCommand::SetMainMute { muted } => (
            MixerParameterCommand::MainMute { muted: *muted },
            if *muted {
                "main output mute queued"
            } else {
                "main output unmute queued"
            },
            "main output mute refused",
        ),
        _ => unreachable!("only mixer boolean commands use this owner path"),
    };
    match host.queue_mixer(command) {
        Ok(()) => diagnostic(accepted.to_owned()),
        Err(error) => diagnostic(format!("{refused}: {error}")),
    }
}

fn drain_operator_mixer_value_command<H: RuntimeOperatorControl>(
    host: &mut H,
    command: &OperatorCommand,
    diagnostic: &dyn Fn(String),
) {
    let (command, name) = match command {
        OperatorCommand::SetMixerChannelGain {
            channel,
            gain_millionths,
        } => match LinearGain::from_millionths(*gain_millionths) {
            Ok(gain) => (
                MixerParameterCommand::ChannelGain {
                    channel: *channel,
                    gain,
                },
                "mixer channel gain",
            ),
            Err(error) => {
                diagnostic(format!("mixer channel gain refused: {error}"));
                return;
            }
        },
        OperatorCommand::SetMixerChannelPan {
            channel,
            pan_millionths,
        } => match Pan::from_millionths(*pan_millionths) {
            Ok(pan) => (
                MixerParameterCommand::ChannelPan {
                    channel: *channel,
                    pan,
                },
                "mixer channel pan",
            ),
            Err(error) => {
                diagnostic(format!("mixer channel pan refused: {error}"));
                return;
            }
        },
        OperatorCommand::SetMixerSendGain {
            send,
            gain_millionths,
        } => match LinearGain::from_millionths(*gain_millionths) {
            Ok(gain) => (
                MixerParameterCommand::SendGain { send: *send, gain },
                "mixer send gain",
            ),
            Err(error) => {
                diagnostic(format!("mixer send gain refused: {error}"));
                return;
            }
        },
        OperatorCommand::SetMixerSendPan {
            send,
            pan_millionths,
        } => match Pan::from_millionths(*pan_millionths) {
            Ok(pan) => (
                MixerParameterCommand::SendPan { send: *send, pan },
                "mixer send pan",
            ),
            Err(error) => {
                diagnostic(format!("mixer send pan refused: {error}"));
                return;
            }
        },
        OperatorCommand::SetMainGain { gain_millionths } => {
            match LinearGain::from_millionths(*gain_millionths) {
                Ok(gain) => (MixerParameterCommand::MainGain { gain }, "main output gain"),
                Err(error) => {
                    diagnostic(format!("main output gain refused: {error}"));
                    return;
                }
            }
        }
        _ => unreachable!("only mixer value commands use this owner path"),
    };
    match host.queue_mixer(command) {
        Ok(()) => diagnostic(format!("{name} queued")),
        Err(error) => diagnostic(format!("{name} refused: {error}")),
    }
}

fn operator_scene_catalog(session: &DesktopPlaybackSession) -> Result<Vec<OperatorScene>, String> {
    build_operator_scene_catalog(
        session.scene_count(),
        |index| session.scene_name(index).map(str::to_owned),
        |index| session.scene_availability(index),
    )
}

fn build_operator_scene_catalog(
    scene_count: usize,
    mut scene_name: impl FnMut(usize) -> Option<String>,
    mut scene_availability: impl FnMut(usize) -> Option<DesktopSceneAvailability>,
) -> Result<Vec<OperatorScene>, String> {
    let mut scenes = Vec::new();
    scenes
        .try_reserve_exact(scene_count)
        .map_err(|_| "operator scene catalog capacity is unavailable".to_owned())?;
    for index in 0..scene_count {
        let name = scene_name(index)
            .ok_or_else(|| format!("prepared scene {index} has no retained name"))?;
        let availability = scene_availability(index)
            .ok_or_else(|| format!("prepared scene {index} has no launch status"))?;
        scenes.push(OperatorScene {
            index,
            name,
            availability,
        });
    }
    Ok(scenes)
}

fn operator_mixer_catalog(
    session: &DesktopPlaybackSession,
) -> Result<Vec<OperatorMixerChannel>, String> {
    build_operator_mixer_catalog(
        session.mixer_channel_count(),
        |index| session.mixer_channel_name(index).map(str::to_owned),
        |index| {
            session
                .resolve_mixer_channel_index(index)
                .map_err(|error| error.to_string())
        },
        |index| {
            session
                .mixer_channel_supports_solo(index)
                .map_err(|error| error.to_string())
        },
    )
}

fn operator_mixer_send_catalog(
    session: &DesktopPlaybackSession,
    channels: &[OperatorMixerChannel],
) -> Result<Vec<OperatorMixerSend>, String> {
    build_operator_mixer_send_catalog(
        channels,
        |channel_index| session.mixer_send_count(channel_index),
        |channel_index, send_index| {
            session
                .mixer_send(channel_index, send_index)
                .map(|send| {
                    send.map(|send| (send.target_name().to_owned(), send.tap(), send.handle()))
                })
                .map_err(|error| error.to_string())
        },
    )
}

fn operator_native_device_catalog(
    session: &DesktopPlaybackSession,
) -> Result<Vec<OperatorNativeDevice>, String> {
    let count = session.native_device_count();
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(count)
        .map_err(|_| "operator native device catalog capacity is unavailable".to_owned())?;
    for index in 0..count {
        let device = session
            .native_device(index)
            .map_err(|error| {
                format!("prepared native device {index} cannot be controlled: {error}")
            })?
            .ok_or_else(|| format!("prepared native device {index} has no retained facts"))?;
        devices.push(operator_native_device(device));
    }
    Ok(devices)
}

fn operator_native_device(device: DesktopNativeDevice<'_>) -> OperatorNativeDevice {
    OperatorNativeDevice {
        track_name: device.track_name().to_owned(),
        kind: match device.kind_id() {
            "org.hyper-module.native-filter" => "Filter".to_owned(),
            "org.hyper-module.native-compressor" => "Compressor".to_owned(),
            _ => device.kind_id().to_owned(),
        },
        handle: device.handle(),
    }
}

/// Builds the immutable operator catalog from verified project facts rather
/// than from flattened session device handles. The resulting row position is
/// the only selection value accepted by the Runtime controller.
fn operator_native_device_chain_catalog(
    project: &PreparedProjectPlayback,
    configuration: &RuntimeNativeDeviceUpdateConfiguration,
) -> Result<Vec<OperatorNativeDeviceChain>, String> {
    let (loaded, mixer, ..) = project.session().parts();
    let catalog = configuration.chain_catalog().map_err(|error| {
        format!("verified native-device update catalog could not be prepared: {error}")
    })?;
    let mut chains = Vec::new();
    chains
        .try_reserve_exact(catalog.len())
        .map_err(|_| "operator native-device chain catalog capacity is unavailable".to_owned())?;
    for chain in catalog {
        let compiled_slot = mixer.slots().get(chain.slot).ok_or_else(|| {
            "verified native-device update catalog has an unavailable mixer slot".to_owned()
        })?;
        if compiled_slot.track_id() != chain.track_id {
            return Err(
                "verified native-device update catalog disagrees with the mixer track order"
                    .to_owned(),
            );
        }
        let track_name = loaded.track_name(&chain.track_id).ok_or_else(|| {
            "verified native-device update catalog has an unavailable track name".to_owned()
        })?;
        let channel = chain.slot.checked_add(1).ok_or_else(|| {
            "verified native-device update catalog has an invalid mixer channel number".to_owned()
        })?;
        let device_count = i32::try_from(chain.device_count).map_err(|_| {
            "verified native-device update catalog has an invalid device count".to_owned()
        })?;
        chains.push(OperatorNativeDeviceChain {
            track_label: format!("Channel {channel} · {track_name}"),
            device_count,
        });
    }
    Ok(chains)
}

fn build_operator_mixer_send_catalog(
    channels: &[OperatorMixerChannel],
    mut send_count: impl FnMut(usize) -> Option<usize>,
    mut send: impl FnMut(
        usize,
        usize,
    ) -> Result<Option<(String, DesktopMixerSendTap, MixerSendHandle)>, String>,
) -> Result<Vec<OperatorMixerSend>, String> {
    let mut sends = Vec::new();
    for channel in channels {
        let count = send_count(channel.index).ok_or_else(|| {
            format!(
                "prepared mixer channel {} has no retained send count",
                channel.index
            )
        })?;
        sends.try_reserve(count).map_err(|_| {
            format!(
                "operator mixer sends for channel {} exceed available capacity",
                channel.index
            )
        })?;
        for send_index in 0..count {
            let (target_name, tap, handle) = send(channel.index, send_index)
                .map_err(|error| {
                    format!(
                        "prepared mixer send {}/{} cannot be controlled: {error}",
                        channel.index, send_index
                    )
                })?
                .ok_or_else(|| {
                    format!(
                        "prepared mixer send {}/{} has no retained routing facts",
                        channel.index, send_index
                    )
                })?;
            sends.push(OperatorMixerSend {
                channel_index: channel.index,
                send_index,
                source_name: channel.name.clone(),
                target_name,
                tap,
                handle,
            });
        }
    }
    Ok(sends)
}

fn build_operator_mixer_catalog(
    channel_count: usize,
    mut channel_name: impl FnMut(usize) -> Option<String>,
    mut channel_handle: impl FnMut(usize) -> Result<MixerChannelHandle, String>,
    mut channel_solo_available: impl FnMut(usize) -> Result<bool, String>,
) -> Result<Vec<OperatorMixerChannel>, String> {
    let mut channels = Vec::new();
    channels
        .try_reserve_exact(channel_count)
        .map_err(|_| "operator mixer catalog capacity is unavailable".to_owned())?;
    for index in 0..channel_count {
        let name = channel_name(index)
            .ok_or_else(|| format!("prepared mixer channel {index} has no retained track name"))?;
        let handle = channel_handle(index).map_err(|error| {
            format!("prepared mixer channel {index} cannot be controlled: {error}")
        })?;
        let solo_available = channel_solo_available(index).map_err(|error| {
            format!("prepared mixer channel {index} has no Solo capability: {error}")
        })?;
        channels.push(OperatorMixerChannel {
            index,
            name,
            handle,
            solo_available,
        });
    }
    Ok(channels)
}

fn operator_control_mapping_summary(
    statuses: impl IntoIterator<Item = ControlBindingAvailability>,
) -> OperatorControlMappingSummary {
    let mut summary = OperatorControlMappingSummary::default();
    for availability in statuses {
        match availability {
            ControlBindingAvailability::Live => {
                summary.live = summary.live.saturating_add(1);
            }
            ControlBindingAvailability::Unavailable(reason) => {
                let count = match reason {
                    ControlBindingUnavailableReason::CueOutputUnavailable => {
                        &mut summary.cue_output_unavailable
                    }
                    ControlBindingUnavailableReason::SceneDisabled => &mut summary.scene_disabled,
                    ControlBindingUnavailableReason::SceneConsolidationRequired => {
                        &mut summary.scene_consolidation_required
                    }
                    ControlBindingUnavailableReason::SceneAudioNotPrepared => {
                        &mut summary.scene_audio_not_prepared
                    }
                };
                *count = count.saturating_add(1);
            }
        }
    }
    summary
}

fn operator_control_mapping_presentation(
    summary: OperatorControlMappingSummary,
    playback_available: bool,
    runtime_terminal: bool,
) -> OperatorControlMappingPresentation {
    if runtime_terminal {
        return OperatorControlMappingPresentation {
            state: "RUNTIME STOPPED".to_owned(),
            detail: "Binding dispatch is unavailable".to_owned(),
            warning: true,
        };
    }
    if !playback_available {
        let enabled = summary.enabled();
        let binding_label = if enabled == 1 { "binding" } else { "bindings" };
        return OperatorControlMappingPresentation {
            state: "PLAYBACK TRANSITION".to_owned(),
            detail: format!(
                "{enabled} enabled {binding_label} · dispatch resumes after owner recovery"
            ),
            warning: true,
        };
    }
    let unavailable = summary.unavailable();
    if summary.enabled() == 0 {
        return OperatorControlMappingPresentation {
            state: "NO ENABLED BINDINGS".to_owned(),
            detail: "Persisted disabled bindings do not enter Runtime".to_owned(),
            warning: false,
        };
    }
    if unavailable == 0 {
        return OperatorControlMappingPresentation {
            state: format!("{} LIVE", summary.live),
            detail: "All enabled bindings accepted by the current playback owner".to_owned(),
            warning: false,
        };
    }
    let mut reasons = Vec::new();
    for (count, label) in [
        (summary.cue_output_unavailable, "No cue output"),
        (summary.scene_disabled, "Scene disabled"),
        (
            summary.scene_consolidation_required,
            "Scene consolidation required",
        ),
        (summary.scene_audio_not_prepared, "Scene audio not prepared"),
    ] {
        if count > 0 {
            reasons.push(format!("{label}: {count}"));
        }
    }
    OperatorControlMappingPresentation {
        state: format!("{} LIVE · {unavailable} UNAVAILABLE", summary.live),
        detail: reasons.join(" · "),
        warning: true,
    }
}

fn apply_operator_control_mapping_observation(
    ui: &OperatorWindow,
    summary: OperatorControlMappingSummary,
    playback_available: bool,
    runtime_terminal: bool,
) {
    let presentation =
        operator_control_mapping_presentation(summary, playback_available, runtime_terminal);
    ui.set_control_mapping_state(presentation.state.into());
    ui.set_control_mapping_detail(presentation.detail.into());
    ui.set_control_mapping_warning(presentation.warning);
}

fn spawn_operator_worker(
    arguments: RuntimeArguments,
    stop: Arc<AtomicBool>,
    commands: Receiver<OperatorCommand>,
    diagnostic: Arc<Mutex<String>>,
) -> OperatorWorker {
    let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
    let (completion_tx, completion) = std::sync::mpsc::sync_channel(1);
    let handle = std::thread::spawn(move || {
        let result = match start_runtime(&arguments) {
            Ok(mut runtime) => {
                let control_mapping = runtime.control_mapping;
                let native_device_chains = runtime.native_device_chains.clone();
                let automation_targets = runtime.automation_targets.clone();
                let recording_targets = runtime.recording_targets.clone();
                let ready_model = runtime
                    .host
                    .owner
                    .playback()
                    .ok_or_else(|| "Runtime playback owner is unavailable at startup".to_owned())
                    .and_then(|playback| {
                        operator_scene_catalog(playback).and_then(|scenes| {
                            operator_mixer_catalog(playback).and_then(|mixer_channels| {
                                operator_mixer_send_catalog(playback, &mixer_channels).and_then(
                                    |mixer_sends| {
                                        operator_native_device_catalog(playback).map(
                                            |native_devices| OperatorReady {
                                                observers: runtime.observers.clone(),
                                                control_mapping,
                                                scenes,
                                                mixer_channels,
                                                mixer_sends,
                                                native_devices,
                                                native_device_chains,
                                                automation_targets,
                                                recording_targets,
                                            },
                                        )
                                    },
                                )
                            })
                        })
                    });
                match ready_model {
                    Ok(ready_model) => {
                        announce_runtime(&arguments, &runtime);
                        if ready_tx.send(Ok(ready_model)).is_err() {
                            Err("operator UI ended before accepting startup evidence".to_owned())
                        } else {
                            host_runtime(
                                &arguments,
                                &mut runtime.host,
                                &runtime.observers.audio,
                                &stop,
                                Some(&commands),
                                &|message| set_diagnostic(&diagnostic, message),
                            )
                        }
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.clone()));
                        Err(error)
                    }
                }
            }
            Err(error) => {
                let _ = ready_tx.send(Err(error.clone()));
                Err(error)
            }
        };
        let _ = completion_tx.send(result.clone());
        result
    });
    OperatorWorker {
        ready,
        completion,
        handle,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the linear scope makes Slint timer and worker ownership explicit"
)]
fn run_operator_ui(arguments: RuntimeArguments, stop: &Arc<AtomicBool>) -> Result<(), String> {
    let ui =
        OperatorWindow::new().map_err(|error| format!("operator UI could not open: {error}"))?;
    let meter_config = SamplePeakBallisticsConfig::try_new(
        SAMPLE_RATE_HZ,
        OUTPUT_METER_FLOOR_DBFS,
        OUTPUT_METER_FALLOFF_DB_PER_SECOND,
        u64::MAX,
    )
    .map_err(|error| format!("operator output meter policy is invalid: {error}"))?;
    let (commands_tx, commands_rx) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
    let diagnostic = Arc::new(Mutex::new(
        "starting verified Runtime deployment".to_owned(),
    ));
    let completion_close_error = Arc::new(Mutex::new(None));
    let worker = spawn_operator_worker(
        arguments,
        Arc::clone(stop),
        commands_rx,
        Arc::clone(&diagnostic),
    );
    let ready = match worker.ready.recv() {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => return finish_failed_operator_startup(error, worker.handle),
        Err(_) => {
            return finish_failed_operator_startup(
                "Runtime worker ended before publishing startup evidence".to_owned(),
                worker.handle,
            );
        }
    };
    let OperatorReady {
        observers,
        control_mapping,
        scenes,
        mixer_channels,
        mixer_sends,
        native_devices,
        native_device_chains,
        automation_targets,
        recording_targets,
    } = ready;
    let RuntimeMeterReceiverGeneration {
        output: meters,
        input: input_meters,
    } = observers
        .meter_receivers
        .take()
        .ok_or_else(|| "operator meter authority was not published at startup".to_owned())?;
    let meter_observer = Rc::new(RefCell::new(OperatorMeterObserver::new(
        meters,
        meter_config,
    )));
    let input_meter_observer =
        Rc::new(RefCell::new(input_meters.map(|meters| {
            OperatorInputMeterObserver::new(meters, meter_config)
        })));
    let timeline_marker_model = Rc::new(VecModel::from(operator_timeline_marker_rows(
        &observers.facts.timeline_markers,
    )));
    let scene_model = Rc::new(VecModel::from(operator_scene_rows(&scenes)));
    let mixer_model = Rc::new(VecModel::from(operator_mixer_rows(&mixer_channels)));
    let mixer_send_model = Rc::new(VecModel::from(operator_mixer_send_rows(&mixer_sends)));
    let native_device_model = Rc::new(VecModel::from(operator_native_device_rows(&native_devices)));
    let native_device_chain_model = Rc::new(VecModel::from(operator_native_device_chain_rows(
        &native_device_chains,
    )));
    let automation_target_count = automation_targets.len();
    let automation_target_names = automation_targets
        .into_iter()
        .map(SharedString::from)
        .collect::<Vec<_>>();
    let recording_target_count = recording_targets.len();
    let recording_target_names = recording_targets
        .into_iter()
        .map(SharedString::from)
        .collect::<Vec<_>>();
    let transport_authority = Rc::new(Cell::new(None));
    let loop_authority = Rc::new(Cell::new(None));
    let metronome_authority = Rc::new(Cell::new(None));
    let seek_authority = Rc::new(Cell::new(None));
    let timeline_marker_authority = Rc::new(RefCell::new(None));
    let scene_launch_authority = Rc::new(Cell::new(false));
    let scene_selection = Rc::new(Cell::new(None));
    let scene_stop_authority = Rc::new(Cell::new(false));
    let loudness_authority = Rc::new(Cell::new(None));
    let mixer_authority = Rc::new(RefCell::new(None));
    let native_device_authority = Rc::new(RefCell::new(None));
    ui.set_timeline_markers(ModelRc::from(Rc::clone(&timeline_marker_model)));
    ui.set_scenes(ModelRc::from(Rc::clone(&scene_model)));
    ui.set_mixer_channels(ModelRc::from(Rc::clone(&mixer_model)));
    ui.set_mixer_sends(ModelRc::from(Rc::clone(&mixer_send_model)));
    ui.set_native_devices(ModelRc::from(Rc::clone(&native_device_model)));
    ui.set_native_device_chains(ModelRc::from(Rc::clone(&native_device_chain_model)));
    ui.set_automation_target_names(ModelRc::from(Rc::new(VecModel::from(
        automation_target_names,
    ))));
    ui.set_selected_automation_target_index(if automation_target_count == 0 { -1 } else { 0 });
    ui.set_recording_target_names(ModelRc::from(Rc::new(VecModel::from(
        recording_target_names,
    ))));
    ui.set_selected_recording_target_index(if recording_target_count == 0 { -1 } else { 0 });

    apply_static_facts(&ui, &observers.facts);
    apply_operator_control_mapping_observation(&ui, control_mapping, true, false);
    wire_operator_transport_controls(&ui, &commands_tx, &diagnostic, &transport_authority);
    wire_operator_seek(&ui, &commands_tx, &diagnostic, &seek_authority);
    wire_operator_timeline_marker_navigation(
        &ui,
        &commands_tx,
        &diagnostic,
        &timeline_marker_authority,
    );
    wire_operator_quit(&ui, &commands_tx, stop, &diagnostic);
    wire_operator_peak_reset(&ui, &meter_observer, &loudness_authority);
    wire_operator_loudness_controls(&ui, &commands_tx, &diagnostic, &loudness_authority);
    wire_operator_mixer_controls(&ui, &commands_tx, &diagnostic, &mixer_authority);
    wire_operator_mixer_update_export(&ui, &commands_tx, &diagnostic, &observers.mixer_update);
    wire_operator_native_device_update_export(
        &ui,
        &commands_tx,
        &diagnostic,
        &native_device_chains,
        &observers.native_device_update,
    );
    wire_operator_loop_update_export(&ui, &commands_tx, &diagnostic, &observers.loop_update);
    wire_operator_native_device_controls(
        &ui,
        &commands_tx,
        &diagnostic,
        &native_device_model,
        &native_device_authority,
    );
    wire_operator_scene_launch(
        &ui,
        &commands_tx,
        &diagnostic,
        &scenes,
        &scene_launch_authority,
        &scene_model,
        &scene_selection,
    );
    wire_operator_scene_stop(&ui, &commands_tx, &diagnostic, &scene_stop_authority);
    wire_operator_scene_performance_capture(
        &ui,
        &commands_tx,
        &diagnostic,
        &observers.scene_performance,
    );
    wire_operator_recording(
        &ui,
        &commands_tx,
        &diagnostic,
        &observers.recording,
        recording_target_count,
    );
    wire_operator_automation_capture(
        &ui,
        &commands_tx,
        &diagnostic,
        &observers.automation_capture,
        automation_target_count,
    );
    wire_operator_loop_toggle(&ui, &commands_tx, &diagnostic, &loop_authority);
    wire_operator_loop_range(&ui, &commands_tx, &diagnostic, &loop_authority);
    wire_operator_metronome_toggle(&ui, &commands_tx, &diagnostic, &metronome_authority);
    wire_operator_metronome_level(&ui, &commands_tx, &diagnostic, &metronome_authority);
    wire_operator_input_monitor_controls(&ui, &commands_tx, &diagnostic);

    let audio = observers.audio.snapshot();
    let output_state = audio.output_status.state();
    apply_operator_cycle_observation(
        &ui,
        &observers,
        output_state,
        &diagnostic,
        &transport_authority,
        &loop_authority,
        &metronome_authority,
        &seek_authority,
        &timeline_marker_model,
        &timeline_marker_authority,
        &scene_model,
        &scene_launch_authority,
        &scene_selection,
        &scene_stop_authority,
        false,
    );
    apply_operator_meter_observation(
        &ui,
        &mut meter_observer.borrow_mut(),
        &loudness_authority,
        false,
    );
    apply_operator_input_monitor_observation(&ui, &observers, &input_meter_observer, false);
    apply_operator_mixer_observation(
        &ui,
        &mixer_model,
        &mixer_send_model,
        &mixer_channels,
        &mixer_sends,
        &mixer_authority,
        &audio.recovery.snapshot(),
        false,
    );
    apply_operator_native_device_observation(
        &ui,
        &native_device_model,
        &native_devices,
        &native_device_authority,
        &audio.recovery.snapshot(),
        false,
    );
    apply_operator_scene_performance_observation(&ui, observers.scene_performance.as_ref(), false);
    apply_operator_recording_observation(&ui, observers.recording.as_ref(), false);
    apply_operator_automation_capture_observation(
        &ui,
        observers.automation_capture.as_ref(),
        false,
    );
    apply_operator_mixer_update_observation(&ui, observers.mixer_update.as_ref(), false);
    apply_operator_native_device_update_observation(
        &ui,
        observers.native_device_update.as_ref(),
        false,
    );
    apply_operator_loop_update_observation(&ui, observers.loop_update.as_ref(), false);

    let weak = ui.as_weak();
    let timer_observers = observers.clone();
    let timer_meter_observer = Rc::clone(&meter_observer);
    let timer_input_meter_observer = Rc::clone(&input_meter_observer);
    let timer_scene_model = Rc::clone(&scene_model);
    let timer_scene_launch_authority = Rc::clone(&scene_launch_authority);
    let timer_scene_selection = Rc::clone(&scene_selection);
    let timer_scene_stop_authority = Rc::clone(&scene_stop_authority);
    let timer_transport_authority = Rc::clone(&transport_authority);
    let timer_loop_authority = Rc::clone(&loop_authority);
    let timer_metronome_authority = Rc::clone(&metronome_authority);
    let timer_seek_authority = Rc::clone(&seek_authority);
    let timer_timeline_marker_model = Rc::clone(&timeline_marker_model);
    let timer_timeline_marker_authority = Rc::clone(&timeline_marker_authority);
    let timer_loudness_authority = Rc::clone(&loudness_authority);
    let timer_mixer_model = Rc::clone(&mixer_model);
    let timer_mixer_send_model = Rc::clone(&mixer_send_model);
    let timer_mixer_authority = Rc::clone(&mixer_authority);
    let timer_native_device_model = Rc::clone(&native_device_model);
    let timer_native_device_authority = Rc::clone(&native_device_authority);
    let timer_scene_performance = Arc::clone(&observers.scene_performance);
    let timer_recording = Arc::clone(&observers.recording);
    let timer_automation_capture = Arc::clone(&observers.automation_capture);
    let timer_mixer_update = Arc::clone(&observers.mixer_update);
    let timer_native_device_update = Arc::clone(&observers.native_device_update);
    let timer_loop_update = Arc::clone(&observers.loop_update);
    let timer_native_devices = native_devices;
    let timer_mixer_channels = mixer_channels;
    let timer_mixer_sends = mixer_sends;
    let timer_diagnostic = Arc::clone(&diagnostic);
    let timer_close_error = Arc::clone(&completion_close_error);
    let mut completion_observed = false;
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        if let Some(ui) = weak.upgrade() {
            if let Some(replacement) = timer_observers.meter_receivers.take() {
                *timer_meter_observer.borrow_mut() =
                    OperatorMeterObserver::new(replacement.output, meter_config);
                *timer_input_meter_observer.borrow_mut() = replacement
                    .input
                    .map(|meters| OperatorInputMeterObserver::new(meters, meter_config));
                timer_loudness_authority.set(None);
            }
            let audio = timer_observers.audio.snapshot();
            let output_state = audio.output_status.state();
            let runtime_terminal = completion_observed || output_state == OutputStreamState::Failed;
            let playback_unavailable = !timer_observers.playback_available.load(Ordering::Acquire);
            let playback_terminal = runtime_terminal || playback_unavailable;
            apply_operator_control_mapping_observation(
                &ui,
                control_mapping,
                !playback_unavailable,
                runtime_terminal,
            );
            apply_operator_cycle_observation(
                &ui,
                &timer_observers,
                output_state,
                &timer_diagnostic,
                &timer_transport_authority,
                &timer_loop_authority,
                &timer_metronome_authority,
                &timer_seek_authority,
                &timer_timeline_marker_model,
                &timer_timeline_marker_authority,
                &timer_scene_model,
                &timer_scene_launch_authority,
                &timer_scene_selection,
                &timer_scene_stop_authority,
                playback_terminal,
            );
            apply_operator_meter_observation(
                &ui,
                &mut timer_meter_observer.borrow_mut(),
                &timer_loudness_authority,
                playback_terminal,
            );
            apply_operator_input_monitor_observation(
                &ui,
                &timer_observers,
                &timer_input_meter_observer,
                playback_terminal,
            );
            apply_operator_mixer_observation(
                &ui,
                &timer_mixer_model,
                &timer_mixer_send_model,
                &timer_mixer_channels,
                &timer_mixer_sends,
                &timer_mixer_authority,
                &audio.recovery.snapshot(),
                playback_terminal,
            );
            apply_operator_native_device_observation(
                &ui,
                &timer_native_device_model,
                &timer_native_devices,
                &timer_native_device_authority,
                &audio.recovery.snapshot(),
                playback_terminal,
            );
            apply_operator_scene_performance_observation(
                &ui,
                timer_scene_performance.as_ref(),
                playback_terminal,
            );
            apply_operator_recording_observation(&ui, timer_recording.as_ref(), runtime_terminal);
            apply_operator_automation_capture_observation(
                &ui,
                timer_automation_capture.as_ref(),
                playback_terminal,
            );
            apply_operator_mixer_update_observation(
                &ui,
                timer_mixer_update.as_ref(),
                playback_terminal,
            );
            apply_operator_native_device_update_observation(
                &ui,
                timer_native_device_update.as_ref(),
                playback_terminal,
            );
            apply_operator_loop_update_observation(
                &ui,
                timer_loop_update.as_ref(),
                playback_terminal,
            );
            if !completion_observed
                && let Some(close_result) = apply_operator_worker_completion(
                    &ui,
                    &worker.completion,
                    &timer_diagnostic,
                    &timer_transport_authority,
                    &timer_scene_model,
                    &timer_scene_launch_authority,
                    &timer_scene_stop_authority,
                    || {
                        close_operator_surface(
                            || {
                                ui.hide().map_err(|error| {
                                    format!("operator surface could not hide: {error}")
                                })
                            },
                            || {
                                slint::quit_event_loop().map_err(|error| {
                                    format!("operator event loop could not terminate: {error}")
                                })
                            },
                        )
                    },
                )
            {
                completion_observed = true;
                revoke_operator_loop_authority(&ui, &timer_loop_authority);
                revoke_operator_seek_authority(&ui, &timer_seek_authority);
                revoke_operator_timeline_marker_authority(
                    &ui,
                    &timer_timeline_marker_model,
                    &timer_timeline_marker_authority,
                );
                revoke_operator_loudness_authority(&ui, &timer_loudness_authority);
                revoke_operator_mixer_authority(
                    &ui,
                    &timer_mixer_model,
                    &timer_mixer_send_model,
                    &timer_mixer_authority,
                    true,
                );
                if let Err(error) = close_result {
                    set_first_error(&timer_close_error, error);
                }
            }
        }
    });

    let ui_result = ui
        .run()
        .map_err(|error| format!("operator UI failed: {error}"));
    drop(timer);
    let ui_result = reconcile_operator_ui_close(ui_result, &completion_close_error);
    finish_operator_worker(ui_result, stop, &commands_tx, worker.handle)
}

fn finish_failed_operator_startup(
    reported_error: String,
    worker: std::thread::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    match worker.join() {
        Ok(Ok(())) => Err(reported_error),
        Ok(Err(worker_error)) => Err(worker_error),
        Err(_) => Err("Runtime worker panicked during startup".to_owned()),
    }
}

fn finish_operator_worker(
    ui_result: Result<(), String>,
    stop: &AtomicBool,
    commands: &SyncSender<OperatorCommand>,
    worker: std::thread::JoinHandle<Result<(), String>>,
) -> Result<(), String> {
    stop.store(true, Ordering::Release);
    let _ = commands.try_send(OperatorCommand::Quit);
    let worker_result = worker
        .join()
        .map_err(|_| "Runtime worker panicked during shutdown".to_owned())?;
    reconcile_operator_results(ui_result, worker_result)
}

fn reconcile_operator_results(
    ui_result: Result<(), String>,
    worker_result: Result<(), String>,
) -> Result<(), String> {
    match (worker_result, ui_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(ui_error)) => Err(ui_error),
        (Err(worker_error), Ok(())) => Err(worker_error),
        (Err(worker_error), Err(ui_error)) => Err(format!(
            "{worker_error}; operator UI shutdown also failed: {ui_error}"
        )),
    }
}

fn reconcile_operator_ui_close(
    ui_result: Result<(), String>,
    close_error: &Mutex<Option<String>>,
) -> Result<(), String> {
    let close_error = close_error.lock().ok().and_then(|mut error| error.take());
    match (ui_result, close_error) {
        (Ok(()), None) => Ok(()),
        (Err(error), None) | (Ok(()), Some(error)) => Err(error),
        (Err(ui_error), Some(close_error)) => Err(format!(
            "{ui_error}; operator surface shutdown also failed: {close_error}"
        )),
    }
}

fn set_diagnostic(diagnostic: &Mutex<String>, message: String) {
    if let Ok(mut current) = diagnostic.lock() {
        *current = message;
    }
}

fn set_first_error(error: &Mutex<Option<String>>, message: String) {
    if let Ok(mut current) = error.lock()
        && current.is_none()
    {
        *current = Some(message);
    }
}

fn close_operator_surface(
    hide: impl FnOnce() -> Result<(), String>,
    quit_event_loop: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let Err(hide_error) = hide() else {
        return Ok(());
    };
    match quit_event_loop() {
        Ok(()) => Err(format!(
            "{hide_error}; explicit event-loop termination was required"
        )),
        Err(quit_error) => Err(format!("{hide_error}; {quit_error}")),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the terminal coordinator must revoke the independently owned transport and scene authorities before it closes the sole operator surface"
)]
fn apply_operator_worker_completion(
    ui: &OperatorWindow,
    completion: &Receiver<Result<(), String>>,
    diagnostic: &Mutex<String>,
    transport_authority: &Cell<Option<PlaybackRecoveryTransportState>>,
    scene_model: &VecModel<OperatorSceneRow>,
    scene_launch_authority: &Cell<bool>,
    scene_stop_authority: &Cell<bool>,
    close_operator_surface: impl FnOnce() -> Result<(), String>,
) -> Option<Result<(), String>> {
    let message = match completion.try_recv() {
        Ok(Ok(())) => "Runtime worker stopped".to_owned(),
        Ok(Err(error)) => format!("Runtime worker stopped: {error}"),
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => {
            "Runtime worker ended without publishing its result".to_owned()
        }
    };
    revoke_operator_transport_authority(ui, transport_authority);
    revoke_operator_scene_launch_authority(ui, scene_model, scene_launch_authority);
    revoke_operator_scene_stop_authority(ui, scene_stop_authority);
    revoke_operator_scene_performance_authority(ui);
    revoke_operator_recording_authority(ui);
    revoke_operator_automation_capture_authority(ui);
    let close_result = close_operator_surface();
    let message = close_result
        .as_ref()
        .err()
        .map_or(message.clone(), |error| format!("{message}; {error}"));
    set_diagnostic(diagnostic, message.clone());
    ui.set_diagnostic(message.into());
    Some(close_result)
}

fn wire_operator_transport_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<PlaybackRecoveryTransportState>>>,
) {
    let register = |action, register: fn(&OperatorWindow, Box<dyn FnMut()>)| {
        let sender = sender.clone();
        let diagnostic = Arc::clone(diagnostic);
        let authority = Rc::clone(authority);
        register(
            ui,
            Box::new(move || {
                queue_operator_transport_action(&sender, &diagnostic, authority.get(), action);
            }),
        );
    };
    register(
        OperatorTransportAction::Start,
        OperatorWindow::on_start_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
    );
    register(
        OperatorTransportAction::Stop,
        OperatorWindow::on_stop_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
    );
    register(
        OperatorTransportAction::Toggle,
        OperatorWindow::on_toggle_transport_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
    );
    register(
        OperatorTransportAction::ReturnToStart,
        OperatorWindow::on_return_to_start_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
    );
}

fn queue_operator_transport_action(
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    state: Option<PlaybackRecoveryTransportState>,
    action: OperatorTransportAction,
) {
    match operator_transport_command(state, action) {
        Ok(command) => {
            send_operator_command(sender, command, diagnostic);
        }
        Err(message) => set_diagnostic(diagnostic, message.to_owned()),
    }
}

fn operator_transport_command(
    state: Option<PlaybackRecoveryTransportState>,
    action: OperatorTransportAction,
) -> Result<OperatorCommand, &'static str> {
    match (action, state) {
        (
            OperatorTransportAction::Start | OperatorTransportAction::Toggle,
            Some(PlaybackRecoveryTransportState::Stopped),
        ) => Ok(OperatorCommand::Start),
        (
            OperatorTransportAction::Stop | OperatorTransportAction::Toggle,
            Some(PlaybackRecoveryTransportState::Rolling),
        ) => Ok(OperatorCommand::Stop),
        (OperatorTransportAction::ReturnToStart, Some(_)) => Ok(OperatorCommand::ReturnToStart),
        (OperatorTransportAction::Start, Some(PlaybackRecoveryTransportState::Rolling)) => {
            Err("transport start refused: recovered transport is already rolling")
        }
        (OperatorTransportAction::Stop, Some(PlaybackRecoveryTransportState::Stopped)) => {
            Err("transport stop refused: recovered transport is already stopped")
        }
        (OperatorTransportAction::Toggle, None) => {
            Err("transport toggle refused: coherent transport state is unavailable")
        }
        (OperatorTransportAction::ReturnToStart, None) => {
            Err("transport return to start refused: coherent transport state is unavailable")
        }
        (OperatorTransportAction::Start, None) => {
            Err("transport start refused: coherent transport state is unavailable")
        }
        (OperatorTransportAction::Stop, None) => {
            Err("transport stop refused: coherent transport state is unavailable")
        }
    }
}

fn wire_operator_seek(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<usize>>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_locate_percent_requested(move |percent| {
        let Some(end_frame) = authority.get() else {
            set_diagnostic(
                &diagnostic,
                "transport locate refused: bounded playback extent is unavailable".to_owned(),
            );
            return;
        };
        let Some(frame) = operator_seek_frame(end_frame, percent) else {
            set_diagnostic(
                &diagnostic,
                "transport locate refused: position is outside 0..=100%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &sender,
            OperatorCommand::Locate {
                frame,
                state: LocateState::Preserve,
            },
            &diagnostic,
        );
    });
}

fn wire_operator_timeline_marker_navigation(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<Arc<[OperatorTimelineMarker]>>>>,
) {
    let register = |state| {
        let sender = sender.clone();
        let diagnostic = Arc::clone(diagnostic);
        let authority = Rc::clone(authority);
        move |requested_id: slint::SharedString| {
            queue_operator_timeline_marker_action(
                &sender,
                &diagnostic,
                &authority,
                requested_id.as_str(),
                state,
            );
        }
    };
    ui.on_locate_timeline_marker_requested(register(LocateState::Preserve));
    ui.on_play_timeline_marker_requested(register(LocateState::Rolling));
}

fn queue_operator_timeline_marker_action(
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Mutex<String>,
    authority: &RefCell<Option<Arc<[OperatorTimelineMarker]>>>,
    requested_id: &str,
    state: LocateState,
) {
    let action = match state {
        LocateState::Preserve => "locate",
        LocateState::Stopped => "locate-and-stop",
        LocateState::Rolling => "play",
    };
    let frame = {
        let catalog = authority.borrow();
        let Some(markers) = catalog.as_ref() else {
            set_diagnostic(
                diagnostic,
                format!(
                    "timeline marker {action} refused: verified marker navigation is unavailable"
                ),
            );
            return;
        };
        let Some(marker) = markers
            .iter()
            .find(|marker| marker.id.as_str() == requested_id)
        else {
            set_diagnostic(
                diagnostic,
                format!("timeline marker {action} refused: marker identity is not current"),
            );
            return;
        };
        let Some(frame) = marker.frame else {
            set_diagnostic(
                diagnostic,
                format!(
                    "timeline marker {action} refused: marker cannot be represented by this Runtime"
                ),
            );
            return;
        };
        frame
    };
    send_operator_command(sender, OperatorCommand::Locate { frame, state }, diagnostic);
}

fn wire_operator_quit(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    stop: &Arc<AtomicBool>,
    diagnostic: &Arc<Mutex<String>>,
) {
    let sender_for_button = sender.clone();
    let stop_for_button = Arc::clone(stop);
    let diagnostic_for_button = Arc::clone(diagnostic);
    let weak = ui.as_weak();
    ui.on_quit_requested(move || {
        request_operator_shutdown(&stop_for_button, &sender_for_button, &diagnostic_for_button);
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let sender = sender.clone();
    let stop = Arc::clone(stop);
    let diagnostic = Arc::clone(diagnostic);
    ui.window().on_close_requested(move || {
        request_operator_shutdown(&stop, &sender, &diagnostic);
        CloseRequestResponse::HideWindow
    });
}

fn wire_operator_peak_reset(
    ui: &OperatorWindow,
    observer: &Rc<RefCell<OperatorMeterObserver>>,
    loudness_authority: &Rc<Cell<Option<LoudnessMeasurementState>>>,
) {
    let observer = Rc::clone(observer);
    let loudness_authority = Rc::clone(loudness_authority);
    let weak = ui.as_weak();
    ui.on_reset_output_peak_requested(move || {
        let mut observer = observer.borrow_mut();
        observer.reset_peak_hold();
        if let Some(ui) = weak.upgrade() {
            let terminal_latched = loudness_authority.get().is_none();
            apply_operator_meter_observation(
                &ui,
                &mut observer,
                &loudness_authority,
                terminal_latched,
            );
        }
    });
}

fn wire_operator_loudness_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<LoudnessMeasurementState>>>,
) {
    let run_sender = sender.clone();
    let run_diagnostic = Arc::clone(diagnostic);
    let run_authority = Rc::clone(authority);
    ui.on_loudness_run_requested(move || {
        if run_authority.get() != Some(LoudnessMeasurementState::Standby) {
            set_diagnostic(
                &run_diagnostic,
                "programme loudness run refused: accepted standby state is unavailable".to_owned(),
            );
            return;
        }
        send_operator_command(
            &run_sender,
            OperatorCommand::SetLoudnessState {
                state: LoudnessMeasurementState::Running,
            },
            &run_diagnostic,
        );
    });

    let standby_sender = sender.clone();
    let standby_diagnostic = Arc::clone(diagnostic);
    let standby_authority = Rc::clone(authority);
    ui.on_loudness_standby_requested(move || {
        if standby_authority.get() != Some(LoudnessMeasurementState::Running) {
            set_diagnostic(
                &standby_diagnostic,
                "programme loudness standby refused: accepted running state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &standby_sender,
            OperatorCommand::SetLoudnessState {
                state: LoudnessMeasurementState::Standby,
            },
            &standby_diagnostic,
        );
    });

    let reset_sender = sender.clone();
    let reset_diagnostic = Arc::clone(diagnostic);
    let reset_authority = Rc::clone(authority);
    ui.on_loudness_reset_requested(move || {
        if reset_authority.get().is_none() {
            set_diagnostic(
                &reset_diagnostic,
                "programme loudness reset refused: accepted state is unavailable".to_owned(),
            );
            return;
        }
        send_operator_command(
            &reset_sender,
            OperatorCommand::ResetLoudness,
            &reset_diagnostic,
        );
    });
}

#[allow(
    clippy::too_many_lines,
    reason = "the one scene owner binds direct launch, local selection, navigation, and selected launch to the same bounded authority"
)]
fn wire_operator_scene_launch(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    scenes: &[OperatorScene],
    authority: &Rc<Cell<bool>>,
    scene_model: &Rc<VecModel<OperatorSceneRow>>,
    selection: &Rc<Cell<Option<usize>>>,
) {
    let direct_sender = sender.clone();
    let direct_diagnostic = Arc::clone(diagnostic);
    let direct_scenes = scenes.to_vec();
    let direct_authority = Rc::clone(authority);
    let direct_model = Rc::clone(scene_model);
    let direct_selection = Rc::clone(selection);
    ui.on_launch_scene_requested(move |row| {
        if !direct_authority.get() {
            set_diagnostic(
                &direct_diagnostic,
                "scene launch refused: accepted scene state is unavailable".to_owned(),
            );
            return;
        }
        let Ok(row) = usize::try_from(row) else {
            set_diagnostic(
                &direct_diagnostic,
                "scene launch refused: invalid row".to_owned(),
            );
            return;
        };
        let Some(scene) = direct_scenes.get(row) else {
            set_diagnostic(
                &direct_diagnostic,
                "scene launch refused: stale row".to_owned(),
            );
            return;
        };
        if scene.availability != DesktopSceneAvailability::Ready {
            set_diagnostic(
                &direct_diagnostic,
                format!("scene {} is not launchable", scene.index + 1),
            );
            return;
        }
        set_operator_scene_selection(&direct_model, &direct_selection, row);
        send_operator_command(
            &direct_sender,
            OperatorCommand::LaunchScene {
                scene_index: scene.index,
            },
            &direct_diagnostic,
        );
    });

    let selection_model = Rc::clone(scene_model);
    let selection_state = Rc::clone(selection);
    let selection_authority = Rc::clone(authority);
    let selection_diagnostic = Arc::clone(diagnostic);
    ui.on_select_scene_requested(move |row| {
        if !selection_authority.get() {
            set_diagnostic(
                &selection_diagnostic,
                "scene selection refused: accepted scene state is unavailable".to_owned(),
            );
            return;
        }
        let Ok(row) = usize::try_from(row) else {
            set_diagnostic(
                &selection_diagnostic,
                "scene selection refused: invalid row".to_owned(),
            );
            return;
        };
        if !operator_scene_row_is_launchable(&selection_model, row) {
            set_diagnostic(
                &selection_diagnostic,
                "scene selection refused: scene is not launchable".to_owned(),
            );
            return;
        }
        set_operator_scene_selection(&selection_model, &selection_state, row);
    });

    let navigation_model = Rc::clone(scene_model);
    let navigation_state = Rc::clone(selection);
    let navigation_authority = Rc::clone(authority);
    ui.on_move_scene_selection_requested(move |delta| {
        if !navigation_authority.get() {
            return;
        }
        let Some(next) =
            operator_scene_selection_target(&navigation_model, navigation_state.get(), delta)
        else {
            return;
        };
        set_operator_scene_selection(&navigation_model, &navigation_state, next);
    });

    let launch_sender = sender.clone();
    let launch_diagnostic = Arc::clone(diagnostic);
    let launch_scenes = scenes.to_vec();
    let launch_authority = Rc::clone(authority);
    let launch_selection = Rc::clone(selection);
    ui.on_launch_selected_scene_requested(move || {
        if !launch_authority.get() {
            set_diagnostic(
                &launch_diagnostic,
                "scene launch refused: accepted scene state is unavailable".to_owned(),
            );
            return;
        }
        let Some(row) = launch_selection.get() else {
            set_diagnostic(
                &launch_diagnostic,
                "scene launch refused: no Ready scene is selected".to_owned(),
            );
            return;
        };
        let Some(scene) = launch_scenes.get(row) else {
            set_diagnostic(
                &launch_diagnostic,
                "scene launch refused: stale row".to_owned(),
            );
            return;
        };
        if scene.availability != DesktopSceneAvailability::Ready {
            set_diagnostic(
                &launch_diagnostic,
                format!("scene {} is not launchable", scene.index + 1),
            );
            return;
        }
        send_operator_command(
            &launch_sender,
            OperatorCommand::LaunchScene {
                scene_index: scene.index,
            },
            &launch_diagnostic,
        );
    });
}

fn wire_operator_scene_stop(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<bool>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_stop_interactive_scenes_requested(move || {
        if !authority.get() {
            set_diagnostic(
                &diagnostic,
                "interactive scene stop refused: accepted scene state is unavailable".to_owned(),
            );
            return;
        }
        send_operator_command(&sender, OperatorCommand::StopInteractiveScenes, &diagnostic);
    });
}

/// Forwards only actions enabled by the Runtime-owned capture observation.
///
/// The UI does not change these flags optimistically: the owner turn publishes
/// the next presentation after it accepts or refuses the command.
fn wire_operator_scene_performance_capture(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    observation: &Arc<Mutex<RuntimeScenePerformanceCaptureObservation>>,
) {
    let start_sender = sender.clone();
    let start_diagnostic = Arc::clone(diagnostic);
    let start_observation = Arc::clone(observation);
    let start_ui = ui.as_weak();
    ui.on_start_scene_performance_capture_requested(move || {
        let start_enabled = start_ui
            .upgrade()
            .is_some_and(|ui| ui.get_scene_performance_start_enabled())
            && start_observation
                .lock()
                .map(|observation| observation.start_enabled)
                .unwrap_or(false);
        if !start_enabled {
            set_diagnostic(
                &start_diagnostic,
                "scene-performance capture start refused: committed Runtime capture state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &start_sender,
            OperatorCommand::StartScenePerformanceCapture,
            &start_diagnostic,
        );
    });

    let stop_sender = sender.clone();
    let stop_diagnostic = Arc::clone(diagnostic);
    let stop_observation = Arc::clone(observation);
    let stop_ui = ui.as_weak();
    ui.on_stop_scene_performance_capture_requested(move || {
        let stop_enabled = stop_ui
            .upgrade()
            .is_some_and(|ui| ui.get_scene_performance_stop_enabled())
            && stop_observation
                .lock()
                .map(|observation| observation.stop_enabled)
                .unwrap_or(false);
        if !stop_enabled {
            set_diagnostic(
                &stop_diagnostic,
                "scene-performance capture stop refused: committed Runtime capture state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &stop_sender,
            OperatorCommand::StopScenePerformanceCapture,
            &stop_diagnostic,
        );
    });
}

/// Forwards one verified audio-track selection only while the owner-published
/// recording observation authorizes the corresponding transition.
fn wire_operator_recording(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    observation: &Arc<Mutex<RuntimeRecordingObservation>>,
    target_count: usize,
) {
    let start_sender = sender.clone();
    let start_diagnostic = Arc::clone(diagnostic);
    let start_observation = Arc::clone(observation);
    let start_ui = ui.as_weak();
    ui.on_start_recording_requested(move |selected_target| {
        let Ok(target_index) = usize::try_from(selected_target) else {
            set_diagnostic(
                &start_diagnostic,
                "Runtime recording start refused: selected target is unavailable".to_owned(),
            );
            return;
        };
        if target_index >= target_count {
            set_diagnostic(
                &start_diagnostic,
                "Runtime recording start refused: selected target is unavailable".to_owned(),
            );
            return;
        }
        let start_enabled = start_ui
            .upgrade()
            .is_some_and(|ui| ui.get_recording_start_enabled())
            && start_observation
                .lock()
                .map(|observation| observation.start_enabled)
                .unwrap_or(false);
        if !start_enabled {
            set_diagnostic(
                &start_diagnostic,
                "Runtime recording start refused: committed recording state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &start_sender,
            OperatorCommand::StartRecording { target_index },
            &start_diagnostic,
        );
    });

    let stop_sender = sender.clone();
    let stop_diagnostic = Arc::clone(diagnostic);
    let stop_observation = Arc::clone(observation);
    let stop_ui = ui.as_weak();
    ui.on_stop_recording_requested(move || {
        let stop_enabled = stop_ui
            .upgrade()
            .is_some_and(|ui| ui.get_recording_stop_enabled())
            && stop_observation
                .lock()
                .map(|observation| observation.stop_enabled)
                .unwrap_or(false);
        if !stop_enabled {
            set_diagnostic(
                &stop_diagnostic,
                "Runtime recording stop refused: committed recording state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &stop_sender,
            OperatorCommand::StopRecording,
            &stop_diagnostic,
        );
    });
}

/// Forwards one fixed-catalog automation target only while the Runtime-owned
/// observation authorizes capture. The UI holds a display index, never a
/// mixer handle or authored project identity; the Runtime validates the index
/// again before it starts capture.
fn wire_operator_automation_capture(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    observation: &Arc<Mutex<RuntimeAutomationCaptureObservation>>,
    target_count: usize,
) {
    let start_sender = sender.clone();
    let start_diagnostic = Arc::clone(diagnostic);
    let start_observation = Arc::clone(observation);
    let start_ui = ui.as_weak();
    ui.on_start_automation_capture_requested(move |selected_target, name| {
        let Ok(target_index) = usize::try_from(selected_target) else {
            set_diagnostic(
                &start_diagnostic,
                "Runtime automation capture start refused: selected target is unavailable".to_owned(),
            );
            return;
        };
        if target_index >= target_count {
            set_diagnostic(
                &start_diagnostic,
                "Runtime automation capture start refused: selected target is unavailable".to_owned(),
            );
            return;
        }
        let start_enabled = start_ui
            .upgrade()
            .is_some_and(|ui| ui.get_automation_capture_start_enabled())
            && start_observation
                .lock()
                .map(|observation| observation.start_enabled)
                .unwrap_or(false);
        if !start_enabled {
            set_diagnostic(
                &start_diagnostic,
                "Runtime automation capture start refused: committed Runtime capture state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &start_sender,
            OperatorCommand::StartAutomationCapture {
                target_index,
                name: name.to_string(),
            },
            &start_diagnostic,
        );
    });

    let stop_sender = sender.clone();
    let stop_diagnostic = Arc::clone(diagnostic);
    let stop_observation = Arc::clone(observation);
    let stop_ui = ui.as_weak();
    ui.on_stop_automation_capture_requested(move || {
        let stop_enabled = stop_ui
            .upgrade()
            .is_some_and(|ui| ui.get_automation_capture_stop_enabled())
            && stop_observation
                .lock()
                .map(|observation| observation.stop_enabled)
                .unwrap_or(false);
        if !stop_enabled {
            set_diagnostic(
                &stop_diagnostic,
                "Runtime automation capture stop refused: committed Runtime capture state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &stop_sender,
            OperatorCommand::StopAutomationCapture,
            &stop_diagnostic,
        );
    });
}

/// Forwards only an export that the Runtime-owned loop observation enables.
///
/// The UI neither snapshots nor persists loop state itself. It simply queues
/// an explicit request for the sole Runtime owner, which publishes the next
/// observation after accepting or refusing it.
fn wire_operator_loop_update_export(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    observation: &Arc<Mutex<RuntimeLoopUpdateObservation>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let observation = Arc::clone(observation);
    let weak = ui.as_weak();
    ui.on_export_loop_update_requested(move || {
        let export_enabled = weak
            .upgrade()
            .is_some_and(|ui| ui.get_loop_update_export_enabled())
            && observation
                .lock()
                .map(|observation| observation.export_enabled)
                .unwrap_or(false);
        if !export_enabled {
            set_diagnostic(
                &diagnostic,
                "loop-update export refused: committed Runtime loop state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(&sender, OperatorCommand::ExportLoopUpdate, &diagnostic);
    });
}

/// Forwards only an export that the Runtime-owned mixer observation enables.
///
/// The UI neither snapshots nor persists mixer state itself. It simply queues
/// an explicit request for the sole Runtime owner, which publishes the next
/// observation after accepting or refusing it.
fn wire_operator_mixer_update_export(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    observation: &Arc<Mutex<RuntimeMixerUpdateObservation>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let observation = Arc::clone(observation);
    let weak = ui.as_weak();
    ui.on_export_mixer_update_requested(move || {
        let export_enabled = weak
            .upgrade()
            .is_some_and(|ui| ui.get_mixer_update_export_enabled())
            && observation
                .lock()
                .map(|observation| observation.export_enabled)
                .unwrap_or(false);
        if !export_enabled {
            set_diagnostic(
                &diagnostic,
                "mixer-update export refused: committed Runtime mixer state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(&sender, OperatorCommand::ExportMixerUpdate, &diagnostic);
    });
}

/// Forwards one explicitly selected chain only when the Runtime-owned
/// observation authorizes it. The UI retains no runtime handle or identity:
/// the bounded row position is verified against its immutable startup catalog
/// before the owner validates it again.
fn wire_operator_native_device_update_export(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    chains: &[OperatorNativeDeviceChain],
    observation: &Arc<Mutex<RuntimeNativeDeviceUpdateObservation>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let chains = chains.to_vec();
    let observation = Arc::clone(observation);
    let weak = ui.as_weak();
    ui.on_export_native_device_update_requested(move |row| {
        let Ok(chain_index) = usize::try_from(row) else {
            set_diagnostic(
                &diagnostic,
                "native-device update export refused: selected chain is unavailable".to_owned(),
            );
            return;
        };
        if chains.get(chain_index).is_none() {
            set_diagnostic(
                &diagnostic,
                "native-device update export refused: selected chain is unavailable".to_owned(),
            );
            return;
        }
        let export_enabled = weak
            .upgrade()
            .is_some_and(|ui| ui.get_native_device_update_export_enabled())
            && observation
                .lock()
                .map(|observation| observation.export_enabled)
                .unwrap_or(false);
        if !export_enabled {
            set_diagnostic(
                &diagnostic,
                "native-device update export refused: committed Runtime native-device state is unavailable"
                    .to_owned(),
            );
            return;
        }
        send_operator_command(
            &sender,
            OperatorCommand::ExportNativeDeviceUpdate { chain_index },
            &diagnostic,
        );
    });
}

fn wire_operator_mixer_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<OperatorMixerAuthority>>>,
) {
    wire_operator_mixer_channel_value_controls(ui, sender, diagnostic, authority);
    wire_operator_mixer_channel_boolean_controls(ui, sender, diagnostic, authority);
    wire_operator_mixer_send_controls(ui, sender, diagnostic, authority);
    wire_operator_main_mixer_controls(ui, sender, diagnostic, authority);
}

fn wire_operator_native_device_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    model: &Rc<VecModel<OperatorNativeDeviceRow>>,
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
) {
    let bypass_sender = sender.clone();
    let bypass_diagnostic = Arc::clone(diagnostic);
    let bypass_model = Rc::clone(model);
    let bypass_authority = Rc::clone(authority);
    ui.on_toggle_native_device_requested(move |row| {
        let Ok(row) = usize::try_from(row) else {
            set_diagnostic(
                &bypass_diagnostic,
                "native device bypass refused: invalid row".to_owned(),
            );
            return;
        };
        let Some(current) = bypass_authority
            .borrow()
            .as_ref()
            .and_then(|devices| devices.get(row))
            .copied()
        else {
            set_diagnostic(
                &bypass_diagnostic,
                "native device bypass refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if send_operator_command(
            &bypass_sender,
            OperatorCommand::SetNativeDeviceEnabled {
                device: current.handle,
                enabled: !current.enabled,
            },
            &bypass_diagnostic,
        ) {
            commit_operator_native_device_enabled(
                &bypass_authority,
                &bypass_model,
                row,
                current.handle,
                !current.enabled,
            );
        }
    });

    let mode_sender = sender.clone();
    let mode_diagnostic = Arc::clone(diagnostic);
    let mode_model = Rc::clone(model);
    let mode_authority = Rc::clone(authority);
    ui.on_toggle_native_filter_mode_requested(move |row| {
        let Some(current) = operator_native_device_authority(&mode_authority, row) else {
            set_diagnostic(
                &mode_diagnostic,
                "native filter mode refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        let NativeDeviceState::Filter(state) = current.state else {
            set_diagnostic(
                &mode_diagnostic,
                "native filter mode refused: accepted device kind changed".to_owned(),
            );
            return;
        };
        let state = NativeFilterState::try_new(
            match state.mode() {
                NativeFilterMode::LowPass => NativeFilterMode::HighPass,
                NativeFilterMode::HighPass => NativeFilterMode::LowPass,
            },
            state.cutoff_millihertz(),
            state.resonance_millionths(),
        )
        .expect("an accepted filter state remains in the declared domain");
        if send_operator_command(
            &mode_sender,
            OperatorCommand::SetNativeDeviceState {
                device: current.handle,
                state: NativeDeviceState::Filter(state),
            },
            &mode_diagnostic,
        ) {
            commit_operator_native_device_state(
                &mode_authority,
                &mode_model,
                row,
                current.handle,
                NativeDeviceState::Filter(state),
            );
        }
    });

    wire_operator_native_filter_value_controls(ui, sender, diagnostic, model, authority);
    wire_operator_native_compressor_value_controls(ui, sender, diagnostic, model, authority);
}

fn commit_operator_native_device_enabled(
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    model: &VecModel<OperatorNativeDeviceRow>,
    row: usize,
    handle: NativeDeviceHandle,
    enabled: bool,
) {
    let mut authority = authority.borrow_mut();
    let Some(device) = authority
        .as_mut()
        .and_then(|devices| devices.get_mut(row))
        .filter(|device| device.handle == handle)
    else {
        return;
    };
    device.enabled = enabled;
    let Some(mut model_row) = model.row_data(row) else {
        return;
    };
    model_row.enabled = enabled;
    model.set_row_data(row, model_row);
}

fn commit_operator_native_device_state(
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    model: &VecModel<OperatorNativeDeviceRow>,
    row: i32,
    handle: NativeDeviceHandle,
    state: NativeDeviceState,
) {
    let Ok(row) = usize::try_from(row) else {
        return;
    };
    let mut authority = authority.borrow_mut();
    let Some(device) = authority
        .as_mut()
        .and_then(|devices| devices.get_mut(row))
        .filter(|device| device.handle == handle)
    else {
        return;
    };
    device.state = state;
    let Some(mut model_row) = model.row_data(row) else {
        return;
    };
    apply_operator_native_device_row_state(&mut model_row, state);
    model.set_row_data(row, model_row);
}

fn operator_native_device_authority(
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    row: i32,
) -> Option<OperatorNativeDeviceAuthority> {
    usize::try_from(row).ok().and_then(|row| {
        authority
            .borrow()
            .as_ref()
            .and_then(|devices| devices.get(row))
            .copied()
    })
}

fn wire_operator_native_filter_value_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    model: &Rc<VecModel<OperatorNativeDeviceRow>>,
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
) {
    let cutoff_sender = sender.clone();
    let cutoff_diagnostic = Arc::clone(diagnostic);
    let cutoff_model = Rc::clone(model);
    let cutoff_authority = Rc::clone(authority);
    ui.on_set_native_filter_cutoff_requested(move |row, hertz| {
        let Some((device, state)) =
            operator_native_filter_update(&cutoff_authority, row, |state: NativeFilterState| {
                NativeFilterState::try_new(
                    state.mode(),
                    operator_scaled_u32(hertz, 20.0, 20_000.0, 1_000.0)?,
                    state.resonance_millionths(),
                )
                .ok()
            })
        else {
            set_diagnostic(
                &cutoff_diagnostic,
                "native filter cutoff refused: accepted state or value is unavailable".to_owned(),
            );
            return;
        };
        if send_operator_command(
            &cutoff_sender,
            OperatorCommand::SetNativeDeviceState { device, state },
            &cutoff_diagnostic,
        ) {
            commit_operator_native_device_state(
                &cutoff_authority,
                &cutoff_model,
                row,
                device,
                state,
            );
        }
    });

    let resonance_sender = sender.clone();
    let resonance_diagnostic = Arc::clone(diagnostic);
    let resonance_model = Rc::clone(model);
    let resonance_authority = Rc::clone(authority);
    ui.on_set_native_filter_resonance_requested(move |row, resonance| {
        let Some((device, state)) =
            operator_native_filter_update(&resonance_authority, row, |state: NativeFilterState| {
                NativeFilterState::try_new(
                    state.mode(),
                    state.cutoff_millihertz(),
                    operator_scaled_u32(resonance, 0.1, 20.0, 1_000_000.0)?,
                )
                .ok()
            })
        else {
            set_diagnostic(
                &resonance_diagnostic,
                "native filter resonance refused: accepted state or value is unavailable"
                    .to_owned(),
            );
            return;
        };
        if send_operator_command(
            &resonance_sender,
            OperatorCommand::SetNativeDeviceState { device, state },
            &resonance_diagnostic,
        ) {
            commit_operator_native_device_state(
                &resonance_authority,
                &resonance_model,
                row,
                device,
                state,
            );
        }
    });
}

fn operator_native_filter_update(
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    row: i32,
    update: impl FnOnce(NativeFilterState) -> Option<NativeFilterState>,
) -> Option<(NativeDeviceHandle, NativeDeviceState)> {
    let current = operator_native_device_authority(authority, row)?;
    let NativeDeviceState::Filter(state) = current.state else {
        return None;
    };
    Some((current.handle, NativeDeviceState::Filter(update(state)?)))
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite Runtime device values are range-checked before fixed-domain quantization"
)]
fn operator_scaled_u32(value: f32, minimum: f32, maximum: f32, scale: f32) -> Option<u32> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        return None;
    }
    Some((value * scale).round() as u32)
}

fn wire_operator_native_compressor_value_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    model: &Rc<VecModel<OperatorNativeDeviceRow>>,
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
) {
    wire_operator_native_compressor_value_control(
        ui,
        sender,
        diagnostic,
        model,
        authority,
        |ui, callback| {
            ui.on_set_native_compressor_threshold_requested(callback);
        },
        |state: NativeCompressorState, value| {
            NativeCompressorState::try_new(
                operator_scaled_i32(value, -80.0, 0.0, 1_000.0)?,
                state.ratio_millionths(),
                state.attack_microseconds(),
                state.release_microseconds(),
                state.makeup_millidecibels(),
            )
            .ok()
        },
    );
    wire_operator_native_compressor_value_control(
        ui,
        sender,
        diagnostic,
        model,
        authority,
        |ui, callback| {
            ui.on_set_native_compressor_ratio_requested(callback);
        },
        |state: NativeCompressorState, value| {
            NativeCompressorState::try_new(
                state.threshold_millidecibels(),
                operator_scaled_u32(value, 1.0, 100.0, 1_000_000.0)?,
                state.attack_microseconds(),
                state.release_microseconds(),
                state.makeup_millidecibels(),
            )
            .ok()
        },
    );
    wire_operator_native_compressor_value_control(
        ui,
        sender,
        diagnostic,
        model,
        authority,
        |ui, callback| {
            ui.on_set_native_compressor_attack_requested(callback);
        },
        |state: NativeCompressorState, value| {
            NativeCompressorState::try_new(
                state.threshold_millidecibels(),
                state.ratio_millionths(),
                operator_scaled_u32(value, 0.1, 500.0, 1_000.0)?,
                state.release_microseconds(),
                state.makeup_millidecibels(),
            )
            .ok()
        },
    );
    wire_operator_native_compressor_value_control(
        ui,
        sender,
        diagnostic,
        model,
        authority,
        |ui, callback| {
            ui.on_set_native_compressor_release_requested(callback);
        },
        |state: NativeCompressorState, value| {
            NativeCompressorState::try_new(
                state.threshold_millidecibels(),
                state.ratio_millionths(),
                state.attack_microseconds(),
                operator_scaled_u32(value, 0.1, 2_000.0, 1_000.0)?,
                state.makeup_millidecibels(),
            )
            .ok()
        },
    );
    wire_operator_native_compressor_value_control(
        ui,
        sender,
        diagnostic,
        model,
        authority,
        |ui, callback| {
            ui.on_set_native_compressor_makeup_requested(callback);
        },
        |state: NativeCompressorState, value| {
            NativeCompressorState::try_new(
                state.threshold_millidecibels(),
                state.ratio_millionths(),
                state.attack_microseconds(),
                state.release_microseconds(),
                operator_scaled_i32(value, -24.0, 24.0, 1_000.0)?,
            )
            .ok()
        },
    );
}

fn wire_operator_native_compressor_value_control(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    model: &Rc<VecModel<OperatorNativeDeviceRow>>,
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    bind: impl FnOnce(&OperatorWindow, Box<dyn FnMut(i32, f32)>),
    update: impl Fn(NativeCompressorState, f32) -> Option<NativeCompressorState> + 'static,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let model = Rc::clone(model);
    let authority = Rc::clone(authority);
    bind(
        ui,
        Box::new(move |row, value| {
            let Some((device, state)) =
                operator_native_compressor_update(&authority, row, |state| update(state, value))
            else {
                set_diagnostic(
                    &diagnostic,
                    "native compressor parameter refused: accepted state or value is unavailable"
                        .to_owned(),
                );
                return;
            };
            if send_operator_command(
                &sender,
                OperatorCommand::SetNativeDeviceState { device, state },
                &diagnostic,
            ) {
                commit_operator_native_device_state(&authority, &model, row, device, state);
            }
        }),
    );
}

fn operator_native_compressor_update(
    authority: &Rc<RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>>,
    row: i32,
    update: impl FnOnce(NativeCompressorState) -> Option<NativeCompressorState>,
) -> Option<(NativeDeviceHandle, NativeDeviceState)> {
    let current = operator_native_device_authority(authority, row)?;
    let NativeDeviceState::Compressor(state) = current.state else {
        return None;
    };
    Some((
        current.handle,
        NativeDeviceState::Compressor(update(state)?),
    ))
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "finite Runtime device values are range-checked before fixed-domain quantization"
)]
fn operator_scaled_i32(value: f32, minimum: f32, maximum: f32, scale: f32) -> Option<i32> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        return None;
    }
    Some((value * scale).round() as i32)
}

fn wire_operator_mixer_send_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<OperatorMixerAuthority>>>,
) {
    let gain_sender = sender.clone();
    let gain_diagnostic = Arc::clone(diagnostic);
    let gain_authority = Rc::clone(authority);
    ui.on_set_mixer_send_gain_requested(move |row, percent| {
        let Some(send) = operator_mixer_send_authority(&gain_authority, row) else {
            set_diagnostic(
                &gain_diagnostic,
                "mixer send gain refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        let Ok(gain_millionths) = operator_gain_percent_to_millionths(percent) else {
            set_diagnostic(
                &gain_diagnostic,
                "mixer send gain refused: value is outside 0..=200%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &gain_sender,
            OperatorCommand::SetMixerSendGain {
                send: send.handle,
                gain_millionths,
            },
            &gain_diagnostic,
        );
    });

    let pan_sender = sender.clone();
    let pan_diagnostic = Arc::clone(diagnostic);
    let pan_authority = Rc::clone(authority);
    ui.on_set_mixer_send_pan_requested(move |row, percent| {
        let Some(send) = operator_mixer_send_authority(&pan_authority, row) else {
            set_diagnostic(
                &pan_diagnostic,
                "mixer send pan refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        let Ok(pan_millionths) = operator_pan_percent_to_millionths(percent) else {
            set_diagnostic(
                &pan_diagnostic,
                "mixer send pan refused: value is outside -100..=100%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &pan_sender,
            OperatorCommand::SetMixerSendPan {
                send: send.handle,
                pan_millionths,
            },
            &pan_diagnostic,
        );
    });

    let toggle_sender = sender.clone();
    let toggle_diagnostic = Arc::clone(diagnostic);
    let toggle_authority = Rc::clone(authority);
    ui.on_toggle_mixer_send_requested(move |row| {
        let Some(send) = operator_mixer_send_authority(&toggle_authority, row) else {
            set_diagnostic(
                &toggle_diagnostic,
                "mixer send toggle refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        send_operator_command(
            &toggle_sender,
            OperatorCommand::SetMixerSendEnabled {
                send: send.handle,
                enabled: !send.enabled,
            },
            &toggle_diagnostic,
        );
    });
}

fn wire_operator_mixer_channel_value_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<OperatorMixerAuthority>>>,
) {
    let gain_sender = sender.clone();
    let gain_diagnostic = Arc::clone(diagnostic);
    let gain_authority = Rc::clone(authority);
    ui.on_set_mixer_channel_gain_requested(move |row, percent| {
        let Some(channel) = operator_mixer_channel_authority(&gain_authority, row) else {
            set_diagnostic(
                &gain_diagnostic,
                "mixer gain refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        let Ok(gain_millionths) = operator_gain_percent_to_millionths(percent) else {
            set_diagnostic(
                &gain_diagnostic,
                "mixer gain refused: value is outside 0..=200%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &gain_sender,
            OperatorCommand::SetMixerChannelGain {
                channel: channel.handle,
                gain_millionths,
            },
            &gain_diagnostic,
        );
    });

    let pan_sender = sender.clone();
    let pan_diagnostic = Arc::clone(diagnostic);
    let pan_authority = Rc::clone(authority);
    ui.on_set_mixer_channel_pan_requested(move |row, percent| {
        let Some(channel) = operator_mixer_channel_authority(&pan_authority, row) else {
            set_diagnostic(
                &pan_diagnostic,
                "mixer pan refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        let Ok(pan_millionths) = operator_pan_percent_to_millionths(percent) else {
            set_diagnostic(
                &pan_diagnostic,
                "mixer pan refused: value is outside -100..=100%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &pan_sender,
            OperatorCommand::SetMixerChannelPan {
                channel: channel.handle,
                pan_millionths,
            },
            &pan_diagnostic,
        );
    });
}

fn wire_operator_mixer_channel_boolean_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<OperatorMixerAuthority>>>,
) {
    let mute_sender = sender.clone();
    let mute_diagnostic = Arc::clone(diagnostic);
    let mute_authority = Rc::clone(authority);
    ui.on_toggle_mixer_channel_mute_requested(move |row| {
        let Ok(row) = usize::try_from(row) else {
            set_diagnostic(&mute_diagnostic, "mixer mute refused: stale row".to_owned());
            return;
        };
        let Some(channel) = mute_authority
            .borrow()
            .as_ref()
            .and_then(|authority| authority.channels.get(row))
            .copied()
        else {
            set_diagnostic(
                &mute_diagnostic,
                "mixer mute refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        send_operator_command(
            &mute_sender,
            OperatorCommand::SetMixerChannelMute {
                channel: channel.handle,
                muted: !channel.muted,
            },
            &mute_diagnostic,
        );
    });

    let solo_sender = sender.clone();
    let solo_diagnostic = Arc::clone(diagnostic);
    let solo_authority = Rc::clone(authority);
    ui.on_toggle_mixer_channel_solo_requested(move |row| {
        let Ok(row) = usize::try_from(row) else {
            set_diagnostic(&solo_diagnostic, "mixer solo refused: stale row".to_owned());
            return;
        };
        let Some(channel) = solo_authority
            .borrow()
            .as_ref()
            .and_then(|authority| authority.channels.get(row))
            .copied()
        else {
            set_diagnostic(
                &solo_diagnostic,
                "mixer solo refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if !channel.solo_available {
            set_diagnostic(
                &solo_diagnostic,
                "mixer solo refused: channel does not support Solo".to_owned(),
            );
            return;
        }
        send_operator_command(
            &solo_sender,
            OperatorCommand::SetMixerChannelSolo {
                channel: channel.handle,
                solo: !channel.solo,
            },
            &solo_diagnostic,
        );
    });
}

fn wire_operator_main_mixer_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<RefCell<Option<OperatorMixerAuthority>>>,
) {
    let main_gain_sender = sender.clone();
    let main_gain_diagnostic = Arc::clone(diagnostic);
    let main_gain_authority = Rc::clone(authority);
    ui.on_set_main_gain_requested(move |percent| {
        if main_gain_authority.borrow().is_none() {
            set_diagnostic(
                &main_gain_diagnostic,
                "main output gain refused: accepted state is unavailable".to_owned(),
            );
            return;
        }
        let Ok(gain_millionths) = operator_gain_percent_to_millionths(percent) else {
            set_diagnostic(
                &main_gain_diagnostic,
                "main output gain refused: value is outside 0..=200%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &main_gain_sender,
            OperatorCommand::SetMainGain { gain_millionths },
            &main_gain_diagnostic,
        );
    });

    let main_sender = sender.clone();
    let main_diagnostic = Arc::clone(diagnostic);
    let main_authority = Rc::clone(authority);
    ui.on_toggle_main_mute_requested(move || {
        let Some(muted) = main_authority
            .borrow()
            .as_ref()
            .map(|authority| authority.main_muted)
        else {
            set_diagnostic(
                &main_diagnostic,
                "main output mute refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        send_operator_command(
            &main_sender,
            OperatorCommand::SetMainMute { muted: !muted },
            &main_diagnostic,
        );
    });
}

fn operator_mixer_channel_authority(
    authority: &RefCell<Option<OperatorMixerAuthority>>,
    row: i32,
) -> Option<OperatorMixerChannelAuthority> {
    usize::try_from(row)
        .ok()
        .and_then(|row| authority.borrow().as_ref()?.channels.get(row).copied())
}

fn operator_mixer_send_authority(
    authority: &RefCell<Option<OperatorMixerAuthority>>,
    row: i32,
) -> Option<OperatorMixerSendAuthority> {
    usize::try_from(row)
        .ok()
        .and_then(|row| authority.borrow().as_ref()?.sends.get(row).copied())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite Runtime mixer gain percent is range-checked before quantization"
)]
fn operator_gain_percent_to_millionths(percent: f32) -> Result<u32, ()> {
    if !percent.is_finite() || !(0.0..=200.0).contains(&percent) {
        return Err(());
    }
    Ok((percent * 10_000.0).round() as u32)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite Runtime metronome percent is range-checked before quantization"
)]
fn operator_metronome_percent_to_gain(percent: f32) -> Result<MetronomeGain, ()> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(());
    }
    let millionths = (percent * 10_000.0).round() as u32;
    MetronomeGain::from_millionths(millionths).map_err(|_| ())
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "the bounded zero-through-unity millionths domain is converted only after validation"
)]
fn operator_input_monitor_gain_from_millionths(gain_millionths: u32) -> Option<f32> {
    (gain_millionths <= 1_000_000).then_some(gain_millionths as f32 / 1_000_000.0)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "finite Runtime input-monitor percent is range-checked before quantization"
)]
fn operator_input_monitor_percent_to_millionths(percent: f32) -> Result<u32, ()> {
    if !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return Err(());
    }
    Ok((percent * 10_000.0).round() as u32)
}

#[allow(
    clippy::cast_precision_loss,
    reason = "the exact zero-through-unity metronome millionths domain is presented as a bounded percentage"
)]
fn operator_metronome_gain_to_percent(gain: MetronomeGain) -> f32 {
    gain.millionths() as f32 / 10_000.0
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "finite Runtime mixer pan percent is range-checked before quantization"
)]
fn operator_pan_percent_to_millionths(percent: f32) -> Result<i32, ()> {
    if !percent.is_finite() || !(-100.0..=100.0).contains(&percent) {
        return Err(());
    }
    Ok((percent * 10_000.0).round() as i32)
}

fn wire_operator_loop_toggle(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<OperatorLoopAuthority>>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_toggle_loop_requested(move || {
        let Some(current) = authority.get() else {
            set_diagnostic(
                &diagnostic,
                "session loop toggle refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if current.range.is_none() {
            set_diagnostic(
                &diagnostic,
                "session loop toggle refused: no session loop is armed".to_owned(),
            );
            return;
        }
        send_operator_command(
            &sender,
            OperatorCommand::SetLoopEnabled {
                enabled: !current.enabled,
            },
            &diagnostic,
        );
    });
}

fn wire_operator_metronome_toggle(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<OperatorMetronomeAuthority>>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_toggle_metronome_requested(move || {
        let Some(current) = authority.get() else {
            set_diagnostic(
                &diagnostic,
                "metronome toggle refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if !current.available {
            set_diagnostic(
                &diagnostic,
                "metronome toggle refused: this session has no metronome plan".to_owned(),
            );
            return;
        }
        send_operator_command(
            &sender,
            OperatorCommand::SetMetronomeEnabled {
                enabled: !current.enabled,
            },
            &diagnostic,
        );
    });
}

fn wire_operator_metronome_level(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<OperatorMetronomeAuthority>>>,
) {
    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_set_metronome_gain_requested(move |percent| {
        let Some(current) = authority.get() else {
            set_diagnostic(
                &diagnostic,
                "metronome level refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if !current.available {
            set_diagnostic(
                &diagnostic,
                "metronome level refused: this session has no metronome plan".to_owned(),
            );
            return;
        }
        let Ok(gain) = operator_metronome_percent_to_gain(percent) else {
            set_diagnostic(
                &diagnostic,
                "metronome level refused: value is outside 0..=100%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &sender,
            OperatorCommand::SetMetronomeGain { gain },
            &diagnostic,
        );
    });
}

/// Sends bounded monitor-control requests without changing the surface until
/// the shared desktop control reports its actual state on the next observation.
fn wire_operator_input_monitor_controls(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
) {
    let gain_ui = ui.as_weak();
    let gain_sender = sender.clone();
    let gain_diagnostic = Arc::clone(diagnostic);
    ui.on_set_input_monitor_gain_requested(move |percent| {
        let Some(ui) = gain_ui.upgrade() else {
            return;
        };
        if !ui.get_input_monitor_controls_enabled() {
            set_diagnostic(
                &gain_diagnostic,
                "input monitor level refused: accepted input authority is unavailable".to_owned(),
            );
            return;
        }
        let Ok(gain_millionths) = operator_input_monitor_percent_to_millionths(percent) else {
            set_diagnostic(
                &gain_diagnostic,
                "input monitor level refused: value is outside 0..=100%".to_owned(),
            );
            return;
        };
        send_operator_command(
            &gain_sender,
            OperatorCommand::SetInputMonitorGain { gain_millionths },
            &gain_diagnostic,
        );
    });

    let mute_ui = ui.as_weak();
    let mute_sender = sender.clone();
    let mute_diagnostic = Arc::clone(diagnostic);
    ui.on_toggle_input_monitor_mute_requested(move || {
        let Some(ui) = mute_ui.upgrade() else {
            return;
        };
        if !ui.get_input_monitor_controls_enabled() {
            set_diagnostic(
                &mute_diagnostic,
                "input monitor mute refused: accepted input authority is unavailable".to_owned(),
            );
            return;
        }
        send_operator_command(
            &mute_sender,
            OperatorCommand::SetInputMonitorMuted {
                muted: !ui.get_input_monitor_muted(),
            },
            &mute_diagnostic,
        );
    });
}

#[derive(Clone, Copy)]
enum OperatorLoopMark {
    In,
    Out,
}

fn operator_loop_range_at_playhead(
    current: OperatorLoopAuthority,
    mark: OperatorLoopMark,
) -> Option<TransportLoopRange> {
    match mark {
        OperatorLoopMark::In => {
            let end_frame = current
                .range
                .filter(|range| range.end_frame() > current.position_frame)
                .map(TransportLoopRange::end_frame)
                .or(match current.extent {
                    PlaybackExtent::Bounded { end_frame } if end_frame > current.position_frame => {
                        Some(end_frame)
                    }
                    PlaybackExtent::Bounded { .. } | PlaybackExtent::Unbounded => None,
                })?;
            operator_runtime_loop_range(current.position_frame, end_frame)
        }
        OperatorLoopMark::Out => {
            let start_frame = current
                .range
                .filter(|range| range.start_frame() < current.position_frame)
                .map_or(0, TransportLoopRange::start_frame);
            operator_runtime_loop_range(start_frame, current.position_frame)
        }
    }
}

fn operator_runtime_loop_range(start_frame: usize, end_frame: usize) -> Option<TransportLoopRange> {
    let range = TransportLoopRange::new(start_frame, end_frame).ok()?;
    (end_frame - start_frame >= MAXIMUM_BLOCK_FRAMES).then_some(range)
}

fn wire_operator_loop_range(
    ui: &OperatorWindow,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Arc<Mutex<String>>,
    authority: &Rc<Cell<Option<OperatorLoopAuthority>>>,
) {
    for (mark, register) in [
        (
            OperatorLoopMark::In,
            OperatorWindow::on_set_loop_in_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
        ),
        (
            OperatorLoopMark::Out,
            OperatorWindow::on_set_loop_out_requested as fn(&OperatorWindow, Box<dyn FnMut()>),
        ),
    ] {
        let sender = sender.clone();
        let diagnostic = Arc::clone(diagnostic);
        let authority = Rc::clone(authority);
        register(
            ui,
            Box::new(move || {
                let Some(current) = authority.get() else {
                    set_diagnostic(
                        &diagnostic,
                        "session loop mark refused: accepted state is unavailable".to_owned(),
                    );
                    return;
                };
                let Some(range) = operator_loop_range_at_playhead(current, mark) else {
                    set_diagnostic(
                        &diagnostic,
                        "session loop mark refused: playhead cannot form a positive range"
                            .to_owned(),
                    );
                    return;
                };
                send_operator_command(
                    &sender,
                    OperatorCommand::SetLoopRange { range: Some(range) },
                    &diagnostic,
                );
            }),
        );
    }

    let sender = sender.clone();
    let diagnostic = Arc::clone(diagnostic);
    let authority = Rc::clone(authority);
    ui.on_clear_loop_range_requested(move || {
        let Some(current) = authority.get() else {
            set_diagnostic(
                &diagnostic,
                "session loop clear refused: accepted state is unavailable".to_owned(),
            );
            return;
        };
        if current.range.is_none() {
            set_diagnostic(
                &diagnostic,
                "session loop clear refused: no committed range".to_owned(),
            );
            return;
        }
        send_operator_command(
            &sender,
            OperatorCommand::SetLoopRange { range: None },
            &diagnostic,
        );
    });
}

fn request_operator_shutdown(
    stop: &AtomicBool,
    sender: &SyncSender<OperatorCommand>,
    diagnostic: &Mutex<String>,
) {
    stop.store(true, Ordering::Release);
    send_operator_command(sender, OperatorCommand::Quit, diagnostic);
}

fn send_operator_command(
    sender: &SyncSender<OperatorCommand>,
    command: OperatorCommand,
    diagnostic: &Mutex<String>,
) -> bool {
    match sender.try_send(command) {
        Ok(()) => true,
        Err(error) => {
            set_diagnostic(diagnostic, format!("operator command refused: {error}"));
            false
        }
    }
}

fn operator_scene_rows(scenes: &[OperatorScene]) -> Vec<OperatorSceneRow> {
    scenes
        .iter()
        .map(|scene| OperatorSceneRow {
            name: scene.name.clone().into(),
            status: operator_scene_availability_text(scene.availability).into(),
            available: scene.availability == DesktopSceneAvailability::Ready,
            controls_enabled: false,
            selected: false,
            queued: false,
            last_launched: false,
        })
        .collect()
}

fn operator_timeline_marker_rows(
    markers: &[OperatorTimelineMarker],
) -> Vec<OperatorTimelineMarkerRow> {
    markers
        .iter()
        .map(|marker| OperatorTimelineMarkerRow {
            id: marker.id.clone().into(),
            name: marker.name.clone().into(),
            tick: marker.tick.to_string().into(),
            musical_position: marker
                .musical_position
                .clone()
                .unwrap_or_else(|| "unavailable".to_owned())
                .into(),
            available: marker.frame.is_some(),
            controls_enabled: false,
        })
        .collect()
}

fn operator_timeline_marker_navigation_state(
    markers: &[OperatorTimelineMarker],
    authority_active: bool,
) -> &'static str {
    if markers.is_empty() {
        "No named markers in this verified project"
    } else if !markers.iter().any(|marker| marker.frame.is_some()) {
        "No named marker can be represented by this Runtime"
    } else if authority_active {
        "Locate preserves recovered state · Play starts"
    } else {
        "Waiting for verified output recovery"
    }
}

fn apply_operator_timeline_marker_observation(
    ui: &OperatorWindow,
    model: &VecModel<OperatorTimelineMarkerRow>,
    authority: &RefCell<Option<Arc<[OperatorTimelineMarker]>>>,
    markers: &Arc<[OperatorTimelineMarker]>,
) {
    if authority.borrow().is_some() {
        return;
    }
    for row in 0..model.row_count() {
        if let Some(mut marker) = model.row_data(row) {
            marker.controls_enabled = false;
            model.set_row_data(row, marker);
        }
    }
    if model.row_count() != markers.len() {
        ui.set_timeline_marker_navigation_state("Marker model unavailable".into());
        return;
    }
    for (row, marker) in markers.iter().enumerate() {
        let Some(mut presented) = model.row_data(row) else {
            ui.set_timeline_marker_navigation_state("Marker model unavailable".into());
            return;
        };
        if presented.id.as_str() != marker.id {
            ui.set_timeline_marker_navigation_state("Marker model unavailable".into());
            return;
        }
        presented.controls_enabled = marker.frame.is_some();
        model.set_row_data(row, presented);
    }
    *authority.borrow_mut() = Some(Arc::clone(markers));
    ui.set_timeline_marker_navigation_state(
        operator_timeline_marker_navigation_state(markers, true).into(),
    );
}

fn revoke_operator_timeline_marker_authority(
    ui: &OperatorWindow,
    model: &VecModel<OperatorTimelineMarkerRow>,
    authority: &RefCell<Option<Arc<[OperatorTimelineMarker]>>>,
) {
    *authority.borrow_mut() = None;
    for row in 0..model.row_count() {
        if let Some(mut marker) = model.row_data(row) {
            marker.controls_enabled = false;
            model.set_row_data(row, marker);
        }
    }
    ui.set_timeline_marker_navigation_state(
        "Runtime stopped · marker navigation unavailable".into(),
    );
}

fn operator_scene_row_is_launchable(scenes: &VecModel<OperatorSceneRow>, row: usize) -> bool {
    scenes
        .row_data(row)
        .is_some_and(|scene| scene.available && scene.controls_enabled)
}

fn operator_scene_selection_target(
    scenes: &VecModel<OperatorSceneRow>,
    selected: Option<usize>,
    delta: i32,
) -> Option<usize> {
    if delta == 0 {
        return selected.filter(|&row| operator_scene_row_is_launchable(scenes, row));
    }
    let launchable: Vec<_> = (0..scenes.row_count())
        .filter(|&row| operator_scene_row_is_launchable(scenes, row))
        .collect();
    let first = *launchable.first()?;
    let last = *launchable.last()?;
    let Some(selected) = selected else {
        return Some(if delta.is_negative() { last } else { first });
    };
    let position = launchable.iter().position(|&row| row == selected);
    let Some(position) = position else {
        return Some(if delta.is_negative() { last } else { first });
    };
    if delta.is_negative() {
        position
            .checked_sub(1)
            .map_or(Some(first), |previous| launchable.get(previous).copied())
    } else {
        launchable.get(position + 1).copied().or(Some(last))
    }
}

fn set_operator_scene_selection(
    scenes: &VecModel<OperatorSceneRow>,
    selection: &Cell<Option<usize>>,
    selected: usize,
) {
    for row in 0..scenes.row_count() {
        if let Some(mut scene) = scenes.row_data(row) {
            scene.selected = row == selected;
            scenes.set_row_data(row, scene);
        }
    }
    selection.set(Some(selected));
}

fn operator_mixer_rows(channels: &[OperatorMixerChannel]) -> Vec<OperatorMixerRow> {
    channels
        .iter()
        .map(|channel| OperatorMixerRow {
            index: i32::try_from(channel.index).unwrap_or(-1),
            name: channel.name.clone().into(),
            gain_percent: 0.0,
            pan_percent: 0.0,
            muted: false,
            soloed: false,
            meter_left_level: 0.0,
            meter_right_level: 0.0,
            meter_left_reading: "— dBFS".into(),
            meter_right_reading: "— dBFS".into(),
            controls_enabled: false,
            solo_available: channel.solo_available,
        })
        .collect()
}

fn operator_mixer_send_rows(sends: &[OperatorMixerSend]) -> Vec<OperatorMixerSendRow> {
    sends
        .iter()
        .map(|send| OperatorMixerSendRow {
            source_name: send.source_name.clone().into(),
            target_name: send.target_name.clone().into(),
            tap: match send.tap {
                DesktopMixerSendTap::PreFader => "PRE",
                DesktopMixerSendTap::PostFader => "POST",
            }
            .into(),
            gain_percent: 0.0,
            pan_percent: 0.0,
            enabled: false,
            controls_enabled: false,
        })
        .collect()
}

fn operator_native_device_rows(devices: &[OperatorNativeDevice]) -> Vec<OperatorNativeDeviceRow> {
    devices
        .iter()
        .map(|device| OperatorNativeDeviceRow {
            track_name: device.track_name.clone().into(),
            kind: device.kind.clone().into(),
            kind_index: i32::from(device.kind != "Filter"),
            enabled: false,
            controls_enabled: false,
            filter_mode_index: 0,
            filter_cutoff_hz: 0.0,
            filter_resonance: 0.0,
            compressor_threshold_decibels: 0.0,
            compressor_ratio: 0.0,
            compressor_attack_milliseconds: 0.0,
            compressor_release_milliseconds: 0.0,
            compressor_makeup_decibels: 0.0,
        })
        .collect()
}

fn operator_native_device_chain_rows(
    chains: &[OperatorNativeDeviceChain],
) -> Vec<OperatorNativeDeviceChainRow> {
    chains
        .iter()
        .map(|chain| OperatorNativeDeviceChainRow {
            track_label: chain.track_label.clone().into(),
            device_count: chain.device_count,
        })
        .collect()
}

fn native_device_authority(
    devices: &[OperatorNativeDevice],
    state: &NativeDeviceStateSnapshot,
) -> Option<Vec<OperatorNativeDeviceAuthority>> {
    let mut authority = Vec::new();
    authority.try_reserve_exact(devices.len()).ok()?;
    for device in devices {
        let enabled = state.enabled(device.handle.slot(), device.handle.device())?;
        let parameters = state.state(device.handle.slot(), device.handle.device())?;
        if !matches!(
            (&device.kind[..], parameters),
            ("Filter", NativeDeviceState::Filter(_))
                | ("Compressor", NativeDeviceState::Compressor(_))
        ) {
            return None;
        }
        authority.push(OperatorNativeDeviceAuthority {
            handle: device.handle,
            enabled,
            state: parameters,
        });
    }
    Some(authority)
}

fn apply_operator_native_device_observation(
    ui: &OperatorWindow,
    model: &VecModel<OperatorNativeDeviceRow>,
    devices: &[OperatorNativeDevice],
    authority: &RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>,
    snapshot: &PlaybackRecoverySnapshot,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_native_device_authority(ui, model, authority, true);
        return;
    }
    let Some(state) = snapshot.native_device_state() else {
        revoke_operator_native_device_authority(ui, model, authority, false);
        return;
    };
    let Some(next) = native_device_authority(devices, &state) else {
        revoke_operator_native_device_authority(ui, model, authority, false);
        return;
    };
    if next.len() != model.row_count() {
        revoke_operator_native_device_authority(ui, model, authority, false);
        return;
    }
    for (row, device) in next.iter().enumerate() {
        let Some(mut model_row) = model.row_data(row) else {
            revoke_operator_native_device_authority(ui, model, authority, false);
            return;
        };
        model_row.enabled = device.enabled;
        model_row.controls_enabled = true;
        apply_operator_native_device_row_state(&mut model_row, device.state);
        model.set_row_data(row, model_row);
    }
    ui.set_native_device_state(
        format!("Committed cycle {}", snapshot.committed_cycle_count()).into(),
    );
    *authority.borrow_mut() = Some(next);
}

#[allow(
    clippy::cast_precision_loss,
    reason = "validated fixed-point parameters are converted only for bounded Slint display controls and are quantized back at the callback boundary"
)]
fn apply_operator_native_device_row_state(
    row: &mut OperatorNativeDeviceRow,
    state: NativeDeviceState,
) {
    match state {
        NativeDeviceState::Filter(state) => {
            row.kind_index = 0;
            row.filter_mode_index = match state.mode() {
                NativeFilterMode::LowPass => 0,
                NativeFilterMode::HighPass => 1,
            };
            row.filter_cutoff_hz = state.cutoff_millihertz() as f32 / 1_000.0;
            row.filter_resonance = state.resonance_millionths() as f32 / 1_000_000.0;
        }
        NativeDeviceState::Compressor(state) => {
            row.kind_index = 1;
            row.compressor_threshold_decibels = state.threshold_millidecibels() as f32 / 1_000.0;
            row.compressor_ratio = state.ratio_millionths() as f32 / 1_000_000.0;
            row.compressor_attack_milliseconds = state.attack_microseconds() as f32 / 1_000.0;
            row.compressor_release_milliseconds = state.release_microseconds() as f32 / 1_000.0;
            row.compressor_makeup_decibels = state.makeup_millidecibels() as f32 / 1_000.0;
        }
    }
}

fn revoke_operator_native_device_authority(
    ui: &OperatorWindow,
    model: &VecModel<OperatorNativeDeviceRow>,
    authority: &RefCell<Option<Vec<OperatorNativeDeviceAuthority>>>,
    terminal_latched: bool,
) {
    authority.borrow_mut().take();
    for row in 0..model.row_count() {
        if let Some(mut device) = model.row_data(row) {
            device.controls_enabled = false;
            model.set_row_data(row, device);
        }
    }
    ui.set_native_device_state(if terminal_latched {
        "Final committed devices · Runtime stopped".into()
    } else {
        "Native device state unavailable".into()
    });
}

fn mixer_authority(
    channels: &[OperatorMixerChannel],
    sends: &[OperatorMixerSend],
    state: &MixerParameterStateSnapshot,
) -> Option<OperatorMixerAuthority> {
    if state.channel_count() != channels.len() {
        return None;
    }
    let mut authority = Vec::new();
    authority.try_reserve_exact(channels.len()).ok()?;
    for channel in channels {
        let accepted = state.channel(channel.index)?;
        if accepted.send_count()
            != sends
                .iter()
                .filter(|send| send.channel_index == channel.index)
                .count()
        {
            return None;
        }
        let parameters = accepted.parameters();
        authority.push(OperatorMixerChannelAuthority {
            handle: channel.handle,
            gain: parameters.gain,
            pan: parameters.pan,
            muted: accepted.muted(),
            solo: accepted.solo(),
            solo_available: channel.solo_available,
        });
    }
    let mut send_authority = Vec::new();
    send_authority.try_reserve_exact(sends.len()).ok()?;
    for send in sends {
        let accepted = state.channel(send.channel_index)?.send(send.send_index)?;
        let parameters = accepted.parameters();
        send_authority.push(OperatorMixerSendAuthority {
            handle: send.handle,
            gain: parameters.gain,
            pan: parameters.pan,
            enabled: accepted.enabled(),
        });
    }
    Some(OperatorMixerAuthority {
        channels: authority,
        sends: send_authority,
        main_gain: state.main_gain(),
        main_muted: state.main_muted(),
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "one coherent recovery observation must validate and update channel and send authority together"
)]
fn apply_operator_mixer_observation(
    ui: &OperatorWindow,
    model: &VecModel<OperatorMixerRow>,
    send_model: &VecModel<OperatorMixerSendRow>,
    channels: &[OperatorMixerChannel],
    sends: &[OperatorMixerSend],
    authority: &RefCell<Option<OperatorMixerAuthority>>,
    snapshot: &PlaybackRecoverySnapshot,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_mixer_authority(ui, model, send_model, authority, true);
        return;
    }
    let Some(state) = snapshot.mixer_parameter_state() else {
        revoke_operator_mixer_authority(ui, model, send_model, authority, false);
        return;
    };
    let Some(next) = mixer_authority(channels, sends, &state) else {
        revoke_operator_mixer_authority(ui, model, send_model, authority, false);
        return;
    };
    for (row, channel) in next.channels.iter().enumerate() {
        let Some(mut model_row) = model.row_data(row) else {
            revoke_operator_mixer_authority(ui, model, send_model, authority, false);
            return;
        };
        model_row.gain_percent = channel.gain.value() * 100.0;
        model_row.pan_percent = channel.pan.value() * 100.0;
        model_row.muted = channel.muted;
        model_row.soloed = channel.solo;
        model_row.controls_enabled = true;
        model.set_row_data(row, model_row);
    }
    for (row, send) in next.sends.iter().enumerate() {
        let Some(mut model_row) = send_model.row_data(row) else {
            revoke_operator_mixer_authority(ui, model, send_model, authority, false);
            return;
        };
        model_row.gain_percent = send.gain.value() * 100.0;
        model_row.pan_percent = send.pan.value() * 100.0;
        model_row.enabled = send.enabled;
        model_row.controls_enabled = true;
        send_model.set_row_data(row, model_row);
    }
    ui.set_main_gain_percent(next.main_gain.value() * 100.0);
    ui.set_main_muted(next.main_muted);
    ui.set_main_mute_enabled(true);
    ui.set_mixer_state(format!("Committed cycle {}", snapshot.committed_cycle_count()).into());
    *authority.borrow_mut() = Some(next);
}

fn revoke_operator_mixer_authority(
    ui: &OperatorWindow,
    model: &VecModel<OperatorMixerRow>,
    send_model: &VecModel<OperatorMixerSendRow>,
    authority: &RefCell<Option<OperatorMixerAuthority>>,
    terminal_latched: bool,
) {
    authority.borrow_mut().take();
    for row in 0..model.row_count() {
        if let Some(mut channel) = model.row_data(row) {
            channel.controls_enabled = false;
            model.set_row_data(row, channel);
        }
    }
    for row in 0..send_model.row_count() {
        if let Some(mut send) = send_model.row_data(row) {
            send.controls_enabled = false;
            send_model.set_row_data(row, send);
        }
    }
    ui.set_main_mute_enabled(false);
    ui.set_mixer_state(if terminal_latched {
        "Final committed mixer · Runtime stopped".into()
    } else {
        "Mixer state unavailable".into()
    });
}

const fn operator_scene_availability_text(availability: DesktopSceneAvailability) -> &'static str {
    match availability {
        DesktopSceneAvailability::Ready => "Ready",
        DesktopSceneAvailability::Disabled => "Disabled",
        DesktopSceneAvailability::ConsolidationRequired => "Needs consolidation",
        DesktopSceneAvailability::NotPrepared => "Audio unavailable",
    }
}

fn apply_operator_scene_observation(
    ui: &OperatorWindow,
    scenes: &VecModel<OperatorSceneRow>,
    observation: PlaybackCycleObservation,
) {
    for row in 0..scenes.row_count() {
        let Some(mut scene) = scenes.row_data(row) else {
            continue;
        };
        scene.queued = observation.queued_scene_index() == Some(row);
        scene.last_launched = observation.last_launched_scene_index() == Some(row);
        scene.status = if scene.queued {
            observation.queued_scene_launch_frame().map_or_else(
                || "Queued".into(),
                |frame| format!("Queued for frame {frame}").into(),
            )
        } else if scene.last_launched {
            "Last launched".into()
        } else if scene.available {
            "Ready".into()
        } else {
            scene.status
        };
        scenes.set_row_data(row, scene);
    }
    let feedback = observation.queued_scene_index().map_or_else(
        || {
            observation.last_launched_scene_index().map_or_else(
                || "No interactive scene launch observed".to_owned(),
                |index| format!("Scene {} was last launched", index + 1),
            )
        },
        |index| {
            observation.queued_scene_launch_frame().map_or_else(
                || format!("Scene {} is queued", index + 1),
                |frame| format!("Scene {} queued for frame {frame}", index + 1),
            )
        },
    );
    ui.set_scene_feedback(feedback.into());
}

fn apply_operator_loop_observation(
    ui: &OperatorWindow,
    authority: &Cell<Option<OperatorLoopAuthority>>,
    observation: PlaybackCycleObservation,
    extent: PlaybackExtent,
) {
    let current = OperatorLoopAuthority {
        range: observation.loop_range(),
        enabled: observation.loop_enabled(),
        position_frame: observation.position_frame(),
        extent,
    };
    authority.set(Some(current));
    ui.set_loop_in_enabled(
        operator_loop_range_at_playhead(current, OperatorLoopMark::In).is_some(),
    );
    ui.set_loop_out_enabled(
        operator_loop_range_at_playhead(current, OperatorLoopMark::Out).is_some(),
    );
    ui.set_loop_clear_enabled(current.range.is_some());
    if let Some(range) = current.range {
        ui.set_loop_state(if current.enabled {
            "Looping".into()
        } else {
            "Loop armed".into()
        });
        ui.set_loop_range(format!("frames {}–{}", range.start_frame(), range.end_frame()).into());
        ui.set_loop_toggle_label(if current.enabled {
            "Disable loop".into()
        } else {
            "Enable loop".into()
        });
        ui.set_loop_toggle_enabled(true);
    } else {
        ui.set_loop_state("No session loop".into());
        ui.set_loop_range("No committed session range".into());
        ui.set_loop_toggle_label("Loop unavailable".into());
        ui.set_loop_toggle_enabled(false);
    }
}

fn apply_operator_metronome_observation(
    ui: &OperatorWindow,
    authority: &Cell<Option<OperatorMetronomeAuthority>>,
    observation: PlaybackCycleObservation,
) {
    let current = OperatorMetronomeAuthority {
        available: observation.metronome_available(),
        enabled: observation.metronome_enabled(),
        gain: observation.metronome_gain().unwrap_or(MetronomeGain::UNITY),
    };
    authority.set(Some(current));
    if current.available {
        ui.set_metronome_state(if current.enabled {
            "Committed on".into()
        } else {
            "Committed off".into()
        });
        ui.set_metronome_toggle_label(if current.enabled {
            "Disable metronome".into()
        } else {
            "Enable metronome".into()
        });
        ui.set_metronome_toggle_enabled(true);
        ui.set_metronome_gain_percent(operator_metronome_gain_to_percent(current.gain));
        ui.set_metronome_gain_enabled(true);
    } else {
        ui.set_metronome_state("Session unavailable".into());
        ui.set_metronome_toggle_label("Metronome unavailable".into());
        ui.set_metronome_toggle_enabled(false);
        ui.set_metronome_gain_percent(0.0);
        ui.set_metronome_gain_enabled(false);
    }
}

fn apply_operator_seek_observation(
    ui: &OperatorWindow,
    authority: &Cell<Option<usize>>,
    extent: PlaybackExtent,
    position_frame: usize,
) {
    match extent {
        PlaybackExtent::Bounded { end_frame: 0 } => {
            authority.set(None);
            ui.set_transport_position_percent(0.0);
            ui.set_transport_seek_state("Empty project · seek unavailable".into());
            ui.set_transport_seek_enabled(false);
        }
        PlaybackExtent::Bounded { end_frame } => {
            authority.set(Some(end_frame));
            ui.set_transport_position_percent(operator_seek_percent(position_frame, end_frame));
            ui.set_transport_seek_state(format!("POSITION · 0–{end_frame} frames").into());
            ui.set_transport_seek_enabled(true);
        }
        PlaybackExtent::Unbounded => {
            authority.set(None);
            ui.set_transport_position_percent(0.0);
            ui.set_transport_seek_state("Unbounded playback · seek unavailable".into());
            ui.set_transport_seek_enabled(false);
        }
    }
}

fn operator_seek_frame(end_frame: usize, percent: f32) -> Option<usize> {
    if end_frame == 0 || !percent.is_finite() || !(0.0..=100.0).contains(&percent) {
        return None;
    }
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "finite bounded percent is checked before rounding to one-millionth authority"
    )]
    let millionths = (percent * 10_000.0).round() as u32;
    let scaled = (end_frame as u128)
        .checked_mul(u128::from(millionths))?
        .checked_add(500_000)?
        / 1_000_000;
    usize::try_from(scaled).ok()
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "the exact frame remains visible; the bounded f32 value only positions a percent marker"
)]
fn operator_seek_percent(position_frame: usize, end_frame: usize) -> f32 {
    if end_frame == 0 {
        return 0.0;
    }
    ((position_frame.min(end_frame) as f64 / end_frame as f64) * 100.0) as f32
}

#[allow(
    clippy::too_many_arguments,
    reason = "the UI timer applies independent recovery authorities without a synthetic aggregate owner"
)]
fn apply_operator_cycle_observation(
    ui: &OperatorWindow,
    observers: &RuntimeObservers,
    output_state: OutputStreamState,
    diagnostic: &Mutex<String>,
    transport_authority: &Cell<Option<PlaybackRecoveryTransportState>>,
    loop_authority: &Cell<Option<OperatorLoopAuthority>>,
    metronome_authority: &Cell<Option<OperatorMetronomeAuthority>>,
    seek_authority: &Cell<Option<usize>>,
    timeline_marker_model: &VecModel<OperatorTimelineMarkerRow>,
    timeline_marker_authority: &RefCell<Option<Arc<[OperatorTimelineMarker]>>>,
    scene_model: &VecModel<OperatorSceneRow>,
    scene_launch_authority: &Cell<bool>,
    scene_selection: &Cell<Option<usize>>,
    scene_stop_authority: &Cell<bool>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_transport_authority(ui, transport_authority);
        revoke_operator_metronome_authority(ui, metronome_authority);
        revoke_operator_timeline_marker_authority(
            ui,
            timeline_marker_model,
            timeline_marker_authority,
        );
        revoke_operator_scene_launch_authority(ui, scene_model, scene_launch_authority);
        revoke_operator_scene_stop_authority(ui, scene_stop_authority);
        revoke_operator_scene_performance_authority(ui);
        revoke_operator_recording_authority(ui);
        revoke_operator_automation_capture_authority(ui);
        return;
    }
    let observation = apply_observations(ui, observers, output_state, diagnostic);
    if output_state == OutputStreamState::Failed {
        revoke_operator_transport_authority(ui, transport_authority);
        revoke_operator_loop_authority(ui, loop_authority);
        revoke_operator_metronome_authority(ui, metronome_authority);
        revoke_operator_seek_authority(ui, seek_authority);
        revoke_operator_timeline_marker_authority(
            ui,
            timeline_marker_model,
            timeline_marker_authority,
        );
        revoke_operator_scene_launch_authority(ui, scene_model, scene_launch_authority);
        revoke_operator_scene_stop_authority(ui, scene_stop_authority);
        revoke_operator_scene_performance_authority(ui);
        revoke_operator_recording_authority(ui);
        revoke_operator_automation_capture_authority(ui);
        return;
    }
    apply_operator_transport_observation(ui, transport_authority, observation.state());
    apply_operator_seek_observation(
        ui,
        seek_authority,
        observers.facts.playback_extent,
        observation.position_frame(),
    );
    apply_operator_timeline_marker_observation(
        ui,
        timeline_marker_model,
        timeline_marker_authority,
        &observers.facts.timeline_markers,
    );
    apply_operator_loop_observation(
        ui,
        loop_authority,
        observation,
        observers.facts.playback_extent,
    );
    apply_operator_metronome_observation(ui, metronome_authority, observation);
    apply_operator_scene_observation(ui, scene_model, observation);
    set_operator_scene_launch_authority(ui, scene_model, scene_launch_authority, scene_selection);
    let available = scene_model.row_count() > 0;
    scene_stop_authority.set(available);
    ui.set_stop_interactive_scenes_enabled(available);
}

fn set_operator_scene_launch_authority(
    ui: &OperatorWindow,
    scenes: &VecModel<OperatorSceneRow>,
    authority: &Cell<bool>,
    selection: &Cell<Option<usize>>,
) {
    authority.set(true);
    ui.set_scene_selection_enabled(true);
    for row in 0..scenes.row_count() {
        if let Some(mut scene) = scenes.row_data(row) {
            scene.controls_enabled = scene.available;
            scenes.set_row_data(row, scene);
        }
    }
    let selected = selection
        .get()
        .filter(|&row| operator_scene_row_is_launchable(scenes, row))
        .or_else(|| operator_scene_selection_target(scenes, None, 1));
    if let Some(row) = selected {
        set_operator_scene_selection(scenes, selection, row);
    }
}

fn revoke_operator_scene_launch_authority(
    ui: &OperatorWindow,
    scenes: &VecModel<OperatorSceneRow>,
    authority: &Cell<bool>,
) {
    authority.set(false);
    ui.set_scene_selection_enabled(false);
    for row in 0..scenes.row_count() {
        if let Some(mut scene) = scenes.row_data(row) {
            scene.controls_enabled = false;
            scenes.set_row_data(row, scene);
        }
    }
}

fn revoke_operator_scene_stop_authority(ui: &OperatorWindow, authority: &Cell<bool>) {
    authority.set(false);
    ui.set_stop_interactive_scenes_enabled(false);
}

fn apply_operator_scene_performance_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeScenePerformanceCaptureObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_scene_performance_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_scene_performance_state("Runtime capture state unavailable".into());
        ui.set_scene_performance_start_enabled(false);
        ui.set_scene_performance_stop_enabled(false);
        return;
    };
    ui.set_scene_performance_state(observation.state.clone().into());
    ui.set_scene_performance_start_enabled(observation.start_enabled);
    ui.set_scene_performance_stop_enabled(observation.stop_enabled);
}

fn apply_operator_recording_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeRecordingObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_recording_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_recording_state("Runtime recording state unavailable".into());
        ui.set_recording_start_enabled(false);
        ui.set_recording_stop_enabled(false);
        return;
    };
    ui.set_recording_state(observation.state.clone().into());
    ui.set_recording_start_enabled(observation.start_enabled);
    ui.set_recording_stop_enabled(observation.stop_enabled);
}

fn apply_operator_automation_capture_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeAutomationCaptureObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_automation_capture_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_automation_capture_state("Runtime automation capture state unavailable".into());
        ui.set_automation_capture_start_enabled(false);
        ui.set_automation_capture_stop_enabled(false);
        return;
    };
    ui.set_automation_capture_state(observation.state.clone().into());
    ui.set_automation_capture_start_enabled(observation.start_enabled);
    ui.set_automation_capture_stop_enabled(observation.stop_enabled);
}

fn apply_operator_mixer_update_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeMixerUpdateObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_mixer_update_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_mixer_update_state("Runtime mixer-update state unavailable".into());
        ui.set_mixer_update_export_enabled(false);
        return;
    };
    ui.set_mixer_update_state(observation.state.clone().into());
    ui.set_mixer_update_export_enabled(observation.export_enabled);
}

fn apply_operator_native_device_update_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeNativeDeviceUpdateObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_native_device_update_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_native_device_update_state("Runtime native-device-update state unavailable".into());
        ui.set_native_device_update_export_enabled(false);
        return;
    };
    ui.set_native_device_update_state(observation.state.clone().into());
    ui.set_native_device_update_export_enabled(observation.export_enabled);
}

fn apply_operator_loop_update_observation(
    ui: &OperatorWindow,
    observation: &Mutex<RuntimeLoopUpdateObservation>,
    terminal_latched: bool,
) {
    if terminal_latched {
        revoke_operator_loop_update_authority(ui);
        return;
    }
    let Ok(observation) = observation.lock() else {
        ui.set_loop_update_state("Runtime loop-update state unavailable".into());
        ui.set_loop_update_export_enabled(false);
        return;
    };
    ui.set_loop_update_state(observation.state.clone().into());
    ui.set_loop_update_export_enabled(observation.export_enabled);
}

fn revoke_operator_mixer_update_authority(ui: &OperatorWindow) {
    ui.set_mixer_update_state("Runtime stopped · mixer update unavailable".into());
    ui.set_mixer_update_export_enabled(false);
}

fn revoke_operator_native_device_update_authority(ui: &OperatorWindow) {
    ui.set_native_device_update_state("Runtime stopped · native-device update unavailable".into());
    ui.set_native_device_update_export_enabled(false);
}

fn revoke_operator_loop_update_authority(ui: &OperatorWindow) {
    ui.set_loop_update_state("Runtime stopped · loop update unavailable".into());
    ui.set_loop_update_export_enabled(false);
}

fn revoke_operator_scene_performance_authority(ui: &OperatorWindow) {
    ui.set_scene_performance_state("Runtime stopped · capture unavailable".into());
    ui.set_scene_performance_start_enabled(false);
    ui.set_scene_performance_stop_enabled(false);
}

fn revoke_operator_recording_authority(ui: &OperatorWindow) {
    ui.set_recording_state("Runtime stopped · recording unavailable".into());
    ui.set_recording_start_enabled(false);
    ui.set_recording_stop_enabled(false);
}

fn revoke_operator_automation_capture_authority(ui: &OperatorWindow) {
    ui.set_automation_capture_state("Runtime stopped · automation capture unavailable".into());
    ui.set_automation_capture_start_enabled(false);
    ui.set_automation_capture_stop_enabled(false);
}

fn apply_operator_transport_observation(
    ui: &OperatorWindow,
    authority: &Cell<Option<PlaybackRecoveryTransportState>>,
    state: PlaybackRecoveryTransportState,
) {
    authority.set(Some(state));
    ui.set_transport_start_enabled(state == PlaybackRecoveryTransportState::Stopped);
    ui.set_transport_stop_enabled(state == PlaybackRecoveryTransportState::Rolling);
    ui.set_transport_return_to_start_enabled(true);
}

fn revoke_operator_transport_authority(
    ui: &OperatorWindow,
    authority: &Cell<Option<PlaybackRecoveryTransportState>>,
) {
    authority.set(None);
    ui.set_transport_start_enabled(false);
    ui.set_transport_stop_enabled(false);
    ui.set_transport_return_to_start_enabled(false);
}

fn revoke_operator_seek_authority(ui: &OperatorWindow, authority: &Cell<Option<usize>>) {
    authority.set(None);
    ui.set_transport_seek_state("Runtime stopped · seek unavailable".into());
    ui.set_transport_seek_enabled(false);
}

fn revoke_operator_loop_authority(
    ui: &OperatorWindow,
    authority: &Cell<Option<OperatorLoopAuthority>>,
) {
    authority.set(None);
    ui.set_loop_toggle_label("Runtime stopped".into());
    ui.set_loop_toggle_enabled(false);
    ui.set_loop_in_enabled(false);
    ui.set_loop_out_enabled(false);
    ui.set_loop_clear_enabled(false);
}

fn revoke_operator_metronome_authority(
    ui: &OperatorWindow,
    authority: &Cell<Option<OperatorMetronomeAuthority>>,
) {
    authority.set(None);
    ui.set_metronome_state("Runtime stopped".into());
    ui.set_metronome_toggle_label("Runtime stopped".into());
    ui.set_metronome_toggle_enabled(false);
    ui.set_metronome_gain_percent(0.0);
    ui.set_metronome_gain_enabled(false);
}

fn revoke_operator_loudness_authority(
    ui: &OperatorWindow,
    authority: &Cell<Option<LoudnessMeasurementState>>,
) {
    authority.set(None);
    ui.set_loudness_run_enabled(false);
    ui.set_loudness_standby_enabled(false);
    ui.set_loudness_reset_enabled(false);
    ui.set_loudness_reset_confirming(false);
}

fn apply_static_facts(ui: &OperatorWindow, facts: &RuntimeFacts) {
    ui.set_project_name(facts.project_name.clone().into());
    ui.set_project_identity(facts.project_id.clone().into());
    ui.set_project_revision(facts.project_revision.to_string().into());
    ui.set_deployment_generation(facts.deployment_generation.to_string().into());
    ui.set_bundle_identity(facts.bundle_identity.clone().into());
    ui.set_output_route(facts.output_route.to_string().into());
    ui.set_input_route(facts.input_route.to_string().into());
    ui.set_timeline_marker_navigation_state(
        operator_timeline_marker_navigation_state(&facts.timeline_markers, false).into(),
    );
    ui.set_recovery_state(
        facts
            .recovered_from
            .as_ref()
            .map_or_else(
                || "activated bundle accepted without fallback".to_owned(),
                |identity| format!("recovered from rejected bundle {identity}"),
            )
            .into(),
    );
}

#[derive(Debug, Eq, PartialEq)]
struct OperatorObservationPresentation {
    transport_state: &'static str,
    transport_time: String,
    transport_frame: String,
    transport_musical_position: String,
    stream_state: String,
    callback_count: String,
    stream_failures: String,
    native_xruns: String,
    panel_state: &'static str,
    diagnostic: String,
}

#[derive(Debug, PartialEq)]
struct OperatorMeterPresentation {
    left_level: f32,
    left_reading: String,
    left_peak_reading: String,
    right_level: f32,
    right_reading: String,
    right_peak_reading: String,
    overload_latched: bool,
    state: &'static str,
    telemetry: String,
    loudness: OperatorLoudnessPresentation,
}

#[derive(Debug)]
struct OperatorMeterObservation {
    presentation: OperatorMeterPresentation,
    strip_meters: Option<MixerStripSamplePeakBallisticsSnapshot>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperatorInputMonitorAuthority {
    Disabled,
    Unavailable,
    Observed {
        stream_state: StreamState,
        terminal_latched: bool,
    },
}

impl OperatorInputMonitorAuthority {
    const fn configured(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    const fn state(self) -> &'static str {
        match self {
            Self::Disabled => "DISABLED",
            Self::Unavailable => "UNAVAILABLE",
            Self::Observed { stream_state, .. } => operator_input_stream_state_text(stream_state),
        }
    }

    const fn running(self) -> bool {
        matches!(
            self,
            Self::Observed {
                stream_state: StreamState::Running,
                terminal_latched: false,
            }
        )
    }
}

#[derive(Debug, PartialEq)]
struct OperatorInputMonitorPresentation {
    authority: OperatorInputMonitorAuthority,
    callback_count: String,
    captured_frame_count: String,
    dropped_frame_count: String,
    gain_percent: f32,
    muted: bool,
}

#[derive(Debug, PartialEq)]
struct OperatorInputMeterPresentation {
    left_level: f32,
    left_reading: String,
    right_level: f32,
    right_reading: String,
    state: String,
    telemetry: String,
    live: bool,
}

#[derive(Debug, PartialEq)]
struct OperatorLoudnessPresentation {
    state: &'static str,
    momentary: String,
    short_term: String,
    integrated: String,
    loudness_range: String,
    maximum_true_peak: String,
    programme_time: String,
    lra_status: String,
}

#[derive(Debug, PartialEq)]
struct OperatorMeterChannelPresentation {
    level: f32,
    reading: String,
    peak_reading: String,
    overload_latched: bool,
}

fn operator_meter_presentation(
    snapshot: Option<StereoSamplePeakBallisticsSnapshot>,
    peak_hold_available: bool,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
    loudness: Option<LoudnessPresentationSnapshot>,
) -> OperatorMeterPresentation {
    let (left, right, frame) = snapshot.map_or_else(
        || {
            (
                operator_meter_channel_values(false, f32::NAN, f32::NAN),
                operator_meter_channel_values(false, f32::NAN, f32::NAN),
                None,
            )
        },
        |snapshot| {
            (
                operator_meter_channel_presentation(snapshot.left(), peak_hold_available),
                operator_meter_channel_presentation(snapshot.right(), peak_hold_available),
                Some(snapshot.processed_frame_end()),
            )
        },
    );
    let state = match (snapshot.is_some(), sender_disconnected) {
        (false, false) => "Waiting for final-output evidence",
        (false, true) => "Meter producer ended before a snapshot",
        (true, false) => "Live final-output sample peak",
        (true, true) => "Final meter snapshot · producer ended",
    };
    let frame = frame.map_or_else(|| "unavailable".to_owned(), |frame| frame.to_string());
    OperatorMeterPresentation {
        left_level: left.level,
        left_reading: left.reading,
        left_peak_reading: left.peak_reading,
        right_level: right.level,
        right_reading: right.reading,
        right_peak_reading: right.peak_reading,
        overload_latched: left.overload_latched || right.overload_latched,
        state,
        telemetry: format!(
            "meter frame {frame} · {coalesced_snapshot_count} coalesced · {dropped_snapshot_count} dropped"
        ),
        loudness: operator_loudness_presentation(loudness),
    }
}

fn operator_input_monitor_presentation(
    configured: bool,
    status: Option<&StreamStatus>,
    counters: Option<&CaptureStreamCounters>,
    control: Option<&DesktopMonitoringControl>,
    terminal_latched: bool,
) -> OperatorInputMonitorPresentation {
    if !configured {
        return OperatorInputMonitorPresentation {
            authority: OperatorInputMonitorAuthority::Disabled,
            callback_count: "—".to_owned(),
            captured_frame_count: "—".to_owned(),
            dropped_frame_count: "—".to_owned(),
            gain_percent: 0.0,
            muted: true,
        };
    }
    let (Some(status), Some(counters), Some(control)) = (status, counters, control) else {
        return OperatorInputMonitorPresentation {
            authority: OperatorInputMonitorAuthority::Unavailable,
            callback_count: "—".to_owned(),
            captured_frame_count: "—".to_owned(),
            dropped_frame_count: "—".to_owned(),
            gain_percent: 0.0,
            muted: true,
        };
    };
    let stream_state = status.state();
    let counters = counters.snapshot();
    OperatorInputMonitorPresentation {
        authority: OperatorInputMonitorAuthority::Observed {
            stream_state,
            terminal_latched,
        },
        callback_count: counters.callback_count.to_string(),
        captured_frame_count: counters.captured_frame_count.to_string(),
        dropped_frame_count: counters.dropped_frame_count.to_string(),
        gain_percent: control.gain_linear() * 100.0,
        muted: control.muted(),
    }
}

const fn operator_input_stream_state_text(state: StreamState) -> &'static str {
    match state {
        StreamState::Starting => "STARTING",
        StreamState::Running => "RUNNING",
        StreamState::StopRequested => "STOP REQUESTED",
        StreamState::Stopped => "STOPPED",
        StreamState::Failed => "FAILED",
    }
}

fn operator_input_meter_presentation(
    snapshot: Option<StereoSamplePeakDisplayBallisticsSnapshot>,
    coalesced_snapshot_count: u64,
    dropped_snapshot_count: u64,
    sender_disconnected: bool,
) -> OperatorInputMeterPresentation {
    let (left, right, frame) = snapshot.map_or_else(
        || {
            (
                operator_input_meter_channel_presentation(None),
                operator_input_meter_channel_presentation(None),
                None,
            )
        },
        |snapshot| {
            (
                operator_input_meter_channel_presentation(Some(snapshot.left())),
                operator_input_meter_channel_presentation(Some(snapshot.right())),
                Some(snapshot.frame_end()),
            )
        },
    );
    let state = match (snapshot.is_some(), sender_disconnected) {
        (false, false) => "Waiting for accepted input evidence".to_owned(),
        (false | true, true) => "Input meter producer ended".to_owned(),
        (true, false) => "Raw accepted-input sample peak".to_owned(),
    };
    let frame = frame.map_or_else(|| "unavailable".to_owned(), |frame| frame.to_string());
    OperatorInputMeterPresentation {
        left_level: left.0,
        left_reading: left.1,
        right_level: right.0,
        right_reading: right.1,
        state,
        telemetry: format!(
            "accepted frame {frame} · {coalesced_snapshot_count} coalesced · {dropped_snapshot_count} dropped display snapshots"
        ),
        live: !sender_disconnected,
    }
}

fn operator_input_meter_channel_presentation(
    channel: Option<hyper_mixer_runtime::ChannelSamplePeakDisplay>,
) -> (f32, String) {
    channel.map_or_else(
        || operator_meter_level_presentation(false, f32::NAN),
        |channel| {
            operator_meter_level_presentation(channel.measurement_available(), channel.level_dbfs())
        },
    )
}

fn operator_loudness_presentation(
    snapshot: Option<LoudnessPresentationSnapshot>,
) -> OperatorLoudnessPresentation {
    let Some(snapshot) = snapshot else {
        return OperatorLoudnessPresentation {
            state: "UNAVAILABLE",
            momentary: "— LUFS".to_owned(),
            short_term: "— LUFS".to_owned(),
            integrated: "— LUFS".to_owned(),
            loudness_range: "— LU".to_owned(),
            maximum_true_peak: "— dBTP".to_owned(),
            programme_time: "00:00:00.000".to_owned(),
            lra_status: "Waiting for final-output evidence".to_owned(),
        };
    };
    let programme_time = format_transport_time(snapshot.programme_frame_count());
    OperatorLoudnessPresentation {
        state: match snapshot.state() {
            LoudnessMeasurementState::Running => "RUNNING",
            LoudnessMeasurementState::Standby => "STANDBY",
        },
        momentary: format_loudness(snapshot.momentary()),
        short_term: format_loudness(snapshot.short_term()),
        integrated: format_loudness(snapshot.integrated()),
        loudness_range: snapshot
            .loudness_range_lu()
            .map_or_else(|| "— LU".to_owned(), |value| format!("{value:.1} LU")),
        maximum_true_peak: format_maximum_true_peak(snapshot.maximum_true_peak()),
        programme_time: programme_time.clone(),
        lra_status: if snapshot.lra_stable() {
            "Stable (60 s programme minimum)".to_owned()
        } else {
            format!("Stabilizing · {programme_time} / 00:01:00.000")
        },
    }
}

fn format_loudness(value: Option<ScaledLoudnessValue>) -> String {
    value.map_or_else(
        || "— LUFS".to_owned(),
        |value| {
            let bound = match value.region() {
                LoudnessScaleRegion::Below => "<",
                LoudnessScaleRegion::OnScale => "",
                LoudnessScaleRegion::Above => ">",
            };
            format!("{bound}{:.1} LUFS", value.displayed())
        },
    )
}

fn format_maximum_true_peak(value: Option<MaximumTruePeakDisplay>) -> String {
    value.map_or_else(
        || "— dBTP".to_owned(),
        |value| {
            value
                .displayed_dbtp()
                .map_or_else(|| "−∞ dBTP".to_owned(), |dbtp| format!("{dbtp:.1} dBTP"))
        },
    )
}

fn operator_meter_channel_presentation(
    channel: ChannelSamplePeakBallistics,
    peak_hold_available: bool,
) -> OperatorMeterChannelPresentation {
    operator_meter_channel_values_with_hold(
        channel.measurement_available(),
        peak_hold_available,
        channel.level_dbfs(),
        channel.held_peak_dbfs(),
    )
}

fn operator_meter_channel_values(
    measurement_available: bool,
    level_dbfs: f32,
    held_peak_dbfs: f32,
) -> OperatorMeterChannelPresentation {
    operator_meter_channel_values_with_hold(
        measurement_available,
        measurement_available,
        level_dbfs,
        held_peak_dbfs,
    )
}

fn operator_meter_channel_values_with_hold(
    measurement_available: bool,
    peak_hold_available: bool,
    level_dbfs: f32,
    held_peak_dbfs: f32,
) -> OperatorMeterChannelPresentation {
    let (level, reading) = operator_meter_level_presentation(measurement_available, level_dbfs);
    let (peak_reading, overload_latched) =
        operator_peak_hold_presentation(peak_hold_available, held_peak_dbfs);
    OperatorMeterChannelPresentation {
        level,
        reading,
        peak_reading,
        overload_latched,
    }
}

fn operator_meter_level_presentation(measurement_available: bool, dbfs: f32) -> (f32, String) {
    if !measurement_available || !dbfs.is_finite() {
        return (0.0, "— dBFS".to_owned());
    }
    let level = ((dbfs - OUTPUT_METER_FLOOR_DBFS) / -OUTPUT_METER_FLOOR_DBFS).clamp(0.0, 1.0);
    let reading = if dbfs <= OUTPUT_METER_FLOOR_DBFS {
        format!("≤ {OUTPUT_METER_FLOOR_DBFS:.1} dBFS")
    } else {
        format!("{dbfs:.1} dBFS")
    };
    (level, reading)
}

fn operator_strip_meter_presentation(channel: ChannelSamplePeakDisplay) -> (f32, String) {
    operator_meter_level_presentation(channel.measurement_available(), channel.level_dbfs())
}

fn operator_peak_hold_presentation(measurement_available: bool, dbfs: f32) -> (String, bool) {
    if !measurement_available || !dbfs.is_finite() {
        return ("hold —".to_owned(), false);
    }
    let reading = if dbfs <= OUTPUT_METER_FLOOR_DBFS {
        format!("hold ≤ {OUTPUT_METER_FLOOR_DBFS:.1}")
    } else {
        format!("hold {dbfs:.1}")
    };
    (reading, dbfs >= 0.0)
}

fn operator_observation_presentation(
    transport_state: PlaybackRecoveryTransportState,
    transport_frame: usize,
    musical_timeline: &RuntimeMusicalTimeline,
    stream_state: OutputStreamState,
    counters: OutputStreamSnapshot,
    panel_connected: bool,
    diagnostic: String,
) -> OperatorObservationPresentation {
    OperatorObservationPresentation {
        transport_state: match transport_state {
            PlaybackRecoveryTransportState::Stopped => "Stopped",
            PlaybackRecoveryTransportState::Rolling => "Rolling",
        },
        transport_time: format_transport_time(transport_frame as u64),
        transport_frame: transport_frame.to_string(),
        transport_musical_position: musical_timeline
            .format_position(transport_frame)
            .unwrap_or_else(|| "unavailable".to_owned()),
        stream_state: format!("{stream_state:?}"),
        callback_count: counters.callback_count.to_string(),
        stream_failures: counters.stream_error_count.to_string(),
        native_xruns: counters
            .native_xruns
            .and_then(|snapshot| snapshot.recovery_count)
            .map_or_else(|| "unavailable".to_owned(), |count| count.to_string()),
        panel_state: if panel_connected {
            "Connected"
        } else {
            "Disconnected"
        },
        diagnostic: if stream_state == OutputStreamState::Failed {
            "native audio ownership ended unexpectedly".to_owned()
        } else {
            diagnostic
        },
    }
}

fn format_transport_time(frame: u64) -> String {
    let sample_rate_hz = u64::from(SAMPLE_RATE_HZ);
    let whole_seconds = frame / sample_rate_hz;
    let milliseconds = (frame % sample_rate_hz) * 1_000 / sample_rate_hz;
    let hours = whole_seconds / 3_600;
    let minutes = whole_seconds / 60 % 60;
    let seconds = whole_seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

fn apply_observations(
    ui: &OperatorWindow,
    observers: &RuntimeObservers,
    output_state: OutputStreamState,
    diagnostic: &Mutex<String>,
) -> PlaybackCycleObservation {
    let audio = observers.audio.snapshot();
    let cycle = audio.recovery.observe_cycle();
    let stream_state = output_state;
    let counters = audio.output_counters.snapshot();
    if stream_state == OutputStreamState::Failed {
        set_diagnostic(
            diagnostic,
            "native audio ownership ended unexpectedly".to_owned(),
        );
    }
    let current_diagnostic = diagnostic.lock().map_or_else(
        |_| "diagnostic state unavailable".to_owned(),
        |value| value.clone(),
    );
    let presentation = operator_observation_presentation(
        cycle.state(),
        cycle.position_frame(),
        &observers.facts.musical_timeline,
        stream_state,
        counters,
        observers.panel_connected.load(Ordering::Acquire),
        current_diagnostic,
    );
    ui.set_transport_state(presentation.transport_state.into());
    ui.set_transport_time(presentation.transport_time.into());
    ui.set_transport_frame(presentation.transport_frame.into());
    ui.set_transport_musical_position(presentation.transport_musical_position.into());
    ui.set_stream_state(presentation.stream_state.into());
    ui.set_callback_count(presentation.callback_count.into());
    ui.set_stream_failures(presentation.stream_failures.into());
    ui.set_native_xruns(presentation.native_xruns.into());
    ui.set_panel_state(presentation.panel_state.into());
    ui.set_diagnostic(presentation.diagnostic.into());
    cycle
}

fn apply_operator_meter_observation(
    ui: &OperatorWindow,
    observer: &mut OperatorMeterObserver,
    loudness_authority: &Cell<Option<LoudnessMeasurementState>>,
    terminal_latched: bool,
) {
    let observation = observer.observe();
    let strip_meters = observation.strip_meters;
    let presentation = observation.presentation;
    ui.set_left_output_level(presentation.left_level);
    ui.set_left_output_reading(presentation.left_reading.into());
    ui.set_left_output_peak_reading(presentation.left_peak_reading.into());
    ui.set_right_output_level(presentation.right_level);
    ui.set_right_output_reading(presentation.right_reading.into());
    ui.set_right_output_peak_reading(presentation.right_peak_reading.into());
    ui.set_output_overload_latched(presentation.overload_latched);
    ui.set_output_meter_state(presentation.state.into());
    ui.set_output_meter_telemetry(presentation.telemetry.into());
    apply_operator_strip_meter_observation(
        ui,
        strip_meters,
        terminal_latched || observer.sender_disconnected,
    );
    ui.set_loudness_state(presentation.loudness.state.into());
    ui.set_loudness_momentary(presentation.loudness.momentary.into());
    ui.set_loudness_short_term(presentation.loudness.short_term.into());
    ui.set_loudness_integrated(presentation.loudness.integrated.into());
    ui.set_loudness_range(presentation.loudness.loudness_range.into());
    ui.set_loudness_maximum_true_peak(presentation.loudness.maximum_true_peak.into());
    ui.set_loudness_programme_time(presentation.loudness.programme_time.into());
    ui.set_loudness_lra_status(presentation.loudness.lra_status.into());
    let authority = (!terminal_latched && !observer.sender_disconnected)
        .then_some(observer.latest_loudness)
        .flatten()
        .map(LoudnessPresentationSnapshot::state);
    loudness_authority.set(authority);
    if terminal_latched {
        ui.set_loudness_reset_confirming(false);
    }
    ui.set_loudness_run_enabled(authority == Some(LoudnessMeasurementState::Standby));
    ui.set_loudness_standby_enabled(authority == Some(LoudnessMeasurementState::Running));
    ui.set_loudness_reset_enabled(authority.is_some());
}

fn apply_operator_input_monitor_observation(
    ui: &OperatorWindow,
    observers: &RuntimeObservers,
    meter_observer: &RefCell<Option<OperatorInputMeterObserver>>,
    terminal_latched: bool,
) {
    let audio = observers.audio.snapshot();
    let configured = matches!(
        &observers.facts.input_route,
        RuntimeInputRoute::Pinned { .. }
    );
    let presentation = operator_input_monitor_presentation(
        configured,
        audio.input_status.as_ref(),
        audio.input_counters.as_ref(),
        audio.input_control.as_ref(),
        terminal_latched,
    );
    ui.set_input_monitor_configured(presentation.authority.configured());
    ui.set_input_monitor_state(presentation.authority.state().into());
    ui.set_input_callback_count(presentation.callback_count.into());
    ui.set_input_captured_frame_count(presentation.captured_frame_count.into());
    ui.set_input_dropped_frame_count(presentation.dropped_frame_count.into());
    ui.set_input_monitor_gain_percent(presentation.gain_percent);
    ui.set_input_monitor_muted(presentation.muted);
    ui.set_input_monitor_controls_enabled(presentation.authority.running());

    if !presentation.authority.configured() {
        clear_operator_input_meter(ui, "No input monitoring requested");
        return;
    }
    if !presentation.authority.running() {
        clear_operator_input_meter(
            ui,
            "Raw input meter unavailable while capture is not running",
        );
        return;
    }
    let mut meter_observer = meter_observer.borrow_mut();
    let Some(meter_observer) = meter_observer.as_mut() else {
        clear_operator_input_meter(ui, "Raw input-meter authority is unavailable");
        return;
    };
    let meter = meter_observer.observe();
    ui.set_left_input_level(meter.left_level);
    ui.set_left_input_reading(meter.left_reading.into());
    ui.set_right_input_level(meter.right_level);
    ui.set_right_input_reading(meter.right_reading.into());
    ui.set_input_meter_live(meter.live);
    ui.set_input_meter_state(meter.state.into());
    ui.set_input_meter_telemetry(meter.telemetry.into());
}

fn clear_operator_input_meter(ui: &OperatorWindow, state: &str) {
    ui.set_left_input_level(0.0);
    ui.set_left_input_reading("— dBFS".into());
    ui.set_right_input_level(0.0);
    ui.set_right_input_reading("— dBFS".into());
    ui.set_input_meter_live(false);
    ui.set_input_meter_state(state.into());
    ui.set_input_meter_telemetry("—".into());
}

fn apply_operator_strip_meter_observation(
    ui: &OperatorWindow,
    strip_meters: Option<MixerStripSamplePeakBallisticsSnapshot>,
    terminal_latched: bool,
) {
    let model = ui.get_mixer_channels();
    let layout_matches = !terminal_latched
        && strip_meters.is_some_and(|strip_meters| {
            model.row_count() == strip_meters.slot_count()
                && (0..strip_meters.slot_count()).all(|slot| {
                    model.row_data(slot).is_some_and(|row| {
                        row.index == i32::try_from(slot).unwrap_or(-1)
                            && strip_meters.slot(slot).is_some()
                    })
                })
        });
    if !layout_matches {
        clear_operator_strip_meter_observation(&model);
        return;
    }
    let Some(strip_meters) = strip_meters else {
        clear_operator_strip_meter_observation(&model);
        return;
    };
    for slot in 0..strip_meters.slot_count() {
        let (Some(strip), Some(mut row)) = (strip_meters.slot(slot), model.row_data(slot)) else {
            clear_operator_strip_meter_observation(&model);
            return;
        };
        let (left_level, left_reading) = operator_strip_meter_presentation(strip.left());
        let (right_level, right_reading) = operator_strip_meter_presentation(strip.right());
        row.meter_left_level = left_level;
        row.meter_right_level = right_level;
        row.meter_left_reading = left_reading.into();
        row.meter_right_reading = right_reading.into();
        model.set_row_data(slot, row);
    }
}

fn clear_operator_strip_meter_observation(model: &ModelRc<OperatorMixerRow>) {
    for slot in 0..model.row_count() {
        if let Some(mut row) = model.row_data(slot) {
            row.meter_left_level = 0.0;
            row.meter_right_level = 0.0;
            row.meter_left_reading = "— dBFS".into();
            row.meter_right_reading = "— dBFS".into();
            model.set_row_data(slot, row);
        }
    }
}

/// Enforces the process startup boundary: artifact acceptance and semantic
/// preparation finish before a native audio owner can be constructed.
fn prepare_output_and_host<P, A, S>(
    prepare: impl FnOnce() -> Result<P, String>,
    prepare_audio: impl FnOnce() -> Result<A, String>,
    host: impl FnOnce(A, P) -> Result<S, String>,
) -> Result<S, String> {
    let prepared = prepare()?;
    let audio = prepare_audio()?;
    host(audio, prepared)
}

fn format_error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut rendered = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use core::fmt::Write;
        write!(&mut rendered, ": {cause}").expect("writing to a string cannot fail");
        source = cause.source();
    }
    rendered
}

fn report_to(served: &ServedPanel, diagnostic: &dyn Fn(String)) {
    diagnostic(match &served.outcome {
        Ok(closure) => format!(
            "panel {} (#{}) ended: {closure:?}",
            served.peer, served.session_id
        ),
        Err(error) => format!(
            "panel {} (#{}) failed: {error}",
            served.peer, served.session_id
        ),
    });
    for notice in &served.notices {
        diagnostic(format!("panel notice: {notice:?}"));
    }
}

fn limits() -> Result<ProjectPlaybackLimits, String> {
    let audio = DecodeLimits::new(
        MAX_AUDIO_FILE_BYTES,
        MAX_AUDIO_FRAMES,
        MAX_AUDIO_DECODED_BYTES,
    )
    .map_err(|error| format!("decode policy is invalid: {error}"))?;
    let max_frames_per_asset = usize::try_from(MAX_AUDIO_FRAMES)
        .map_err(|_| "decode policy exceeds this platform's addressable frames".to_owned())?;
    Ok(ProjectPlaybackLimits {
        max_total_resource_bytes: MAX_PROJECT_RESOURCE_BYTES,
        audio,
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: LoaderLimits::VERSION_1.max_clip_documents,
                max_frames_per_asset,
                max_decoded_sample_bytes: MAX_PROJECT_DECODED_BYTES,
            },
        },
    })
}

/// Requests headless shutdown when standard input closes or asks it to.
///
/// Standard input shares the same stop authority as operating-system
/// termination and native-output failure. The serving loop observes this flag
/// between reads, so it takes effect while a panel is connected rather than
/// only between connections.
fn watch_for_quit(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut input = std::io::stdin().lock();
        loop {
            line.clear();
            let Ok(read) = input.read_line(&mut line) else {
                break;
            };
            if read == 0 || line.trim() == "quit" {
                break;
            }
        }
        stop.store(true, Ordering::Release);
    });
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::fs;
    use std::path::{Path, PathBuf};

    use hyper_arrangement_renderer::RenderScratchSlot;
    use hyper_audio_engine::{InterleavedStereoCallback, StereoOutput};
    use hyper_audio_source::DecodedAudio;
    use hyper_desktop_audio::{InputMeterTestPublication, input_meter_channel_for_test};
    use hyper_mixer_runtime::{PreparedPlaybackMainRuntime, PreparedPlaybackTransportRuntime};
    use hyper_playback_compiler::{MeterEvent, TempoEvent, graph::GraphLimits, scene::SceneLimits};
    use hyper_playback_session::{
        DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyInput,
        assemble_playback_session,
    };
    use uuid::Uuid;

    use super::*;

    fn assert_operator_float(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() <= f32::EPSILON,
            "{actual} != {expected}"
        );
    }

    fn test_scene_performance_observation() -> Arc<Mutex<RuntimeScenePerformanceCaptureObservation>>
    {
        Arc::new(Mutex::new(RuntimeScenePerformanceCaptureObservation {
            state: "fixture capture state".to_owned(),
            start_enabled: false,
            stop_enabled: false,
        }))
    }

    fn test_automation_capture_observation() -> Arc<Mutex<RuntimeAutomationCaptureObservation>> {
        Arc::new(Mutex::new(RuntimeAutomationCaptureObservation {
            state: "fixture automation capture state".to_owned(),
            start_enabled: false,
            stop_enabled: false,
        }))
    }

    fn test_recording_observation() -> Arc<Mutex<RuntimeRecordingObservation>> {
        Arc::new(Mutex::new(RuntimeRecordingObservation {
            state: "fixture recording state".to_owned(),
            start_enabled: false,
            stop_enabled: false,
        }))
    }

    fn test_mixer_update_observation() -> Arc<Mutex<RuntimeMixerUpdateObservation>> {
        Arc::new(Mutex::new(RuntimeMixerUpdateObservation {
            state: "fixture mixer-update state".to_owned(),
            export_enabled: false,
        }))
    }

    fn test_native_device_update_observation() -> Arc<Mutex<RuntimeNativeDeviceUpdateObservation>> {
        Arc::new(Mutex::new(RuntimeNativeDeviceUpdateObservation {
            state: "fixture native-device-update state".to_owned(),
            export_enabled: false,
        }))
    }

    fn test_loop_update_observation() -> Arc<Mutex<RuntimeLoopUpdateObservation>> {
        Arc::new(Mutex::new(RuntimeLoopUpdateObservation {
            state: "fixture loop-update state".to_owned(),
            export_enabled: false,
        }))
    }

    fn test_audio_observers(recovery: PlaybackRecoveryCheckpoint) -> RuntimeAudioObservers {
        RuntimeAudioObservers {
            handles: Arc::new(Mutex::new(RuntimeAudioObservationHandles {
                output_status: OutputStreamStatus::default(),
                output_counters: OutputStreamCounters::default(),
                input_status: None,
                input_counters: None,
                input_control: None,
                recovery,
            })),
        }
    }

    fn test_timeline_marker_observers(
        timeline: RuntimeMusicalTimeline,
        markers: Arc<[OperatorTimelineMarker]>,
    ) -> RuntimeObservers {
        RuntimeObservers {
            facts: RuntimeFacts {
                bundle_identity: "marker-fixture".to_owned(),
                deployment_generation: 1,
                recovered_from: None,
                project_name: "Marker fixture".to_owned(),
                project_id: "project-marker-fixture".to_owned(),
                project_revision: 1,
                output_route: RuntimeOutputRoute::SystemDefault,
                input_route: RuntimeInputRoute::Disabled,
                playback_extent: PlaybackExtent::Unbounded,
                musical_timeline: timeline,
                timeline_markers: markers,
            },
            audio: test_audio_observers(PlaybackRecoveryCheckpoint::default()),
            meter_receivers: RuntimeMeterReceivers::default(),
            panel_connected: Arc::new(AtomicBool::new(false)),
            recording: test_recording_observation(),
            playback_available: Arc::new(AtomicBool::new(true)),
            automation_capture: test_automation_capture_observation(),
            mixer_update: test_mixer_update_observation(),
            native_device_update: test_native_device_update_observation(),
            loop_update: test_loop_update_observation(),
            scene_performance: test_scene_performance_observation(),
        }
    }

    #[test]
    fn audio_observer_rebind_replaces_the_complete_playback_generation() {
        let original_range = TransportLoopRange::new(64, 256).unwrap();
        let (_original_publisher, original) =
            hyper_mixer_runtime::playback_recovery_channel_with_initial_loop_and_metronome(
                None,
                None,
                Some(original_range),
                true,
                hyper_mixer_runtime::InitialMetronomeState::unavailable(),
            );
        let observers = test_audio_observers(original);
        assert_eq!(
            observers.snapshot().recovery.snapshot().loop_range(),
            Some(original_range)
        );

        let replacement_range = TransportLoopRange::new(512, 1_024).unwrap();
        let (_replacement_publisher, replacement) =
            hyper_mixer_runtime::playback_recovery_channel_with_initial_loop_and_metronome(
                None,
                None,
                Some(replacement_range),
                false,
                hyper_mixer_runtime::InitialMetronomeState::unavailable(),
            );
        let mut replacement_handles = observers.snapshot();
        replacement_handles.recovery = replacement;
        observers.replace(replacement_handles);

        let rebound = observers.snapshot().recovery.snapshot();
        assert_eq!(rebound.loop_range(), Some(replacement_range));
        assert!(!rebound.loop_enabled());
    }

    #[test]
    fn meter_receiver_handoff_replaces_one_complete_pending_generation() {
        let receivers = RuntimeMeterReceivers::default();
        let (_original_output_sender, original_output) =
            hyper_mixer_runtime::output_meter_channel();
        let (_original_input_sender, original_input) =
            hyper_desktop_audio::input_meter_channel_for_test().unwrap();
        receivers.replace(original_output, Some(original_input));

        let (_replacement_output_sender, replacement_output) =
            hyper_mixer_runtime::output_meter_channel();
        receivers.replace(replacement_output, None);

        let replacement = receivers.take().unwrap();
        assert!(replacement.input.is_none());
        drop(replacement.output);
        assert!(receivers.take().is_none());
    }

    #[test]
    fn playback_owner_poll_rebinds_exactly_one_completed_replacement() {
        let refreshes = Cell::new(0_u8);
        let pending = finish_runtime_playback_owner_poll(
            RuntimePlaybackOwnerPoll {
                message: Some("waiting for native shutdown".to_owned()),
                playback_replaced: false,
                terminal_output_failure: false,
            },
            || {
                refreshes.set(refreshes.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            pending.message.as_deref(),
            Some("waiting for native shutdown")
        );
        assert!(!pending.terminal_output_failure);
        assert_eq!(refreshes.get(), 0);

        let replaced = finish_runtime_playback_owner_poll(
            RuntimePlaybackOwnerPoll {
                message: Some("Runtime playback restored".to_owned()),
                playback_replaced: true,
                terminal_output_failure: false,
            },
            || {
                refreshes.set(refreshes.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            replaced.message.as_deref(),
            Some("Runtime playback restored")
        );
        assert!(!replaced.terminal_output_failure);
        assert_eq!(refreshes.get(), 1);

        let error = finish_runtime_playback_owner_poll(
            RuntimePlaybackOwnerPoll {
                message: None,
                playback_replaced: true,
                terminal_output_failure: false,
            },
            || {
                refreshes.set(refreshes.get() + 1);
                Err("receiver rebind failed".to_owned())
            },
        )
        .unwrap_err();
        assert_eq!(error, "receiver rebind failed");
        assert_eq!(refreshes.get(), 2);
    }

    #[test]
    fn runtime_shutdown_terminalizes_every_playback_owner_phase() {
        for (phase, expected) in [
            (
                RuntimePlaybackOwnerPhase::Playback,
                RuntimePlaybackShutdownAction::RetainPlayback,
            ),
            (
                RuntimePlaybackOwnerPhase::Recording,
                RuntimePlaybackShutdownAction::AbortRecording,
            ),
            (
                RuntimePlaybackOwnerPhase::PendingStop,
                RuntimePlaybackShutdownAction::RetainPendingStop,
            ),
            (
                RuntimePlaybackOwnerPhase::Recovery,
                RuntimePlaybackShutdownAction::RetainRecovery,
            ),
            (
                RuntimePlaybackOwnerPhase::PendingStart,
                RuntimePlaybackShutdownAction::RetainPendingStart,
            ),
            (
                RuntimePlaybackOwnerPhase::StopError,
                RuntimePlaybackShutdownAction::RetainStopError,
            ),
            (
                RuntimePlaybackOwnerPhase::ShutdownRetained,
                RuntimePlaybackShutdownAction::AlreadyRetained,
            ),
        ] {
            assert_eq!(runtime_playback_shutdown_action(phase), expected);
        }
    }

    #[test]
    fn recording_interruption_requires_terminal_native_evidence() {
        let input_states = [
            None,
            Some(StreamState::Starting),
            Some(StreamState::Running),
            Some(StreamState::StopRequested),
            Some(StreamState::Stopped),
            Some(StreamState::Failed),
        ];
        for input in input_states {
            for output in [OutputStreamState::Running, OutputStreamState::Failed] {
                let interruption = classify_runtime_recording_interruption(input, output);
                assert_eq!(
                    interruption.is_some_and(RuntimeRecordingInterruption::output_failed),
                    output == OutputStreamState::Failed
                );
                assert_eq!(
                    interruption.and_then(|interruption| interruption.input),
                    match input {
                        None => Some(RuntimeRecordingInputInterruption::Unavailable),
                        Some(StreamState::Failed) => {
                            Some(RuntimeRecordingInputInterruption::Failed)
                        }
                        Some(_) => None,
                    }
                );
            }
        }
    }

    #[test]
    fn finalized_recording_recovery_routes_one_move_only_owner() {
        struct MoveOnlyRecovery(u8);

        let restored = route_finalized_recording_recovery(MoveOnlyRecovery(1), None, false);
        let RuntimeFinalizedRecordingRecoveryRoute::RestorePlayback(restored) = restored else {
            panic!("clean manual finalization must restore playback")
        };
        assert_eq!(restored.0, 1);

        let input_failure = RuntimeRecordingInterruption {
            input: Some(RuntimeRecordingInputInterruption::Failed),
            output_failed: false,
        };
        let restored =
            route_finalized_recording_recovery(MoveOnlyRecovery(2), Some(input_failure), false);
        let RuntimeFinalizedRecordingRecoveryRoute::RestorePlayback(restored) = restored else {
            panic!("input-only finalization may restore playback")
        };
        assert_eq!(restored.0, 2);

        let output_failure = RuntimeRecordingInterruption {
            input: None,
            output_failed: true,
        };
        for (token, interruption, finalized_output_failure) in [
            (3, Some(output_failure), false),
            (4, None, true),
            (5, Some(input_failure), true),
        ] {
            let retained = route_finalized_recording_recovery(
                MoveOnlyRecovery(token),
                interruption,
                finalized_output_failure,
            );
            let RuntimeFinalizedRecordingRecoveryRoute::RetainForShutdown(retained) = retained
            else {
                panic!("any pre- or post-finalization output failure must retain shutdown")
            };
            assert_eq!(retained.0, token);
        }
    }

    #[test]
    fn pending_recording_recovery_routes_terminal_output_without_replacement() {
        struct MoveOnlyRecovery(u8);

        let retained = route_pending_stop_recovery(
            MoveOnlyRecovery(1),
            RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
            true,
        );
        let RuntimePendingStopRecoveryRoute::RetainForShutdown(retained) = retained else {
            panic!("terminal finalized-recording output must remain shutdown-retained")
        };
        assert_eq!(retained.0, 1);

        let completed = route_pending_stop_recovery(
            MoveOnlyRecovery(2),
            RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
            false,
        );
        let RuntimePendingStopRecoveryRoute::Continue {
            recovery: completed,
            action: RuntimePlaybackRecoveryAction::RestoreFinalizedRecordingPlayback,
        } = completed
        else {
            panic!("clean finalized-recording teardown may restore playback")
        };
        assert_eq!(completed.0, 2);

        let start_failure = route_pending_stop_recovery(
            MoveOnlyRecovery(3),
            RuntimePlaybackRecoveryAction::StartRecording { target_index: 4 },
            true,
        );
        let RuntimePendingStopRecoveryRoute::RetainForShutdown(retained) = start_failure else {
            panic!("terminal output failure before recording must not create a replacement owner")
        };
        assert_eq!(retained.0, 3);

        let input_only_failure = route_pending_stop_recovery(
            MoveOnlyRecovery(4),
            RuntimePlaybackRecoveryAction::StartRecording { target_index: 5 },
            false,
        );
        let RuntimePendingStopRecoveryRoute::Continue {
            recovery,
            action: RuntimePlaybackRecoveryAction::StartRecording { target_index },
        } = input_only_failure
        else {
            panic!("input-only shutdown does not make the output terminal")
        };
        assert_eq!(recovery.0, 4);
        assert_eq!(target_index, 5);
    }

    #[test]
    fn recording_start_classifies_only_terminal_output_stop_errors() {
        assert!(!terminal_playback_output_stop_failure(None));
        assert!(terminal_playback_output_stop_failure(Some(
            &DesktopOutputStopError::RuntimeFailed
        )));
        assert!(terminal_playback_output_stop_failure(Some(
            &DesktopOutputStopError::ThreadPanicked
        )));
    }

    #[test]
    fn audio_target_listings_are_standalone_invocations() {
        assert_eq!(
            parse_runtime_invocation(["--list-audio-outputs".to_owned()]),
            Ok(RuntimeInvocation::ListAudioOutputs)
        );
        assert_eq!(
            parse_runtime_invocation(["--list-audio-inputs".to_owned()]),
            Ok(RuntimeInvocation::ListAudioInputs)
        );

        let RuntimeInvocation::Host(arguments) = parse_runtime_invocation([
            "--audio-output".to_owned(),
            "studio-right".to_owned(),
            "--audio-input".to_owned(),
            "studio-left".to_owned(),
            "deployment".to_owned(),
        ])
        .unwrap() else {
            panic!("a deployment invocation must remain a host invocation");
        };
        assert_eq!(arguments.audio_output(), Some("studio-right"));
        assert_eq!(arguments.audio_input(), Some("studio-left"));

        assert_eq!(
            parse_runtime_invocation(["--list-audio-outputs".to_owned(), "deployment".to_owned(),]),
            Err("audio target listing options must be used by themselves".to_owned())
        );
        assert_eq!(
            parse_runtime_invocation([
                "--list-audio-inputs".to_owned(),
                "--list-audio-outputs".to_owned(),
            ]),
            Err("audio target listing options must be used by themselves".to_owned())
        );
    }

    #[test]
    fn help_and_version_are_standalone_informational_invocations() {
        for option in ["-h", "--help"] {
            assert_eq!(
                parse_runtime_invocation([option.to_owned()]),
                Ok(RuntimeInvocation::Help)
            );
        }
        for option in ["-V", "--version"] {
            assert_eq!(
                parse_runtime_invocation([option.to_owned()]),
                Ok(RuntimeInvocation::Version)
            );
        }
        for option in ["-h", "--help", "-V", "--version"] {
            assert_eq!(
                parse_runtime_invocation([option.to_owned(), "deployment".to_owned()]),
                Err("help and version options must be used by themselves".to_owned())
            );
        }
        assert_eq!(
            RUNTIME_VERSION.strip_prefix("hyper-runtime "),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn installed_shutdown_signal_requests_the_shared_runtime_stop() {
        let installed = Arc::new(Mutex::new(None::<RuntimeShutdownHandler>));
        let installed_by_registration = Arc::clone(&installed);
        let stop = install_runtime_shutdown_signal_with(move |handler| {
            *installed_by_registration.lock().unwrap() = Some(handler);
            Ok::<(), &'static str>(())
        })
        .unwrap();
        assert!(!stop.load(Ordering::Acquire));

        let mut handler = installed.lock().unwrap().take().unwrap();
        handler();
        assert!(stop.load(Ordering::Acquire));
        handler();
        assert!(stop.load(Ordering::Acquire));
    }

    #[test]
    fn rejected_shutdown_signal_installation_publishes_no_stop_authority() {
        let result = install_runtime_shutdown_signal_with(|_| Err("registration refused"));
        assert!(matches!(result, Err("registration refused")));
    }

    #[test]
    fn runtime_help_accounts_for_every_supported_option() {
        for option in [
            "-h, --help",
            "-V, --version",
            "--list-audio-outputs",
            "--list-audio-inputs",
            "--audio-output",
            "--audio-input",
            "--panel-listen",
            "--recording-updates",
            "--performance-updates",
            "--automation-updates",
            "--mixer-updates",
            "--native-device-updates",
            "--loop-updates",
            "--operator-ui",
        ] {
            assert!(RUNTIME_HELP.contains(option), "help must describe {option}");
        }
    }

    #[test]
    fn control_mapping_summary_preserves_every_native_unavailability_reason() {
        let summary = operator_control_mapping_summary([
            ControlBindingAvailability::Live,
            ControlBindingAvailability::Live,
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::CueOutputUnavailable,
            ),
            ControlBindingAvailability::Unavailable(ControlBindingUnavailableReason::SceneDisabled),
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneConsolidationRequired,
            ),
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared,
            ),
        ]);
        assert_eq!(summary.live, 2);
        assert_eq!(summary.unavailable(), 4);
        assert_eq!(summary.enabled(), 6);
        assert_eq!(summary.cue_output_unavailable, 1);
        assert_eq!(summary.scene_disabled, 1);
        assert_eq!(summary.scene_consolidation_required, 1);
        assert_eq!(summary.scene_audio_not_prepared, 1);

        assert_eq!(
            operator_control_mapping_presentation(summary, true, false),
            OperatorControlMappingPresentation {
                state: "2 LIVE · 4 UNAVAILABLE".to_owned(),
                detail: "No cue output: 1 · Scene disabled: 1 · Scene consolidation required: 1 · Scene audio not prepared: 1".to_owned(),
                warning: true,
            }
        );
        assert_eq!(
            runtime_control_mapping_announcement(summary),
            "control mapping: 2 LIVE · 4 UNAVAILABLE — No cue output: 1 · Scene disabled: 1 · Scene consolidation required: 1 · Scene audio not prepared: 1"
        );
    }

    #[test]
    fn control_mapping_presentation_revokes_live_claims_without_playback_authority() {
        let summary = operator_control_mapping_summary([ControlBindingAvailability::Live]);
        assert_eq!(
            operator_control_mapping_presentation(summary, true, false),
            OperatorControlMappingPresentation {
                state: "1 LIVE".to_owned(),
                detail: "All enabled bindings accepted by the current playback owner".to_owned(),
                warning: false,
            }
        );
        assert_eq!(
            operator_control_mapping_presentation(summary, false, false),
            OperatorControlMappingPresentation {
                state: "PLAYBACK TRANSITION".to_owned(),
                detail: "1 enabled binding · dispatch resumes after owner recovery".to_owned(),
                warning: true,
            }
        );
        assert_eq!(
            operator_control_mapping_presentation(summary, true, true),
            OperatorControlMappingPresentation {
                state: "RUNTIME STOPPED".to_owned(),
                detail: "Binding dispatch is unavailable".to_owned(),
                warning: true,
            }
        );
        assert_eq!(
            operator_control_mapping_presentation(
                OperatorControlMappingSummary::default(),
                true,
                false,
            ),
            OperatorControlMappingPresentation {
                state: "NO ENABLED BINDINGS".to_owned(),
                detail: "Persisted disabled bindings do not enter Runtime".to_owned(),
                warning: false,
            }
        );
    }

    #[test]
    fn control_mapping_observation_updates_the_operator_surface() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let summary = operator_control_mapping_summary([
            ControlBindingAvailability::Live,
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared,
            ),
        ]);

        apply_operator_control_mapping_observation(&ui, summary, true, false);
        assert_eq!(ui.get_control_mapping_state(), "1 LIVE · 1 UNAVAILABLE");
        assert_eq!(
            ui.get_control_mapping_detail(),
            "Scene audio not prepared: 1"
        );
        assert!(ui.get_control_mapping_warning());

        apply_operator_control_mapping_observation(&ui, summary, false, false);
        assert_eq!(ui.get_control_mapping_state(), "PLAYBACK TRANSITION");
        assert!(ui.get_control_mapping_warning());

        apply_operator_control_mapping_observation(&ui, summary, true, true);
        assert_eq!(ui.get_control_mapping_state(), "RUNTIME STOPPED");
        assert!(ui.get_control_mapping_warning());
    }

    #[test]
    fn selected_input_becomes_the_bounded_monitoring_authority() {
        let output = hyper_desktop_audio::DesktopOutputConfig::try_new(48_000, 1_024)
            .unwrap()
            .with_routing_id("studio-output")
            .unwrap();
        let input = hyper_desktop_audio::DesktopInputConfig::try_new(48_000, 1_024)
            .unwrap()
            .with_routing_id("studio-input")
            .unwrap();

        let audio = configure_runtime_audio(
            output.clone(),
            RuntimeOutputRoute::Pinned {
                display_name: "Studio Output".to_owned(),
            },
            Some(input),
            RuntimeInputRoute::Pinned {
                display_name: "Studio Input".to_owned(),
            },
        )
        .unwrap();
        let monitoring = audio.monitoring.expect("selected input must be consumed");

        assert_eq!(audio.output, output);
        assert_eq!(audio.output_route.to_string(), "Pinned: Studio Output");
        assert_eq!(audio.input_route.to_string(), "Pinned: Studio Input");
        assert_eq!(monitoring.input().routing_id(), Some("studio-input"));
        assert_eq!(monitoring.output(), &audio.output);
        assert_eq!(
            monitoring.transport_capacity_frames(),
            INPUT_MONITORING_TRANSPORT_FRAMES
        );
        assert_eq!(
            monitoring.target_latency_frames(),
            INPUT_MONITORING_TARGET_FRAMES
        );
        assert_eq!(
            monitoring.processing_chunk_frames(),
            INPUT_MONITORING_CHUNK_FRAMES
        );
    }

    #[test]
    fn disabled_input_does_not_construct_monitoring_authority() {
        let output = hyper_desktop_audio::DesktopOutputConfig::try_new(48_000, 1_024).unwrap();

        let audio = configure_runtime_audio(
            output,
            RuntimeOutputRoute::SystemDefault,
            None,
            RuntimeInputRoute::Disabled,
        )
        .unwrap();

        assert!(audio.monitoring.is_none());
        assert_eq!(audio.input_route, RuntimeInputRoute::Disabled);
    }

    #[test]
    fn input_monitor_controls_queue_without_optimistic_ui_state() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_input_monitor_controls(&ui, &sender, &diagnostic);
        ui.set_input_monitor_muted(true);
        ui.set_input_monitor_gain_percent(50.0);

        ui.invoke_set_input_monitor_gain_requested(72.5);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "input monitor level refused: accepted input authority is unavailable"
        );

        ui.set_input_monitor_controls_enabled(true);
        ui.invoke_set_input_monitor_gain_requested(72.5);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetInputMonitorGain {
                gain_millionths: 725_000,
            })
        );
        assert_operator_float(ui.get_input_monitor_gain_percent(), 50.0);

        ui.invoke_toggle_input_monitor_mute_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetInputMonitorMuted { muted: false })
        );
        assert!(ui.get_input_monitor_muted());

        ui.invoke_set_input_monitor_gain_requested(f32::NAN);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "input monitor level refused: value is outside 0..=100%"
        );
    }

    #[test]
    fn input_monitor_presentation_fails_closed_without_capture_authority() {
        let disabled = operator_input_monitor_presentation(false, None, None, None, false);
        assert_eq!(disabled.authority, OperatorInputMonitorAuthority::Disabled);
        assert!(!disabled.authority.configured());
        assert_eq!(disabled.authority.state(), "DISABLED");
        assert!(!disabled.authority.running());

        let unavailable = operator_input_monitor_presentation(true, None, None, None, false);
        assert_eq!(
            unavailable.authority,
            OperatorInputMonitorAuthority::Unavailable
        );
        assert!(unavailable.authority.configured());
        assert_eq!(unavailable.authority.state(), "UNAVAILABLE");
        assert!(!unavailable.authority.running());
    }

    #[test]
    fn input_monitor_commands_require_running_capture() {
        for state in [
            None,
            Some(StreamState::Starting),
            Some(StreamState::StopRequested),
            Some(StreamState::Stopped),
            Some(StreamState::Failed),
        ] {
            assert!(!runtime_input_monitor_running(state));
        }
        assert!(runtime_input_monitor_running(Some(StreamState::Running)));
    }

    #[test]
    fn input_meter_presentation_labels_raw_evidence_and_disconnects() {
        let config = SamplePeakBallisticsConfig::try_new(48_000, -90.0, 13.3, 0).unwrap();
        let snapshot = StereoSamplePeakDisplayBallistics::new(config)
            .update(7, 480, Some(0.5), Some(0.25))
            .snapshot();

        let live = operator_input_meter_presentation(Some(snapshot), 2, 3, false);
        assert!(live.live);
        assert_eq!(live.state, "Raw accepted-input sample peak");
        assert_eq!(live.left_reading, "-6.0 dBFS");
        assert_eq!(live.right_reading, "-12.0 dBFS");
        assert_eq!(
            live.telemetry,
            "accepted frame 480 · 2 coalesced · 3 dropped display snapshots"
        );

        let ended = operator_input_meter_presentation(Some(snapshot), 2, 3, true);
        assert!(!ended.live);
        assert_eq!(ended.state, "Input meter producer ended");
    }

    #[test]
    fn input_meter_observer_preserves_raw_capture_scope_and_clears_on_disconnect() {
        let (mut publisher, receiver) = input_meter_channel_for_test().unwrap();
        let config = SamplePeakBallisticsConfig::try_new(48_000, -90.0, 13.3, 0).unwrap();
        let mut observer = OperatorInputMeterObserver::new(receiver, config);
        assert_eq!(
            publisher.publish(&[0.5, 0.25]),
            InputMeterTestPublication::Published
        );

        let live = observer.observe();
        assert!(live.live);
        assert_eq!(live.state, "Raw accepted-input sample peak");
        assert_eq!(live.left_reading, "-6.0 dBFS");
        assert_eq!(live.right_reading, "-12.0 dBFS");

        drop(publisher);
        let ended = observer.observe();
        assert!(!ended.live);
        assert_eq!(ended.state, "Input meter producer ended");
        assert_eq!(ended.left_reading, "— dBFS");
        assert_eq!(ended.right_reading, "— dBFS");
    }

    struct RuntimeTemporaryDirectory {
        path: PathBuf,
    }

    impl RuntimeTemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-test-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir(&path).expect("create isolated Runtime test directory");
            Self { path }
        }

        fn child(&self, name: &str) -> PathBuf {
            self.path.join(name)
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for RuntimeTemporaryDirectory {
        fn drop(&mut self) {
            if self.path.starts_with(std::env::temp_dir()) {
                let _ = fs::remove_dir_all(&self.path);
            }
        }
    }

    fn runtime_arguments_with_scene_performance_output(
        deployment: &Path,
        output: Option<&Path>,
    ) -> RuntimeArguments {
        let mut arguments = Vec::new();
        if let Some(output) = output {
            arguments.push("--performance-updates".to_owned());
            arguments.push(output.display().to_string());
        }
        arguments.push(deployment.display().to_string());
        RuntimeArguments::parse(arguments).expect("construct Runtime arguments")
    }

    fn runtime_arguments_with_recording_output(
        deployment: &Path,
        output: Option<&Path>,
    ) -> RuntimeArguments {
        let mut arguments = Vec::new();
        if let Some(output) = output {
            arguments.push("--recording-updates".to_owned());
            arguments.push(output.display().to_string());
        }
        arguments.push(deployment.display().to_string());
        RuntimeArguments::parse(arguments).expect("construct Runtime arguments")
    }

    fn runtime_arguments_with_mixer_update_output(
        deployment: &Path,
        output: Option<&Path>,
    ) -> RuntimeArguments {
        let mut arguments = Vec::new();
        if let Some(output) = output {
            arguments.push("--mixer-updates".to_owned());
            arguments.push(output.display().to_string());
        }
        arguments.push(deployment.display().to_string());
        RuntimeArguments::parse(arguments).expect("construct Runtime arguments")
    }

    fn runtime_arguments_with_native_device_update_output(
        deployment: &Path,
        output: Option<&Path>,
    ) -> RuntimeArguments {
        let mut arguments = Vec::new();
        if let Some(output) = output {
            arguments.push("--native-device-updates".to_owned());
            arguments.push(output.display().to_string());
        }
        arguments.push(deployment.display().to_string());
        RuntimeArguments::parse(arguments).expect("construct Runtime arguments")
    }

    fn runtime_arguments_with_loop_update_output(
        deployment: &Path,
        output: Option<&Path>,
    ) -> RuntimeArguments {
        let mut arguments = Vec::new();
        if let Some(output) = output {
            arguments.push("--loop-updates".to_owned());
            arguments.push(output.display().to_string());
        }
        arguments.push(deployment.display().to_string());
        RuntimeArguments::parse(arguments).expect("construct Runtime arguments")
    }

    fn test_musical_timeline() -> RuntimeMusicalTimeline {
        RuntimeMusicalTimeline::try_new(
            SAMPLE_RATE_HZ,
            &[TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_000,
            }],
            &[MeterEvent {
                tick: 0,
                numerator: 4,
                denominator: 4,
            }],
        )
        .unwrap()
    }

    #[test]
    fn runtime_musical_position_uses_the_verified_tempo_and_meter_maps() {
        let timeline = RuntimeMusicalTimeline::try_new(
            SAMPLE_RATE_HZ,
            &[
                TempoEvent {
                    tick: 0,
                    microseconds_per_quarter: 500_000,
                },
                TempoEvent {
                    tick: 7_680,
                    microseconds_per_quarter: 250_000,
                },
            ],
            &[
                MeterEvent {
                    tick: 0,
                    numerator: 4,
                    denominator: 4,
                },
                MeterEvent {
                    tick: 15_360,
                    numerator: 3,
                    denominator: 4,
                },
            ],
        )
        .unwrap();

        assert_eq!(timeline.format_position(0).as_deref(), Some("1|1|000"));
        assert_eq!(timeline.format_position(96_000).as_deref(), Some("2|1|000"));
        assert_eq!(
            timeline.format_position(144_000).as_deref(),
            Some("3|1|000")
        );
        assert!(timeline.format_position(usize::MAX).is_some());
        assert!(
            RuntimeMusicalTimeline::try_new(
                0,
                &[TempoEvent {
                    tick: 0,
                    microseconds_per_quarter: 500_000,
                }],
                &[MeterEvent {
                    tick: 0,
                    numerator: 4,
                    denominator: 4,
                }],
            )
            .is_err()
        );
        assert!(
            RuntimeMusicalTimeline::try_new(
                SAMPLE_RATE_HZ,
                &[TempoEvent {
                    tick: 0,
                    microseconds_per_quarter: 500_000,
                }],
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn operator_mixer_value_conversion_is_bounded_finite_and_rounded() {
        for (percent, expected) in [(0.0, 0), (200.0, 2_000_000), (12.345_67, 123_457)] {
            assert_eq!(operator_gain_percent_to_millionths(percent), Ok(expected));
        }
        for (percent, expected) in [
            (-100.0, -1_000_000),
            (100.0, 1_000_000),
            (-12.345_67, -123_457),
            (12.345_67, 123_457),
        ] {
            assert_eq!(operator_pan_percent_to_millionths(percent), Ok(expected));
        }

        for percent in [
            f32::from_bits(0.0_f32.to_bits() | 0x8000_0001),
            f32::from_bits(200.0_f32.to_bits() + 1),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(operator_gain_percent_to_millionths(percent), Err(()));
        }
        for percent in [
            f32::from_bits((-100.0_f32).to_bits() + 1),
            f32::from_bits(100.0_f32.to_bits() + 1),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(operator_pan_percent_to_millionths(percent), Err(()));
        }

        assert_eq!(
            operator_metronome_percent_to_gain(0.0),
            Ok(MetronomeGain::SILENCE)
        );
        assert_eq!(
            operator_metronome_percent_to_gain(72.5),
            Ok(MetronomeGain::from_millionths(725_000).unwrap())
        );
        assert_eq!(
            operator_metronome_percent_to_gain(100.0),
            Ok(MetronomeGain::UNITY)
        );
        for percent in [
            f32::from_bits(0.0_f32.to_bits() | 0x8000_0001),
            f32::from_bits(100.0_f32.to_bits() + 1),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(operator_metronome_percent_to_gain(percent), Err(()));
        }
    }

    #[test]
    fn operator_native_device_value_conversion_is_bounded_finite_and_rounded() {
        assert_eq!(
            operator_scaled_u32(20.0, 20.0, 20_000.0, 1_000.0),
            Some(20_000)
        );
        assert_eq!(
            operator_scaled_u32(12.345_67, 0.1, 20.0, 1_000_000.0),
            Some(12_345_670)
        );
        assert_eq!(
            operator_scaled_i32(-12.345_67, -80.0, 0.0, 1_000.0),
            Some(-12_346)
        );

        for value in [
            f32::from_bits(20.0_f32.to_bits() - 1),
            f32::from_bits(20_000.0_f32.to_bits() + 1),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(operator_scaled_u32(value, 20.0, 20_000.0, 1_000.0), None);
        }
        for value in [
            f32::from_bits((-80.0_f32).to_bits() + 1),
            f32::from_bits(0.0_f32.to_bits() + 1),
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            assert_eq!(operator_scaled_i32(value, -80.0, 0.0, 1_000.0), None);
        }
    }

    #[test]
    fn rejected_bundle_preparation_cannot_open_output() {
        let events = RefCell::new(Vec::new());

        let result = prepare_output_and_host(
            || {
                events.borrow_mut().push("prepare");
                Err::<(), _>("bundle rejected".to_owned())
            },
            || {
                events.borrow_mut().push("output");
                Ok(())
            },
            |(), ()| {
                events.borrow_mut().push("host");
                Ok(())
            },
        );

        assert_eq!(result.unwrap_err(), "bundle rejected");
        assert_eq!(*events.borrow(), ["prepare"]);
    }

    #[test]
    fn successful_startup_prepares_then_opens_output_then_hosts() {
        let events = RefCell::new(Vec::new());

        prepare_output_and_host(
            || {
                events.borrow_mut().push("prepare");
                Ok(())
            },
            || {
                events.borrow_mut().push("output");
                Ok(())
            },
            |(), ()| {
                events.borrow_mut().push("host");
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(*events.borrow(), ["prepare", "output", "host"]);
    }

    #[test]
    fn operator_scene_catalog_preserves_bounded_authority_and_fails_closed() {
        let names = [
            "Apertura",
            "Caffè notturno",
            "東京の雨",
            "Στροφή",
            "Затишье",
            "ليلة",
            "Pausa lunga",
            "Finale ✦",
        ];
        let availabilities = [
            DesktopSceneAvailability::Ready,
            DesktopSceneAvailability::Disabled,
            DesktopSceneAvailability::ConsolidationRequired,
            DesktopSceneAvailability::NotPrepared,
            DesktopSceneAvailability::Ready,
            DesktopSceneAvailability::Disabled,
            DesktopSceneAvailability::ConsolidationRequired,
            DesktopSceneAvailability::NotPrepared,
        ];
        let name_queries = RefCell::new(Vec::new());
        let availability_queries = RefCell::new(Vec::new());

        let catalog = build_operator_scene_catalog(
            names.len(),
            |index| {
                name_queries.borrow_mut().push(index);
                names.get(index).map(|name| (*name).to_owned())
            },
            |index| {
                availability_queries.borrow_mut().push(index);
                availabilities.get(index).copied()
            },
        )
        .unwrap();

        assert_eq!(*name_queries.borrow(), (0..names.len()).collect::<Vec<_>>());
        assert_eq!(
            *availability_queries.borrow(),
            (0..names.len()).collect::<Vec<_>>()
        );
        assert_eq!(catalog.len(), names.len());
        for (index, scene) in catalog.iter().enumerate() {
            assert_eq!(scene.index, index);
            assert_eq!(scene.name, names[index]);
            assert_eq!(scene.availability, availabilities[index]);
        }

        assert_eq!(
            build_operator_scene_catalog(
                2,
                |index| (index == 0).then(|| "Only name".to_owned()),
                |_| Some(DesktopSceneAvailability::Ready),
            )
            .unwrap_err(),
            "prepared scene 1 has no retained name"
        );
        assert_eq!(
            build_operator_scene_catalog(
                2,
                |index| Some(format!("Scene {index}")),
                |index| (index == 0).then_some(DesktopSceneAvailability::Ready),
            )
            .unwrap_err(),
            "prepared scene 1 has no launch status"
        );
    }

    #[test]
    fn scene_keyboard_selection_skips_unavailable_rows_and_clamps() {
        let scenes = VecModel::from(vec![
            OperatorSceneRow {
                name: "Intro".into(),
                status: "Ready".into(),
                available: true,
                controls_enabled: true,
                selected: false,
                queued: false,
                last_launched: false,
            },
            OperatorSceneRow {
                name: "Missing".into(),
                status: "Audio unavailable".into(),
                available: false,
                controls_enabled: false,
                selected: false,
                queued: false,
                last_launched: false,
            },
            OperatorSceneRow {
                name: "Finale".into(),
                status: "Ready".into(),
                available: true,
                controls_enabled: true,
                selected: false,
                queued: false,
                last_launched: false,
            },
        ]);

        assert_eq!(operator_scene_selection_target(&scenes, None, 1), Some(0));
        assert_eq!(operator_scene_selection_target(&scenes, None, -1), Some(2));
        assert_eq!(
            operator_scene_selection_target(&scenes, Some(0), 1),
            Some(2)
        );
        assert_eq!(
            operator_scene_selection_target(&scenes, Some(2), 1),
            Some(2)
        );
        assert_eq!(
            operator_scene_selection_target(&scenes, Some(0), -1),
            Some(0)
        );
        assert_eq!(
            operator_scene_selection_target(&scenes, Some(1), 1),
            Some(0)
        );
    }

    #[test]
    fn startup_diagnostics_retain_the_bundle_rejection_cause() {
        let output_opened = Cell::new(false);
        let missing = std::env::temp_dir().join(format!(
            "hyper-runtime-missing-bundle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let diagnostic = prepare_output_and_host(
            || {
                resolve_runtime_deployment(
                    missing,
                    env!("CARGO_PKG_VERSION"),
                    MAX_RUNTIME_BUNDLE_BYTES,
                    limits().unwrap(),
                )
                .map_err(|error| format_error_chain(&error))
            },
            || {
                output_opened.set(true);
                Ok(())
            },
            |(), _| Ok(()),
        )
        .unwrap_err();

        assert!(diagnostic.starts_with("inspect deployment target: "));
        assert!(
            diagnostic.len() > "inspect deployment target: ".len(),
            "the operating-system cause remains visible"
        );
        assert!(!output_opened.get());
    }

    #[test]
    fn scene_performance_output_is_explicit_and_outside_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = temporary.child("performance-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();

        let disabled = runtime_arguments_with_scene_performance_output(&deployment, None);
        assert!(
            open_runtime_scene_performance_output(&disabled)
                .unwrap()
                .is_none()
        );

        let arguments = runtime_arguments_with_scene_performance_output(&deployment, Some(&output));
        let retained = open_runtime_scene_performance_output(&arguments)
            .unwrap()
            .expect("the explicit external directory is retained");
        assert_eq!(retained.root_path(), output);
        assert!(retained.root_path().starts_with(temporary.path()));
    }

    #[test]
    fn scene_performance_output_never_targets_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = deployment.join("performance-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();
        let arguments = runtime_arguments_with_scene_performance_output(&deployment, Some(&output));

        let Err(error) = open_runtime_scene_performance_output(&arguments) else {
            panic!("a deployment child cannot be used for performance updates");
        };
        assert!(
            error.contains("must be outside deployment target"),
            "{error}"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());
    }

    #[test]
    fn recording_output_is_explicit_and_outside_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = temporary.child("recording-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();

        let disabled = runtime_arguments_with_recording_output(&deployment, None);
        assert!(open_runtime_recording_output(&disabled).unwrap().is_none());

        let arguments = runtime_arguments_with_recording_output(&deployment, Some(&output));
        let retained = open_runtime_recording_output(&arguments)
            .unwrap()
            .expect("the explicit external recording directory is retained");
        assert_eq!(retained.root_path(), output);
        assert!(retained.root_path().starts_with(temporary.path()));
    }

    #[test]
    fn recording_output_never_targets_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = deployment.join("recording-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();
        let arguments = runtime_arguments_with_recording_output(&deployment, Some(&output));

        let Err(error) = open_runtime_recording_output(&arguments) else {
            panic!("a deployment child cannot be used for recording updates");
        };
        assert!(
            error.contains("must be outside deployment target"),
            "{error}"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());
    }

    #[test]
    fn mixer_update_output_is_explicit_and_outside_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = temporary.child("mixer-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();

        let disabled = runtime_arguments_with_mixer_update_output(&deployment, None);
        assert!(
            open_runtime_mixer_update_output(&disabled)
                .unwrap()
                .is_none()
        );

        let arguments = runtime_arguments_with_mixer_update_output(&deployment, Some(&output));
        let retained = open_runtime_mixer_update_output(&arguments)
            .unwrap()
            .expect("the explicit external directory is retained");
        assert_eq!(retained.root_path(), output);
        assert!(retained.root_path().starts_with(temporary.path()));
    }

    #[test]
    fn mixer_update_output_never_targets_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = deployment.join("mixer-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();
        let arguments = runtime_arguments_with_mixer_update_output(&deployment, Some(&output));

        let Err(error) = open_runtime_mixer_update_output(&arguments) else {
            panic!("a deployment child cannot be used for mixer updates");
        };
        assert!(
            error.contains("must be outside deployment target"),
            "{error}"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());
    }

    #[test]
    fn native_device_update_output_is_explicit_and_outside_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = temporary.child("native-device-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();

        let disabled = runtime_arguments_with_native_device_update_output(&deployment, None);
        assert!(
            open_runtime_native_device_update_output(&disabled)
                .unwrap()
                .is_none()
        );

        let arguments =
            runtime_arguments_with_native_device_update_output(&deployment, Some(&output));
        let retained = open_runtime_native_device_update_output(&arguments)
            .unwrap()
            .expect("the explicit external directory is retained");
        assert_eq!(retained.root_path(), output);
        assert!(retained.root_path().starts_with(temporary.path()));
    }

    #[test]
    fn native_device_update_output_never_targets_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = deployment.join("native-device-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();
        let arguments =
            runtime_arguments_with_native_device_update_output(&deployment, Some(&output));

        let Err(error) = open_runtime_native_device_update_output(&arguments) else {
            panic!("a deployment child cannot be used for native-device updates");
        };
        assert!(
            error.contains("must be outside deployment target"),
            "{error}"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());
    }

    #[test]
    fn loop_update_output_is_explicit_and_outside_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = temporary.child("loop-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();

        let disabled = runtime_arguments_with_loop_update_output(&deployment, None);
        assert!(
            open_runtime_loop_update_output(&disabled)
                .unwrap()
                .is_none()
        );

        let arguments = runtime_arguments_with_loop_update_output(&deployment, Some(&output));
        let retained = open_runtime_loop_update_output(&arguments)
            .unwrap()
            .expect("the explicit external directory is retained");
        assert_eq!(retained.root_path(), output);
        assert!(retained.root_path().starts_with(temporary.path()));
    }

    #[test]
    fn loop_update_output_never_targets_the_deployment_tree() {
        let temporary = RuntimeTemporaryDirectory::new();
        let deployment = temporary.child("deployment");
        let output = deployment.join("loop-updates");
        fs::create_dir(&deployment).unwrap();
        fs::create_dir(&output).unwrap();
        let arguments = runtime_arguments_with_loop_update_output(&deployment, Some(&output));

        let Err(error) = open_runtime_loop_update_output(&arguments) else {
            panic!("a deployment child cannot be used for loop updates");
        };
        assert!(
            error.contains("must be outside deployment target"),
            "{error}"
        );
        assert!(fs::read_dir(&output).unwrap().next().is_none());
    }

    #[test]
    fn an_unavailable_panel_listener_keeps_the_runtime_owner_until_quit() {
        let occupied = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = occupied.local_addr().unwrap();
        let arguments = RuntimeArguments::parse([
            "--panel-listen".to_owned(),
            address.to_string(),
            "deployment".to_owned(),
        ])
        .unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Quit).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap();

        assert!(
            diagnostics
                .borrow()
                .iter()
                .any(|message| message.contains("cannot listen")),
            "the occupied endpoint remains visible: {:?}",
            diagnostics.borrow()
        );
        assert!(
            diagnostics
                .borrow()
                .iter()
                .any(|message| message.contains("project stays hosted"))
        );
        assert_eq!(owner.disconnects, 1, "the sole owner gets one release turn");
        assert!(stop.load(Ordering::Acquire));
        drop(occupied);
    }

    #[test]
    fn operator_quit_stops_queued_controls_and_later_owner_polls() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_poll: Some("recording owner checked before shutdown".to_owned()),
            recording_abort: Some(Ok(Some("recording aborted for shutdown".to_owned()))),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            automation_capture_poll: Some("unexpected automation poll".to_owned()),
            mixer_update_poll: Some("unexpected mixer-update poll".to_owned()),
            native_device_update_poll: Some("unexpected native-device poll".to_owned()),
            loop_update_poll: Some("unexpected loop-update poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands.send(OperatorCommand::Quit).unwrap();
        commands.send(OperatorCommand::StopRecording).unwrap();
        commands.send(OperatorCommand::Start).unwrap();

        host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap();

        assert!(stop.load(Ordering::Acquire));
        assert!(owner.recording_poll.is_none());
        assert!(owner.recording_abort.is_none());
        assert!(owner.recording_commands.is_empty());
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.automation_capture_poll.is_some());
        assert!(owner.mixer_update_poll.is_some());
        assert!(owner.native_device_update_poll.is_some());
        assert!(owner.loop_update_poll.is_some());
        assert_eq!(
            command_receiver.try_recv(),
            Ok(OperatorCommand::StopRecording)
        );
        assert_eq!(command_receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(command_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "recording owner checked before shutdown",
                "recording aborted for shutdown",
            ]
        );
    }

    #[test]
    fn pre_requested_shutdown_leaves_operator_commands_queued() {
        let stop = AtomicBool::new(true);
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands.send(OperatorCommand::Start).unwrap();
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        assert!(
            drain_operator_commands(&mut owner, &command_receiver, &stop, &|message| diagnostics
                .borrow_mut()
                .push(message),)
            .is_none()
        );

        assert!(owner.commands.is_empty());
        assert!(diagnostics.borrow().is_empty());
        assert_eq!(command_receiver.try_recv(), Ok(OperatorCommand::Start));
    }

    #[test]
    fn pre_requested_shutdown_skips_the_owner_turn_after_loop_entry() {
        let stop = AtomicBool::new(true);
        let terminal_output_failure = Cell::new(false);
        let terminal_owner_failure = RefCell::new(None);
        let mut owner = OperatorControl {
            recording_poll: Some("unexpected recording poll".to_owned()),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands.send(OperatorCommand::Start).unwrap();

        run_runtime_owner_turn(
            &mut owner,
            &OutputObservation(OutputStreamState::Failed),
            &terminal_output_failure,
            &terminal_owner_failure,
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        );

        assert!(!terminal_output_failure.get());
        assert!(owner.recording_poll.is_some());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.commands.is_empty());
        assert!(diagnostics.borrow().is_empty());
        assert_eq!(command_receiver.try_recv(), Ok(OperatorCommand::Start));
    }

    #[test]
    fn native_output_failure_ends_the_same_production_host_loop() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let output_state = Arc::new(Mutex::new(OutputStreamState::Failed));
        let mut owner = OperatorControl {
            recording_poll: Some("recording owner settled before shutdown".to_owned()),
            recording_poll_output_rebind: Some(Arc::clone(&output_state)),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            automation_capture_poll: Some("unexpected automation poll".to_owned()),
            mixer_update_poll: Some("unexpected mixer-update poll".to_owned()),
            native_device_update_poll: Some("unexpected native-device poll".to_owned()),
            loop_update_poll: Some("unexpected loop-update poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &RebindableOutputObservation(Arc::clone(&output_state)),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(error, "native audio ownership ended unexpectedly");
        assert!(stop.load(Ordering::Acquire));
        assert!(owner.recording_poll.is_none());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.automation_capture_poll.is_some());
        assert!(owner.mixer_update_poll.is_some());
        assert!(owner.native_device_update_poll.is_some());
        assert!(owner.loop_update_poll.is_some());
        assert_eq!(*output_state.lock().unwrap(), OutputStreamState::Running);
        assert_eq!(
            owner.disconnects, 0,
            "there was no panel service to release"
        );
        assert!(
            owner.commands.is_empty(),
            "terminal output is observed before queued controls are forwarded"
        );
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "recording owner settled before shutdown",
                "native audio ownership ended unexpectedly",
            ]
        );
    }

    #[test]
    fn finalization_output_failure_ends_host_without_failed_observer() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_poll: Some("recording finalization found output panic".to_owned()),
            recording_poll_terminal_output_failure: true,
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            automation_capture_poll: Some("unexpected automation poll".to_owned()),
            mixer_update_poll: Some("unexpected mixer-update poll".to_owned()),
            native_device_update_poll: Some("unexpected native-device poll".to_owned()),
            loop_update_poll: Some("unexpected loop-update poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::StopRequested),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(error, "native audio ownership ended unexpectedly");
        assert!(stop.load(Ordering::Acquire));
        assert!(owner.recording_poll.is_none());
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.automation_capture_poll.is_some());
        assert!(owner.mixer_update_poll.is_some());
        assert!(owner.native_device_update_poll.is_some());
        assert!(owner.loop_update_poll.is_some());
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "recording finalization found output panic",
                "native audio ownership ended unexpectedly",
            ]
        );
    }

    #[test]
    fn receiver_transfer_failure_in_recording_poll_ends_host_before_other_work() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_poll: Some("recording transition settled".to_owned()),
            recording_poll_owner_failure: Some(
                "Runtime playback receiver transfer failed: automation receiver rebind failed"
                    .to_owned(),
            ),
            recording_abort: Some(Ok(Some("recording journal retained".to_owned()))),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            automation_capture_poll: Some("unexpected automation poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands.send(OperatorCommand::Start).unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(
            error,
            "Runtime playback receiver transfer failed: automation receiver rebind failed"
        );
        assert!(stop.load(Ordering::Acquire));
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.automation_capture_poll.is_some());
        assert_eq!(command_receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "recording transition settled",
                "Runtime playback receiver transfer failed: automation receiver rebind failed",
                "recording journal retained",
            ]
        );
    }

    #[test]
    fn receiver_transfer_failure_in_recording_command_interrupts_drain() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_start_owner_failure: Some(
                "Runtime playback receiver transfer failed: scene receiver missing".to_owned(),
            ),
            recording_abort: Some(Ok(Some("recording journal retained".to_owned()))),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands
            .send(OperatorCommand::StartRecording { target_index: 0 })
            .unwrap();
        commands
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(
            error,
            "Runtime playback receiver transfer failed: scene receiver missing"
        );
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(
            owner.recording_commands,
            [OperatorRecordingAction::Start { target_index: 0 }]
        );
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert_eq!(
            command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoopEnabled { enabled: true })
        );
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "Runtime recording started",
                "Runtime playback receiver transfer failed: scene receiver missing",
                "recording journal retained",
            ]
        );
    }

    #[test]
    fn operator_recording_stop_failure_interrupts_command_drain() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_stop_terminal_output_failure: true,
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            automation_capture_poll: Some("unexpected automation poll".to_owned()),
            mixer_update_poll: Some("unexpected mixer-update poll".to_owned()),
            native_device_update_poll: Some("unexpected native-device poll".to_owned()),
            loop_update_poll: Some("unexpected loop-update poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands.send(OperatorCommand::StopRecording).unwrap();
        commands
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(error, "native audio ownership ended unexpectedly");
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(owner.recording_commands, [OperatorRecordingAction::Stop]);
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert!(owner.automation_capture_poll.is_some());
        assert!(owner.mixer_update_poll.is_some());
        assert!(owner.native_device_update_poll.is_some());
        assert!(owner.loop_update_poll.is_some());
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "Runtime recording stopped",
                "native audio ownership ended unexpectedly",
            ]
        );
    }

    #[test]
    fn operator_recording_start_output_failure_interrupts_command_drain() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            recording_start_output_state: Some(OutputStreamState::Failed),
            scene_performance_poll: Some("unexpected scene-performance poll".to_owned()),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());
        let (commands, command_receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        commands
            .send(OperatorCommand::StartRecording { target_index: 0 })
            .unwrap();
        commands
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();

        let error = host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            Some(&command_receiver),
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap_err();

        assert_eq!(error, "native audio ownership ended unexpectedly");
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(
            owner.recording_commands,
            [OperatorRecordingAction::Start { target_index: 0 }]
        );
        assert!(owner.commands.is_empty());
        assert!(owner.scene_performance_poll.is_some());
        assert_eq!(
            diagnostics.borrow().as_slice(),
            [
                "no panel link requested",
                "Runtime recording started",
                "native audio ownership ended unexpectedly",
            ]
        );
    }

    #[test]
    fn a_pre_requested_stop_skips_owner_work_and_settles_recording() {
        let arguments = RuntimeArguments::parse(["deployment".to_owned()]).unwrap();
        let stop = AtomicBool::new(true);
        let mut owner = OperatorControl {
            recording_poll: Some("unexpected owner turn".to_owned()),
            recording_abort: Some(Ok(Some("recording journal retained".to_owned()))),
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());

        host_runtime(
            &arguments,
            &mut owner,
            &OutputObservation(OutputStreamState::Running),
            &stop,
            None,
            &|message| diagnostics.borrow_mut().push(message),
        )
        .unwrap();

        assert_eq!(
            owner.recording_poll.as_deref(),
            Some("unexpected owner turn")
        );
        assert_eq!(
            diagnostics.into_inner(),
            ["no panel link requested", "recording journal retained"]
        );
    }

    #[derive(Default)]
    struct OperatorControl {
        commands: Vec<TransportCommand>,
        loudness_commands: Vec<LoudnessMeasurementCommand>,
        mixer_commands: Vec<MixerParameterCommand>,
        native_device_commands: Vec<NativeDeviceCommand>,
        input_monitor_gains: Vec<f32>,
        input_monitor_mutes: Vec<bool>,
        mixer_update_commands: usize,
        mixer_update_poll: Option<String>,
        native_device_update_commands: Vec<usize>,
        native_device_update_poll: Option<String>,
        loop_update_commands: usize,
        loop_update_poll: Option<String>,
        automation_capture_commands: Vec<OperatorAutomationCaptureAction>,
        automation_capture_poll: Option<String>,
        recording_commands: Vec<OperatorRecordingAction>,
        recording_poll: Option<String>,
        recording_poll_owner_failure: Option<String>,
        recording_poll_terminal_output_failure: bool,
        recording_poll_output_rebind: Option<Arc<Mutex<OutputStreamState>>>,
        recording_start_output_state: Option<OutputStreamState>,
        recording_start_owner_failure: Option<String>,
        recording_stop_terminal_output_failure: bool,
        recording_abort: Option<Result<Option<String>, String>>,
        scene_performance_commands: Vec<OperatorScenePerformanceAction>,
        scene_performance_poll: Option<String>,
        reject: bool,
        disconnects: usize,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum OperatorScenePerformanceAction {
        Start,
        Stop,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum OperatorAutomationCaptureAction {
        Start { target_index: usize, name: String },
        Stop,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum OperatorRecordingAction {
        Start { target_index: usize },
        Stop,
    }

    impl RuntimeOperatorControl for OperatorControl {
        fn queue_transport(&mut self, command: TransportCommand) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.commands.push(command);
                Ok(())
            }
        }

        fn queue_loudness(&mut self, command: LoudnessMeasurementCommand) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.loudness_commands.push(command);
                Ok(())
            }
        }

        fn queue_mixer(&mut self, command: MixerParameterCommand) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.mixer_commands.push(command);
                Ok(())
            }
        }

        fn queue_native_device(&mut self, command: NativeDeviceCommand) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.native_device_commands.push(command);
                Ok(())
            }
        }

        fn set_input_monitor_gain(&mut self, gain: f32) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.input_monitor_gains.push(gain);
                Ok(())
            }
        }

        fn set_input_monitor_muted(&mut self, muted: bool) -> Result<(), String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.input_monitor_mutes.push(muted);
                Ok(())
            }
        }
    }

    impl RuntimeScenePerformanceControl for OperatorControl {
        fn start_scene_performance_capture(&mut self) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.scene_performance_commands
                    .push(OperatorScenePerformanceAction::Start);
                Ok("scene-performance capture started".to_owned())
            }
        }

        fn stop_scene_performance_capture(&mut self) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.scene_performance_commands
                    .push(OperatorScenePerformanceAction::Stop);
                Ok("scene-performance capture stop requested".to_owned())
            }
        }

        fn poll_scene_performance_capture(&mut self) -> Option<String> {
            self.scene_performance_poll.take()
        }
    }

    impl RuntimeRecordingControl for OperatorControl {
        fn start_recording(
            &mut self,
            target_index: usize,
        ) -> Result<RuntimeRecordingControlOutcome, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.recording_commands
                    .push(OperatorRecordingAction::Start { target_index });
                Ok(RuntimeRecordingControlOutcome {
                    message: Some("Runtime recording started".to_owned()),
                    terminal_output_failure: matches!(
                        self.recording_start_output_state.take(),
                        Some(OutputStreamState::Failed)
                    ),
                    terminal_owner_failure: self.recording_start_owner_failure.take(),
                })
            }
        }

        fn stop_recording(&mut self) -> Result<RuntimeRecordingControlOutcome, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.recording_commands.push(OperatorRecordingAction::Stop);
                Ok(RuntimeRecordingControlOutcome {
                    message: Some("Runtime recording stopped".to_owned()),
                    terminal_output_failure: std::mem::take(
                        &mut self.recording_stop_terminal_output_failure,
                    ),
                    terminal_owner_failure: None,
                })
            }
        }

        fn poll_recording(&mut self) -> RuntimeRecordingControlOutcome {
            let message = self.recording_poll.take();
            if message.is_some()
                && let Some(output) = self.recording_poll_output_rebind.take()
            {
                *output.lock().unwrap() = OutputStreamState::Running;
            }
            RuntimeRecordingControlOutcome {
                message,
                terminal_output_failure: std::mem::take(
                    &mut self.recording_poll_terminal_output_failure,
                ),
                terminal_owner_failure: self.recording_poll_owner_failure.take(),
            }
        }

        fn abort_recording_for_shutdown(&mut self) -> Result<Option<String>, String> {
            self.recording_abort.take().unwrap_or(Ok(None))
        }
    }

    impl RuntimeAutomationCaptureControl for OperatorControl {
        fn start_automation_capture(
            &mut self,
            target_index: usize,
            name: String,
        ) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.automation_capture_commands
                    .push(OperatorAutomationCaptureAction::Start { target_index, name });
                Ok("Runtime automation capture started".to_owned())
            }
        }

        fn stop_automation_capture(&mut self) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.automation_capture_commands
                    .push(OperatorAutomationCaptureAction::Stop);
                Ok("Runtime automation capture stop requested".to_owned())
            }
        }

        fn poll_automation_capture(&mut self) -> Option<String> {
            self.automation_capture_poll.take()
        }
    }

    impl RuntimeMixerUpdateControl for OperatorControl {
        fn export_mixer_update(&mut self) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.mixer_update_commands = self.mixer_update_commands.saturating_add(1);
                Ok("mixer-update export queued".to_owned())
            }
        }

        fn poll_mixer_update_export(&mut self) -> Option<String> {
            self.mixer_update_poll.take()
        }
    }

    impl RuntimeNativeDeviceUpdateControl for OperatorControl {
        fn export_native_device_update(&mut self, chain_index: usize) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.native_device_update_commands.push(chain_index);
                Ok("native-device update export queued".to_owned())
            }
        }

        fn poll_native_device_update_export(&mut self) -> Option<String> {
            self.native_device_update_poll.take()
        }
    }

    impl RuntimeLoopUpdateControl for OperatorControl {
        fn export_loop_update(&mut self) -> Result<String, String> {
            if self.reject {
                Err("owner busy".to_owned())
            } else {
                self.loop_update_commands = self.loop_update_commands.saturating_add(1);
                Ok("loop-update export queued".to_owned())
            }
        }

        fn poll_loop_update_export(&mut self) -> Option<String> {
            self.loop_update_poll.take()
        }
    }

    impl PanelHost for OperatorControl {
        type Error = core::convert::Infallible;

        fn mixer_channel_count(&self) -> usize {
            0
        }

        fn connect_panel(
            &mut self,
            _descriptor: PanelDescriptor,
            _bindings: &[PanelBinding],
        ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
            panic!("the occupied-port test cannot accept a panel")
        }

        fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
            self.disconnects += 1;
            Vec::new()
        }

        fn panel_connected(&self) -> bool {
            false
        }

        fn dispatch_panel(
            &mut self,
            _message: Message,
        ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
            panic!("the occupied-port test cannot dispatch a panel")
        }

        fn panel_display(&mut self, _out: &mut Vec<PanelDisplay>) {}
    }

    #[test]
    fn input_monitor_commands_are_bounded_and_applied_by_the_owner() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender
            .send(OperatorCommand::SetInputMonitorGain {
                gain_millionths: 725_000,
            })
            .unwrap();
        sender
            .send(OperatorCommand::SetInputMonitorMuted { muted: false })
            .unwrap();
        sender
            .send(OperatorCommand::SetInputMonitorGain {
                gain_millionths: 1_000_001,
            })
            .unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert_eq!(owner.input_monitor_gains.len(), 1);
        assert_operator_float(owner.input_monitor_gains[0], 0.725);
        assert_eq!(owner.input_monitor_mutes, [false]);
        assert_eq!(
            diagnostics.borrow().last().map(String::as_str),
            Some("input monitor level refused: value is outside 0..=100%")
        );
        assert!(!stop.load(Ordering::Acquire));
    }

    struct OutputObservation(OutputStreamState);

    impl RuntimeOutputObservation for OutputObservation {
        fn output_state(&self) -> OutputStreamState {
            self.0
        }
    }

    #[derive(Clone)]
    struct RebindableOutputObservation(Arc<Mutex<OutputStreamState>>);

    impl RuntimeOutputObservation for RebindableOutputObservation {
        fn output_state(&self) -> OutputStreamState {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn runtime_shutdown_preserves_output_and_recording_outcomes() {
        let diagnostics = RefCell::new(Vec::new());
        let mut clean = OperatorControl {
            recording_abort: Some(Ok(Some("recording journal retained".to_owned()))),
            ..OperatorControl::default()
        };
        assert_eq!(
            finish_runtime_host(
                &mut clean,
                &OutputObservation(OutputStreamState::Running),
                false,
                None,
                &|message| diagnostics.borrow_mut().push(message),
            ),
            Ok(())
        );
        assert_eq!(
            diagnostics.borrow().as_slice(),
            ["recording journal retained"]
        );

        diagnostics.borrow_mut().clear();
        let mut recording_failed = OperatorControl {
            recording_abort: Some(Err("recording settlement failed".to_owned())),
            ..OperatorControl::default()
        };
        assert_eq!(
            finish_runtime_host(
                &mut recording_failed,
                &OutputObservation(OutputStreamState::Running),
                false,
                None,
                &|message| diagnostics.borrow_mut().push(message),
            ),
            Err("recording settlement failed".to_owned())
        );

        diagnostics.borrow_mut().clear();
        let mut output_failed = OperatorControl::default();
        assert_eq!(
            finish_runtime_host(
                &mut output_failed,
                &OutputObservation(OutputStreamState::Failed),
                false,
                None,
                &|message| diagnostics.borrow_mut().push(message),
            ),
            Err("native audio ownership ended unexpectedly".to_owned())
        );
        assert_eq!(
            diagnostics.borrow().as_slice(),
            ["native audio ownership ended unexpectedly"]
        );

        diagnostics.borrow_mut().clear();
        let mut both_failed = OperatorControl {
            recording_abort: Some(Err("recording settlement failed".to_owned())),
            ..OperatorControl::default()
        };
        assert_eq!(
            finish_runtime_host(
                &mut both_failed,
                &OutputObservation(OutputStreamState::Failed),
                false,
                None,
                &|message| diagnostics.borrow_mut().push(message),
            ),
            Err(
                "native audio ownership ended unexpectedly; recording shutdown also failed: recording settlement failed"
                    .to_owned()
            )
        );

        let mut owner_failed = OperatorControl {
            recording_abort: Some(Err("recording settlement failed".to_owned())),
            ..OperatorControl::default()
        };
        assert_eq!(
            finish_runtime_host(
                &mut owner_failed,
                &OutputObservation(OutputStreamState::Failed),
                false,
                Some("playback receiver transfer failed".to_owned()),
                &|_| {},
            ),
            Err("native audio ownership ended unexpectedly; recording shutdown also failed: recording settlement failed; playback receiver transfer failed".to_owned())
        );
    }

    #[test]
    fn operator_commands_reach_the_sole_transport_owner_in_order() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Start).unwrap();
        sender.send(OperatorCommand::Stop).unwrap();
        sender.send(OperatorCommand::ReturnToStart).unwrap();
        sender
            .send(OperatorCommand::Locate {
                frame: 960,
                state: LocateState::Rolling,
            })
            .unwrap();
        sender
            .send(OperatorCommand::LaunchScene { scene_index: 3 })
            .unwrap();
        sender.send(OperatorCommand::StopInteractiveScenes).unwrap();
        sender
            .send(OperatorCommand::SetLoopEnabled { enabled: true })
            .unwrap();
        sender
            .send(OperatorCommand::SetLoudnessState {
                state: LoudnessMeasurementState::Standby,
            })
            .unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });
        sender
            .send(OperatorCommand::StartScenePerformanceCapture)
            .unwrap();
        sender
            .send(OperatorCommand::StopScenePerformanceCapture)
            .unwrap();
        sender.send(OperatorCommand::ExportMixerUpdate).unwrap();
        sender
            .send(OperatorCommand::ExportNativeDeviceUpdate { chain_index: 0 })
            .unwrap();
        sender.send(OperatorCommand::ExportLoopUpdate).unwrap();
        sender.send(OperatorCommand::ResetLoudness).unwrap();
        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert_eq!(
            owner.commands,
            [
                TransportCommand::Start,
                TransportCommand::Stop,
                TransportCommand::Locate {
                    frame: 0,
                    state: LocateState::Preserve,
                },
                TransportCommand::Locate {
                    frame: 960,
                    state: LocateState::Rolling,
                },
                TransportCommand::LaunchScene { scene_index: 3 },
                TransportCommand::StopInteractiveScenes,
                TransportCommand::SetLoopEnabled(true),
            ]
        );
        assert_eq!(
            owner.loudness_commands,
            [
                LoudnessMeasurementCommand::SetState(LoudnessMeasurementState::Standby),
                LoudnessMeasurementCommand::Reset,
            ]
        );
        assert_eq!(
            owner.scene_performance_commands,
            [
                OperatorScenePerformanceAction::Start,
                OperatorScenePerformanceAction::Stop,
            ]
        );
        assert_eq!(owner.mixer_update_commands, 1);
        assert_eq!(owner.native_device_update_commands, [0]);
        assert_eq!(owner.loop_update_commands, 1);
        assert_eq!(
            diagnostics.into_inner(),
            [
                "transport start queued",
                "transport stop queued",
                "transport return to start queued",
                "transport play to frame 960 queued",
                "scene 4 launch queued",
                "interactive scene stop queued",
                "session loop enable queued",
                "programme loudness standby queued",
                "scene-performance capture started",
                "scene-performance capture stop requested",
                "mixer-update export queued",
                "native-device update export queued",
                "loop-update export queued",
                "programme loudness reset queued",
            ]
        );
        assert!(!stop.load(Ordering::Acquire));
    }

    #[test]
    fn operator_automation_capture_commands_reach_the_sole_owner() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender
            .send(OperatorCommand::StartAutomationCapture {
                target_index: 2,
                name: "Runtime pass".to_owned(),
            })
            .unwrap();
        sender.send(OperatorCommand::StopAutomationCapture).unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert_eq!(
            owner.automation_capture_commands,
            [
                OperatorAutomationCaptureAction::Start {
                    target_index: 2,
                    name: "Runtime pass".to_owned(),
                },
                OperatorAutomationCaptureAction::Stop,
            ]
        );
        assert_eq!(
            diagnostics.into_inner(),
            [
                "Runtime automation capture started",
                "Runtime automation capture stop requested",
            ]
        );
        assert!(!stop.load(Ordering::Acquire));
    }

    #[test]
    fn operator_scene_performance_controls_follow_committed_runtime_observation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let observation = test_scene_performance_observation();
        wire_operator_scene_performance_capture(&ui, &sender, &diagnostic, &observation);
        apply_operator_scene_performance_observation(&ui, observation.as_ref(), false);

        ui.invoke_start_scene_performance_capture_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "scene-performance capture start refused: committed Runtime capture state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeScenePerformanceCaptureObservation {
            state: "Ready to capture a scene performance".to_owned(),
            start_enabled: true,
            stop_enabled: false,
        };
        apply_operator_scene_performance_observation(&ui, observation.as_ref(), false);
        assert_eq!(
            ui.get_scene_performance_state().as_str(),
            "Ready to capture a scene performance"
        );
        assert!(ui.get_scene_performance_start_enabled());
        assert!(!ui.get_scene_performance_stop_enabled());

        ui.invoke_start_scene_performance_capture_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StartScenePerformanceCapture)
        );
        assert!(
            ui.get_scene_performance_start_enabled(),
            "the UI does not claim that the queued request was committed"
        );

        *observation.lock().unwrap() = RuntimeScenePerformanceCaptureObservation {
            state: "Capturing 0 committed launch(es)".to_owned(),
            start_enabled: false,
            stop_enabled: true,
        };
        apply_operator_scene_performance_observation(&ui, observation.as_ref(), false);
        assert!(!ui.get_scene_performance_start_enabled());
        assert!(ui.get_scene_performance_stop_enabled());

        ui.invoke_stop_scene_performance_capture_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StopScenePerformanceCapture)
        );
        assert!(
            ui.get_scene_performance_stop_enabled(),
            "the UI retains the committed presentation until Runtime publishes another one"
        );

        revoke_operator_scene_performance_authority(&ui);
        ui.invoke_stop_scene_performance_capture_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "scene-performance capture stop refused: committed Runtime capture state is unavailable"
        );
    }

    #[test]
    fn operator_automation_capture_controls_follow_committed_runtime_observation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        ui.set_automation_target_names(ModelRc::from(Rc::new(VecModel::from(vec![
            SharedString::from("Track / Gain"),
        ]))));
        ui.set_selected_automation_target_index(0);
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let observation = test_automation_capture_observation();
        wire_operator_automation_capture(&ui, &sender, &diagnostic, &observation, 1);
        apply_operator_automation_capture_observation(&ui, observation.as_ref(), false);

        ui.invoke_start_automation_capture_requested(0, "Runtime pass".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime automation capture start refused: committed Runtime capture state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeAutomationCaptureObservation {
            state: "Ready to capture a Runtime automation take".to_owned(),
            start_enabled: true,
            stop_enabled: false,
        };
        apply_operator_automation_capture_observation(&ui, observation.as_ref(), false);
        assert!(ui.get_automation_capture_start_enabled());
        assert!(!ui.get_automation_capture_stop_enabled());

        ui.invoke_start_automation_capture_requested(0, "Runtime pass".into());
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StartAutomationCapture {
                target_index: 0,
                name: "Runtime pass".to_owned(),
            })
        );
        assert!(
            ui.get_automation_capture_start_enabled(),
            "the UI retains committed Runtime state until the owner publishes another observation"
        );

        ui.invoke_start_automation_capture_requested(-1, "Runtime pass".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime automation capture start refused: selected target is unavailable"
        );

        *observation.lock().unwrap() = RuntimeAutomationCaptureObservation {
            state: "Capturing 0 committed parameter change(s)".to_owned(),
            start_enabled: false,
            stop_enabled: true,
        };
        apply_operator_automation_capture_observation(&ui, observation.as_ref(), false);
        assert!(!ui.get_automation_capture_start_enabled());
        assert!(ui.get_automation_capture_stop_enabled());

        ui.invoke_stop_automation_capture_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StopAutomationCapture)
        );

        revoke_operator_automation_capture_authority(&ui);
        ui.invoke_stop_automation_capture_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime automation capture stop refused: committed Runtime capture state is unavailable"
        );
    }

    #[test]
    fn operator_recording_controls_follow_owner_observation_and_bound_targets() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        ui.set_recording_target_names(ModelRc::from(Rc::new(VecModel::from(vec![
            SharedString::from("Vocal"),
        ]))));
        ui.set_selected_recording_target_index(0);
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let observation = test_recording_observation();
        wire_operator_recording(&ui, &sender, &diagnostic, &observation, 1);
        apply_operator_recording_observation(&ui, observation.as_ref(), false);

        ui.invoke_start_recording_requested(0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime recording start refused: committed recording state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeRecordingObservation {
            state: "Ready to record a verified audio track".to_owned(),
            start_enabled: true,
            stop_enabled: false,
        };
        apply_operator_recording_observation(&ui, observation.as_ref(), false);
        assert!(ui.get_recording_start_enabled());
        assert!(!ui.get_recording_stop_enabled());

        ui.invoke_start_recording_requested(0);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StartRecording { target_index: 0 })
        );
        ui.invoke_start_recording_requested(-1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime recording start refused: selected target is unavailable"
        );

        *observation.lock().unwrap() = RuntimeRecordingObservation {
            state: "Recording Vocal · 1024 accepted frame(s)".to_owned(),
            start_enabled: false,
            stop_enabled: true,
        };
        apply_operator_recording_observation(&ui, observation.as_ref(), false);
        assert!(!ui.get_recording_start_enabled());
        assert!(ui.get_recording_stop_enabled());
        ui.invoke_stop_recording_requested();
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::StopRecording));

        revoke_operator_recording_authority(&ui);
        ui.invoke_stop_recording_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "Runtime recording stop refused: committed recording state is unavailable"
        );
    }

    #[test]
    fn mixer_update_export_control_follows_committed_runtime_observation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let observation = test_mixer_update_observation();
        wire_operator_mixer_update_export(&ui, &sender, &diagnostic, &observation);
        apply_operator_mixer_update_observation(&ui, observation.as_ref(), false);

        ui.invoke_export_mixer_update_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "mixer-update export refused: committed Runtime mixer state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeMixerUpdateObservation {
            state: "Ready to export the committed mixer state".to_owned(),
            export_enabled: true,
        };
        apply_operator_mixer_update_observation(&ui, observation.as_ref(), false);
        assert_eq!(
            ui.get_mixer_update_state().as_str(),
            "Ready to export the committed mixer state"
        );
        assert!(ui.get_mixer_update_export_enabled());

        ui.invoke_export_mixer_update_requested();
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::ExportMixerUpdate));
        assert!(
            ui.get_mixer_update_export_enabled(),
            "the UI retains the committed presentation until Runtime publishes another one"
        );

        revoke_operator_mixer_update_authority(&ui);
        ui.invoke_export_mixer_update_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "mixer-update export refused: committed Runtime mixer state is unavailable"
        );
    }

    #[test]
    fn native_device_update_export_is_chain_bounded_and_recovery_gated() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let chains = vec![
            OperatorNativeDeviceChain {
                track_label: "Channel 1 · Filter bus".to_owned(),
                device_count: 1,
            },
            OperatorNativeDeviceChain {
                track_label: "Channel 3 · Master bus".to_owned(),
                device_count: 2,
            },
        ];
        let model = Rc::new(VecModel::from(operator_native_device_chain_rows(&chains)));
        ui.set_native_device_chains(ModelRc::from(Rc::clone(&model)));
        assert_eq!(
            model.row_data(0).unwrap().track_label.as_str(),
            "Channel 1 · Filter bus"
        );
        assert_eq!(model.row_data(1).unwrap().device_count, 2);

        let observation = test_native_device_update_observation();
        wire_operator_native_device_update_export(&ui, &sender, &diagnostic, &chains, &observation);
        apply_operator_native_device_update_observation(&ui, observation.as_ref(), false);

        ui.invoke_export_native_device_update_requested(0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "native-device update export refused: committed Runtime native-device state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeNativeDeviceUpdateObservation {
            state: "Ready to export one committed native-device chain".to_owned(),
            export_enabled: true,
        };
        apply_operator_native_device_update_observation(&ui, observation.as_ref(), false);
        assert_eq!(
            ui.get_native_device_update_state().as_str(),
            "Ready to export one committed native-device chain"
        );
        assert!(ui.get_native_device_update_export_enabled());

        ui.invoke_export_native_device_update_requested(-1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "native-device update export refused: selected chain is unavailable"
        );
        ui.invoke_export_native_device_update_requested(2);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "native-device update export refused: selected chain is unavailable"
        );

        ui.invoke_export_native_device_update_requested(1);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::ExportNativeDeviceUpdate { chain_index: 1 })
        );
        assert!(
            ui.get_native_device_update_export_enabled(),
            "the UI retains the committed presentation until Runtime publishes another one"
        );

        revoke_operator_native_device_update_authority(&ui);
        ui.invoke_export_native_device_update_requested(0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "native-device update export refused: committed Runtime native-device state is unavailable"
        );
    }

    #[test]
    fn loop_update_export_control_follows_committed_runtime_observation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let observation = test_loop_update_observation();
        wire_operator_loop_update_export(&ui, &sender, &diagnostic, &observation);
        apply_operator_loop_update_observation(&ui, observation.as_ref(), false);

        ui.invoke_export_loop_update_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "loop-update export refused: committed Runtime loop state is unavailable"
        );

        *observation.lock().unwrap() = RuntimeLoopUpdateObservation {
            state: "Ready to export the committed loop state".to_owned(),
            export_enabled: true,
        };
        apply_operator_loop_update_observation(&ui, observation.as_ref(), false);
        assert_eq!(
            ui.get_loop_update_state().as_str(),
            "Ready to export the committed loop state"
        );
        assert!(ui.get_loop_update_export_enabled());

        ui.invoke_export_loop_update_requested();
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::ExportLoopUpdate));
        assert!(
            ui.get_loop_update_export_enabled(),
            "the UI retains the committed presentation until Runtime publishes another one"
        );

        revoke_operator_loop_update_authority(&ui);
        ui.invoke_export_loop_update_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "loop-update export refused: committed Runtime loop state is unavailable"
        );
    }

    #[test]
    fn operator_seek_conversion_and_owner_delivery_are_bounded_and_exact() {
        assert_eq!(operator_seek_frame(1_500, 0.0), Some(0));
        assert_eq!(operator_seek_frame(1_500, 50.0), Some(750));
        assert_eq!(operator_seek_frame(1_500, 100.0), Some(1_500));
        assert_eq!(operator_seek_frame(3, 50.0), Some(2));
        assert_eq!(operator_seek_frame(usize::MAX, 100.0), Some(usize::MAX));
        assert_eq!(
            operator_seek_frame(usize::MAX, 50.0),
            Some(usize::MAX / 2 + 1)
        );
        assert_operator_float(operator_seek_percent(0, 1_500), 0.0);
        assert_operator_float(operator_seek_percent(1_500, 1_500), 100.0);
        assert_operator_float(operator_seek_percent(1_501, 1_500), 100.0);
        for invalid in [f32::NAN, f32::INFINITY, -0.1, 100.1] {
            assert_eq!(operator_seek_frame(1_500, invalid), None);
        }
        assert_eq!(operator_seek_frame(0, 50.0), None);

        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .send(OperatorCommand::Locate {
                frame: 751,
                state: LocateState::Preserve,
            })
            .unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostic = RefCell::new(String::new());
        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            *diagnostic.borrow_mut() = message;
        });
        assert_eq!(
            owner.commands,
            [TransportCommand::Locate {
                frame: 751,
                state: LocateState::Preserve,
            }]
        );
        assert_eq!(
            diagnostic.into_inner(),
            "transport locate to frame 751 queued"
        );
    }

    #[test]
    fn operator_timeline_marker_navigation_is_identity_safe_and_recovery_gated() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let timeline = test_musical_timeline();
        let markers: Arc<[OperatorTimelineMarker]> = Arc::from([
            operator_timeline_marker(
                "marker-intro".to_owned(),
                "Intro".to_owned(),
                960,
                &timeline,
            ),
            OperatorTimelineMarker {
                id: "marker-unavailable".to_owned(),
                name: "Far away".to_owned(),
                tick: u64::MAX,
                musical_position: None,
                frame: None,
            },
        ]);
        let model = Rc::new(VecModel::from(operator_timeline_marker_rows(&markers)));
        let authority = Rc::new(RefCell::new(None));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        ui.set_timeline_markers(ModelRc::from(Rc::clone(&model)));
        ui.set_transport_frame("0".into());
        wire_operator_timeline_marker_navigation(&ui, &sender, &diagnostic, &authority);

        ui.invoke_locate_timeline_marker_requested("marker-intro".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker locate refused: verified marker navigation is unavailable"
        );

        apply_operator_timeline_marker_observation(&ui, &model, &authority, &markers);
        assert_eq!(
            ui.get_timeline_marker_navigation_state().as_str(),
            "Locate preserves recovered state · Play starts"
        );
        assert_eq!(model.row_data(0).unwrap().id.as_str(), "marker-intro");
        assert_eq!(model.row_data(0).unwrap().name.as_str(), "Intro");
        assert_eq!(model.row_data(0).unwrap().tick.as_str(), "960");
        assert_eq!(
            model.row_data(0).unwrap().musical_position.as_str(),
            "1|1|960"
        );
        assert!(model.row_data(0).unwrap().available);
        assert!(model.row_data(0).unwrap().controls_enabled);
        assert!(!model.row_data(1).unwrap().available);
        assert!(!model.row_data(1).unwrap().controls_enabled);

        ui.invoke_locate_timeline_marker_requested("stale-marker".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker locate refused: marker identity is not current"
        );
        ui.invoke_locate_timeline_marker_requested("marker-unavailable".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker locate refused: marker cannot be represented by this Runtime"
        );

        ui.invoke_locate_timeline_marker_requested("marker-intro".into());
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::Locate {
                frame: 12_000,
                state: LocateState::Preserve,
            })
        );
        assert_eq!(ui.get_transport_frame().as_str(), "0");

        let (saturated_sender, saturated_receiver) = std::sync::mpsc::sync_channel(1);
        saturated_sender.send(OperatorCommand::Quit).unwrap();
        let saturated_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_timeline_marker_navigation(
            &ui,
            &saturated_sender,
            &saturated_diagnostic,
            &authority,
        );
        ui.invoke_locate_timeline_marker_requested("marker-intro".into());
        assert_eq!(saturated_receiver.try_recv(), Ok(OperatorCommand::Quit));
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            saturated_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_eq!(ui.get_transport_frame().as_str(), "0");

        revoke_operator_timeline_marker_authority(&ui, &model, &authority);
        assert!(authority.borrow().is_none());
        assert!(!model.row_data(0).unwrap().controls_enabled);
        assert_eq!(
            ui.get_timeline_marker_navigation_state().as_str(),
            "Runtime stopped · marker navigation unavailable"
        );
        ui.invoke_locate_timeline_marker_requested("marker-intro".into());
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            saturated_diagnostic.lock().unwrap().as_str(),
            "timeline marker locate refused: verified marker navigation is unavailable"
        );
    }

    #[test]
    fn operator_timeline_marker_play_uses_rolling_locate_and_fails_closed() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let timeline = test_musical_timeline();
        let markers: Arc<[OperatorTimelineMarker]> = Arc::from([
            operator_timeline_marker(
                "marker-intro".to_owned(),
                "Intro".to_owned(),
                960,
                &timeline,
            ),
            OperatorTimelineMarker {
                id: "marker-unavailable".to_owned(),
                name: "Far away".to_owned(),
                tick: u64::MAX,
                musical_position: None,
                frame: None,
            },
        ]);
        let model = Rc::new(VecModel::from(operator_timeline_marker_rows(&markers)));
        let authority = Rc::new(RefCell::new(None));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let diagnostic = Arc::new(Mutex::new(String::new()));
        ui.set_timeline_markers(ModelRc::from(Rc::clone(&model)));
        ui.set_transport_frame("0".into());
        ui.set_transport_state("Stopped".into());
        wire_operator_timeline_marker_navigation(&ui, &sender, &diagnostic, &authority);

        ui.invoke_play_timeline_marker_requested("marker-intro".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker play refused: verified marker navigation is unavailable"
        );

        apply_operator_timeline_marker_observation(&ui, &model, &authority, &markers);
        ui.invoke_play_timeline_marker_requested("stale-marker".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker play refused: marker identity is not current"
        );
        ui.invoke_play_timeline_marker_requested("marker-unavailable".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker play refused: marker cannot be represented by this Runtime"
        );

        ui.invoke_play_timeline_marker_requested("marker-intro".into());
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::Locate {
                frame: 12_000,
                state: LocateState::Rolling,
            })
        );
        assert_eq!(ui.get_transport_frame().as_str(), "0");
        assert_eq!(ui.get_transport_state().as_str(), "Stopped");

        let (saturated_sender, saturated_receiver) = std::sync::mpsc::sync_channel(1);
        saturated_sender.send(OperatorCommand::Quit).unwrap();
        let saturated_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_timeline_marker_navigation(
            &ui,
            &saturated_sender,
            &saturated_diagnostic,
            &authority,
        );
        ui.invoke_play_timeline_marker_requested("marker-intro".into());
        assert_eq!(saturated_receiver.try_recv(), Ok(OperatorCommand::Quit));
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            saturated_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );

        revoke_operator_timeline_marker_authority(&ui, &model, &authority);
        ui.invoke_play_timeline_marker_requested("marker-intro".into());
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            saturated_diagnostic.lock().unwrap().as_str(),
            "timeline marker play refused: verified marker navigation is unavailable"
        );
    }

    #[test]
    fn terminal_and_failed_runtime_observations_revoke_timeline_marker_navigation() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let timeline = test_musical_timeline();
        let markers: Arc<[OperatorTimelineMarker]> = Arc::from([operator_timeline_marker(
            "marker-intro".to_owned(),
            "Intro".to_owned(),
            960,
            &timeline,
        )]);
        let observers = test_timeline_marker_observers(timeline, Arc::clone(&markers));
        let marker_model = Rc::new(VecModel::from(operator_timeline_marker_rows(&markers)));
        let marker_authority = Rc::new(RefCell::new(None));
        let scene_model = Rc::new(VecModel::from(Vec::<OperatorSceneRow>::new()));
        let transport_authority = Rc::new(Cell::new(None));
        let loop_authority = Rc::new(Cell::new(None));
        let metronome_authority = Rc::new(Cell::new(None));
        let seek_authority = Rc::new(Cell::new(None));
        let scene_launch_authority = Rc::new(Cell::new(false));
        let scene_selection = Rc::new(Cell::new(None));
        let scene_stop_authority = Rc::new(Cell::new(false));
        let diagnostic = Arc::new(Mutex::new(String::new()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);

        ui.set_timeline_markers(ModelRc::from(Rc::clone(&marker_model)));
        wire_operator_timeline_marker_navigation(&ui, &sender, &diagnostic, &marker_authority);
        let apply = |output_state, terminal_latched| {
            apply_operator_cycle_observation(
                &ui,
                &observers,
                output_state,
                &diagnostic,
                &transport_authority,
                &loop_authority,
                &metronome_authority,
                &seek_authority,
                &marker_model,
                &marker_authority,
                &scene_model,
                &scene_launch_authority,
                &scene_selection,
                &scene_stop_authority,
                terminal_latched,
            );
        };
        apply(observers.audio.snapshot().output_status.state(), false);
        assert!(marker_authority.borrow().is_some());
        assert!(marker_model.row_data(0).unwrap().controls_enabled);
        assert!(!ui.get_transport_seek_enabled());
        assert_eq!(
            ui.get_timeline_marker_navigation_state().as_str(),
            "Locate preserves recovered state · Play starts"
        );

        apply(observers.audio.snapshot().output_status.state(), true);

        assert!(marker_authority.borrow().is_none());
        assert!(!marker_model.row_data(0).unwrap().controls_enabled);
        assert_eq!(
            ui.get_timeline_marker_navigation_state().as_str(),
            "Runtime stopped · marker navigation unavailable"
        );

        apply_operator_timeline_marker_observation(&ui, &marker_model, &marker_authority, &markers);
        assert!(marker_authority.borrow().is_some());
        assert!(marker_model.row_data(0).unwrap().controls_enabled);

        apply(OutputStreamState::Failed, false);

        assert!(marker_authority.borrow().is_none());
        assert!(!marker_model.row_data(0).unwrap().controls_enabled);
        assert_eq!(ui.get_stream_state().as_str(), "Failed");
        assert_eq!(
            ui.get_diagnostic().as_str(),
            "native audio ownership ended unexpectedly"
        );
        ui.invoke_play_timeline_marker_requested("marker-intro".into());
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "timeline marker play refused: verified marker navigation is unavailable"
        );
    }

    #[test]
    fn operator_loop_marks_derive_exact_positive_session_ranges() {
        let bounded = PlaybackExtent::Bounded { end_frame: 1_500 };
        let authority = |range, position_frame, extent| OperatorLoopAuthority {
            range,
            enabled: false,
            position_frame,
            extent,
        };
        assert_eq!(
            operator_loop_range_at_playhead(authority(None, 375, bounded), OperatorLoopMark::In),
            Some(TransportLoopRange::new(375, 1_500).unwrap())
        );
        assert_eq!(
            operator_loop_range_at_playhead(authority(None, 1_200, bounded), OperatorLoopMark::Out),
            Some(TransportLoopRange::new(0, 1_200).unwrap())
        );
        let wide_bounded = PlaybackExtent::Bounded { end_frame: 5_000 };
        let existing = Some(TransportLoopRange::new(500, 4_000).unwrap());
        assert_eq!(
            operator_loop_range_at_playhead(
                authority(existing, 2_000, wide_bounded),
                OperatorLoopMark::In
            ),
            Some(TransportLoopRange::new(2_000, 4_000).unwrap())
        );
        assert_eq!(
            operator_loop_range_at_playhead(
                authority(existing, 2_000, wide_bounded),
                OperatorLoopMark::Out
            ),
            Some(TransportLoopRange::new(500, 2_000).unwrap())
        );
        assert_eq!(
            operator_loop_range_at_playhead(
                authority(None, 10, PlaybackExtent::Unbounded),
                OperatorLoopMark::In
            ),
            None
        );
        assert_eq!(
            operator_loop_range_at_playhead(
                authority(None, 1_024, PlaybackExtent::Unbounded),
                OperatorLoopMark::Out
            ),
            Some(TransportLoopRange::new(0, 1_024).unwrap())
        );
        assert_eq!(
            operator_loop_range_at_playhead(authority(None, 0, bounded), OperatorLoopMark::Out),
            None
        );
        assert_eq!(
            operator_loop_range_at_playhead(authority(None, 1_500, bounded), OperatorLoopMark::In),
            None
        );
        assert_eq!(operator_runtime_loop_range(0, 1_023), None);
        assert_eq!(
            operator_runtime_loop_range(0, 1_024),
            Some(TransportLoopRange::new(0, 1_024).unwrap())
        );

        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        sender
            .send(OperatorCommand::SetLoopRange {
                range: Some(TransportLoopRange::new(375, 900).unwrap()),
            })
            .unwrap();
        sender
            .send(OperatorCommand::SetLoopRange { range: None })
            .unwrap();
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        let diagnostics = RefCell::new(Vec::new());
        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });
        assert_eq!(
            owner.commands,
            [
                TransportCommand::SetLoopRange(Some(TransportLoopRange::new(375, 900).unwrap())),
                TransportCommand::SetLoopRange(None),
            ]
        );
        assert_eq!(
            diagnostics.into_inner(),
            [
                "session loop range 375–900 queued",
                "session loop range clear queued",
            ]
        );
    }

    #[test]
    fn a_refused_operator_command_is_visible_and_a_lost_ui_stops_the_worker() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        sender.send(OperatorCommand::Start).unwrap();
        drop(sender);
        let stop = AtomicBool::new(false);
        let mut owner = OperatorControl {
            reject: true,
            ..OperatorControl::default()
        };
        let diagnostics = RefCell::new(Vec::new());

        drain_operator_commands(&mut owner, &receiver, &stop, &|message| {
            diagnostics.borrow_mut().push(message);
        });

        assert!(owner.commands.is_empty());
        assert_eq!(
            diagnostics.into_inner(),
            ["transport start refused: owner busy"]
        );
        assert!(
            stop.load(Ordering::Acquire),
            "losing the UI command owner cannot leave a hidden Runtime behind"
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one linear fixture keeps the real mixer-to-generated-window evidence auditable"
    )]
    fn assert_generated_operator_meter_contract(ui: &OperatorWindow) {
        const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339500";
        const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122339504";
        const TIMELINE: &[u8] = br#"{
            "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339501",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","ticks_per_quarter":1920,
            "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
            "meter_events":[{"tick":0,"numerator":4,"denominator":4}],"extensions":{}
        }"#;
        const TRACK: &[u8] = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339502",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Meter source","kind":"audio",
            "order":0,"active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122339503"],"extensions":{}
        }"#;
        const SCENE_TRACK: &[u8] = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339508",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Scene group","kind":"group",
            "order":1,"active":true,"clip_ids":[],"extensions":{}
        }"#;
        const RETURN_TRACK: &[u8] = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-00112233950a",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Ritorno \u03a9","kind":"return",
            "order":2,"active":true,"clip_ids":[],"extensions":{}
        }"#;
        const SECOND_RETURN_TRACK: &[u8] = br#"{
            "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-00112233950c",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Return Delta","kind":"return",
            "order":3,"active":true,"clip_ids":[],"extensions":{}
        }"#;
        const CLIP: &[u8] = br#"{
            "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339503",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","track_id":"018f47a2-6b2d-7c11-8a44-001122339502",
            "name":"Meter clip","enabled":true,"duration_ticks":60,
            "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122339504","start_frame":0,"frame_count":1500},
            "loop":{"enabled":false,"start_tick":0,"end_tick":60},
            "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
            "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
            "warp_markers":[{"tick":0,"source_frame_offset":0},{"tick":60,"source_frame_offset":1500}],
            "extensions":{}
        }"#;
        const ARRANGEMENT: &[u8] = br#"{
            "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339505",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Main","enabled":true,
            "placements":[{"id":"018f47a2-6b2d-7c11-8a44-001122339506","track_id":"018f47a2-6b2d-7c11-8a44-001122339502",
            "clip_id":"018f47a2-6b2d-7c11-8a44-001122339503","start_tick":0,"clip_start_tick":0,"duration_ticks":60,"enabled":true}],
            "scene_launches":[],"extensions":{}
        }"#;
        const SCENE: &[u8] = br#"{
            "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339509",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500","name":"Stop layer","order":0,"enabled":true,
            "launch_quantization":{"mode":"immediate"},
            "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122339508","action":"stop"}],"extensions":{}
        }"#;
        const MIXER: &[u8] = br#"{
            "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122339507",
            "project_id":"018f47a2-6b2d-7c11-8a44-001122339500",
            "channels":[
            {"track_id":"018f47a2-6b2d-7c11-8a44-001122339502","gain_millionths":1000000,
            "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},
            "sends":[{"id":"018f47a2-6b2d-7c11-8a44-00112233950b","bus":"a","tap":"post-fader",
            "gain_millionths":800000,"pan_millionths":-300000,"enabled":true},
            {"id":"018f47a2-6b2d-7c11-8a44-00112233950d","bus":"b","tap":"pre-fader",
            "gain_millionths":400000,"pan_millionths":600000,"enabled":false}]},
            {"track_id":"018f47a2-6b2d-7c11-8a44-001122339508","gain_millionths":1000000,
            "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},
            "sends":[{"id":"018f47a2-6b2d-7c11-8a44-00112233950e","bus":"a","tap":"pre-fader",
            "gain_millionths":1250000,"pan_millionths":-700000,"enabled":true}]},
            {"track_id":"018f47a2-6b2d-7c11-8a44-00112233950a","gain_millionths":1000000,
            "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]},
            {"track_id":"018f47a2-6b2d-7c11-8a44-00112233950c","gain_millionths":1000000,
            "pan_millionths":0,"mute":false,"solo":false,"destination":{"kind":"main"},"sends":[]}],
            "return_buses":[
            {"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-00112233950a"},
            {"bus":"b","track_id":"018f47a2-6b2d-7c11-8a44-00112233950c"}],
            "main":{"gain_millionths":1000000,"mute":false},
            "cue":{"source":"main","gain_millionths":1000000,"mute":false},"extensions":{}
        }"#;

        let mut left = vec![0.5_f32; 480];
        left.extend([0.05; 480]);
        left.extend([2.0; 540]);
        let mut right = vec![0.25_f32; 480];
        right.extend([0.025; 480]);
        right.extend([1.5; 540]);
        let session = assemble_playback_session(PlaybackSessionAssemblyInput {
            documents: PlaybackDocumentBytes {
                project_id: PROJECT_ID.to_owned(),
                sample_rate_hz: SAMPLE_RATE_HZ,
                timeline: TIMELINE.to_vec(),
                tracks: vec![
                    TRACK.to_vec(),
                    SCENE_TRACK.to_vec(),
                    RETURN_TRACK.to_vec(),
                    SECOND_RETURN_TRACK.to_vec(),
                ],
                clips: vec![CLIP.to_vec()],
                scenes: vec![SCENE.to_vec()],
                arrangement: Some(ARRANGEMENT.to_vec()),
                mixer: MIXER.to_vec(),
                automation_takes: Vec::new(),
            },
            decoded_assets: vec![DecodedPlaybackAsset::new(
                ASSET_ID.to_owned(),
                DecodedAudio::from_stereo(SAMPLE_RATE_HZ, left, right).unwrap(),
            )],
            limits: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: 1,
                    max_frames_per_asset: 1_500,
                    max_decoded_sample_bytes: 12_000,
                },
            },
        })
        .unwrap();
        let (project, mixer, _, media) = session.parts();
        let runtime_musical_timeline = RuntimeMusicalTimeline::try_new(
            project.sample_rate_hz(),
            project.tempo_events(),
            project.meter_events(),
        )
        .unwrap();
        let facts: Vec<_> = media.media_asset_facts().collect();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap();
        let prepared = media.prepare_playback(&graph).unwrap();
        let runtime =
            PreparedPlaybackMainRuntime::try_new_prepared_playback(&prepared, mixer, 480).unwrap();
        let (mut transport_sender, mut mixer_sender, mut transport) =
            PreparedPlaybackTransportRuntime::with_command_queues(runtime);
        let initial_mixer_state = transport.mixer_parameter_state();
        let mut callback = InterleavedStereoCallback::try_new(480, SAMPLE_RATE_HZ).unwrap();
        let mut scratch_left = [0.0_f32; 480];
        let mut scratch_right = [0.0_f32; 480];
        let mut scene_scratch_left = [0.0_f32; 480];
        let mut scene_scratch_right = [0.0_f32; 480];
        let mut return_scratch_left = [0.0_f32; 480];
        let mut return_scratch_right = [0.0_f32; 480];
        let mut second_return_scratch_left = [0.0_f32; 480];
        let mut second_return_scratch_right = [0.0_f32; 480];
        let mut scratch = [
            RenderScratchSlot::new(&mut scratch_left, &mut scratch_right),
            RenderScratchSlot::new(&mut scene_scratch_left, &mut scene_scratch_right),
            RenderScratchSlot::new(&mut return_scratch_left, &mut return_scratch_right),
            RenderScratchSlot::new(
                &mut second_return_scratch_left,
                &mut second_return_scratch_right,
            ),
        ];
        let mut interleaved = [0.0_f32; 960];
        let mut operator_scenes = build_operator_scene_catalog(
            transport_sender.scene_count(),
            |index| session.arrangement().scene_name(index).map(str::to_owned),
            |index| {
                transport_sender
                    .scene_availability(index)
                    .map(DesktopSceneAvailability::from)
            },
        )
        .unwrap();
        assert_eq!(
            operator_scenes,
            [OperatorScene {
                index: 0,
                name: "Stop layer".to_owned(),
                availability: DesktopSceneAvailability::Ready,
            }]
        );
        operator_scenes.extend([
            OperatorScene {
                index: 1,
                name: "Disabled scene".to_owned(),
                availability: DesktopSceneAvailability::Disabled,
            },
            OperatorScene {
                index: 2,
                name: "Desktop instrument".to_owned(),
                availability: DesktopSceneAvailability::ConsolidationRequired,
            },
            OperatorScene {
                index: 3,
                name: "Missing audio".to_owned(),
                availability: DesktopSceneAvailability::NotPrepared,
            },
        ]);
        let scene_model = Rc::new(VecModel::from(operator_scene_rows(&operator_scenes)));
        ui.set_scenes(ModelRc::from(Rc::clone(&scene_model)));
        let (scene_commands, scene_command_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let scene_diagnostic = Arc::new(Mutex::new(String::new()));
        let scene_launch_authority = Rc::new(Cell::new(false));
        let scene_selection = Rc::new(Cell::new(None));
        wire_operator_scene_launch(
            ui,
            &scene_commands,
            &scene_diagnostic,
            &operator_scenes,
            &scene_launch_authority,
            &scene_model,
            &scene_selection,
        );
        let scene_stop_authority = Rc::new(Cell::new(false));
        wire_operator_scene_stop(
            ui,
            &scene_commands,
            &scene_diagnostic,
            &scene_stop_authority,
        );
        assert!(!ui.get_stop_interactive_scenes_enabled());
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(scene_command_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            scene_diagnostic.lock().unwrap().as_str(),
            "interactive scene stop refused: accepted scene state is unavailable"
        );
        scene_stop_authority.set(true);
        ui.set_stop_interactive_scenes_enabled(true);
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(
            scene_command_receiver.try_recv(),
            Ok(OperatorCommand::StopInteractiveScenes)
        );
        let initial_scene = scene_model.row_data(0).unwrap();
        let mut committed_scene = initial_scene.clone();
        committed_scene.queued = true;
        committed_scene.last_launched = true;
        committed_scene.status = "Queued for frame 321".into();
        scene_model.set_row_data(0, committed_scene.clone());
        ui.set_scene_feedback("Scene 1 queued for frame 321".into());
        let committed_feedback = ui.get_scene_feedback();
        scene_stop_authority.set(true);
        ui.set_stop_interactive_scenes_enabled(true);
        for _ in 0..OPERATOR_COMMAND_CAPACITY {
            scene_commands.send(OperatorCommand::Start).unwrap();
        }
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(
            scene_command_receiver.try_recv(),
            Ok(OperatorCommand::Start)
        );
        assert!(
            scene_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert!(scene_stop_authority.get());
        assert!(ui.get_stop_interactive_scenes_enabled());
        assert_eq!(scene_model.row_data(0), Some(committed_scene.clone()));
        assert_eq!(ui.get_scene_feedback(), committed_feedback);
        for _ in 1..OPERATOR_COMMAND_CAPACITY {
            assert_eq!(
                scene_command_receiver.try_recv(),
                Ok(OperatorCommand::Start)
            );
        }
        assert_eq!(scene_command_receiver.try_recv(), Err(TryRecvError::Empty));

        let (lost_scene_commands, lost_scene_command_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        wire_operator_scene_stop(
            ui,
            &lost_scene_commands,
            &scene_diagnostic,
            &scene_stop_authority,
        );
        drop(lost_scene_command_receiver);
        ui.invoke_stop_interactive_scenes_requested();
        assert!(
            scene_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert!(scene_stop_authority.get());
        assert!(ui.get_stop_interactive_scenes_enabled());
        assert_eq!(scene_model.row_data(0), Some(committed_scene));
        assert_eq!(ui.get_scene_feedback(), committed_feedback);
        scene_model.set_row_data(0, initial_scene);
        scene_stop_authority.set(false);
        ui.set_stop_interactive_scenes_enabled(false);
        let generated_scenes = ui.get_scenes();
        assert_eq!(generated_scenes.row_count(), 4);
        set_operator_scene_launch_authority(
            ui,
            &scene_model,
            &scene_launch_authority,
            &scene_selection,
        );
        assert!(scene_launch_authority.get());
        assert_eq!(scene_selection.get(), Some(0));
        set_operator_scene_launch_authority(
            ui,
            &scene_model,
            &scene_launch_authority,
            &scene_selection,
        );
        assert_eq!(
            scene_selection.get(),
            Some(0),
            "a later accepted observation retains local scene selection"
        );
        for (row, expected) in [
            (0, (true, "Ready")),
            (1, (false, "Disabled")),
            (2, (false, "Needs consolidation")),
            (3, (false, "Audio unavailable")),
        ] {
            let scene = generated_scenes.row_data(row).unwrap();
            assert_eq!((scene.available, scene.status.as_str()), expected);
            assert_eq!(scene.controls_enabled, scene.available);
        }
        ui.invoke_launch_scene_requested(0);
        assert_eq!(
            scene_command_receiver.try_recv(),
            Ok(OperatorCommand::LaunchScene { scene_index: 0 })
        );
        ui.invoke_launch_selected_scene_requested();
        assert_eq!(
            scene_command_receiver.try_recv(),
            Ok(OperatorCommand::LaunchScene { scene_index: 0 })
        );
        for refused_row in [1, 2, 3, -1, 9] {
            ui.invoke_launch_scene_requested(refused_row);
            assert_eq!(scene_command_receiver.try_recv(), Err(TryRecvError::Empty));
        }
        assert_eq!(
            scene_diagnostic.lock().unwrap().as_str(),
            "scene launch refused: stale row"
        );

        let mixer_channels = build_operator_mixer_catalog(
            mixer_sender.channel_count(),
            |index| session.mixer_channel_name_at(index).map(str::to_owned),
            |index| {
                mixer_sender
                    .resolve_channel_index(index)
                    .map_err(|error| error.to_string())
            },
            |index| {
                mixer_sender
                    .channel_supports_solo(index)
                    .map_err(|error| error.to_string())
            },
        )
        .unwrap();
        assert_eq!(
            mixer_channels
                .iter()
                .map(|channel| (channel.index, channel.name.as_str()))
                .collect::<Vec<_>>(),
            [
                (0, "Meter source"),
                (1, "Scene group"),
                (2, "Ritorno Ω"),
                (3, "Return Delta"),
            ]
        );
        let mixer_sends = build_operator_mixer_send_catalog(
            &mixer_channels,
            |channel_index| session.mixer_send_count_at(channel_index),
            |channel_index, send_index| {
                let send = session.mixer_send_at(channel_index, send_index);
                send.map(|send| {
                    mixer_sender
                        .resolve_send_address(channel_index, send_index)
                        .map(|handle| {
                            (
                                send.target_name().to_owned(),
                                match send.tap() {
                                    hyper_playback_session::PlaybackMixerSendTap::PreFader => {
                                        DesktopMixerSendTap::PreFader
                                    }
                                    hyper_playback_session::PlaybackMixerSendTap::PostFader => {
                                        DesktopMixerSendTap::PostFader
                                    }
                                },
                                handle,
                            )
                        })
                        .map_err(|error| error.to_string())
                })
                .transpose()
            },
        )
        .unwrap();
        assert_eq!(session.mixer_send_count_at(0), Some(2));
        assert_eq!(session.mixer_send_count_at(1), Some(1));
        assert_eq!(mixer_sends.len(), 3);
        assert_eq!(
            mixer_sends
                .iter()
                .map(|send| (
                    send.channel_index,
                    send.send_index,
                    send.source_name.as_str(),
                    send.target_name.as_str(),
                    send.tap,
                ))
                .collect::<Vec<_>>(),
            [
                (
                    0,
                    0,
                    "Meter source",
                    "Ritorno Ω",
                    DesktopMixerSendTap::PostFader
                ),
                (
                    0,
                    1,
                    "Meter source",
                    "Return Delta",
                    DesktopMixerSendTap::PreFader
                ),
                (
                    1,
                    0,
                    "Scene group",
                    "Ritorno Ω",
                    DesktopMixerSendTap::PreFader
                ),
            ]
        );
        assert!(
            build_operator_mixer_send_catalog(
                &mixer_channels,
                |channel_index| (channel_index != 1).then_some(0),
                |_, _| panic!("a missing send count cannot query routing facts"),
            )
            .is_err()
        );
        assert!(
            build_operator_mixer_send_catalog(
                &mixer_channels,
                |channel_index| Some(usize::from(channel_index == 0)),
                |_, _| Ok(None),
            )
            .is_err()
        );
        assert!(
            build_operator_mixer_send_catalog(
                &mixer_channels,
                |channel_index| Some(usize::from(channel_index == 0)),
                |_, _| Err("generation-bound handle is unavailable".to_owned()),
            )
            .is_err()
        );
        assert!(
            build_operator_mixer_send_catalog(
                &mixer_channels,
                |_| Some(0),
                |_, _| panic!("an empty send catalog cannot query routing facts"),
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            build_operator_mixer_catalog(
                2,
                |index| (index == 0).then(|| "Only one name".to_owned()),
                |index| mixer_sender
                    .resolve_channel_index(index)
                    .map_err(|error| error.to_string()),
                |index| mixer_sender
                    .channel_supports_solo(index)
                    .map_err(|error| error.to_string()),
            )
            .is_err()
        );
        assert!(
            build_operator_mixer_catalog(
                5,
                |index| Some(format!("Channel {index}")),
                |index| mixer_sender
                    .resolve_channel_index(index)
                    .map_err(|error| error.to_string()),
                |index| mixer_sender
                    .channel_supports_solo(index)
                    .map_err(|error| error.to_string()),
            )
            .is_err()
        );
        assert!(
            build_operator_mixer_catalog(
                2,
                |index| Some(format!("Channel {index}")),
                |index| mixer_sender
                    .resolve_channel_index(index)
                    .map_err(|error| error.to_string()),
                |index| {
                    if index == 0 {
                        Ok(true)
                    } else {
                        Err("capability unavailable".to_owned())
                    }
                },
            )
            .is_err()
        );
        let empty_mixer = build_operator_mixer_catalog(
            0,
            |_| panic!("an empty catalog cannot query a name"),
            |_| panic!("an empty catalog cannot query a handle"),
            |_| panic!("an empty catalog cannot query a capability"),
        )
        .unwrap();
        assert!(operator_mixer_rows(&empty_mixer).is_empty());
        let boundary_names = [
            "Canale Ω molto lungo 0",
            "Canal 1",
            "Canal 2",
            "Canal 3",
            "Canal 4",
            "Canal 5",
            "Canal 6",
            "Canal 7",
        ];
        let boundary_mixer = build_operator_mixer_catalog(
            boundary_names.len(),
            |index| Some(boundary_names[index].to_owned()),
            |index| Ok(mixer_channels[index % mixer_channels.len()].handle),
            |index| Ok(index.is_multiple_of(2)),
        )
        .unwrap();
        let boundary_rows = operator_mixer_rows(&boundary_mixer);
        assert_eq!(boundary_rows.len(), 8);
        assert_eq!(boundary_rows[0].name.as_str(), boundary_names[0]);
        assert!(boundary_rows[0].solo_available);
        assert!(!boundary_rows[1].solo_available);
        let (owner_mixer_commands, owner_mixer_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        owner_mixer_commands
            .send(OperatorCommand::SetMixerChannelGain {
                channel: mixer_channels[1].handle,
                gain_millionths: 1_255_000,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMixerChannelPan {
                channel: mixer_channels[0].handle,
                pan_millionths: -252_500,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMixerChannelMute {
                channel: mixer_channels[1].handle,
                muted: true,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMixerChannelSolo {
                channel: mixer_channels[0].handle,
                solo: true,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMainGain {
                gain_millionths: 750_000,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMainMute { muted: true })
            .unwrap();
        let owner_stop = AtomicBool::new(false);
        let mut owner = OperatorControl::default();
        drain_operator_commands(&mut owner, &owner_mixer_receiver, &owner_stop, &|_| {});
        assert_eq!(
            owner.mixer_commands,
            [
                MixerParameterCommand::ChannelGain {
                    channel: mixer_channels[1].handle,
                    gain: LinearGain::from_millionths(1_255_000).unwrap(),
                },
                MixerParameterCommand::ChannelPan {
                    channel: mixer_channels[0].handle,
                    pan: Pan::from_millionths(-252_500).unwrap(),
                },
                MixerParameterCommand::ChannelMute {
                    channel: mixer_channels[1].handle,
                    muted: true,
                },
                MixerParameterCommand::ChannelSolo {
                    channel: mixer_channels[0].handle,
                    solo: true,
                },
                MixerParameterCommand::MainGain {
                    gain: LinearGain::from_millionths(750_000).unwrap(),
                },
                MixerParameterCommand::MainMute { muted: true },
            ]
        );
        owner_mixer_commands
            .send(OperatorCommand::SetMixerSendGain {
                send: mixer_sends[0].handle,
                gain_millionths: 555_000,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMixerSendPan {
                send: mixer_sends[0].handle,
                pan_millionths: 250_000,
            })
            .unwrap();
        owner_mixer_commands
            .send(OperatorCommand::SetMixerSendEnabled {
                send: mixer_sends[0].handle,
                enabled: false,
            })
            .unwrap();
        drain_operator_commands(&mut owner, &owner_mixer_receiver, &owner_stop, &|_| {});
        assert_eq!(
            &owner.mixer_commands[6..],
            [
                MixerParameterCommand::SendGain {
                    send: mixer_sends[0].handle,
                    gain: LinearGain::from_millionths(555_000).unwrap(),
                },
                MixerParameterCommand::SendPan {
                    send: mixer_sends[0].handle,
                    pan: Pan::from_millionths(250_000).unwrap(),
                },
                MixerParameterCommand::SendEnabled {
                    send: mixer_sends[0].handle,
                    enabled: false,
                },
            ]
        );
        let mixer_model = Rc::new(VecModel::from(operator_mixer_rows(&mixer_channels)));
        let mixer_send_model = Rc::new(VecModel::from(operator_mixer_send_rows(&mixer_sends)));
        let mixer_authority = Rc::new(RefCell::new(None));
        ui.set_mixer_channels(ModelRc::from(Rc::clone(&mixer_model)));
        ui.set_mixer_sends(ModelRc::from(Rc::clone(&mixer_send_model)));
        let (mixer_commands, mixer_command_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY * 2);
        let mixer_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_mixer_controls(ui, &mixer_commands, &mixer_diagnostic, &mixer_authority);

        let (mut recovery_publisher, recovery) =
            hyper_mixer_runtime::playback_recovery_channel(None, Some(&initial_mixer_state));
        let loop_authority = Rc::new(Cell::new(None));
        let (loop_commands, loop_command_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let loop_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_loop_toggle(ui, &loop_commands, &loop_diagnostic, &loop_authority);
        wire_operator_loop_range(ui, &loop_commands, &loop_diagnostic, &loop_authority);
        let initial_observation = recovery.observe_cycle();
        apply_operator_loop_observation(
            ui,
            &loop_authority,
            initial_observation,
            PlaybackExtent::Bounded { end_frame: 1_500 },
        );
        apply_operator_scene_observation(ui, &scene_model, initial_observation);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            false,
        );
        assert_eq!(ui.get_loop_state().as_str(), "No session loop");
        assert_eq!(ui.get_loop_range().as_str(), "No committed session range");
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Loop unavailable");
        assert!(!ui.get_loop_toggle_enabled());
        assert_eq!(ui.get_mixer_state().as_str(), "Committed cycle 0");
        assert_operator_float(ui.get_main_gain_percent(), 100.0);
        assert!(!ui.get_main_muted());
        assert!(ui.get_main_mute_enabled());
        let generated_mixer = ui.get_mixer_channels();
        assert_eq!(generated_mixer.row_count(), 4);
        for (row, name) in [
            (0, "Meter source"),
            (1, "Scene group"),
            (2, "Ritorno Ω"),
            (3, "Return Delta"),
        ] {
            let channel = generated_mixer.row_data(row).unwrap();
            assert_eq!(channel.name.as_str(), name);
            assert_operator_float(channel.gain_percent, 100.0);
            assert_operator_float(channel.pan_percent, 0.0);
            assert!(!channel.muted);
            assert!(!channel.soloed);
            assert!(channel.controls_enabled);
        }
        let generated_sends = ui.get_mixer_sends();
        assert_eq!(generated_sends.row_count(), 3);
        let send_row = generated_sends.row_data(0).unwrap();
        assert_eq!(send_row.source_name.as_str(), "Meter source");
        assert_eq!(send_row.target_name.as_str(), "Ritorno Ω");
        assert_eq!(send_row.tap.as_str(), "POST");
        assert_operator_float(send_row.gain_percent, 80.0);
        assert_operator_float(
            send_row.pan_percent,
            Pan::from_millionths(-300_000).unwrap().value() * 100.0,
        );
        assert!(send_row.enabled);
        assert!(send_row.controls_enabled);
        let second_send_row = generated_sends.row_data(1).unwrap();
        assert_eq!(second_send_row.source_name.as_str(), "Meter source");
        assert_eq!(second_send_row.target_name.as_str(), "Return Delta");
        assert_eq!(second_send_row.tap.as_str(), "PRE");
        assert_operator_float(second_send_row.gain_percent, 40.0);
        assert_operator_float(
            second_send_row.pan_percent,
            Pan::from_millionths(600_000).unwrap().value() * 100.0,
        );
        assert!(!second_send_row.enabled);
        assert!(second_send_row.controls_enabled);
        let later_channel_send_row = generated_sends.row_data(2).unwrap();
        assert_eq!(later_channel_send_row.source_name.as_str(), "Scene group");
        assert_eq!(later_channel_send_row.target_name.as_str(), "Ritorno Ω");
        assert_eq!(later_channel_send_row.tap.as_str(), "PRE");
        assert_operator_float(later_channel_send_row.gain_percent, 125.0);
        assert_operator_float(
            later_channel_send_row.pan_percent,
            Pan::from_millionths(-700_000).unwrap().value() * 100.0,
        );
        assert!(later_channel_send_row.enabled);
        assert!(later_channel_send_row.controls_enabled);

        assert!(generated_mixer.row_data(0).unwrap().solo_available);
        assert!(!generated_mixer.row_data(1).unwrap().solo_available);
        ui.invoke_set_mixer_channel_gain_requested(1, 125.5);
        let gain = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            gain,
            OperatorCommand::SetMixerChannelGain {
                channel: mixer_channels[1].handle,
                gain_millionths: 1_255_000,
            }
        );
        ui.invoke_set_mixer_channel_pan_requested(0, -25.25);
        let pan = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            pan,
            OperatorCommand::SetMixerChannelPan {
                channel: mixer_channels[0].handle,
                pan_millionths: -252_500,
            }
        );
        ui.invoke_toggle_mixer_channel_mute_requested(1);
        let mute = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            mute,
            OperatorCommand::SetMixerChannelMute {
                channel: mixer_channels[1].handle,
                muted: true,
            }
        );
        ui.invoke_toggle_mixer_channel_solo_requested(0);
        let solo = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            solo,
            OperatorCommand::SetMixerChannelSolo {
                channel: mixer_channels[0].handle,
                solo: true,
            }
        );
        ui.invoke_set_mixer_send_gain_requested(1, 65.25);
        let second_send_gain = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            second_send_gain,
            OperatorCommand::SetMixerSendGain {
                send: mixer_sends[1].handle,
                gain_millionths: 652_500,
            }
        );
        ui.invoke_set_mixer_send_gain_requested(2, 55.5);
        let send_gain = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            send_gain,
            OperatorCommand::SetMixerSendGain {
                send: mixer_sends[2].handle,
                gain_millionths: 555_000,
            }
        );
        ui.invoke_set_mixer_send_pan_requested(2, 25.0);
        let send_pan = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            send_pan,
            OperatorCommand::SetMixerSendPan {
                send: mixer_sends[2].handle,
                pan_millionths: 250_000,
            }
        );
        ui.invoke_toggle_mixer_send_requested(2);
        let send_enabled = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            send_enabled,
            OperatorCommand::SetMixerSendEnabled {
                send: mixer_sends[2].handle,
                enabled: false,
            }
        );
        ui.invoke_set_main_gain_requested(75.0);
        let main_gain = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(
            main_gain,
            OperatorCommand::SetMainGain {
                gain_millionths: 750_000,
            }
        );
        ui.invoke_toggle_main_mute_requested();
        let main_mute = mixer_command_receiver.try_recv().unwrap();
        assert_eq!(main_mute, OperatorCommand::SetMainMute { muted: true });
        assert!(
            !ui.get_main_muted(),
            "command acceptance is not publication"
        );
        assert_operator_float(ui.get_main_gain_percent(), 100.0);
        assert_operator_float(generated_mixer.row_data(1).unwrap().gain_percent, 100.0);
        assert_operator_float(generated_mixer.row_data(0).unwrap().pan_percent, 0.0);
        assert!(!generated_mixer.row_data(1).unwrap().muted);
        assert!(!generated_mixer.row_data(0).unwrap().soloed);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert_operator_float(
            generated_sends.row_data(0).unwrap().pan_percent,
            Pan::from_millionths(-300_000).unwrap().value() * 100.0,
        );
        assert!(generated_sends.row_data(0).unwrap().enabled);
        for invalid_gain in [f32::NAN, -0.1, 200.1] {
            ui.invoke_set_mixer_channel_gain_requested(0, invalid_gain);
            ui.invoke_set_mixer_send_gain_requested(0, invalid_gain);
            ui.invoke_set_main_gain_requested(invalid_gain);
        }
        for invalid_pan in [f32::NAN, -100.1, 100.1] {
            ui.invoke_set_mixer_channel_pan_requested(0, invalid_pan);
            ui.invoke_set_mixer_send_pan_requested(0, invalid_pan);
        }
        assert_eq!(mixer_command_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            mixer_diagnostic.lock().unwrap().as_str(),
            "mixer send pan refused: value is outside -100..=100%"
        );
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);

        let mismatched_channels = &mixer_channels[..1];
        let mismatched_model = VecModel::from(operator_mixer_rows(mismatched_channels));
        let mismatched_send_model = VecModel::from(operator_mixer_send_rows(&mixer_sends));
        let mut retained_row = mismatched_model.row_data(0).unwrap();
        retained_row.muted = true;
        retained_row.soloed = true;
        retained_row.controls_enabled = true;
        mismatched_model.set_row_data(0, retained_row);
        let mismatched_authority = RefCell::new(Some(OperatorMixerAuthority {
            channels: vec![OperatorMixerChannelAuthority {
                handle: mismatched_channels[0].handle,
                gain: LinearGain::UNITY,
                pan: Pan::CENTER,
                muted: true,
                solo: true,
                solo_available: true,
            }],
            sends: vec![OperatorMixerSendAuthority {
                handle: mixer_sends[0].handle,
                gain: LinearGain::from_millionths(800_000).unwrap(),
                pan: Pan::from_millionths(-300_000).unwrap(),
                enabled: true,
            }],
            main_gain: LinearGain::UNITY,
            main_muted: true,
        }));
        apply_operator_mixer_observation(
            ui,
            &mismatched_model,
            &mismatched_send_model,
            mismatched_channels,
            &mixer_sends,
            &mismatched_authority,
            &recovery.snapshot(),
            false,
        );
        let retained_row = mismatched_model.row_data(0).unwrap();
        assert!(retained_row.muted);
        assert!(retained_row.soloed);
        assert!(!retained_row.controls_enabled);
        assert_eq!(*mismatched_authority.borrow(), None);
        let missing_send_model = VecModel::from(Vec::<OperatorMixerSendRow>::new());
        let missing_send_authority = RefCell::new(None);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &missing_send_model,
            &mixer_channels,
            &[],
            &missing_send_authority,
            &recovery.snapshot(),
            false,
        );
        assert_eq!(*missing_send_authority.borrow(), None);
        assert!(!mixer_model.row_data(0).unwrap().controls_enabled);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            false,
        );
        for command in [
            gain,
            pan,
            mute,
            solo,
            main_gain,
            main_mute,
            second_send_gain,
            send_gain,
            send_pan,
            send_enabled,
        ] {
            match command {
                OperatorCommand::SetMixerChannelGain {
                    channel,
                    gain_millionths,
                } => mixer_sender
                    .try_send(MixerParameterCommand::ChannelGain {
                        channel,
                        gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                    })
                    .unwrap(),
                OperatorCommand::SetMixerChannelPan {
                    channel,
                    pan_millionths,
                } => mixer_sender
                    .try_send(MixerParameterCommand::ChannelPan {
                        channel,
                        pan: Pan::from_millionths(pan_millionths).unwrap(),
                    })
                    .unwrap(),
                OperatorCommand::SetMixerChannelMute { channel, muted } => mixer_sender
                    .try_send(MixerParameterCommand::ChannelMute { channel, muted })
                    .unwrap(),
                OperatorCommand::SetMixerChannelSolo { channel, solo } => mixer_sender
                    .try_send(MixerParameterCommand::ChannelSolo { channel, solo })
                    .unwrap(),
                OperatorCommand::SetMixerSendGain {
                    send,
                    gain_millionths,
                } => mixer_sender
                    .try_send(MixerParameterCommand::SendGain {
                        send,
                        gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                    })
                    .unwrap(),
                OperatorCommand::SetMixerSendPan {
                    send,
                    pan_millionths,
                } => mixer_sender
                    .try_send(MixerParameterCommand::SendPan {
                        send,
                        pan: Pan::from_millionths(pan_millionths).unwrap(),
                    })
                    .unwrap(),
                OperatorCommand::SetMixerSendEnabled { send, enabled } => mixer_sender
                    .try_send(MixerParameterCommand::SendEnabled { send, enabled })
                    .unwrap(),
                OperatorCommand::SetMainMute { muted } => mixer_sender
                    .try_send(MixerParameterCommand::MainMute { muted })
                    .unwrap(),
                OperatorCommand::SetMainGain { gain_millionths } => mixer_sender
                    .try_send(MixerParameterCommand::MainGain {
                        gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                    })
                    .unwrap(),
                _ => unreachable!("the mixer fixture routes only mixer commands"),
            }
        }
        ui.invoke_toggle_loop_requested();
        assert_eq!(loop_command_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            loop_diagnostic.lock().unwrap().as_str(),
            "session loop toggle refused: no session loop is armed"
        );
        assert!(ui.get_loop_in_enabled());
        assert!(!ui.get_loop_out_enabled());
        assert!(!ui.get_loop_clear_enabled());
        ui.invoke_set_loop_out_requested();
        ui.invoke_clear_loop_range_requested();
        assert_eq!(loop_command_receiver.try_recv(), Err(TryRecvError::Empty));
        ui.invoke_set_loop_in_requested();
        let requested_range = TransportLoopRange::new(0, 1_500).unwrap();
        assert_eq!(
            loop_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoopRange {
                range: Some(requested_range)
            })
        );
        assert_eq!(ui.get_loop_state().as_str(), "No session loop");
        assert_eq!(ui.get_loop_range().as_str(), "No committed session range");
        transport_sender
            .try_send(TransportCommand::LaunchScene { scene_index: 0 })
            .unwrap();
        transport_sender
            .try_send(TransportCommand::SetLoopRange(Some(requested_range)))
            .unwrap();
        let mut stopped_left = [0.0_f32; 480];
        let mut stopped_right = [0.0_f32; 480];
        let queued = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut stopped_left,
                    right: &mut stopped_right,
                },
            )
            .unwrap();
        let committed_mixer = transport.mixer_parameter_state();
        recovery_publisher.publish_cycle(queued, Some(&committed_mixer));
        let queued_observation = recovery.observe_cycle();
        apply_operator_loop_observation(
            ui,
            &loop_authority,
            queued_observation,
            PlaybackExtent::Bounded { end_frame: 1_500 },
        );
        apply_operator_scene_observation(ui, &scene_model, queued_observation);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            false,
        );
        assert_eq!(ui.get_mixer_state().as_str(), "Committed cycle 1");
        assert_operator_float(ui.get_main_gain_percent(), 75.0);
        assert!(ui.get_main_muted());
        assert_operator_float(generated_mixer.row_data(1).unwrap().gain_percent, 125.5);
        assert_operator_float(generated_mixer.row_data(0).unwrap().pan_percent, -25.25);
        assert!(generated_mixer.row_data(1).unwrap().muted);
        assert!(generated_mixer.row_data(0).unwrap().soloed);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert_operator_float(generated_sends.row_data(1).unwrap().gain_percent, 65.25);
        assert_operator_float(generated_sends.row_data(2).unwrap().gain_percent, 55.5);
        assert_operator_float(generated_sends.row_data(2).unwrap().pan_percent, 25.0);
        assert!(!generated_sends.row_data(2).unwrap().enabled);

        ui.invoke_toggle_mixer_channel_solo_requested(1);
        assert_eq!(mixer_command_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            mixer_diagnostic.lock().unwrap().as_str(),
            "mixer solo refused: channel does not support Solo"
        );

        ui.invoke_set_mixer_channel_gain_requested(1, 100.0);
        ui.invoke_set_mixer_channel_pan_requested(0, 0.0);
        ui.invoke_toggle_mixer_channel_mute_requested(1);
        ui.invoke_toggle_mixer_channel_solo_requested(0);
        ui.invoke_set_mixer_send_gain_requested(1, 40.0);
        ui.invoke_set_mixer_send_gain_requested(2, 125.0);
        ui.invoke_set_mixer_send_pan_requested(2, -70.0);
        ui.invoke_toggle_mixer_send_requested(2);
        ui.invoke_set_main_gain_requested(100.0);
        ui.invoke_toggle_main_mute_requested();
        for expected in [
            MixerParameterCommand::ChannelGain {
                channel: mixer_channels[1].handle,
                gain: LinearGain::UNITY,
            },
            MixerParameterCommand::ChannelPan {
                channel: mixer_channels[0].handle,
                pan: Pan::CENTER,
            },
            MixerParameterCommand::ChannelMute {
                channel: mixer_channels[1].handle,
                muted: false,
            },
            MixerParameterCommand::ChannelSolo {
                channel: mixer_channels[0].handle,
                solo: false,
            },
            MixerParameterCommand::SendGain {
                send: mixer_sends[1].handle,
                gain: LinearGain::from_millionths(400_000).unwrap(),
            },
            MixerParameterCommand::SendGain {
                send: mixer_sends[2].handle,
                gain: LinearGain::from_millionths(1_250_000).unwrap(),
            },
            MixerParameterCommand::SendPan {
                send: mixer_sends[2].handle,
                pan: Pan::from_millionths(-700_000).unwrap(),
            },
            MixerParameterCommand::SendEnabled {
                send: mixer_sends[2].handle,
                enabled: true,
            },
            MixerParameterCommand::MainGain {
                gain: LinearGain::UNITY,
            },
            MixerParameterCommand::MainMute { muted: false },
        ] {
            let command = mixer_command_receiver.try_recv().unwrap();
            let actual = match command {
                OperatorCommand::SetMixerChannelGain {
                    channel,
                    gain_millionths,
                } => MixerParameterCommand::ChannelGain {
                    channel,
                    gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                },
                OperatorCommand::SetMixerChannelPan {
                    channel,
                    pan_millionths,
                } => MixerParameterCommand::ChannelPan {
                    channel,
                    pan: Pan::from_millionths(pan_millionths).unwrap(),
                },
                OperatorCommand::SetMixerChannelMute { channel, muted } => {
                    MixerParameterCommand::ChannelMute { channel, muted }
                }
                OperatorCommand::SetMixerChannelSolo { channel, solo } => {
                    MixerParameterCommand::ChannelSolo { channel, solo }
                }
                OperatorCommand::SetMixerSendGain {
                    send,
                    gain_millionths,
                } => MixerParameterCommand::SendGain {
                    send,
                    gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                },
                OperatorCommand::SetMixerSendPan {
                    send,
                    pan_millionths,
                } => MixerParameterCommand::SendPan {
                    send,
                    pan: Pan::from_millionths(pan_millionths).unwrap(),
                },
                OperatorCommand::SetMixerSendEnabled { send, enabled } => {
                    MixerParameterCommand::SendEnabled { send, enabled }
                }
                OperatorCommand::SetMainMute { muted } => MixerParameterCommand::MainMute { muted },
                OperatorCommand::SetMainGain { gain_millionths } => {
                    MixerParameterCommand::MainGain {
                        gain: LinearGain::from_millionths(gain_millionths).unwrap(),
                    }
                }
                _ => unreachable!("the mixer fixture routes only mixer commands"),
            };
            assert_eq!(actual, expected);
            mixer_sender.try_send(actual).unwrap();
        }
        let mut mixer_left = [0.0_f32; 480];
        let mut mixer_right = [0.0_f32; 480];
        let mixer_cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut mixer_left,
                    right: &mut mixer_right,
                },
            )
            .unwrap();
        let committed_mixer = transport.mixer_parameter_state();
        recovery_publisher.publish_cycle(mixer_cycle, Some(&committed_mixer));
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            false,
        );
        assert_operator_float(ui.get_main_gain_percent(), 100.0);
        assert!(!ui.get_main_muted());
        assert_operator_float(generated_mixer.row_data(1).unwrap().gain_percent, 100.0);
        assert_operator_float(generated_mixer.row_data(0).unwrap().pan_percent, 0.0);
        assert!(!generated_mixer.row_data(1).unwrap().muted);
        assert!(!generated_mixer.row_data(0).unwrap().soloed);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert_operator_float(
            generated_sends.row_data(0).unwrap().pan_percent,
            Pan::from_millionths(-300_000).unwrap().value() * 100.0,
        );
        assert!(generated_sends.row_data(0).unwrap().enabled);
        assert_operator_float(generated_sends.row_data(1).unwrap().gain_percent, 40.0);
        assert_operator_float(generated_sends.row_data(2).unwrap().gain_percent, 125.0);
        assert_operator_float(
            generated_sends.row_data(2).unwrap().pan_percent,
            Pan::from_millionths(-700_000).unwrap().value() * 100.0,
        );
        assert!(generated_sends.row_data(2).unwrap().enabled);

        for stale_row in [-1, 4, 9] {
            ui.invoke_set_mixer_channel_gain_requested(stale_row, 50.0);
            ui.invoke_set_mixer_channel_pan_requested(stale_row, 25.0);
            ui.invoke_toggle_mixer_channel_mute_requested(stale_row);
            ui.invoke_toggle_mixer_channel_solo_requested(stale_row);
        }
        for stale_row in [-1, 3, 9] {
            ui.invoke_set_mixer_send_gain_requested(stale_row, 50.0);
            ui.invoke_set_mixer_send_pan_requested(stale_row, 25.0);
            ui.invoke_toggle_mixer_send_requested(stale_row);
        }
        assert_eq!(mixer_command_receiver.try_recv(), Err(TryRecvError::Empty));

        let (_missing_mixer_publisher, missing_mixer) =
            hyper_mixer_runtime::playback_recovery_channel(None, None);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &missing_mixer.snapshot(),
            false,
        );
        assert_eq!(ui.get_mixer_state().as_str(), "Mixer state unavailable");
        assert!(!ui.get_main_muted(), "last committed state remains visible");
        assert!(!ui.get_main_mute_enabled());
        assert!(!generated_mixer.row_data(0).unwrap().muted);
        assert!(!generated_mixer.row_data(0).unwrap().controls_enabled);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert!(generated_sends.row_data(0).unwrap().enabled);
        assert!(!generated_sends.row_data(0).unwrap().controls_enabled);
        ui.invoke_set_mixer_channel_gain_requested(0, 50.0);
        ui.invoke_set_mixer_channel_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_channel_mute_requested(0);
        ui.invoke_toggle_mixer_channel_solo_requested(1);
        ui.invoke_set_mixer_send_gain_requested(0, 50.0);
        ui.invoke_set_mixer_send_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_send_requested(0);
        ui.invoke_set_main_gain_requested(50.0);
        ui.invoke_toggle_main_mute_requested();
        assert_eq!(mixer_command_receiver.try_recv(), Err(TryRecvError::Empty));

        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            false,
        );
        let (saturated_mixer_sender, saturated_mixer_receiver) = std::sync::mpsc::sync_channel(1);
        saturated_mixer_sender.send(OperatorCommand::Start).unwrap();
        let saturated_mixer_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_mixer_controls(
            ui,
            &saturated_mixer_sender,
            &saturated_mixer_diagnostic,
            &mixer_authority,
        );
        ui.invoke_set_mixer_channel_gain_requested(0, 50.0);
        ui.invoke_set_mixer_channel_pan_requested(0, 25.0);
        ui.invoke_set_main_gain_requested(50.0);
        ui.invoke_toggle_mixer_channel_mute_requested(0);
        ui.invoke_set_mixer_send_gain_requested(0, 50.0);
        ui.invoke_toggle_mixer_send_requested(0);
        assert_eq!(
            saturated_mixer_receiver.try_recv(),
            Ok(OperatorCommand::Start)
        );
        assert_eq!(
            saturated_mixer_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        assert!(
            saturated_mixer_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert!(!generated_mixer.row_data(0).unwrap().muted);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert!(generated_sends.row_data(0).unwrap().enabled);

        let (disconnected_mixer_sender, disconnected_mixer_receiver) =
            std::sync::mpsc::sync_channel(1);
        drop(disconnected_mixer_receiver);
        let disconnected_mixer_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_mixer_controls(
            ui,
            &disconnected_mixer_sender,
            &disconnected_mixer_diagnostic,
            &mixer_authority,
        );
        ui.invoke_set_mixer_channel_gain_requested(1, 50.0);
        ui.invoke_set_mixer_channel_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_channel_mute_requested(1);
        ui.invoke_toggle_mixer_channel_solo_requested(0);
        ui.invoke_set_mixer_send_gain_requested(0, 50.0);
        ui.invoke_set_mixer_send_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_send_requested(0);
        ui.invoke_set_main_gain_requested(50.0);
        ui.invoke_toggle_main_mute_requested();
        assert!(
            disconnected_mixer_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert!(!generated_mixer.row_data(1).unwrap().muted);
        assert!(!generated_mixer.row_data(0).unwrap().soloed);
        assert!(!ui.get_main_muted());
        assert_eq!(ui.get_loop_state().as_str(), "Loop armed");
        assert_eq!(ui.get_loop_range().as_str(), "frames 0–1500");
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Enable loop");
        assert!(ui.get_loop_toggle_enabled());
        assert!(ui.get_loop_in_enabled());
        assert!(!ui.get_loop_out_enabled());
        assert!(ui.get_loop_clear_enabled());
        ui.invoke_toggle_loop_requested();
        assert_eq!(
            loop_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoopEnabled { enabled: true })
        );
        let queued_row = ui.get_scenes().row_data(0).unwrap();
        assert!(queued_row.queued);
        assert!(!queued_row.last_launched);
        assert_eq!(queued_row.status.as_str(), "Queued for frame 0");
        assert_eq!(
            ui.get_scene_feedback().as_str(),
            "Scene 1 queued for frame 0"
        );

        transport_sender
            .try_send(TransportCommand::SetLoopEnabled(true))
            .unwrap();
        transport_sender.try_send(TransportCommand::Start).unwrap();
        let (mut meter_sender, meter_receiver) = hyper_mixer_runtime::output_meter_channel();
        let meter_config = SamplePeakBallisticsConfig::try_new(
            SAMPLE_RATE_HZ,
            OUTPUT_METER_FLOOR_DBFS,
            OUTPUT_METER_FALLOFF_DB_PER_SECOND,
            u64::MAX,
        )
        .unwrap();
        let meter_observer = Rc::new(RefCell::new(OperatorMeterObserver::new(
            meter_receiver,
            meter_config,
        )));
        let loudness_authority = Rc::new(Cell::new(None));
        let (loudness_commands, loudness_command_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let loudness_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_peak_reset(ui, &meter_observer, &loudness_authority);
        wire_operator_loudness_controls(
            ui,
            &loudness_commands,
            &loudness_diagnostic,
            &loudness_authority,
        );
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );

        assert!(ui.get_left_output_level().abs() <= f32::EPSILON);
        assert_eq!(ui.get_left_output_reading().as_str(), "— dBFS");
        assert_eq!(ui.get_left_output_peak_reading().as_str(), "hold —");
        assert!(ui.get_right_output_level().abs() <= f32::EPSILON);
        assert_eq!(ui.get_right_output_reading().as_str(), "— dBFS");
        assert_eq!(ui.get_right_output_peak_reading().as_str(), "hold —");
        assert!(!ui.get_output_overload_latched());
        assert_eq!(
            ui.get_output_meter_state().as_str(),
            "Waiting for final-output evidence"
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame unavailable · 0 coalesced · 0 dropped"
        );
        assert_eq!(ui.get_loudness_state().as_str(), "UNAVAILABLE");
        assert_eq!(ui.get_loudness_momentary().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_short_term().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_integrated().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_range().as_str(), "— LU");
        assert_eq!(ui.get_loudness_maximum_true_peak().as_str(), "— dBTP");
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:00.000");
        assert!(!ui.get_loudness_run_enabled());
        assert!(!ui.get_loudness_standby_enabled());
        assert!(!ui.get_loudness_reset_enabled());
        ui.invoke_loudness_run_requested();
        ui.invoke_loudness_standby_requested();
        ui.invoke_loudness_reset_requested();
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        assert_eq!(
            loudness_diagnostic.lock().unwrap().as_str(),
            "programme loudness reset refused: accepted state is unavailable"
        );

        let launched = transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        recovery_publisher.publish_cycle(launched.snapshot().cycle(), None);
        let launched_observation = recovery.observe_cycle();
        apply_operator_loop_observation(
            ui,
            &loop_authority,
            launched_observation,
            PlaybackExtent::Bounded { end_frame: 1_500 },
        );
        apply_operator_scene_observation(ui, &scene_model, launched_observation);
        assert_eq!(ui.get_loop_state().as_str(), "Looping");
        assert_eq!(ui.get_loop_range().as_str(), "frames 0–1500");
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Disable loop");
        assert!(ui.get_loop_toggle_enabled());
        ui.invoke_toggle_loop_requested();
        assert_eq!(
            loop_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoopEnabled { enabled: false })
        );

        loop_authority.set(Some(OperatorLoopAuthority {
            range: Some(TransportLoopRange::new(0, 1_500).unwrap()),
            enabled: true,
            position_frame: 300,
            extent: PlaybackExtent::Bounded { end_frame: 1_500 },
        }));
        ui.set_loop_in_enabled(true);
        ui.set_loop_out_enabled(false);

        let accepted_authority = loop_authority.get();
        let accepted_state = ui.get_loop_state();
        let accepted_range = ui.get_loop_range();
        let accepted_label = ui.get_loop_toggle_label();
        let (saturated_sender, saturated_receiver) = std::sync::mpsc::sync_channel(1);
        saturated_sender.send(OperatorCommand::Start).unwrap();
        let saturated_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_loop_toggle(
            ui,
            &saturated_sender,
            &saturated_diagnostic,
            &loop_authority,
        );
        wire_operator_loop_range(
            ui,
            &saturated_sender,
            &saturated_diagnostic,
            &loop_authority,
        );
        ui.invoke_toggle_loop_requested();
        ui.invoke_set_loop_in_requested();
        ui.invoke_clear_loop_range_requested();
        assert_eq!(saturated_receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(loop_authority.get(), accepted_authority);
        assert_eq!(ui.get_loop_state(), accepted_state);
        assert_eq!(ui.get_loop_range(), accepted_range);
        assert_eq!(ui.get_loop_toggle_label(), accepted_label);
        assert!(ui.get_loop_toggle_enabled());
        assert!(ui.get_loop_in_enabled());
        assert!(!ui.get_loop_out_enabled());
        assert!(ui.get_loop_clear_enabled());
        assert!(
            saturated_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );

        loop_authority.set(Some(OperatorLoopAuthority {
            range: Some(TransportLoopRange::new(0, 1_500).unwrap()),
            enabled: true,
            position_frame: 1_200,
            extent: PlaybackExtent::Bounded { end_frame: 1_500 },
        }));
        ui.set_loop_in_enabled(false);
        ui.set_loop_out_enabled(true);
        let disconnected_authority = loop_authority.get();
        let disconnected_state = ui.get_loop_state();
        let disconnected_range = ui.get_loop_range();
        let disconnected_enabled = (
            ui.get_loop_toggle_enabled(),
            ui.get_loop_in_enabled(),
            ui.get_loop_out_enabled(),
            ui.get_loop_clear_enabled(),
        );
        let (disconnected_sender, disconnected_receiver) = std::sync::mpsc::sync_channel(1);
        drop(disconnected_receiver);
        let disconnected_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_loop_range(
            ui,
            &disconnected_sender,
            &disconnected_diagnostic,
            &loop_authority,
        );
        ui.invoke_set_loop_out_requested();
        ui.invoke_clear_loop_range_requested();
        assert!(
            disconnected_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_eq!(loop_authority.get(), disconnected_authority);
        assert_eq!(ui.get_loop_state(), disconnected_state);
        assert_eq!(ui.get_loop_range(), disconnected_range);
        assert_eq!(
            (
                ui.get_loop_toggle_enabled(),
                ui.get_loop_in_enabled(),
                ui.get_loop_out_enabled(),
                ui.get_loop_clear_enabled(),
            ),
            disconnected_enabled
        );

        let terminal_state = ui.get_loop_state();
        let terminal_range = ui.get_loop_range();
        let (terminal_sender, terminal_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let terminal_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_loop_range(ui, &terminal_sender, &terminal_diagnostic, &loop_authority);
        revoke_operator_loop_authority(ui, &loop_authority);
        ui.invoke_toggle_loop_requested();
        ui.invoke_set_loop_in_requested();
        ui.invoke_set_loop_out_requested();
        ui.invoke_clear_loop_range_requested();
        assert_eq!(terminal_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            terminal_diagnostic.lock().unwrap().as_str(),
            "session loop clear refused: accepted state is unavailable"
        );
        assert_eq!(saturated_receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            saturated_diagnostic.lock().unwrap().as_str(),
            "session loop toggle refused: accepted state is unavailable"
        );
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Runtime stopped");
        assert!(!ui.get_loop_toggle_enabled());
        assert!(!ui.get_loop_in_enabled());
        assert!(!ui.get_loop_out_enabled());
        assert!(!ui.get_loop_clear_enabled());
        assert_eq!(ui.get_loop_state(), terminal_state);
        assert_eq!(ui.get_loop_range(), terminal_range);
        let launched_row = ui.get_scenes().row_data(0).unwrap();
        assert!(!launched_row.queued);
        assert!(launched_row.last_launched);
        assert_eq!(launched_row.status.as_str(), "Last launched");
        assert_eq!(
            ui.get_scene_feedback().as_str(),
            "Scene 1 was last launched"
        );
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        let strip_meters = meter_observer
            .borrow()
            .latest_strip_ballistics
            .expect("the delivered meter snapshot must produce strip ballistics");
        assert_eq!(generated_mixer.row_count(), strip_meters.slot_count());
        for slot in 0..strip_meters.slot_count() {
            let strip = strip_meters.slot(slot).unwrap();
            let row = generated_mixer.row_data(slot).unwrap();
            let (expected_left_level, expected_left_reading) =
                operator_strip_meter_presentation(strip.left());
            let (expected_right_level, expected_right_reading) =
                operator_strip_meter_presentation(strip.right());
            assert_eq!(row.index, i32::try_from(slot).unwrap());
            assert_operator_float(row.meter_left_level, expected_left_level);
            assert_operator_float(row.meter_right_level, expected_right_level);
            assert_eq!(row.meter_left_reading.as_str(), expected_left_reading);
            assert_eq!(row.meter_right_reading.as_str(), expected_right_reading);
        }
        let first_left_peak = interleaved
            .iter()
            .step_by(2)
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let first_right_peak = interleaved
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let first_left_dbfs = 20.0 * first_left_peak.log10();
        let first_right_dbfs = 20.0 * first_right_peak.log10();
        assert_eq!(
            ui.get_left_output_reading().as_str(),
            format!("{first_left_dbfs:.1} dBFS")
        );
        assert_eq!(
            ui.get_right_output_reading().as_str(),
            format!("{first_right_dbfs:.1} dBFS")
        );
        assert_ne!(ui.get_left_output_reading(), ui.get_right_output_reading());
        assert_eq!(
            ui.get_left_output_peak_reading().as_str(),
            format!("hold {first_left_dbfs:.1}")
        );
        assert_eq!(
            ui.get_right_output_peak_reading().as_str(),
            format!("hold {first_right_dbfs:.1}")
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 480 · 0 coalesced · 0 dropped"
        );
        assert_eq!(ui.get_loudness_state().as_str(), "RUNNING");
        assert_eq!(ui.get_loudness_momentary().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_short_term().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_integrated().as_str(), "— LUFS");
        assert_ne!(ui.get_loudness_maximum_true_peak().as_str(), "— dBTP");
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:00.010");
        assert!(!ui.get_loudness_run_enabled());
        assert!(ui.get_loudness_standby_enabled());
        assert!(ui.get_loudness_reset_enabled());

        transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        let live_left_before_reset = ui.get_left_output_reading();
        let live_right_before_reset = ui.get_right_output_reading();
        let expected_left_after_falloff =
            first_left_dbfs - OUTPUT_METER_FALLOFF_DB_PER_SECOND * 480.0 / 48_000.0;
        let expected_right_after_falloff =
            first_right_dbfs - OUTPUT_METER_FALLOFF_DB_PER_SECOND * 480.0 / 48_000.0;
        assert_eq!(
            live_left_before_reset.as_str(),
            format!("{expected_left_after_falloff:.1} dBFS")
        );
        assert_eq!(
            live_right_before_reset.as_str(),
            format!("{expected_right_after_falloff:.1} dBFS")
        );
        assert_eq!(
            ui.get_left_output_peak_reading().as_str(),
            format!("hold {first_left_dbfs:.1}")
        );
        assert_eq!(
            ui.get_right_output_peak_reading().as_str(),
            format!("hold {first_right_dbfs:.1}")
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 960 · 0 coalesced · 0 dropped"
        );
        let decayed_strip_meters = meter_observer
            .borrow()
            .latest_strip_ballistics
            .expect("the next delivered meter snapshot must update strip ballistics");
        assert_eq!(
            generated_mixer.row_count(),
            decayed_strip_meters.slot_count()
        );
        for slot in 0..decayed_strip_meters.slot_count() {
            let strip = decayed_strip_meters.slot(slot).unwrap();
            let row = generated_mixer.row_data(slot).unwrap();
            let (expected_left_level, expected_left_reading) =
                operator_strip_meter_presentation(strip.left());
            let (expected_right_level, expected_right_reading) =
                operator_strip_meter_presentation(strip.right());
            assert_operator_float(row.meter_left_level, expected_left_level);
            assert_operator_float(row.meter_right_level, expected_right_level);
            assert_eq!(row.meter_left_reading.as_str(), expected_left_reading);
            assert_eq!(row.meter_right_reading.as_str(), expected_right_reading);
        }

        ui.invoke_reset_output_peak_requested();
        assert_eq!(ui.get_left_output_reading(), live_left_before_reset);
        assert_eq!(ui.get_right_output_reading(), live_right_before_reset);
        assert_eq!(ui.get_left_output_peak_reading().as_str(), "hold —");
        assert_eq!(ui.get_right_output_peak_reading().as_str(), "hold —");
        assert!(!ui.get_output_overload_latched());
        assert_eq!(
            ui.get_output_meter_state().as_str(),
            "Live final-output sample peak"
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 960 · 0 coalesced · 0 dropped"
        );

        transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        let overload_left_peak = interleaved
            .iter()
            .step_by(2)
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let overload_right_peak = interleaved
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let overload_left_dbfs = 20.0 * overload_left_peak.log10();
        let overload_right_dbfs = 20.0 * overload_right_peak.log10();
        assert!(ui.get_output_overload_latched());
        assert!((ui.get_left_output_level() - 1.0).abs() <= f32::EPSILON);
        assert!((ui.get_right_output_level() - 1.0).abs() <= f32::EPSILON);
        assert_eq!(
            ui.get_left_output_reading().as_str(),
            format!("{overload_left_dbfs:.1} dBFS")
        );
        assert_eq!(
            ui.get_right_output_reading().as_str(),
            format!("{overload_right_dbfs:.1} dBFS")
        );
        assert_eq!(
            ui.get_left_output_peak_reading().as_str(),
            format!("hold {overload_left_dbfs:.1}")
        );
        assert_eq!(
            ui.get_right_output_peak_reading().as_str(),
            format!("hold {overload_right_dbfs:.1}")
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 1440 · 0 coalesced · 0 dropped"
        );

        let mut newest_loudness = None;
        for cycle_index in 0..297 {
            let programme_gain = match cycle_index {
                0..=98 => 0.08,
                99..=197 => 0.25,
                _ => 0.9,
            };
            let cycle = transport
                .process_interleaved_cycle_with_post_mix(
                    &mut callback,
                    &mut scratch,
                    &mut interleaved,
                    &mut meter_sender,
                    |output| {
                        for sample in output.left.iter_mut().chain(output.right.iter_mut()) {
                            *sample *= programme_gain;
                        }
                    },
                )
                .unwrap();
            newest_loudness = Some(
                cycle
                    .snapshot()
                    .present_loudness(EbuLoudnessPresentationConfig::default()),
            );
            apply_operator_meter_observation(
                ui,
                &mut meter_observer.borrow_mut(),
                &loudness_authority,
                false,
            );
        }
        assert_eq!(ui.get_loudness_state().as_str(), "RUNNING");
        assert_ne!(ui.get_loudness_momentary().as_str(), "— LUFS");
        assert_ne!(ui.get_loudness_short_term().as_str(), "— LUFS");
        assert_ne!(ui.get_loudness_integrated().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:03.000");
        assert_ne!(ui.get_loudness_range().as_str(), "— LU");
        assert!(
            ui.get_loudness_lra_status()
                .as_str()
                .starts_with("Stabilizing · 00:00:03.000")
        );
        let shaped_loudness = newest_loudness.unwrap();
        let shaped_values = [
            shaped_loudness.momentary().unwrap().displayed(),
            shaped_loudness.short_term().unwrap().displayed(),
            shaped_loudness.integrated().unwrap().displayed(),
        ];
        for left in 0..shaped_values.len() {
            for right in (left + 1)..shaped_values.len() {
                assert!(
                    (shaped_values[left] - shaped_values[right]).abs() >= 0.1,
                    "the deterministic programme must keep M/S/I distinct at display precision"
                );
            }
        }

        transport
            .process_interleaved_cycle_with_post_mix(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
                |output| {
                    for sample in output.left.iter_mut().chain(output.right.iter_mut()) {
                        *sample *= 0.05;
                    }
                },
            )
            .unwrap();
        let newest_cycle = transport
            .process_interleaved_cycle_with_post_mix(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
                |output| {
                    for sample in output.left.iter_mut().chain(output.right.iter_mut()) {
                        *sample *= 0.8;
                    }
                },
            )
            .unwrap();
        let newest_loudness = newest_cycle
            .snapshot()
            .present_loudness(EbuLoudnessPresentationConfig::default());
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        let expected_scaled = |value: Option<ScaledLoudnessValue>| {
            value.map_or_else(
                || "— LUFS".to_owned(),
                |value| {
                    let prefix = match value.region() {
                        LoudnessScaleRegion::Below => "<",
                        LoudnessScaleRegion::OnScale => "",
                        LoudnessScaleRegion::Above => ">",
                    };
                    format!("{prefix}{:.1} LUFS", value.displayed())
                },
            )
        };
        let expected_true_peak = newest_loudness.maximum_true_peak().map_or_else(
            || "— dBTP".to_owned(),
            |value| {
                value.displayed_dbtp().map_or_else(
                    || "−∞ dBTP".to_owned(),
                    |displayed| format!("{displayed:.1} dBTP"),
                )
            },
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 144960 · 1 coalesced · 0 dropped"
        );
        assert_eq!(ui.get_loudness_state().as_str(), "RUNNING");
        assert_eq!(
            ui.get_loudness_momentary().as_str(),
            expected_scaled(newest_loudness.momentary())
        );
        assert_eq!(
            ui.get_loudness_short_term().as_str(),
            expected_scaled(newest_loudness.short_term())
        );
        assert_eq!(
            ui.get_loudness_integrated().as_str(),
            expected_scaled(newest_loudness.integrated())
        );
        assert_eq!(
            ui.get_loudness_range().as_str(),
            newest_loudness
                .loudness_range_lu()
                .map_or_else(|| "— LU".to_owned(), |value| format!("{value:.1} LU"))
        );
        assert_eq!(
            ui.get_loudness_maximum_true_peak().as_str(),
            expected_true_peak
        );
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:03.020");
        ui.invoke_loudness_standby_requested();
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoudnessState {
                state: LoudnessMeasurementState::Standby,
            })
        );
        callback.apply_loudness_command(LoudnessMeasurementCommand::SetState(
            LoudnessMeasurementState::Standby,
        ));
        let standby_cycle = transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        let standby_loudness = standby_cycle
            .snapshot()
            .present_loudness(EbuLoudnessPresentationConfig::default());
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        assert_eq!(ui.get_loudness_state().as_str(), "STANDBY");
        assert!(ui.get_loudness_run_enabled());
        assert!(!ui.get_loudness_standby_enabled());
        assert!(ui.get_loudness_reset_enabled());
        assert_eq!(
            ui.get_loudness_momentary().as_str(),
            expected_scaled(standby_loudness.momentary())
        );
        assert_eq!(
            ui.get_loudness_short_term().as_str(),
            expected_scaled(standby_loudness.short_term())
        );
        assert_eq!(
            ui.get_loudness_integrated().as_str(),
            expected_scaled(standby_loudness.integrated())
        );
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:03.020");

        let loudness_before_peak_reset = (
            ui.get_loudness_momentary(),
            ui.get_loudness_short_term(),
            ui.get_loudness_integrated(),
            ui.get_loudness_range(),
            ui.get_loudness_maximum_true_peak(),
            ui.get_loudness_programme_time(),
            ui.get_loudness_lra_status(),
        );
        let authority_before_peak_reset = loudness_authority.get();
        ui.invoke_reset_output_peak_requested();
        assert_eq!(loudness_authority.get(), authority_before_peak_reset);
        assert_eq!(
            loudness_before_peak_reset,
            (
                ui.get_loudness_momentary(),
                ui.get_loudness_short_term(),
                ui.get_loudness_integrated(),
                ui.get_loudness_range(),
                ui.get_loudness_maximum_true_peak(),
                ui.get_loudness_programme_time(),
                ui.get_loudness_lra_status(),
            )
        );
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );

        ui.invoke_begin_loudness_reset();
        assert!(ui.get_loudness_reset_confirming());
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        ui.invoke_cancel_loudness_reset();
        assert!(!ui.get_loudness_reset_confirming());
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        ui.invoke_begin_loudness_reset();
        let peaks_before_programme_reset = (
            ui.get_left_output_reading(),
            ui.get_left_output_peak_reading(),
            ui.get_right_output_reading(),
            ui.get_right_output_peak_reading(),
            ui.get_output_overload_latched(),
        );
        ui.invoke_confirm_loudness_reset();
        assert!(!ui.get_loudness_reset_confirming());
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Ok(OperatorCommand::ResetLoudness)
        );
        assert_eq!(
            peaks_before_programme_reset,
            (
                ui.get_left_output_reading(),
                ui.get_left_output_peak_reading(),
                ui.get_right_output_reading(),
                ui.get_right_output_peak_reading(),
                ui.get_output_overload_latched(),
            )
        );
        assert_eq!(ui.get_loudness_state().as_str(), "STANDBY");
        assert_eq!(
            loudness_before_peak_reset,
            (
                ui.get_loudness_momentary(),
                ui.get_loudness_short_term(),
                ui.get_loudness_integrated(),
                ui.get_loudness_range(),
                ui.get_loudness_maximum_true_peak(),
                ui.get_loudness_programme_time(),
                ui.get_loudness_lra_status(),
            )
        );
        callback.apply_loudness_command(LoudnessMeasurementCommand::Reset);
        transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        assert_eq!(ui.get_loudness_state().as_str(), "STANDBY");
        assert_eq!(ui.get_loudness_integrated().as_str(), "— LUFS");
        assert_eq!(ui.get_loudness_range().as_str(), "— LU");
        assert_eq!(ui.get_loudness_programme_time().as_str(), "00:00:00.000");

        ui.invoke_loudness_run_requested();
        assert_eq!(
            loudness_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoudnessState {
                state: LoudnessMeasurementState::Running,
            })
        );
        callback.apply_loudness_command(LoudnessMeasurementCommand::SetState(
            LoudnessMeasurementState::Running,
        ));
        transport
            .process_interleaved_cycle(
                &mut callback,
                &mut scratch,
                &mut interleaved,
                &mut meter_sender,
            )
            .unwrap();
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        assert_eq!(ui.get_loudness_state().as_str(), "RUNNING");
        let accepted_loudness_after_run = (
            ui.get_loudness_momentary(),
            ui.get_loudness_short_term(),
            ui.get_loudness_integrated(),
            ui.get_loudness_range(),
            ui.get_loudness_maximum_true_peak(),
            ui.get_loudness_programme_time(),
            ui.get_loudness_lra_status(),
        );

        loudness_authority.set(Some(LoudnessMeasurementState::Running));
        let (saturated_loudness_sender, saturated_loudness_receiver) =
            std::sync::mpsc::sync_channel(1);
        saturated_loudness_sender
            .send(OperatorCommand::Start)
            .unwrap();
        let saturated_loudness_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_loudness_controls(
            ui,
            &saturated_loudness_sender,
            &saturated_loudness_diagnostic,
            &loudness_authority,
        );
        ui.invoke_loudness_standby_requested();
        assert_eq!(
            saturated_loudness_receiver.try_recv(),
            Ok(OperatorCommand::Start)
        );
        assert_eq!(
            saturated_loudness_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        assert!(
            saturated_loudness_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_eq!(
            accepted_loudness_after_run,
            (
                ui.get_loudness_momentary(),
                ui.get_loudness_short_term(),
                ui.get_loudness_integrated(),
                ui.get_loudness_range(),
                ui.get_loudness_maximum_true_peak(),
                ui.get_loudness_programme_time(),
                ui.get_loudness_lra_status(),
            )
        );

        let terminal_peak_measurements = (
            ui.get_left_output_level(),
            ui.get_left_output_reading(),
            ui.get_left_output_peak_reading(),
            ui.get_right_output_level(),
            ui.get_right_output_reading(),
            ui.get_right_output_peak_reading(),
            ui.get_output_overload_latched(),
            ui.get_output_meter_state(),
            ui.get_output_meter_telemetry(),
        );
        let terminal_loudness_measurements = (
            ui.get_loudness_state(),
            ui.get_loudness_momentary(),
            ui.get_loudness_short_term(),
            ui.get_loudness_integrated(),
            ui.get_loudness_range(),
            ui.get_loudness_maximum_true_peak(),
            ui.get_loudness_programme_time(),
            ui.get_loudness_lra_status(),
        );
        ui.invoke_begin_loudness_reset();
        assert!(ui.get_loudness_reset_confirming());
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            true,
        );
        for slot in 0..generated_mixer.row_count() {
            let row = generated_mixer.row_data(slot).unwrap();
            assert_operator_float(row.meter_left_level, 0.0);
            assert_operator_float(row.meter_right_level, 0.0);
            assert_eq!(row.meter_left_reading.as_str(), "— dBFS");
            assert_eq!(row.meter_right_reading.as_str(), "— dBFS");
        }
        assert_eq!(
            terminal_peak_measurements,
            (
                ui.get_left_output_level(),
                ui.get_left_output_reading(),
                ui.get_left_output_peak_reading(),
                ui.get_right_output_level(),
                ui.get_right_output_reading(),
                ui.get_right_output_peak_reading(),
                ui.get_output_overload_latched(),
                ui.get_output_meter_state(),
                ui.get_output_meter_telemetry(),
            )
        );
        assert_eq!(
            terminal_loudness_measurements,
            (
                ui.get_loudness_state(),
                ui.get_loudness_momentary(),
                ui.get_loudness_short_term(),
                ui.get_loudness_integrated(),
                ui.get_loudness_range(),
                ui.get_loudness_maximum_true_peak(),
                ui.get_loudness_programme_time(),
                ui.get_loudness_lra_status(),
            )
        );
        assert!(!ui.get_loudness_reset_confirming());
        assert_eq!(loudness_authority.get(), None);
        assert!(!ui.get_loudness_run_enabled());
        assert!(!ui.get_loudness_standby_enabled());
        assert!(!ui.get_loudness_reset_enabled());
        ui.invoke_loudness_run_requested();
        ui.invoke_loudness_standby_requested();
        ui.invoke_begin_loudness_reset();
        ui.invoke_confirm_loudness_reset();
        assert!(!ui.get_loudness_reset_confirming());
        assert_eq!(
            saturated_loudness_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            true,
        );
        assert_eq!(loudness_authority.get(), None);
        apply_operator_mixer_observation(
            ui,
            &mixer_model,
            &mixer_send_model,
            &mixer_channels,
            &mixer_sends,
            &mixer_authority,
            &recovery.snapshot(),
            true,
        );
        assert_eq!(
            ui.get_mixer_state().as_str(),
            "Final committed mixer · Runtime stopped"
        );
        assert!(!ui.get_main_muted());
        assert!(!ui.get_main_mute_enabled());
        assert!(!generated_mixer.row_data(0).unwrap().muted);
        assert!(!generated_mixer.row_data(0).unwrap().controls_enabled);
        assert_operator_float(generated_sends.row_data(0).unwrap().gain_percent, 80.0);
        assert!(generated_sends.row_data(0).unwrap().enabled);
        assert!(!generated_sends.row_data(0).unwrap().controls_enabled);
        ui.invoke_set_mixer_channel_gain_requested(0, 50.0);
        ui.invoke_set_mixer_channel_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_channel_mute_requested(0);
        ui.invoke_toggle_mixer_channel_solo_requested(1);
        ui.invoke_set_mixer_send_gain_requested(0, 50.0);
        ui.invoke_set_mixer_send_pan_requested(0, 25.0);
        ui.invoke_toggle_mixer_send_requested(0);
        ui.invoke_set_main_gain_requested(50.0);
        ui.invoke_toggle_main_mute_requested();
        assert_eq!(
            saturated_mixer_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );

        transport_sender.try_send(TransportCommand::Stop).unwrap();
        let stopped_with_range = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut mixer_left,
                    right: &mut mixer_right,
                },
            )
            .unwrap();
        recovery_publisher.publish_cycle(stopped_with_range, None);
        let before_clear = recovery.observe_cycle();
        assert_eq!(
            before_clear.state(),
            PlaybackRecoveryTransportState::Stopped
        );
        assert_eq!(
            before_clear.loop_range(),
            Some(TransportLoopRange::new(0, 1_500).unwrap())
        );
        assert!(before_clear.loop_enabled());
        apply_operator_loop_observation(
            ui,
            &loop_authority,
            before_clear,
            PlaybackExtent::Bounded { end_frame: 1_500 },
        );
        wire_operator_loop_range(ui, &loop_commands, &loop_diagnostic, &loop_authority);
        let before_clear_state = ui.get_loop_state();
        let before_clear_range = ui.get_loop_range();
        ui.invoke_clear_loop_range_requested();
        assert_eq!(
            loop_command_receiver.try_recv(),
            Ok(OperatorCommand::SetLoopRange { range: None })
        );
        assert_eq!(ui.get_loop_state(), before_clear_state);
        assert_eq!(ui.get_loop_range(), before_clear_range);
        transport_sender
            .try_send(TransportCommand::SetLoopRange(None))
            .unwrap();
        let cleared_cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut mixer_left,
                    right: &mut mixer_right,
                },
            )
            .unwrap();
        recovery_publisher.publish_cycle(cleared_cycle, None);
        let cleared = recovery.observe_cycle();
        assert_eq!(cleared.position_frame(), before_clear.position_frame());
        assert_eq!(cleared.state(), before_clear.state());
        assert_eq!(cleared.loop_range(), None);
        assert!(cleared.loop_enabled());
        apply_operator_loop_observation(
            ui,
            &loop_authority,
            cleared,
            PlaybackExtent::Bounded { end_frame: 1_500 },
        );
        assert_eq!(ui.get_loop_state().as_str(), "No session loop");
        assert_eq!(ui.get_loop_range().as_str(), "No committed session range");
        assert!(!ui.get_loop_toggle_enabled());
        assert!(!ui.get_loop_clear_enabled());

        let seek_authority = Rc::new(Cell::new(None));
        let (seek_commands, seek_receiver) =
            std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let seek_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_seek(ui, &seek_commands, &seek_diagnostic, &seek_authority);
        let before_seek = recovery.snapshot();
        apply_operator_seek_observation(
            ui,
            &seek_authority,
            PlaybackExtent::Bounded { end_frame: 1_500 },
            before_seek.position_frame(),
        );
        let committed_percent = ui.get_transport_position_percent();
        ui.invoke_locate_percent_requested(50.0);
        assert_operator_float(ui.get_transport_position_percent(), committed_percent);
        let locate = seek_receiver.try_recv().unwrap();
        assert_eq!(
            locate,
            OperatorCommand::Locate {
                frame: 750,
                state: LocateState::Preserve,
            }
        );
        let OperatorCommand::Locate {
            frame,
            state: LocateState::Preserve,
        } = locate
        else {
            unreachable!("the seek fixture emits one exact locate")
        };
        transport_sender.try_send(TransportCommand::Stop).unwrap();
        transport_sender
            .try_send(TransportCommand::Locate {
                frame,
                state: LocateState::Preserve,
            })
            .unwrap();
        let seek_cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut mixer_left,
                    right: &mut mixer_right,
                },
            )
            .unwrap();
        let committed_mixer = transport.mixer_parameter_state();
        recovery_publisher.publish_cycle(seek_cycle, Some(&committed_mixer));
        let sought = recovery.observe_cycle();
        assert_eq!(sought.position_frame(), 750);
        assert_eq!(sought.state(), PlaybackRecoveryTransportState::Stopped);
        apply_operator_seek_observation(
            ui,
            &seek_authority,
            PlaybackExtent::Bounded { end_frame: 1_500 },
            sought.position_frame(),
        );
        assert_operator_float(ui.get_transport_position_percent(), 50.0);

        ui.invoke_locate_percent_requested(25.0);
        let rolling_locate = seek_receiver.try_recv().unwrap();
        assert_eq!(
            rolling_locate,
            OperatorCommand::Locate {
                frame: 375,
                state: LocateState::Preserve,
            }
        );
        let OperatorCommand::Locate {
            frame,
            state: LocateState::Preserve,
        } = rolling_locate
        else {
            unreachable!("the rolling seek fixture emits one exact locate")
        };
        transport_sender.try_send(TransportCommand::Start).unwrap();
        transport_sender
            .try_send(TransportCommand::Locate {
                frame,
                state: LocateState::Preserve,
            })
            .unwrap();
        let rolling_seek_cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut mixer_left,
                    right: &mut mixer_right,
                },
            )
            .unwrap();
        let committed_mixer = transport.mixer_parameter_state();
        recovery_publisher.publish_cycle(rolling_seek_cycle, Some(&committed_mixer));
        let rolling_sought = recovery.observe_cycle();
        assert_eq!(rolling_sought.position_frame(), 855);
        assert_eq!(
            rolling_sought.state(),
            PlaybackRecoveryTransportState::Rolling
        );
        apply_operator_seek_observation(
            ui,
            &seek_authority,
            PlaybackExtent::Bounded { end_frame: 1_500 },
            rolling_sought.position_frame(),
        );
        assert_operator_float(ui.get_transport_position_percent(), 57.0);
        let seek_observers = RuntimeObservers {
            facts: RuntimeFacts {
                bundle_identity: "seek-fixture".to_owned(),
                deployment_generation: 1,
                recovered_from: None,
                project_name: "Seek fixture".to_owned(),
                project_id: PROJECT_ID.to_owned(),
                project_revision: 1,
                output_route: RuntimeOutputRoute::SystemDefault,
                input_route: RuntimeInputRoute::Disabled,
                playback_extent: PlaybackExtent::Bounded { end_frame: 1_500 },
                musical_timeline: runtime_musical_timeline.clone(),
                timeline_markers: Arc::from([]),
            },
            audio: test_audio_observers(recovery.clone()),
            meter_receivers: RuntimeMeterReceivers::default(),
            panel_connected: Arc::new(AtomicBool::new(false)),
            recording: test_recording_observation(),
            playback_available: Arc::new(AtomicBool::new(true)),
            automation_capture: test_automation_capture_observation(),
            mixer_update: test_mixer_update_observation(),
            native_device_update: test_native_device_update_observation(),
            loop_update: test_loop_update_observation(),
            scene_performance: test_scene_performance_observation(),
        };
        let _ = apply_observations(
            ui,
            &seek_observers,
            seek_observers.audio.snapshot().output_status.state(),
            &seek_diagnostic,
        );
        assert_eq!(ui.get_transport_frame().as_str(), "855");
        assert_eq!(ui.get_transport_musical_position().as_str(), "1|1|034");
        let committed_frame = ui.get_transport_frame();
        revoke_operator_seek_authority(ui, &seek_authority);
        assert_eq!(seek_authority.get(), None);
        assert!(!ui.get_transport_seek_enabled());
        assert_eq!(ui.get_transport_frame(), committed_frame);
        assert_operator_float(ui.get_transport_position_percent(), 57.0);

        drop(meter_sender);
        apply_operator_meter_observation(
            ui,
            &mut meter_observer.borrow_mut(),
            &loudness_authority,
            false,
        );
        assert_eq!(
            ui.get_output_meter_state().as_str(),
            "Final meter snapshot · producer ended"
        );
        assert_eq!(
            ui.get_output_meter_telemetry().as_str(),
            "meter frame 146400 · 1 coalesced · 0 dropped"
        );
        assert_eq!(ui.get_loudness_state().as_str(), "RUNNING");
        assert_eq!(loudness_authority.get(), None);
        assert!(!ui.get_loudness_run_enabled());
        assert!(!ui.get_loudness_standby_enabled());
        assert!(!ui.get_loudness_reset_enabled());
        ui.invoke_loudness_reset_requested();
        assert_eq!(
            saturated_loudness_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );

        let mismatched_model = Rc::new(VecModel::from(operator_mixer_rows(&mixer_channels)));
        let mut mismatched_row = mismatched_model.row_data(0).unwrap();
        mismatched_row.index = 99;
        mismatched_row.controls_enabled = true;
        mismatched_row.meter_left_level = 0.75;
        mismatched_row.meter_right_level = 0.5;
        mismatched_row.meter_left_reading = "-2.5 dBFS".into();
        mismatched_row.meter_right_reading = "-6.0 dBFS".into();
        mismatched_model.set_row_data(0, mismatched_row);
        ui.set_mixer_channels(ModelRc::from(Rc::clone(&mismatched_model)));

        apply_operator_strip_meter_observation(ui, Some(strip_meters), false);

        let cleared = mismatched_model.row_data(0).unwrap();
        assert!(cleared.controls_enabled);
        assert_operator_float(cleared.meter_left_level, 0.0);
        assert_operator_float(cleared.meter_right_level, 0.0);
        assert_eq!(cleared.meter_left_reading.as_str(), "— dBFS");
        assert_eq!(cleared.meter_right_reading.as_str(), "— dBFS");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear test proves generated command and authority wiring"
    )]
    fn operator_ui_wires_commands_and_authoritative_properties() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = OperatorWindow::new().unwrap();
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let diagnostic = Arc::new(Mutex::new("authoritative diagnostic".to_owned()));
        let stop = Arc::new(AtomicBool::new(false));
        let transport_authority = Rc::new(Cell::new(None));
        wire_operator_transport_controls(&ui, &sender, &diagnostic, &transport_authority);
        wire_operator_quit(&ui, &sender, &stop, &diagnostic);
        let loop_authority = Rc::new(Cell::new(None));
        let metronome_authority = Rc::new(Cell::new(None));
        let seek_authority = Rc::new(Cell::new(None));
        let native_device_authority = Rc::new(RefCell::new(None));
        wire_operator_loop_toggle(&ui, &sender, &diagnostic, &loop_authority);
        wire_operator_loop_range(&ui, &sender, &diagnostic, &loop_authority);
        wire_operator_metronome_toggle(&ui, &sender, &diagnostic, &metronome_authority);
        wire_operator_metronome_level(&ui, &sender, &diagnostic, &metronome_authority);
        wire_operator_seek(&ui, &sender, &diagnostic, &seek_authority);

        let facts = RuntimeFacts {
            bundle_identity: "f".repeat(64),
            deployment_generation: u64::MAX,
            recovered_from: Some("e".repeat(64)),
            project_name: "Night Drive".to_owned(),
            project_id: "project-night-drive".to_owned(),
            project_revision: u64::MAX - 1,
            output_route: RuntimeOutputRoute::Pinned {
                display_name: "Studio Output".to_owned(),
            },
            input_route: RuntimeInputRoute::Disabled,
            playback_extent: PlaybackExtent::Bounded { end_frame: 1_500 },
            musical_timeline: test_musical_timeline(),
            timeline_markers: Arc::from([]),
        };
        let observers = RuntimeObservers {
            facts: facts.clone(),
            audio: test_audio_observers(PlaybackRecoveryCheckpoint::default()),
            meter_receivers: RuntimeMeterReceivers::default(),
            panel_connected: Arc::new(AtomicBool::new(false)),
            recording: test_recording_observation(),
            playback_available: Arc::new(AtomicBool::new(true)),
            automation_capture: test_automation_capture_observation(),
            mixer_update: test_mixer_update_observation(),
            native_device_update: test_native_device_update_observation(),
            loop_update: test_loop_update_observation(),
            scene_performance: test_scene_performance_observation(),
        };
        apply_static_facts(&ui, &facts);
        assert_eq!(ui.get_output_route().as_str(), "Pinned: Studio Output");
        let observation = apply_observations(
            &ui,
            &observers,
            observers.audio.snapshot().output_status.state(),
            &diagnostic,
        );
        apply_operator_transport_observation(&ui, &transport_authority, observation.state());
        assert_eq!(ui.get_transport_musical_position().as_str(), "1|1|000");
        assert_eq!(
            transport_authority.get(),
            Some(PlaybackRecoveryTransportState::Stopped)
        );
        assert!(ui.get_transport_start_enabled());
        assert!(!ui.get_transport_stop_enabled());
        assert!(ui.get_transport_return_to_start_enabled());
        let committed_transport_controls = (
            ui.get_transport_start_enabled(),
            ui.get_transport_stop_enabled(),
            ui.get_transport_return_to_start_enabled(),
        );
        let (saturated_transport_sender, saturated_transport_receiver) =
            std::sync::mpsc::sync_channel(1);
        saturated_transport_sender
            .send(OperatorCommand::Quit)
            .unwrap();
        let saturated_transport_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_transport_controls(
            &ui,
            &saturated_transport_sender,
            &saturated_transport_diagnostic,
            &transport_authority,
        );
        ui.invoke_start_requested();
        assert_eq!(
            saturated_transport_receiver.try_recv(),
            Ok(OperatorCommand::Quit)
        );
        assert_eq!(
            saturated_transport_receiver.try_recv(),
            Err(TryRecvError::Empty)
        );
        assert!(
            saturated_transport_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_eq!(
            (
                ui.get_transport_start_enabled(),
                ui.get_transport_stop_enabled(),
                ui.get_transport_return_to_start_enabled(),
            ),
            committed_transport_controls
        );

        apply_operator_transport_observation(
            &ui,
            &transport_authority,
            PlaybackRecoveryTransportState::Rolling,
        );
        let committed_transport_controls = (
            ui.get_transport_start_enabled(),
            ui.get_transport_stop_enabled(),
            ui.get_transport_return_to_start_enabled(),
        );
        let (lost_transport_sender, lost_transport_receiver) = std::sync::mpsc::sync_channel(1);
        drop(lost_transport_receiver);
        let lost_transport_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_transport_controls(
            &ui,
            &lost_transport_sender,
            &lost_transport_diagnostic,
            &transport_authority,
        );
        ui.invoke_stop_requested();
        assert!(
            lost_transport_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_eq!(
            (
                ui.get_transport_start_enabled(),
                ui.get_transport_stop_enabled(),
                ui.get_transport_return_to_start_enabled(),
            ),
            committed_transport_controls
        );
        apply_operator_transport_observation(
            &ui,
            &transport_authority,
            PlaybackRecoveryTransportState::Stopped,
        );
        wire_operator_transport_controls(&ui, &sender, &diagnostic, &transport_authority);
        apply_operator_loop_observation(&ui, &loop_authority, observation, facts.playback_extent);
        apply_operator_seek_observation(
            &ui,
            &seek_authority,
            facts.playback_extent,
            observation.position_frame(),
        );
        assert_eq!(ui.get_loop_state().as_str(), "No session loop");
        assert_eq!(ui.get_loop_range().as_str(), "No committed session range");
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Loop unavailable");
        assert!(!ui.get_loop_toggle_enabled());
        assert!(ui.get_loop_in_enabled());
        assert!(!ui.get_loop_out_enabled());
        assert!(!ui.get_loop_clear_enabled());
        let editable_range = TransportLoopRange::new(0, 1_500).unwrap();
        loop_authority.set(Some(OperatorLoopAuthority {
            range: Some(editable_range),
            enabled: false,
            position_frame: 375,
            extent: facts.playback_extent,
        }));
        ui.invoke_set_loop_in_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetLoopRange {
                range: Some(TransportLoopRange::new(375, 1_500).unwrap())
            })
        );
        loop_authority.set(Some(OperatorLoopAuthority {
            range: Some(editable_range),
            enabled: false,
            position_frame: 1_200,
            extent: facts.playback_extent,
        }));
        ui.invoke_set_loop_out_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetLoopRange {
                range: Some(TransportLoopRange::new(0, 1_200).unwrap())
            })
        );
        ui.invoke_clear_loop_range_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetLoopRange { range: None })
        );
        apply_operator_loop_observation(&ui, &loop_authority, observation, facts.playback_extent);
        assert!(ui.get_transport_seek_enabled());
        assert_operator_float(ui.get_transport_position_percent(), 0.0);
        ui.invoke_locate_percent_requested(50.0);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::Locate {
                frame: 750,
                state: LocateState::Preserve,
            })
        );
        assert_operator_float(ui.get_transport_position_percent(), 0.0);
        apply_operator_seek_observation(&ui, &seek_authority, facts.playback_extent, 375);
        assert_operator_float(ui.get_transport_position_percent(), 25.0);
        ui.invoke_locate_percent_requested(f32::NAN);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "transport locate refused: position is outside 0..=100%"
        );

        let (full_seek_sender, full_seek_receiver) = std::sync::mpsc::sync_channel(1);
        full_seek_sender.send(OperatorCommand::Start).unwrap();
        let full_seek_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_seek(
            &ui,
            &full_seek_sender,
            &full_seek_diagnostic,
            &seek_authority,
        );
        ui.invoke_locate_percent_requested(50.0);
        assert_eq!(full_seek_receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(full_seek_receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            full_seek_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_operator_float(ui.get_transport_position_percent(), 25.0);

        let (lost_seek_sender, lost_seek_receiver) = std::sync::mpsc::sync_channel(1);
        drop(lost_seek_receiver);
        let lost_seek_diagnostic = Arc::new(Mutex::new(String::new()));
        wire_operator_seek(
            &ui,
            &lost_seek_sender,
            &lost_seek_diagnostic,
            &seek_authority,
        );
        ui.invoke_locate_percent_requested(50.0);
        assert!(
            lost_seek_diagnostic
                .lock()
                .unwrap()
                .contains("operator command refused")
        );
        assert_operator_float(ui.get_transport_position_percent(), 25.0);
        wire_operator_seek(&ui, &sender, &diagnostic, &seek_authority);

        apply_operator_seek_observation(
            &ui,
            &seek_authority,
            PlaybackExtent::Bounded { end_frame: 0 },
            0,
        );
        assert_eq!(seek_authority.get(), None);
        assert!(!ui.get_transport_seek_enabled());
        apply_operator_seek_observation(&ui, &seek_authority, PlaybackExtent::Unbounded, 0);
        assert_eq!(seek_authority.get(), None);
        assert_eq!(
            ui.get_transport_seek_state().as_str(),
            "Unbounded playback · seek unavailable"
        );
        apply_operator_seek_observation(
            &ui,
            &seek_authority,
            facts.playback_extent,
            observation.position_frame(),
        );
        *diagnostic.lock().unwrap() = "authoritative diagnostic".to_owned();
        assert_generated_operator_meter_contract(&ui);

        let terminal_range = TransportLoopRange::new(120, 360).unwrap();
        let (_publisher, terminal_recovery) =
            hyper_mixer_runtime::playback_recovery_channel_with_initial_loop_and_metronome(
                None,
                None,
                Some(terminal_range),
                true,
                hyper_mixer_runtime::InitialMetronomeState::prepared(
                    false,
                    MetronomeGain::from_millionths(725_000).unwrap(),
                ),
            );
        let terminal_observers = RuntimeObservers {
            facts: facts.clone(),
            audio: test_audio_observers(terminal_recovery),
            meter_receivers: RuntimeMeterReceivers::default(),
            panel_connected: Arc::new(AtomicBool::new(false)),
            recording: test_recording_observation(),
            playback_available: Arc::new(AtomicBool::new(true)),
            automation_capture: test_automation_capture_observation(),
            mixer_update: test_mixer_update_observation(),
            native_device_update: test_native_device_update_observation(),
            loop_update: test_loop_update_observation(),
            scene_performance: test_scene_performance_observation(),
        };
        let terminal_operator_scenes = [OperatorScene {
            index: 0,
            name: "Terminal scene authority".to_owned(),
            availability: DesktopSceneAvailability::Ready,
        }];
        let terminal_scenes = Rc::new(VecModel::from(operator_scene_rows(
            &terminal_operator_scenes,
        )));
        let terminal_scene_launch_authority = Rc::new(Cell::new(false));
        let terminal_scene_selection = Rc::new(Cell::new(None));
        let terminal_scene_stop_authority = Rc::new(Cell::new(false));
        let terminal_timeline_marker_model = Rc::new(VecModel::from(
            operator_timeline_marker_rows(&facts.timeline_markers),
        ));
        let terminal_timeline_marker_authority = Rc::new(RefCell::new(None));
        wire_operator_loop_toggle(&ui, &sender, &diagnostic, &loop_authority);
        wire_operator_scene_launch(
            &ui,
            &sender,
            &diagnostic,
            &terminal_operator_scenes,
            &terminal_scene_launch_authority,
            &terminal_scenes,
            &terminal_scene_selection,
        );
        wire_operator_scene_stop(&ui, &sender, &diagnostic, &terminal_scene_stop_authority);
        apply_operator_cycle_observation(
            &ui,
            &terminal_observers,
            terminal_observers.audio.snapshot().output_status.state(),
            &diagnostic,
            &transport_authority,
            &loop_authority,
            &metronome_authority,
            &seek_authority,
            &terminal_timeline_marker_model,
            &terminal_timeline_marker_authority,
            &terminal_scenes,
            &terminal_scene_launch_authority,
            &terminal_scene_selection,
            &terminal_scene_stop_authority,
            false,
        );
        assert_eq!(
            loop_authority.get(),
            Some(OperatorLoopAuthority {
                range: Some(terminal_range),
                enabled: true,
                position_frame: 0,
                extent: facts.playback_extent,
            })
        );
        assert_eq!(
            metronome_authority.get(),
            Some(OperatorMetronomeAuthority {
                available: true,
                enabled: false,
                gain: MetronomeGain::from_millionths(725_000).unwrap(),
            })
        );
        assert_eq!(ui.get_metronome_state().as_str(), "Committed off");
        assert!(ui.get_metronome_toggle_enabled());
        assert!(ui.get_metronome_gain_enabled());
        assert_operator_float(ui.get_metronome_gain_percent(), 72.5);
        ui.invoke_set_metronome_gain_requested(75.25);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetMetronomeGain {
                gain: MetronomeGain::from_millionths(752_500).unwrap(),
            })
        );
        assert_operator_float(ui.get_metronome_gain_percent(), 72.5);
        ui.invoke_set_metronome_gain_requested(100.1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "metronome level refused: value is outside 0..=100%"
        );
        ui.invoke_toggle_metronome_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::SetMetronomeEnabled { enabled: true })
        );
        assert!(terminal_scene_launch_authority.get());
        assert!(terminal_scenes.row_data(0).unwrap().controls_enabled);
        ui.invoke_launch_scene_requested(0);
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::LaunchScene { scene_index: 0 })
        );
        assert!(terminal_scene_stop_authority.get());
        assert!(ui.get_stop_interactive_scenes_enabled());
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(
            receiver.try_recv(),
            Ok(OperatorCommand::StopInteractiveScenes)
        );
        revoke_operator_loop_authority(&ui, &loop_authority);
        apply_operator_cycle_observation(
            &ui,
            &terminal_observers,
            terminal_observers.audio.snapshot().output_status.state(),
            &diagnostic,
            &transport_authority,
            &loop_authority,
            &metronome_authority,
            &seek_authority,
            &terminal_timeline_marker_model,
            &terminal_timeline_marker_authority,
            &terminal_scenes,
            &terminal_scene_launch_authority,
            &terminal_scene_selection,
            &terminal_scene_stop_authority,
            true,
        );
        assert_eq!(loop_authority.get(), None);
        assert_eq!(metronome_authority.get(), None);
        assert_eq!(transport_authority.get(), None);
        assert!(!ui.get_transport_start_enabled());
        assert!(!ui.get_transport_stop_enabled());
        assert!(!ui.get_transport_return_to_start_enabled());
        assert!(!ui.get_metronome_toggle_enabled());
        assert!(!ui.get_metronome_gain_enabled());
        assert_operator_float(ui.get_metronome_gain_percent(), 0.0);
        assert!(!terminal_scene_launch_authority.get());
        assert!(!terminal_scenes.row_data(0).unwrap().controls_enabled);
        assert!(!terminal_scene_stop_authority.get());
        assert!(!ui.get_stop_interactive_scenes_enabled());
        ui.invoke_launch_scene_requested(0);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "scene launch refused: accepted scene state is unavailable"
        );
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "interactive scene stop refused: accepted scene state is unavailable"
        );
        apply_operator_seek_observation(&ui, &seek_authority, facts.playback_extent, 375);
        assert_operator_float(ui.get_transport_position_percent(), 25.0);
        revoke_operator_seek_authority(&ui, &seek_authority);
        assert_eq!(ui.get_loop_toggle_label().as_str(), "Runtime stopped");
        assert!(!ui.get_loop_toggle_enabled());
        assert_eq!(seek_authority.get(), None);
        assert!(!ui.get_transport_seek_enabled());
        assert_operator_float(ui.get_transport_position_percent(), 25.0);
        ui.invoke_locate_percent_requested(50.0);
        ui.invoke_toggle_loop_requested();
        ui.invoke_set_loop_in_requested();
        ui.invoke_set_loop_out_requested();
        ui.invoke_clear_loop_range_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));

        ui.invoke_start_requested();
        ui.invoke_stop_requested();
        ui.invoke_toggle_transport_requested();
        ui.invoke_return_to_start_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "transport return to start refused: coherent transport state is unavailable"
        );

        apply_operator_transport_observation(
            &ui,
            &transport_authority,
            PlaybackRecoveryTransportState::Stopped,
        );
        ui.invoke_start_requested();
        ui.invoke_toggle_transport_requested();
        ui.invoke_return_to_start_requested();
        apply_operator_transport_observation(
            &ui,
            &transport_authority,
            PlaybackRecoveryTransportState::Rolling,
        );
        ui.invoke_stop_requested();
        ui.invoke_quit_requested();

        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::ReturnToStart));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Stop));
        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Quit));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(ui.get_project_name().as_str(), "Night Drive");
        assert_eq!(ui.get_project_identity().as_str(), "project-night-drive");
        assert_eq!(
            ui.get_project_revision().as_str(),
            (u64::MAX - 1).to_string()
        );
        assert_eq!(
            ui.get_deployment_generation().as_str(),
            u64::MAX.to_string()
        );
        assert_eq!(ui.get_bundle_identity().as_str(), "f".repeat(64));
        assert!(ui.get_recovery_state().contains(&"e".repeat(64)));
        assert_eq!(ui.get_transport_state().as_str(), "Stopped");
        assert_eq!(ui.get_transport_time().as_str(), "00:00:00.000");
        assert_eq!(ui.get_transport_frame().as_str(), "0");
        assert_eq!(ui.get_stream_state().as_str(), "Starting");
        assert_eq!(ui.get_callback_count().as_str(), "0");
        assert_eq!(ui.get_stream_failures().as_str(), "0");
        assert_eq!(ui.get_native_xruns().as_str(), "unavailable");
        assert_eq!(ui.get_panel_state().as_str(), "Disconnected");
        assert_eq!(ui.get_diagnostic().as_str(), "authoritative diagnostic");

        let (completion_sender, completion_receiver) = std::sync::mpsc::sync_channel(1);
        let surface_closed = Cell::new(false);
        assert_eq!(
            apply_operator_worker_completion(
                &ui,
                &completion_receiver,
                &diagnostic,
                &transport_authority,
                &terminal_scenes,
                &terminal_scene_launch_authority,
                &terminal_scene_stop_authority,
                || {
                    surface_closed.set(true);
                    Ok(())
                },
            ),
            None
        );
        assert!(!surface_closed.get());
        apply_operator_transport_observation(
            &ui,
            &transport_authority,
            PlaybackRecoveryTransportState::Stopped,
        );
        assert!(ui.get_transport_start_enabled());
        set_operator_scene_launch_authority(
            &ui,
            &terminal_scenes,
            &terminal_scene_launch_authority,
            &terminal_scene_selection,
        );
        assert!(terminal_scene_launch_authority.get());
        completion_sender
            .send(Err("native audio ownership ended unexpectedly".to_owned()))
            .unwrap();
        assert_eq!(
            apply_operator_worker_completion(
                &ui,
                &completion_receiver,
                &diagnostic,
                &transport_authority,
                &terminal_scenes,
                &terminal_scene_launch_authority,
                &terminal_scene_stop_authority,
                || {
                    surface_closed.set(true);
                    Ok(())
                },
            ),
            Some(Ok(()))
        );
        assert!(surface_closed.get());
        assert_eq!(transport_authority.get(), None);
        assert!(!ui.get_transport_start_enabled());
        assert!(!ui.get_transport_stop_enabled());
        assert!(!ui.get_transport_return_to_start_enabled());
        assert!(!terminal_scene_launch_authority.get());
        assert!(!terminal_scenes.row_data(0).unwrap().controls_enabled);
        assert_eq!(
            ui.get_diagnostic().as_str(),
            "Runtime worker stopped: native audio ownership ended unexpectedly"
        );
        ui.invoke_toggle_transport_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "transport toggle refused: coherent transport state is unavailable"
        );

        let (lost_sender, lost_receiver) = std::sync::mpsc::sync_channel(1);
        drop(lost_sender);
        surface_closed.set(false);
        set_operator_scene_launch_authority(
            &ui,
            &terminal_scenes,
            &terminal_scene_launch_authority,
            &terminal_scene_selection,
        );
        terminal_scene_stop_authority.set(true);
        ui.set_stop_interactive_scenes_enabled(true);
        let retained_scene_feedback = ui.get_scene_feedback();
        assert_eq!(
            apply_operator_worker_completion(
                &ui,
                &lost_receiver,
                &diagnostic,
                &transport_authority,
                &terminal_scenes,
                &terminal_scene_launch_authority,
                &terminal_scene_stop_authority,
                || {
                    surface_closed.set(true);
                    Err("adapter refused to hide".to_owned())
                },
            ),
            Some(Err("adapter refused to hide".to_owned()))
        );
        assert!(surface_closed.get());
        assert!(!terminal_scene_launch_authority.get());
        assert!(!terminal_scenes.row_data(0).unwrap().controls_enabled);
        assert!(!terminal_scene_stop_authority.get());
        assert!(!ui.get_stop_interactive_scenes_enabled());
        ui.invoke_stop_interactive_scenes_requested();
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            ui.get_scene_feedback().as_str(),
            retained_scene_feedback.as_str()
        );
        assert_eq!(
            ui.get_diagnostic().as_str(),
            "Runtime worker ended without publishing its result; adapter refused to hide"
        );
        let rows = Rc::new(VecModel::from(vec![OperatorNativeDeviceRow {
            track_name: "Audio".into(),
            kind: "Filter".into(),
            enabled: true,
            controls_enabled: false,
            kind_index: 0,
            filter_mode_index: 0,
            filter_cutoff_hz: 0.0,
            filter_resonance: 0.0,
            compressor_threshold_decibels: 0.0,
            compressor_ratio: 0.0,
            compressor_attack_milliseconds: 0.0,
            compressor_release_milliseconds: 0.0,
            compressor_makeup_decibels: 0.0,
        }]));
        ui.set_native_devices(ModelRc::from(Rc::clone(&rows)));
        wire_operator_native_device_controls(
            &ui,
            &sender,
            &diagnostic,
            &rows,
            &native_device_authority,
        );

        ui.invoke_toggle_native_device_requested(0);

        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(
            diagnostic.lock().unwrap().as_str(),
            "native device bypass refused: accepted state is unavailable"
        );
    }

    #[test]
    fn operator_shutdown_waits_for_the_worker_it_signals() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = Arc::clone(&joined);
        let worker = std::thread::spawn(move || {
            assert_eq!(receiver.recv(), Ok(OperatorCommand::Quit));
            worker_joined.store(true, Ordering::Release);
            Ok(())
        });

        finish_operator_worker(Ok(()), &stop, &sender, worker).unwrap();

        assert!(stop.load(Ordering::Acquire));
        assert!(
            joined.load(Ordering::Acquire),
            "the join observed worker exit"
        );
    }

    #[test]
    fn operator_terminal_results_keep_worker_failure_primary() {
        assert_eq!(reconcile_operator_results(Ok(()), Ok(())), Ok(()));
        assert_eq!(
            reconcile_operator_results(Err("UI failed".to_owned()), Ok(())),
            Err("UI failed".to_owned())
        );
        assert_eq!(
            reconcile_operator_results(Ok(()), Err("audio owner failed".to_owned())),
            Err("audio owner failed".to_owned())
        );
        assert_eq!(
            reconcile_operator_results(
                Err("UI failed".to_owned()),
                Err("audio owner failed".to_owned())
            ),
            Err("audio owner failed; operator UI shutdown also failed: UI failed".to_owned())
        );

        let close_error = Mutex::new(Some("surface adapter refused hide".to_owned()));
        assert_eq!(
            reconcile_operator_ui_close(Ok(()), &close_error),
            Err("surface adapter refused hide".to_owned())
        );
        let close_error = Mutex::new(Some("surface adapter refused hide".to_owned()));
        assert_eq!(
            reconcile_operator_ui_close(Err("event loop failed".to_owned()), &close_error),
            Err(
                "event loop failed; operator surface shutdown also failed: surface adapter refused hide"
                    .to_owned()
            )
        );

        let stop = AtomicBool::new(false);
        let (sender, _receiver) = std::sync::mpsc::sync_channel(OPERATOR_COMMAND_CAPACITY);
        let worker =
            std::thread::spawn(|| -> Result<(), String> { panic!("worker panic evidence") });
        assert_eq!(
            finish_operator_worker(Err("UI failed".to_owned()), &stop, &sender, worker),
            Err("Runtime worker panicked during shutdown".to_owned())
        );
    }

    #[test]
    fn operator_surface_close_uses_checked_event_loop_fallback() {
        let quit_called = Cell::new(false);
        assert_eq!(
            close_operator_surface(
                || Err("surface adapter refused hide".to_owned()),
                || {
                    quit_called.set(true);
                    Ok(())
                }
            ),
            Err(
                "surface adapter refused hide; explicit event-loop termination was required"
                    .to_owned()
            )
        );
        assert!(quit_called.get());

        assert_eq!(
            close_operator_surface(
                || Err("surface adapter refused hide".to_owned()),
                || Err("event loop refused quit".to_owned())
            ),
            Err("surface adapter refused hide; event loop refused quit".to_owned())
        );

        quit_called.set(false);
        assert_eq!(
            close_operator_surface(
                || Ok(()),
                || {
                    quit_called.set(true);
                    Ok(())
                }
            ),
            Ok(())
        );
        assert!(!quit_called.get());
    }

    #[test]
    fn failed_operator_startup_joins_the_worker_and_returns_its_error() {
        let joined = Arc::new(AtomicBool::new(false));
        let worker_joined = Arc::clone(&joined);
        let worker = std::thread::spawn(move || {
            worker_joined.store(true, Ordering::Release);
            Err("verified deployment was rejected".to_owned())
        });

        let error =
            finish_failed_operator_startup("startup evidence channel closed".to_owned(), worker)
                .unwrap_err();

        assert_eq!(error, "verified deployment was rejected");
        assert!(joined.load(Ordering::Acquire));

        let panicked =
            std::thread::spawn(|| -> Result<(), String> { panic!("startup panic evidence") });
        assert_eq!(
            finish_failed_operator_startup("readiness disconnected".to_owned(), panicked),
            Err("Runtime worker panicked during startup".to_owned())
        );
    }

    #[test]
    fn a_full_operator_queue_refuses_without_replacing_the_queued_command() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.send(OperatorCommand::Start).unwrap();
        let diagnostic = Mutex::new("healthy".to_owned());

        send_operator_command(&sender, OperatorCommand::ReturnToStart, &diagnostic);

        assert_eq!(receiver.try_recv(), Ok(OperatorCommand::Start));
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(
            diagnostic
                .lock()
                .unwrap()
                .starts_with("operator command refused:")
        );
    }

    #[test]
    fn operator_observation_formatting_preserves_full_width_evidence() {
        let presentation = operator_observation_presentation(
            PlaybackRecoveryTransportState::Rolling,
            usize::MAX,
            &test_musical_timeline(),
            OutputStreamState::Failed,
            OutputStreamSnapshot {
                callback_count: u64::MAX,
                stream_error_count: u64::MAX - 1,
                native_xruns: Some(hyper_desktop_audio::NativeXrunSnapshot {
                    recovery_count: Some(u64::MAX - 2),
                    ..hyper_desktop_audio::NativeXrunSnapshot::default()
                }),
                ..OutputStreamSnapshot::default()
            },
            true,
            "stale diagnostic".to_owned(),
        );

        assert_eq!(presentation.transport_state, "Rolling");
        assert_eq!(
            presentation.transport_time,
            format_transport_time(usize::MAX as u64)
        );
        assert_eq!(presentation.transport_frame, usize::MAX.to_string());
        assert!(presentation.transport_musical_position.contains('|'));
        assert_eq!(presentation.stream_state, "Failed");
        assert_eq!(presentation.callback_count, u64::MAX.to_string());
        assert_eq!(presentation.stream_failures, (u64::MAX - 1).to_string());
        assert_eq!(presentation.native_xruns, (u64::MAX - 2).to_string());
        assert_eq!(presentation.panel_state, "Connected");
        assert_eq!(
            presentation.diagnostic,
            "native audio ownership ended unexpectedly"
        );
    }

    #[test]
    fn runtime_transport_clock_truncates_frames_without_wrapping_hours() {
        assert_eq!(format_transport_time(0), "00:00:00.000");
        assert_eq!(format_transport_time(47_999), "00:00:00.999");
        assert_eq!(format_transport_time(72_000), "00:00:01.500");
        assert_eq!(
            format_transport_time(48_000 * (3 * 3_600 + 2 * 60 + 1) + 24_000),
            "03:02:01.500"
        );
        assert_eq!(format_transport_time(u64::MAX), "106751991167:18:02.325");
    }

    #[test]
    fn runtime_output_meter_presentation_preserves_floor_and_overload_evidence() {
        assert_eq!(
            operator_meter_level_presentation(false, -20.0),
            (0.0, "— dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, f32::NAN),
            (0.0, "— dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, OUTPUT_METER_FLOOR_DBFS),
            (0.0, "≤ -90.0 dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, -45.0),
            (0.5, "-45.0 dBFS".to_owned())
        );
        assert_eq!(
            operator_meter_level_presentation(true, 3.0),
            (1.0, "3.0 dBFS".to_owned())
        );
        assert_eq!(
            operator_peak_hold_presentation(false, 3.0),
            ("hold —".to_owned(), false)
        );
        assert_eq!(
            operator_peak_hold_presentation(true, -12.5),
            ("hold -12.5".to_owned(), false)
        );
        assert_eq!(
            operator_peak_hold_presentation(true, 1.25),
            ("hold 1.2".to_owned(), true)
        );

        let overloaded = operator_meter_channel_values(true, -18.0, 1.25);
        assert!((overloaded.level - 0.8).abs() <= f32::EPSILON);
        assert_eq!(overloaded.reading, "-18.0 dBFS");
        assert_eq!(overloaded.peak_reading, "hold 1.2");
        assert!(overloaded.overload_latched);

        let (_meter_sender, meter_receiver) = hyper_mixer_runtime::output_meter_channel();
        let meter_config = SamplePeakBallisticsConfig::try_new(
            SAMPLE_RATE_HZ,
            OUTPUT_METER_FLOOR_DBFS,
            OUTPUT_METER_FALLOFF_DB_PER_SECOND,
            u64::MAX,
        )
        .unwrap();
        let mut observer = OperatorMeterObserver::new(meter_receiver, meter_config);
        observer.coalesced_snapshot_count = 7;
        observer.dropped_snapshot_count = 11;
        observer.sender_disconnected = true;
        observer.reset_peak_hold();
        assert_eq!(observer.coalesced_snapshot_count, 7);
        assert_eq!(observer.dropped_snapshot_count, 11);
        assert!(observer.sender_disconnected);
        assert!(observer.latest.is_none());
        assert!(observer.latest_strip_ballistics.is_none());

        let unavailable =
            operator_meter_presentation(None, false, u64::MAX, u64::MAX - 1, true, None);
        assert_eq!(unavailable.state, "Meter producer ended before a snapshot");
        assert!(!unavailable.overload_latched);
        assert_eq!(
            unavailable.telemetry,
            format!(
                "meter frame unavailable · {} coalesced · {} dropped",
                u64::MAX,
                u64::MAX - 1
            )
        );
    }
}
