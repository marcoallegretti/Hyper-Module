//! Runtime ownership of one bounded mixer-automation capture.
//!
//! The controller consumes the session's committed parameter receiver after an
//! explicit transfer. It has no project-writing authority: a completed capture
//! becomes a provenance-bound update in an external directory for the IDE to
//! verify and import.

use core::error::Error;
use core::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper_desktop_audio::{DesktopDirectControlError, DesktopPlaybackSession};
use hyper_mixer_runtime::{
    InteractiveMixerParameterObservation, InteractiveMixerParameterReceiver,
    PlaybackRecoverySnapshot, PlaybackRecoveryTransportState, TransportCommand,
};
use hyper_playback_compiler::{FrameTimeline, FrameTimelineError};
use hyper_playback_json::{
    AutomationChannelParameter, AutomationMainParameter, AutomationPoint, AutomationSendParameter,
    AutomationTarget, AutomationValue, CompiledMixerTrackKind, LoaderLimits,
};
use hyper_playback_session::{PlaybackAutomationTake, PreparedPlaybackSession};
use hyper_project_loader::PreparedProjectPlayback;
use hyper_runtime_automation_update::{
    MAX_RUNTIME_AUTOMATION_UPDATE_POINTS, RuntimeAutomationTarget, RuntimeAutomationUpdate,
    RuntimeAutomationUpdateDirectory, RuntimeAutomationUpdateError, RuntimeAutomationUpdateSource,
};
use uuid::Uuid;

/// One coherent transport observation used by automation capture finalization.
///
/// It contains only the facts needed by this controller, keeping production
/// session ownership narrow and controller tests independent of a native audio
/// device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AutomationCaptureRecovery {
    state: PlaybackRecoveryTransportState,
    position_frame: usize,
    committed_cycle_count: u64,
    committed_capture_start_sequence: u64,
    loop_enabled: bool,
}

impl From<PlaybackRecoverySnapshot> for AutomationCaptureRecovery {
    fn from(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            state: snapshot.state(),
            position_frame: snapshot.position_frame(),
            committed_cycle_count: snapshot.committed_cycle_count(),
            committed_capture_start_sequence: snapshot.committed_capture_start_sequence(),
            loop_enabled: snapshot.loop_enabled(),
        }
    }
}

/// Minimal Runtime session authority required by automation capture.
///
/// The production implementation remains the sole `DesktopPlaybackSession`
/// owner. The receiver is held by this controller, so neither an operator UI
/// nor a second consumer can race it for committed parameter evidence.
pub(super) trait RuntimeAutomationCaptureSession {
    fn automation_capture_recovery(&self) -> AutomationCaptureRecovery;

    fn try_start_automation_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError>;

    fn try_stop_automation_capture_transport(&mut self) -> Result<(), DesktopDirectControlError>;
}

impl RuntimeAutomationCaptureSession for DesktopPlaybackSession {
    fn automation_capture_recovery(&self) -> AutomationCaptureRecovery {
        self.recovery_checkpoint().snapshot().into()
    }

    fn try_start_automation_capture_transport(&mut self) -> Result<u64, DesktopDirectControlError> {
        self.try_start_capture_transport()
    }

    fn try_stop_automation_capture_transport(&mut self) -> Result<(), DesktopDirectControlError> {
        self.try_send_transport_command(TransportCommand::Stop)
    }
}

/// One verified Runtime target selectable by the operator surface.
///
/// `authored_target` is what persists in an external update. `resolved_target`
/// is an in-memory mixer address that must never leave the Runtime process.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeAutomationTargetCatalogEntry {
    authored_target: RuntimeAutomationTarget,
    resolved_target: AutomationTarget,
    label: String,
}

/// Immutable Runtime automation facts prepared before native audio starts.
pub(super) struct RuntimeAutomationCaptureConfiguration {
    source: RuntimeAutomationUpdateSource,
    targets: Vec<RuntimeAutomationTargetCatalogEntry>,
    timeline: FrameTimeline,
    take_capacity_available: bool,
    maximum_point_count: usize,
    output_directory: Option<RuntimeAutomationUpdateDirectory>,
}

impl RuntimeAutomationCaptureConfiguration {
    /// Builds immutable capture facts from a verified Runtime deployment.
    ///
    /// Runtime targets are catalogued in compiled mixer order, while the
    /// handback retains only stable track/send identities. Existing enabled
    /// automation targets are deliberately excluded: Runtime captures follow
    /// the IDE's stricter no-enabled-target policy rather than trying to infer
    /// interval scheduling from a detached process.
    pub(super) fn try_new(
        source: RuntimeAutomationUpdateSource,
        project: &PreparedProjectPlayback,
        output_directory: Option<RuntimeAutomationUpdateDirectory>,
    ) -> Result<Self, RuntimeAutomationCaptureError> {
        let session = project.session();
        if session.project_id() != source.project_id() {
            return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
        }
        let (loaded, mixer, _, _) = session.parts();
        if mixer.project_id() != source.project_id() {
            return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
        }
        let timeline = FrameTimeline::try_new(loaded.sample_rate_hz(), loaded.tempo_events())
            .map_err(RuntimeAutomationCaptureError::Timeline)?;
        let capacity = remaining_capture_capacity(session)?;
        let existing = session.automation_takes().takes();
        let mut targets = Vec::new();
        let channel_capacity = mixer
            .slots()
            .len()
            .checked_mul(4)
            .ok_or(RuntimeAutomationCaptureError::CapacityUnavailable)?;
        targets
            .try_reserve(channel_capacity)
            .map_err(|_| RuntimeAutomationCaptureError::CapacityUnavailable)?;
        for (slot, channel) in mixer.slots().iter().enumerate() {
            let track_id = channel.track_id();
            let track_name = loaded
                .track_name(track_id)
                .ok_or(RuntimeAutomationCaptureError::ProjectCatalogUnavailable)?;
            for parameter in [
                AutomationChannelParameter::Gain,
                AutomationChannelParameter::Pan,
                AutomationChannelParameter::Mute,
            ] {
                let resolved_target = AutomationTarget::Channel { slot, parameter };
                if !enabled_target_exists(existing, resolved_target) {
                    push_channel_target(&mut targets, track_id, track_name, resolved_target)?;
                }
            }
            if channel.kind() == CompiledMixerTrackKind::Audio {
                let resolved_target = AutomationTarget::Channel {
                    slot,
                    parameter: AutomationChannelParameter::Solo,
                };
                if !enabled_target_exists(existing, resolved_target) {
                    push_channel_target(&mut targets, track_id, track_name, resolved_target)?;
                }
            }
            for (send, compiled_send) in channel.sends().iter().enumerate() {
                for parameter in [
                    AutomationSendParameter::Gain,
                    AutomationSendParameter::Pan,
                    AutomationSendParameter::Enabled,
                ] {
                    let resolved_target = AutomationTarget::Send {
                        slot,
                        send,
                        parameter,
                    };
                    if !enabled_target_exists(existing, resolved_target) {
                        push_send_target(
                            &mut targets,
                            compiled_send.id(),
                            track_name,
                            resolved_target,
                        )?;
                    }
                }
            }
        }
        for parameter in [AutomationMainParameter::Gain, AutomationMainParameter::Mute] {
            let resolved_target = AutomationTarget::Main { parameter };
            if !enabled_target_exists(existing, resolved_target) {
                push_main_target(&mut targets, resolved_target)?;
            }
        }
        Ok(Self {
            source,
            targets,
            timeline,
            take_capacity_available: capacity.take_capacity_available,
            maximum_point_count: capacity.maximum_point_count,
            output_directory,
        })
    }

    /// Copies verified display labels for the presentation-only operator model.
    pub(super) fn target_labels(&self) -> Result<Vec<String>, RuntimeAutomationCaptureError> {
        let mut labels = Vec::new();
        labels
            .try_reserve_exact(self.targets.len())
            .map_err(|_| RuntimeAutomationCaptureError::CapacityUnavailable)?;
        for target in &self.targets {
            let mut label = String::new();
            label
                .try_reserve_exact(target.label.len())
                .map_err(|_| RuntimeAutomationCaptureError::CapacityUnavailable)?;
            label.push_str(&target.label);
            labels.push(label);
        }
        Ok(labels)
    }

    /// Attaches the one committed parameter receiver transferred by the session.
    #[must_use]
    pub(super) fn attach(
        self,
        receiver: InteractiveMixerParameterReceiver,
    ) -> RuntimeAutomationCaptureController {
        let initial_state = initial_state(
            self.output_directory.is_some(),
            self.take_capacity_available,
            self.maximum_point_count,
            self.targets.len(),
        );
        let mut controller = RuntimeAutomationCaptureController {
            receiver: None,
            receiver_disconnected: false,
            source: self.source,
            targets: self.targets,
            timeline: self.timeline,
            take_capacity_available: self.take_capacity_available,
            maximum_point_count: self.maximum_point_count,
            output_directory: self.output_directory,
            active: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeAutomationCaptureObservation {
                state: initial_state,
                start_enabled: false,
                stop_enabled: false,
            })),
        };
        controller
            .rebind_receiver(receiver)
            .expect("a newly constructed automation controller has no active capture");
        controller
    }
}

fn initial_state(
    output_configured: bool,
    take_capacity_available: bool,
    maximum_point_count: usize,
    target_count: usize,
) -> String {
    if !output_configured {
        "Capture unavailable: no external automation-update directory".to_owned()
    } else if !take_capacity_available {
        "Capture unavailable: automation take or lane capacity is exhausted".to_owned()
    } else if maximum_point_count == 0 {
        "Capture unavailable: automation capacity is exhausted".to_owned()
    } else if target_count == 0 {
        "Capture unavailable: every verified mixer target is already automated".to_owned()
    } else {
        "Waiting for committed Runtime transport recovery".to_owned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RuntimeAutomationCaptureCapacity {
    take_capacity_available: bool,
    maximum_point_count: usize,
}

fn remaining_capture_capacity(
    session: &PreparedPlaybackSession,
) -> Result<RuntimeAutomationCaptureCapacity, RuntimeAutomationCaptureError> {
    let takes = session.automation_takes().takes();
    let lane_count = takes.iter().try_fold(0_usize, |count, take| {
        count
            .checked_add(take.lanes().len())
            .ok_or(RuntimeAutomationCaptureError::CapacityUnavailable)
    })?;
    let point_count = takes
        .iter()
        .flat_map(PlaybackAutomationTake::lanes)
        .try_fold(0_usize, |count, lane| {
            count
                .checked_add(lane.point_count())
                .ok_or(RuntimeAutomationCaptureError::CapacityUnavailable)
        })?;
    let limits = LoaderLimits::VERSION_1;
    if takes.len() > limits.max_automation_documents
        || lane_count > limits.max_automation_lanes
        || point_count > limits.max_automation_points
    {
        return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
    }
    let take_capacity_available =
        takes.len() < limits.max_automation_documents && lane_count < limits.max_automation_lanes;
    let maximum_point_count = if take_capacity_available {
        limits
            .max_automation_points
            .saturating_sub(point_count)
            .min(MAX_RUNTIME_AUTOMATION_UPDATE_POINTS)
    } else {
        0
    };
    Ok(RuntimeAutomationCaptureCapacity {
        take_capacity_available,
        maximum_point_count,
    })
}

fn enabled_target_exists(takes: &[PlaybackAutomationTake], target: AutomationTarget) -> bool {
    takes
        .iter()
        .filter(|take| take.enabled())
        .flat_map(PlaybackAutomationTake::lanes)
        .any(|lane| lane.target() == target)
}

fn push_channel_target(
    targets: &mut Vec<RuntimeAutomationTargetCatalogEntry>,
    track_id: &str,
    track_name: &str,
    resolved_target: AutomationTarget,
) -> Result<(), RuntimeAutomationCaptureError> {
    let AutomationTarget::Channel { parameter, .. } = resolved_target else {
        return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
    };
    let authored_target = RuntimeAutomationTarget::try_channel(track_id.to_owned(), parameter)
        .map_err(RuntimeAutomationCaptureError::Update)?;
    push_catalog_target(
        targets,
        authored_target,
        resolved_target,
        track_name,
        channel_parameter_label(parameter),
    )
}

fn push_send_target(
    targets: &mut Vec<RuntimeAutomationTargetCatalogEntry>,
    send_id: &str,
    track_name: &str,
    resolved_target: AutomationTarget,
) -> Result<(), RuntimeAutomationCaptureError> {
    let AutomationTarget::Send { parameter, .. } = resolved_target else {
        return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
    };
    let authored_target = RuntimeAutomationTarget::try_send(send_id.to_owned(), parameter)
        .map_err(RuntimeAutomationCaptureError::Update)?;
    push_catalog_target(
        targets,
        authored_target,
        resolved_target,
        track_name,
        send_parameter_label(parameter),
    )
}

fn push_main_target(
    targets: &mut Vec<RuntimeAutomationTargetCatalogEntry>,
    resolved_target: AutomationTarget,
) -> Result<(), RuntimeAutomationCaptureError> {
    let AutomationTarget::Main { parameter } = resolved_target else {
        return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
    };
    push_catalog_target(
        targets,
        RuntimeAutomationTarget::main(parameter),
        resolved_target,
        "Main output",
        main_parameter_label(parameter),
    )
}

fn push_catalog_target(
    targets: &mut Vec<RuntimeAutomationTargetCatalogEntry>,
    authored_target: RuntimeAutomationTarget,
    resolved_target: AutomationTarget,
    owner_label: &str,
    parameter_label: &str,
) -> Result<(), RuntimeAutomationCaptureError> {
    if targets
        .iter()
        .any(|candidate| candidate.resolved_target == resolved_target)
    {
        return Err(RuntimeAutomationCaptureError::ProjectCatalogUnavailable);
    }
    let label_length = owner_label
        .len()
        .checked_add(3)
        .and_then(|length| length.checked_add(parameter_label.len()))
        .ok_or(RuntimeAutomationCaptureError::CapacityUnavailable)?;
    let mut label = String::new();
    label
        .try_reserve_exact(label_length)
        .map_err(|_| RuntimeAutomationCaptureError::CapacityUnavailable)?;
    label.push_str(owner_label);
    label.push_str(" / ");
    label.push_str(parameter_label);
    targets
        .try_reserve(1)
        .map_err(|_| RuntimeAutomationCaptureError::CapacityUnavailable)?;
    targets.push(RuntimeAutomationTargetCatalogEntry {
        authored_target,
        resolved_target,
        label,
    });
    Ok(())
}

const fn channel_parameter_label(parameter: AutomationChannelParameter) -> &'static str {
    match parameter {
        AutomationChannelParameter::Gain => "Gain",
        AutomationChannelParameter::Pan => "Pan",
        AutomationChannelParameter::Mute => "Mute",
        AutomationChannelParameter::Solo => "Solo",
    }
}

const fn send_parameter_label(parameter: AutomationSendParameter) -> &'static str {
    match parameter {
        AutomationSendParameter::Gain => "Send gain",
        AutomationSendParameter::Pan => "Send pan",
        AutomationSendParameter::Enabled => "Send enabled",
    }
}

const fn main_parameter_label(parameter: AutomationMainParameter) -> &'static str {
    match parameter {
        AutomationMainParameter::Gain => "Gain",
        AutomationMainParameter::Mute => "Mute",
    }
}

/// A classified Runtime automation-capture refusal or terminal failure.
#[derive(Debug)]
pub(super) enum RuntimeAutomationCaptureError {
    OutputDirectoryNotConfigured,
    AlreadyCapturing,
    NoActiveCapture,
    InvalidName,
    TargetUnavailable,
    TransportMustBeStopped,
    LoopEnabled,
    NoRemainingAutomationCapacity,
    NoRemainingPointCapacity,
    ProjectCatalogUnavailable,
    CapacityUnavailable,
    EventOverflow {
        dropped_event_count: usize,
    },
    EventSourceDisconnected,
    LoopEnabledDuringCapture,
    InvalidValueForTarget,
    NonMonotonicPoint {
        previous_tick: u64,
        observed_tick: u64,
    },
    PointCapacityReached {
        maximum_point_count: usize,
    },
    PointCapacityUnavailable,
    NoParameterChangesCaptured,
    IntervalNotRepresentable {
        start_tick: u64,
        end_tick: u64,
    },
    Timeline(FrameTimelineError),
    Transport(DesktopDirectControlError),
    StopUnavailable {
        capture: Box<Self>,
        source: DesktopDirectControlError,
    },
    Update(RuntimeAutomationUpdateError),
}

impl fmt::Display for RuntimeAutomationCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external automation-update output directory",
            ),
            Self::AlreadyCapturing => {
                formatter.write_str("a Runtime automation capture is already active")
            }
            Self::NoActiveCapture => formatter.write_str("no Runtime automation capture is active"),
            Self::InvalidName => {
                formatter.write_str("automation take name is outside the version-one domain")
            }
            Self::TargetUnavailable => {
                formatter.write_str("the selected Runtime automation target is unavailable")
            }
            Self::TransportMustBeStopped => {
                formatter.write_str("transport must be stopped before Runtime automation capture")
            }
            Self::LoopEnabled => formatter
                .write_str("loop playback must be disabled before Runtime automation capture"),
            Self::NoRemainingAutomationCapacity => formatter
                .write_str("the verified project has no remaining automation take capacity"),
            Self::NoRemainingPointCapacity => formatter
                .write_str("the verified project has no remaining automation point capacity"),
            Self::ProjectCatalogUnavailable => formatter.write_str(
                "the verified Runtime project has inconsistent automation capture facts",
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime automation capture capacity is unavailable")
            }
            Self::EventOverflow {
                dropped_event_count,
            } => write!(
                formatter,
                "Runtime automation capture lost {dropped_event_count} committed parameter change(s)"
            ),
            Self::EventSourceDisconnected => {
                formatter.write_str("Runtime committed mixer-parameter source disconnected")
            }
            Self::LoopEnabledDuringCapture => formatter
                .write_str("loop playback became enabled during Runtime automation capture"),
            Self::InvalidValueForTarget => {
                formatter.write_str("captured parameter value differs from its selected target")
            }
            Self::NonMonotonicPoint {
                previous_tick,
                observed_tick,
            } => write!(
                formatter,
                "captured automation tick moved backward from {previous_tick} to {observed_tick}"
            ),
            Self::PointCapacityReached {
                maximum_point_count,
            } => write!(
                formatter,
                "Runtime automation capture reached its {maximum_point_count}-point capacity"
            ),
            Self::PointCapacityUnavailable => {
                formatter.write_str("Runtime could not extend the automation point buffer")
            }
            Self::NoParameterChangesCaptured => formatter
                .write_str("Runtime automation capture contained no committed parameter changes"),
            Self::IntervalNotRepresentable {
                start_tick,
                end_tick,
            } => write!(
                formatter,
                "captured automation interval [{start_tick}, {end_tick}) is not representable"
            ),
            Self::Timeline(source) => {
                write!(formatter, "Runtime automation timeline failed: {source}")
            }
            Self::Transport(source) => source.fmt(formatter),
            Self::StopUnavailable { capture, source } => write!(
                formatter,
                "Runtime automation capture failed and transport could not stop it: {capture}; {source}"
            ),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeAutomationCaptureError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Timeline(source) => Some(source),
            Self::Transport(source) | Self::StopUnavailable { source, .. } => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::AlreadyCapturing
            | Self::NoActiveCapture
            | Self::InvalidName
            | Self::TargetUnavailable
            | Self::TransportMustBeStopped
            | Self::LoopEnabled
            | Self::NoRemainingAutomationCapacity
            | Self::NoRemainingPointCapacity
            | Self::ProjectCatalogUnavailable
            | Self::CapacityUnavailable
            | Self::EventOverflow { .. }
            | Self::EventSourceDisconnected
            | Self::LoopEnabledDuringCapture
            | Self::InvalidValueForTarget
            | Self::NonMonotonicPoint { .. }
            | Self::PointCapacityReached { .. }
            | Self::PointCapacityUnavailable
            | Self::NoParameterChangesCaptured
            | Self::IntervalNotRepresentable { .. } => None,
        }
    }
}

#[derive(Debug)]
enum RuntimeAutomationCapturePhase {
    Capturing,
    Stopping {
        after_committed_cycle_count: u64,
        failure: Option<RuntimeAutomationCaptureError>,
    },
}

/// One active capture that has a committed capture-start acknowledgement.
#[derive(Debug)]
struct ActiveRuntimeAutomationCapture {
    name: String,
    authored_target: RuntimeAutomationTarget,
    resolved_target: AutomationTarget,
    timeline: FrameTimeline,
    points: Vec<AutomationPoint>,
    maximum_point_count: usize,
    capture_start_sequence: u64,
    phase: RuntimeAutomationCapturePhase,
}

impl ActiveRuntimeAutomationCapture {
    fn try_new(
        name: String,
        catalog_target: RuntimeAutomationTargetCatalogEntry,
        timeline: FrameTimeline,
        maximum_point_count: usize,
        capture_start_sequence: u64,
    ) -> Result<Self, RuntimeAutomationCaptureError> {
        let mut points = Vec::new();
        points
            .try_reserve_exact(maximum_point_count)
            .map_err(|_| RuntimeAutomationCaptureError::PointCapacityUnavailable)?;
        Ok(Self {
            name,
            authored_target: catalog_target.authored_target,
            resolved_target: catalog_target.resolved_target,
            timeline,
            points,
            maximum_point_count,
            capture_start_sequence,
            phase: RuntimeAutomationCapturePhase::Capturing,
        })
    }

    const fn captured_point_count(&self) -> usize {
        self.points.len()
    }

    const fn is_capturing(&self) -> bool {
        matches!(self.phase, RuntimeAutomationCapturePhase::Capturing)
    }

    fn stopping_after_committed_cycle_count(&self) -> Option<u64> {
        match self.phase {
            RuntimeAutomationCapturePhase::Capturing => None,
            RuntimeAutomationCapturePhase::Stopping {
                after_committed_cycle_count,
                ..
            } => Some(after_committed_cycle_count),
        }
    }

    fn begin_stopping(
        &mut self,
        after_committed_cycle_count: u64,
        failure: Option<RuntimeAutomationCaptureError>,
    ) {
        self.phase = RuntimeAutomationCapturePhase::Stopping {
            after_committed_cycle_count,
            failure,
        };
    }

    fn remember_failure(&mut self, failure: RuntimeAutomationCaptureError) {
        if let RuntimeAutomationCapturePhase::Stopping {
            failure: terminal_failure,
            ..
        } = &mut self.phase
            && terminal_failure.is_none()
        {
            *terminal_failure = Some(failure);
        }
    }

    fn take_terminal_failure(&mut self) -> Option<RuntimeAutomationCaptureError> {
        match &mut self.phase {
            RuntimeAutomationCapturePhase::Capturing => None,
            RuntimeAutomationCapturePhase::Stopping { failure, .. } => failure.take(),
        }
    }

    fn stopped_after_start(&self, recovery: AutomationCaptureRecovery) -> bool {
        recovery.state == PlaybackRecoveryTransportState::Stopped
            && recovery.committed_capture_start_sequence >= self.capture_start_sequence
    }

    fn stopped_after_explicit_stop(&self, recovery: AutomationCaptureRecovery) -> bool {
        self.stopping_after_committed_cycle_count()
            .is_some_and(|after_committed_cycle_count| {
                recovery.state == PlaybackRecoveryTransportState::Stopped
                    && recovery.committed_cycle_count > after_committed_cycle_count
            })
    }

    fn observe(
        &mut self,
        observation: InteractiveMixerParameterObservation,
    ) -> Result<(), RuntimeAutomationCaptureError> {
        self.observe_change(
            observation.start_frame(),
            observation.transport_rolling(),
            observation.loop_enabled(),
            observation.target(),
            observation.value(),
        )
    }

    #[cfg(test)]
    fn observe_change_for_test(
        &mut self,
        start_frame: usize,
        transport_rolling: bool,
        loop_enabled: bool,
        target: AutomationTarget,
        value: AutomationValue,
    ) -> Result<(), RuntimeAutomationCaptureError> {
        self.observe_change(start_frame, transport_rolling, loop_enabled, target, value)
    }

    fn observe_change(
        &mut self,
        start_frame: usize,
        transport_rolling: bool,
        loop_enabled: bool,
        target: AutomationTarget,
        value: AutomationValue,
    ) -> Result<(), RuntimeAutomationCaptureError> {
        if loop_enabled {
            return Err(RuntimeAutomationCaptureError::LoopEnabledDuringCapture);
        }
        if !transport_rolling || target != self.resolved_target {
            return Ok(());
        }
        if !automation_value_matches_target(target, value) {
            return Err(RuntimeAutomationCaptureError::InvalidValueForTarget);
        }
        let tick = self
            .timeline
            .position_at_frame(start_frame)
            .and_then(|position| position.quantize_forward(1))
            .map_err(RuntimeAutomationCaptureError::Timeline)?;
        if let Some(previous) = self.points.last().copied() {
            if tick < previous.tick() {
                return Err(RuntimeAutomationCaptureError::NonMonotonicPoint {
                    previous_tick: previous.tick(),
                    observed_tick: tick,
                });
            }
            if tick == previous.tick() {
                *self
                    .points
                    .last_mut()
                    .expect("the prior captured point remains present") =
                    AutomationPoint::new(tick, value);
                return Ok(());
            }
        }
        if self.points.len() >= self.maximum_point_count {
            return Err(RuntimeAutomationCaptureError::PointCapacityReached {
                maximum_point_count: self.maximum_point_count,
            });
        }
        self.points
            .try_reserve(1)
            .map_err(|_| RuntimeAutomationCaptureError::PointCapacityUnavailable)?;
        self.points.push(AutomationPoint::new(tick, value));
        Ok(())
    }

    fn into_update(
        self,
        source: RuntimeAutomationUpdateSource,
        end_frame: usize,
    ) -> Result<RuntimeAutomationUpdate, RuntimeAutomationCaptureError> {
        let Some(first) = self.points.first().copied() else {
            return Err(RuntimeAutomationCaptureError::NoParameterChangesCaptured);
        };
        let last = self
            .points
            .last()
            .copied()
            .expect("a nonempty captured point list retains its final point");
        let end_tick = self
            .timeline
            .position_at_frame(end_frame)
            .and_then(|position| position.quantize_forward(1))
            .map_err(RuntimeAutomationCaptureError::Timeline)?;
        if end_tick <= last.tick() {
            return Err(RuntimeAutomationCaptureError::IntervalNotRepresentable {
                start_tick: first.tick(),
                end_tick,
            });
        }
        RuntimeAutomationUpdate::try_new(
            Uuid::new_v4().to_string(),
            source,
            self.name,
            self.authored_target,
            first.tick(),
            end_tick,
            self.points,
        )
        .map_err(RuntimeAutomationCaptureError::Update)
    }
}

fn automation_value_matches_target(target: AutomationTarget, value: AutomationValue) -> bool {
    match (target.is_boolean(), value) {
        (true, AutomationValue::Boolean(_)) => true,
        (false, AutomationValue::Integer(value)) => target
            .integer_bounds()
            .is_some_and(|(minimum, maximum)| (minimum..=maximum).contains(&value)),
        (true, AutomationValue::Integer(_)) | (false, AutomationValue::Boolean(_)) => false,
    }
}

/// Runtime-owned presentation state for the explicit operator controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeAutomationCaptureObservation {
    pub(super) state: String,
    pub(super) start_enabled: bool,
    pub(super) stop_enabled: bool,
}

/// One durable Runtime automation update visible to the IDE picker.
#[derive(Debug)]
pub(super) struct RuntimeAutomationCaptureSuccess {
    pub(super) path: PathBuf,
    pub(super) point_count: usize,
}

/// Terminal result reported by one Runtime owner turn.
#[derive(Debug)]
pub(super) enum RuntimeAutomationCapturePoll {
    Saved(RuntimeAutomationCaptureSuccess),
    Failed(RuntimeAutomationCaptureError),
}

/// Runtime-side controller for an explicit one-target automation capture.
pub(super) struct RuntimeAutomationCaptureController {
    receiver: Option<InteractiveMixerParameterReceiver>,
    receiver_disconnected: bool,
    source: RuntimeAutomationUpdateSource,
    targets: Vec<RuntimeAutomationTargetCatalogEntry>,
    timeline: FrameTimeline,
    take_capacity_available: bool,
    maximum_point_count: usize,
    output_directory: Option<RuntimeAutomationUpdateDirectory>,
    active: Option<ActiveRuntimeAutomationCapture>,
    last_status: Option<String>,
    observation: Arc<Mutex<RuntimeAutomationCaptureObservation>>,
}

impl RuntimeAutomationCaptureController {
    /// Returns the Runtime-owned presentation snapshot for the operator UI.
    #[must_use]
    pub(super) fn observation(&self) -> Arc<Mutex<RuntimeAutomationCaptureObservation>> {
        Arc::clone(&self.observation)
    }

    /// Replaces the sole receiver after a fully stopped playback owner has
    /// published its replacement. Active and stopping captures retain their
    /// original receiver until they reach a terminal outcome.
    pub(super) fn rebind_receiver(
        &mut self,
        receiver: InteractiveMixerParameterReceiver,
    ) -> Result<(), RuntimeAutomationCaptureError> {
        if self.active.is_some() {
            return Err(RuntimeAutomationCaptureError::AlreadyCapturing);
        }
        self.receiver = Some(receiver);
        self.receiver_disconnected = false;
        self.last_status = None;
        let mut observation = self
            .observation
            .lock()
            .expect("Runtime automation observation lock is not poisoned");
        observation.state = initial_state(
            self.output_directory.is_some(),
            self.take_capacity_available,
            self.maximum_point_count,
            self.targets.len(),
        );
        observation.start_enabled = false;
        observation.stop_enabled = false;
        Ok(())
    }

    /// Explicitly starts one bounded capture through the sole transport owner.
    pub(super) fn start<S: RuntimeAutomationCaptureSession>(
        &mut self,
        session: &mut S,
        target_index: usize,
        name: String,
    ) -> Result<usize, RuntimeAutomationCaptureError> {
        if self.active.is_some() {
            return Err(RuntimeAutomationCaptureError::AlreadyCapturing);
        }
        if self.output_directory.is_none() {
            return Err(RuntimeAutomationCaptureError::OutputDirectoryNotConfigured);
        }
        if !automation_take_name_is_valid(&name) {
            return Err(RuntimeAutomationCaptureError::InvalidName);
        }
        if self.receiver_disconnected {
            return Err(RuntimeAutomationCaptureError::EventSourceDisconnected);
        }
        let catalog_target = self
            .targets
            .get(target_index)
            .cloned()
            .ok_or(RuntimeAutomationCaptureError::TargetUnavailable)?;
        let recovery = session.automation_capture_recovery();
        if recovery.state != PlaybackRecoveryTransportState::Stopped {
            return Err(RuntimeAutomationCaptureError::TransportMustBeStopped);
        }
        if recovery.loop_enabled {
            return Err(RuntimeAutomationCaptureError::LoopEnabled);
        }
        if !self.take_capacity_available {
            return Err(RuntimeAutomationCaptureError::NoRemainingAutomationCapacity);
        }
        if self.maximum_point_count == 0 {
            return Err(RuntimeAutomationCaptureError::NoRemainingPointCapacity);
        }
        let candidate = ActiveRuntimeAutomationCapture::try_new(
            name,
            catalog_target,
            self.timeline.clone(),
            self.maximum_point_count,
            0,
        )?;

        // History before capture-start belongs to no capture. Overflow in that
        // stale history is discardable; a disconnected source cannot supply a
        // lossless later capture and remains terminal.
        let discarded_history = self.receiver_mut().drain(|_| {});
        if discarded_history.sender_disconnected() {
            self.receiver_disconnected = true;
            self.refresh_observation(session);
            return Err(RuntimeAutomationCaptureError::EventSourceDisconnected);
        }
        let capture_start_sequence = session
            .try_start_automation_capture_transport()
            .map_err(RuntimeAutomationCaptureError::Transport)?;
        self.active = Some(ActiveRuntimeAutomationCapture {
            capture_start_sequence,
            ..candidate
        });
        self.last_status = None;
        self.refresh_observation(session);
        Ok(self.maximum_point_count)
    }

    /// Requests the one stopped boundary used to finish an active capture.
    pub(super) fn stop<S: RuntimeAutomationCaptureSession>(
        &mut self,
        session: &mut S,
    ) -> Result<usize, RuntimeAutomationCaptureError> {
        let captured_point_count = self
            .active
            .as_ref()
            .ok_or(RuntimeAutomationCaptureError::NoActiveCapture)?
            .captured_point_count();
        self.queue_stop(session, None)?;
        self.refresh_observation(session);
        Ok(captured_point_count)
    }

    /// Drains committed parameter evidence and finalizes at a committed stop.
    pub(super) fn poll<S: RuntimeAutomationCaptureSession>(
        &mut self,
        session: &mut S,
    ) -> Option<RuntimeAutomationCapturePoll> {
        if self.active.is_none() {
            self.drain_idle_history();
            self.refresh_observation(session);
            return None;
        }
        let recovery = session.automation_capture_recovery();
        let (drain, mut observation_failure) = {
            let (receiver, active) = (&mut self.receiver, &mut self.active);
            let active = active
                .as_mut()
                .expect("active capture was checked before draining evidence");
            let mut observation_failure = None;
            let drain = receiver
                .as_mut()
                .expect("an attached Runtime automation controller retains one receiver")
                .drain(|observation| {
                    if observation_failure.is_none() {
                        observation_failure = active.observe(observation).err();
                    }
                });
            (drain, observation_failure)
        };
        if drain.dropped_event_count() > 0 {
            observation_failure = Some(RuntimeAutomationCaptureError::EventOverflow {
                dropped_event_count: drain.dropped_event_count(),
            });
        }
        if drain.sender_disconnected() {
            self.receiver_disconnected = true;
            observation_failure = Some(RuntimeAutomationCaptureError::EventSourceDisconnected);
        }
        if self
            .active
            .as_ref()
            .is_some_and(ActiveRuntimeAutomationCapture::is_capturing)
            && recovery.loop_enabled
        {
            observation_failure = Some(RuntimeAutomationCaptureError::LoopEnabledDuringCapture);
        }
        if let Some(reason) = observation_failure {
            if self
                .active
                .as_ref()
                .is_some_and(ActiveRuntimeAutomationCapture::is_capturing)
            {
                if let Err(error) = self.queue_stop(session, Some(reason)) {
                    self.active = None;
                    self.last_status = Some(format!("Capture failed: {error}"));
                    self.refresh_observation(session);
                    return Some(RuntimeAutomationCapturePoll::Failed(error));
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
        Some(match self.finish(recovery.position_frame) {
            Ok(saved) => {
                self.last_status = Some(format!(
                    "Update published: {} committed parameter change(s)",
                    saved.point_count
                ));
                self.refresh_observation(session);
                RuntimeAutomationCapturePoll::Saved(saved)
            }
            Err(error) => {
                self.last_status = Some(format!("Capture failed: {error}"));
                self.refresh_observation(session);
                RuntimeAutomationCapturePoll::Failed(error)
            }
        })
    }

    fn drain_idle_history(&mut self) {
        let drain = self.receiver_mut().drain(|_| {});
        if drain.sender_disconnected() {
            self.receiver_disconnected = true;
        }
    }

    fn receiver_mut(&mut self) -> &mut InteractiveMixerParameterReceiver {
        self.receiver
            .as_mut()
            .expect("an attached Runtime automation controller retains one receiver")
    }

    fn queue_stop<S: RuntimeAutomationCaptureSession>(
        &mut self,
        session: &mut S,
        failure: Option<RuntimeAutomationCaptureError>,
    ) -> Result<(), RuntimeAutomationCaptureError> {
        if !self
            .active
            .as_ref()
            .is_some_and(ActiveRuntimeAutomationCapture::is_capturing)
        {
            return Ok(());
        }
        let recovery = session.automation_capture_recovery();
        match session.try_stop_automation_capture_transport() {
            Ok(()) => {
                self.active
                    .as_mut()
                    .expect("active capture cannot disappear while Runtime owns it")
                    .begin_stopping(recovery.committed_cycle_count, failure);
                Ok(())
            }
            Err(source) => match failure {
                Some(capture) => Err(RuntimeAutomationCaptureError::StopUnavailable {
                    capture: Box::new(capture),
                    source,
                }),
                None => Err(RuntimeAutomationCaptureError::Transport(source)),
            },
        }
    }

    fn finish(
        &mut self,
        end_frame: usize,
    ) -> Result<RuntimeAutomationCaptureSuccess, RuntimeAutomationCaptureError> {
        let mut active = self
            .active
            .take()
            .expect("only a completed active capture can be finalized");
        if let Some(error) = active.take_terminal_failure() {
            return Err(error);
        }
        let point_count = active.captured_point_count();
        let update = active.into_update(self.source.clone(), end_frame)?;
        let output_directory = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeAutomationCaptureError::OutputDirectoryNotConfigured)?;
        let path = output_directory
            .publish(&update)
            .map_err(RuntimeAutomationCaptureError::Update)?;
        Ok(RuntimeAutomationCaptureSuccess { path, point_count })
    }

    fn refresh_observation<S: RuntimeAutomationCaptureSession>(&self, session: &S) {
        let recovery = session.automation_capture_recovery();
        let (state, start_enabled, stop_enabled) = match self.active.as_ref() {
            Some(active) if active.is_capturing() => {
                let state =
                    if recovery.committed_capture_start_sequence >= active.capture_start_sequence {
                        format!(
                            "Capturing {} committed parameter change(s)",
                            active.captured_point_count()
                        )
                    } else {
                        "Capture start queued; awaiting a committed audio cycle".to_owned()
                    };
                (state, false, true)
            }
            Some(active) => (
                format!(
                    "Capture stopping after {} committed parameter change(s)",
                    active.captured_point_count()
                ),
                false,
                false,
            ),
            None => {
                let unavailable = if self.output_directory.is_none() {
                    Some("Capture unavailable: no external automation-update directory".to_owned())
                } else if self.receiver_disconnected {
                    Some(
                        "Capture unavailable: committed mixer-parameter source disconnected"
                            .to_owned(),
                    )
                } else if !self.take_capacity_available {
                    Some(
                        "Capture unavailable: automation take or lane capacity is exhausted"
                            .to_owned(),
                    )
                } else if self.maximum_point_count == 0 {
                    Some("Capture unavailable: automation capacity is exhausted".to_owned())
                } else if self.targets.is_empty() {
                    Some(
                        "Capture unavailable: every verified mixer target is already automated"
                            .to_owned(),
                    )
                } else if recovery.state != PlaybackRecoveryTransportState::Stopped {
                    Some("Capture requires stopped transport".to_owned())
                } else if recovery.loop_enabled {
                    Some("Capture requires loop playback to be disabled".to_owned())
                } else {
                    None
                };
                let start_enabled = unavailable.is_none();
                (
                    self.last_status
                        .clone()
                        .or(unavailable)
                        .unwrap_or_else(|| "Ready to capture one mixer automation take".to_owned()),
                    start_enabled,
                    false,
                )
            }
        };
        if let Ok(mut observation) = self.observation.lock() {
            *observation = RuntimeAutomationCaptureObservation {
                state,
                start_enabled,
                stop_enabled,
            };
        }
    }
}

fn automation_take_name_is_valid(name: &str) -> bool {
    let character_count = name.chars().count();
    name.trim() == name && (1..=120).contains(&character_count)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use hyper_mixer_runtime::interactive_mixer_parameter_channel;
    use hyper_runtime_automation_update::read_runtime_automation_update;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-automation-capture-{}",
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

    fn source() -> RuntimeAutomationUpdateSource {
        RuntimeAutomationUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn target() -> RuntimeAutomationTargetCatalogEntry {
        RuntimeAutomationTargetCatalogEntry {
            authored_target: RuntimeAutomationTarget::try_channel(
                TRACK_ID.to_owned(),
                AutomationChannelParameter::Gain,
            )
            .unwrap(),
            resolved_target: AutomationTarget::Channel {
                slot: 0,
                parameter: AutomationChannelParameter::Gain,
            },
            label: "Track / Gain".to_owned(),
        }
    }

    fn test_timeline() -> FrameTimeline {
        FrameTimeline::try_new(
            48_000,
            &[hyper_playback_compiler::TempoEvent {
                tick: 0,
                microseconds_per_quarter: 500_000,
            }],
        )
        .unwrap()
    }

    fn controller(
        output_directory: Option<RuntimeAutomationUpdateDirectory>,
        maximum_point_count: usize,
    ) -> (
        RuntimeAutomationCaptureController,
        hyper_mixer_runtime::InteractiveMixerParameterPublisher,
    ) {
        let (publisher, receiver) = interactive_mixer_parameter_channel();
        (
            RuntimeAutomationCaptureController {
                receiver: Some(receiver),
                receiver_disconnected: false,
                source: source(),
                targets: vec![target()],
                timeline: test_timeline(),
                take_capacity_available: true,
                maximum_point_count,
                output_directory,
                active: None,
                last_status: None,
                observation: Arc::new(Mutex::new(RuntimeAutomationCaptureObservation {
                    state: "fixture".to_owned(),
                    start_enabled: false,
                    stop_enabled: false,
                })),
            },
            publisher,
        )
    }

    fn active(sequence: u64) -> ActiveRuntimeAutomationCapture {
        ActiveRuntimeAutomationCapture::try_new(
            "Runtime gain pass".to_owned(),
            target(),
            test_timeline(),
            3,
            sequence,
        )
        .unwrap()
    }

    #[test]
    fn receiver_rebind_replaces_idle_history_and_refuses_active_capture() {
        let (mut controller, original) = controller(None, 3);
        let (replacement, receiver) = interactive_mixer_parameter_channel();

        controller.last_status = Some("previous owner published a take".to_owned());
        controller.rebind_receiver(receiver).unwrap();
        assert_eq!(controller.last_status, None);
        assert_eq!(
            controller.observation.lock().unwrap().state,
            "Capture unavailable: no external automation-update directory"
        );
        drop(original);
        assert!(
            !controller
                .receiver_mut()
                .drain(|_| {})
                .sender_disconnected()
        );
        drop(replacement);
        assert!(
            controller
                .receiver_mut()
                .drain(|_| {})
                .sender_disconnected()
        );

        let (_replacement, receiver) = interactive_mixer_parameter_channel();
        controller.active = Some(active(5));
        assert!(matches!(
            controller.rebind_receiver(receiver),
            Err(RuntimeAutomationCaptureError::AlreadyCapturing)
        ));
    }

    #[derive(Debug)]
    struct FakeSession {
        recovery: AutomationCaptureRecovery,
        start_sequence: u64,
        start_refused: bool,
        stop_refused: bool,
        start_count: usize,
        stop_count: usize,
    }

    impl FakeSession {
        fn stopped() -> Self {
            Self {
                recovery: AutomationCaptureRecovery {
                    state: PlaybackRecoveryTransportState::Stopped,
                    position_frame: 0,
                    committed_cycle_count: 4,
                    committed_capture_start_sequence: 0,
                    loop_enabled: false,
                },
                start_sequence: 5,
                start_refused: false,
                stop_refused: false,
                start_count: 0,
                stop_count: 0,
            }
        }
    }

    impl RuntimeAutomationCaptureSession for FakeSession {
        fn automation_capture_recovery(&self) -> AutomationCaptureRecovery {
            self.recovery
        }

        fn try_start_automation_capture_transport(
            &mut self,
        ) -> Result<u64, DesktopDirectControlError> {
            self.start_count = self.start_count.saturating_add(1);
            if self.start_refused {
                Err(DesktopDirectControlError::CaptureStartSequenceExhausted)
            } else {
                Ok(self.start_sequence)
            }
        }

        fn try_stop_automation_capture_transport(
            &mut self,
        ) -> Result<(), DesktopDirectControlError> {
            self.stop_count = self.stop_count.saturating_add(1);
            if self.stop_refused {
                Err(DesktopDirectControlError::CaptureStartSequenceExhausted)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn start_requires_verified_output_stopped_transport_valid_name_and_capacity() {
        let (mut disabled, _publisher) = controller(None, 3);
        let mut session = FakeSession::stopped();
        assert!(matches!(
            disabled.start(&mut session, 0, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::OutputDirectoryNotConfigured)
        ));
        assert_eq!(session.start_count, 0);

        let temporary = TemporaryDirectory::new();
        let output = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output), 3);
        assert!(matches!(
            controller.start(&mut session, 0, " not trimmed".to_owned()),
            Err(RuntimeAutomationCaptureError::InvalidName)
        ));
        session.recovery.state = PlaybackRecoveryTransportState::Rolling;
        assert!(matches!(
            controller.start(&mut session, 0, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::TransportMustBeStopped)
        ));
        session.recovery.state = PlaybackRecoveryTransportState::Stopped;
        session.recovery.loop_enabled = true;
        assert!(matches!(
            controller.start(&mut session, 0, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::LoopEnabled)
        ));
        session.recovery.loop_enabled = false;
        assert!(matches!(
            controller.start(&mut session, 1, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::TargetUnavailable)
        ));
        controller.take_capacity_available = false;
        assert!(matches!(
            controller.start(&mut session, 0, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::NoRemainingAutomationCapacity)
        ));
        controller.take_capacity_available = true;
        controller.maximum_point_count = 0;
        assert!(matches!(
            controller.start(&mut session, 0, "Pass".to_owned()),
            Err(RuntimeAutomationCaptureError::NoRemainingPointCapacity)
        ));
        assert_eq!(session.start_count, 0);
    }

    #[test]
    fn start_and_stop_use_one_capture_acknowledgement_and_stop_boundary() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output), 3);
        let mut session = FakeSession::stopped();

        assert_eq!(
            controller
                .start(&mut session, 0, "Pass".to_owned())
                .unwrap(),
            3
        );
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
    fn finalization_publishes_one_provenance_bound_external_update() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output), 3);
        let mut active = active(3);
        let resolved = target().resolved_target;
        active
            .observe_change_for_test(
                0,
                true,
                false,
                resolved,
                AutomationValue::Integer(1_000_000),
            )
            .unwrap();
        active
            .observe_change_for_test(
                24_000,
                true,
                false,
                resolved,
                AutomationValue::Integer(1_500_000),
            )
            .unwrap();
        active.begin_stopping(4, None);
        controller.active = Some(active);

        let saved = controller.finish(48_000).unwrap();
        assert_eq!(saved.point_count, 2);
        assert!(saved.path.starts_with(temporary.path()));
        let parsed = read_runtime_automation_update(&saved.path).unwrap();
        assert_eq!(parsed.source(), &source());
        assert_eq!(parsed.name(), "Runtime gain pass");
        assert_eq!(parsed.target(), &target().authored_target);
        assert_eq!(parsed.start_tick(), 0);
        assert_eq!(parsed.end_tick(), 3_840);
        assert_eq!(
            parsed.points(),
            [
                AutomationPoint::new(0, AutomationValue::Integer(1_000_000)),
                AutomationPoint::new(1_920, AutomationValue::Integer(1_500_000)),
            ]
        );
    }

    #[test]
    fn terminal_failure_never_publishes_a_partial_update() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output), 3);
        let mut active = active(3);
        active
            .observe_change_for_test(
                0,
                true,
                false,
                target().resolved_target,
                AutomationValue::Integer(1_000_000),
            )
            .unwrap();
        active.begin_stopping(
            4,
            Some(RuntimeAutomationCaptureError::EventOverflow {
                dropped_event_count: 1,
            }),
        );
        controller.active = Some(active);

        assert!(matches!(
            controller.finish(48_000),
            Err(RuntimeAutomationCaptureError::EventOverflow {
                dropped_event_count: 1
            })
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }

    #[test]
    fn capture_replaces_same_tick_and_refuses_backward_or_invalid_points() {
        let mut active = active(3);
        let resolved = target().resolved_target;
        active
            .observe_change_for_test(
                0,
                true,
                false,
                resolved,
                AutomationValue::Integer(1_000_000),
            )
            .unwrap();
        active
            .observe_change_for_test(
                0,
                true,
                false,
                resolved,
                AutomationValue::Integer(1_500_000),
            )
            .unwrap();
        assert_eq!(active.captured_point_count(), 1);
        assert_eq!(
            active.points[0].value(),
            AutomationValue::Integer(1_500_000)
        );
        assert!(matches!(
            active.observe_change_for_test(
                48_000,
                true,
                false,
                resolved,
                AutomationValue::Boolean(true),
            ),
            Err(RuntimeAutomationCaptureError::InvalidValueForTarget)
        ));
        assert!(matches!(
            active.observe_change_for_test(
                48_000,
                true,
                true,
                resolved,
                AutomationValue::Integer(1_000_000),
            ),
            Err(RuntimeAutomationCaptureError::LoopEnabledDuringCapture)
        ));
    }

    #[test]
    fn poll_waits_for_committed_stop_and_does_not_publish_empty_capture() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeAutomationUpdateDirectory::open(temporary.path()).unwrap();
        let (mut controller, _publisher) = controller(Some(output), 3);
        let mut session = FakeSession::stopped();
        controller
            .start(&mut session, 0, "Pass".to_owned())
            .unwrap();
        session.recovery.state = PlaybackRecoveryTransportState::Rolling;
        session.recovery.committed_capture_start_sequence = session.start_sequence;
        assert!(controller.poll(&mut session).is_none());
        controller.stop(&mut session).unwrap();
        session.recovery.state = PlaybackRecoveryTransportState::Stopped;
        session.recovery.position_frame = 48_000;
        session.recovery.committed_cycle_count = 5;
        assert!(matches!(
            controller.poll(&mut session),
            Some(RuntimeAutomationCapturePoll::Failed(
                RuntimeAutomationCaptureError::NoParameterChangesCaptured
            ))
        ));
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
    }
}
