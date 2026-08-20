use core::num::NonZeroU32;
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[path = "../../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_control_mappings::{
    ChannelParameter, ControlBinding, ControlMappingDocument, ControlMappingProject, ControlScope,
    ControlSource, ControlTarget, ControlTransform, CueParameter, Edge, MainParameter, SceneAction,
    SendParameter, TransportAction, TriggerBehavior,
};
use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingAvailability, ControlBindingUnavailableReason,
    ControlContext, ControlDispatchError, ControlDispatchOutcome, ControlEvent, ControlRuntime,
    ControlRuntimeBuildErrorReason, DirectControlStateUpdate, PanelActionTarget, PanelBinding,
    PanelBindingRefusalReason, PanelButtonBehavior, PanelConnectError, PanelControl,
    PanelDispatchError, PanelDispatchOutcome, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget, UnboundReason, ValueControlTarget, ValueDomain,
};
use hyper_mapping_compiler::{ControlMappingResolver, MixerSendAddress, compile_control_mapping};
use hyper_panel_protocol::{FirmwareVersion, LinkConfig, Message, PanelDescriptor};
use hyper_panel_simulator::{PanelSimulator, PanelSimulatorConfig};
use hyper_runtime_panel_session::{PanelEvent, PanelSession};
use realtime_allocator::HeapMeasurement;

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334474";
const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";

struct Project;

impl ControlMappingProject for Project {
    fn project_id(&self) -> &str {
        PROJECT_ID
    }

    fn cv_inputs(&self) -> u8 {
        4
    }

    fn has_clock_input(&self) -> bool {
        true
    }

    fn has_reset_input(&self) -> bool {
        true
    }

    fn has_mixer(&self) -> bool {
        true
    }

    fn contains_nested_entity(&self, id: &str) -> bool {
        matches!(id, TRACK_ID | SCENE_ID | SEND_ID)
    }

    fn contains_entity(&self, _id: &str) -> bool {
        false
    }

    fn contains_track(&self, id: &str) -> bool {
        id == TRACK_ID
    }

    fn contains_scene(&self, id: &str) -> bool {
        id == SCENE_ID
    }

    fn contains_mixer_channel(&self, track_id: &str) -> bool {
        track_id == TRACK_ID
    }

    fn contains_mixer_send(&self, send_id: &str) -> bool {
        send_id == SEND_ID
    }

    fn is_solo_source_track(&self, track_id: &str) -> bool {
        track_id == TRACK_ID
    }
}

impl ControlMappingResolver for Project {
    fn track_index(&self, track_id: &str) -> Option<usize> {
        (track_id == TRACK_ID).then_some(0)
    }

    fn scene_index(&self, scene_id: &str) -> Option<usize> {
        (scene_id == SCENE_ID).then_some(0)
    }

    fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
        (track_id == TRACK_ID).then_some(0)
    }

    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress> {
        (send_id == SEND_ID).then_some(MixerSendAddress::new(0, 0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SinkError {
    InvalidTarget,
    Rejected,
}

#[derive(Debug, Default)]
struct Sink {
    invalid_target: Option<RuntimeControlTarget>,
    reject_on_attempt: Option<usize>,
    attempts: usize,
    booleans: Vec<(BooleanControlTarget, bool)>,
    values: Vec<(ValueControlTarget, i32)>,
    state_ready: Option<Arc<AtomicBool>>,
    transport_rolling: Option<bool>,
    scene_ready: bool,
    commands: Vec<RuntimeControlCommand>,
}

impl RuntimeControlSink for Sink {
    type Error = SinkError;
    type DirectCommand = RuntimeControlCommand;
    type DirectError = SinkError;

    fn validate_target(&self, target: RuntimeControlTarget) -> Result<(), Self::Error> {
        if self.invalid_target == Some(target) {
            Err(SinkError::InvalidTarget)
        } else {
            Ok(())
        }
    }

    fn target_availability(&self, target: RuntimeControlTarget) -> ControlBindingAvailability {
        if matches!(target, RuntimeControlTarget::Scene { .. }) && self.scene_ready {
            ControlBindingAvailability::Live
        } else if matches!(target, RuntimeControlTarget::Scene { .. }) {
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared,
            )
        } else {
            ControlBindingAvailability::Live
        }
    }

    fn boolean_state(&self, target: BooleanControlTarget) -> Option<bool> {
        if self
            .state_ready
            .as_ref()
            .is_some_and(|ready| !ready.load(Ordering::Acquire))
        {
            return None;
        }
        self.booleans
            .iter()
            .find_map(|(candidate, value)| (*candidate == target).then_some(*value))
    }

    fn transport_rolling(&self) -> Option<bool> {
        self.transport_rolling
    }

    fn value_state(&self, target: ValueControlTarget) -> Option<i32> {
        if self
            .state_ready
            .as_ref()
            .is_some_and(|ready| !ready.load(Ordering::Acquire))
        {
            return None;
        }
        self.values
            .iter()
            .find_map(|(candidate, value)| (*candidate == target).then_some(*value))
    }

    fn value_domain(&self, target: ValueControlTarget) -> ValueDomain {
        match target {
            ValueControlTarget::ChannelGain { .. }
            | ValueControlTarget::SendGain { .. }
            | ValueControlTarget::MainGain => ValueDomain::new(0, 2_000_000),
            ValueControlTarget::ChannelPan { .. } | ValueControlTarget::SendPan { .. } => {
                ValueDomain::new(-1_000_000, 1_000_000)
            }
        }
    }

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        self.attempts += 1;
        if self.reject_on_attempt == Some(self.attempts) {
            Err(SinkError::Rejected)
        } else {
            self.apply(command);
            self.commands.push(command);
            Ok(())
        }
    }

    fn try_send_direct(
        &mut self,
        command: Self::DirectCommand,
    ) -> Result<DirectControlStateUpdate, Self::DirectError> {
        self.try_send(command)?;
        Ok(match command {
            RuntimeControlCommand::MainMute { muted } => DirectControlStateUpdate::Boolean {
                target: BooleanControlTarget::MainMute,
                value: muted,
            },
            RuntimeControlCommand::TransportStart => {
                DirectControlStateUpdate::Transport { rolling: true }
            }
            RuntimeControlCommand::TransportStop => {
                DirectControlStateUpdate::Transport { rolling: false }
            }
            RuntimeControlCommand::ChannelGain {
                channel_index,
                gain_millionths,
            } => DirectControlStateUpdate::Value {
                target: ValueControlTarget::ChannelGain { channel_index },
                value: i32::try_from(gain_millionths).unwrap(),
            },
            RuntimeControlCommand::ChannelPan {
                channel_index,
                pan_millionths,
            } => DirectControlStateUpdate::Value {
                target: ValueControlTarget::ChannelPan { channel_index },
                value: pan_millionths,
            },
            RuntimeControlCommand::SendGain {
                channel_index,
                send_index,
                gain_millionths,
            } => DirectControlStateUpdate::Value {
                target: ValueControlTarget::SendGain {
                    channel_index,
                    send_index,
                },
                value: i32::try_from(gain_millionths).unwrap(),
            },
            RuntimeControlCommand::SendPan {
                channel_index,
                send_index,
                pan_millionths,
            } => DirectControlStateUpdate::Value {
                target: ValueControlTarget::SendPan {
                    channel_index,
                    send_index,
                },
                value: pan_millionths,
            },
            RuntimeControlCommand::MainGain { gain_millionths } => {
                DirectControlStateUpdate::Value {
                    target: ValueControlTarget::MainGain,
                    value: i32::try_from(gain_millionths).unwrap(),
                }
            }
            RuntimeControlCommand::ChannelMute { .. }
            | RuntimeControlCommand::ChannelSolo { .. }
            | RuntimeControlCommand::SendEnabled { .. }
            | RuntimeControlCommand::TransportLocateStartPreserve
            | RuntimeControlCommand::SceneLaunch { .. } => DirectControlStateUpdate::Unchanged,
        })
    }
}

impl Sink {
    /// Applies a delivered command to the double's own reported state, so a
    /// shadow that drifts away from its sink is observable rather than
    /// invisible.
    fn apply(&mut self, command: RuntimeControlCommand) {
        let value = match command {
            RuntimeControlCommand::ChannelGain {
                channel_index,
                gain_millionths,
            } => Some((
                ValueControlTarget::ChannelGain { channel_index },
                i32::try_from(gain_millionths).unwrap(),
            )),
            RuntimeControlCommand::ChannelPan {
                channel_index,
                pan_millionths,
            } => Some((
                ValueControlTarget::ChannelPan { channel_index },
                pan_millionths,
            )),
            RuntimeControlCommand::MainGain { gain_millionths } => Some((
                ValueControlTarget::MainGain,
                i32::try_from(gain_millionths).unwrap(),
            )),
            _ => None,
        };
        if let Some((target, value)) = value
            && let Some(entry) = self.values.iter_mut().find(|(entry, _)| *entry == target)
        {
            entry.1 = value;
        }
        let boolean = match command {
            RuntimeControlCommand::ChannelMute {
                channel_index,
                muted,
            } => Some((BooleanControlTarget::ChannelMute { channel_index }, muted)),
            RuntimeControlCommand::ChannelSolo {
                channel_index,
                solo,
            } => Some((BooleanControlTarget::ChannelSolo { channel_index }, solo)),
            RuntimeControlCommand::MainMute { muted } => {
                Some((BooleanControlTarget::MainMute, muted))
            }
            _ => None,
        };
        if let Some((target, value)) = boolean
            && let Some(entry) = self.booleans.iter_mut().find(|(entry, _)| *entry == target)
        {
            entry.1 = value;
        }
    }
}

fn binding_id(index: u16) -> String {
    format!("018f47a2-6b2d-7c11-8a44-00112233{index:04}")
}

fn cv_binding(index: u16, scope: ControlScope, target: ControlTarget) -> ControlBinding {
    ControlBinding::try_new(
        binding_id(index),
        true,
        ControlSource::CvInput { input_index: 0 },
        scope,
        ControlTransform::Linear {
            input_min: -1_000_000,
            input_max: 1_000_000,
            output_min: 0,
            output_max: 1_000_000,
        },
        target,
    )
    .unwrap()
}

fn trigger_binding(
    index: u16,
    scope: ControlScope,
    behavior: TriggerBehavior,
    target: ControlTarget,
) -> ControlBinding {
    trigger_binding_from(
        index,
        ControlSource::ClockInput { edge: Edge::Rising },
        scope,
        behavior,
        target,
    )
}

fn trigger_binding_from(
    index: u16,
    source: ControlSource,
    scope: ControlScope,
    behavior: TriggerBehavior,
    target: ControlTarget,
) -> ControlBinding {
    ControlBinding::try_new(
        binding_id(index),
        true,
        source,
        scope,
        ControlTransform::Trigger { behavior },
        target,
    )
    .unwrap()
}

fn table(bindings: Vec<ControlBinding>) -> hyper_mapping_compiler::CompiledControlMappingTable {
    let document = ControlMappingDocument::try_new(
        MAPPING_ID.to_owned(),
        PROJECT_ID.to_owned(),
        bindings,
        BTreeMap::new(),
    )
    .unwrap();
    compile_control_mapping(&document, MAPPING_ID, &Project).unwrap()
}

#[test]
fn continuous_dispatch_maps_exact_values_and_preserves_sink_rejections() {
    let compiled = table(vec![cv_binding(
        500,
        ControlScope::Global,
        ControlTarget::MixerChannel {
            track_id: TRACK_ID.to_owned(),
            parameter: ChannelParameter::Gain,
        },
    )]);
    let sink = Sink {
        reject_on_attempt: Some(2),
        ..Sink::default()
    };
    let mut runtime = ControlRuntime::try_new(compiled, sink).unwrap();

    assert_eq!(
        runtime.dispatch(
            ControlEvent::Cv {
                input_index: 1,
                value: 0,
            },
            ControlContext::default(),
        ),
        Ok(ControlDispatchOutcome::NoMatch)
    );
    let command = RuntimeControlCommand::ChannelGain {
        channel_index: 0,
        gain_millionths: 1_000_000,
    };
    assert_eq!(
        runtime.dispatch(
            ControlEvent::Cv {
                input_index: 0,
                value: 0,
            },
            ControlContext::default(),
        ),
        Ok(ControlDispatchOutcome::Sent {
            binding_index: 0,
            command,
        })
    );
    assert_eq!(runtime.sink().commands, [command]);

    let rejected = runtime.dispatch(
        ControlEvent::Cv {
            input_index: 0,
            value: 1_000_000,
        },
        ControlContext::default(),
    );
    assert!(matches!(
        rejected,
        Err(ControlDispatchError::Sink {
            binding_index: 0,
            source: SinkError::Rejected,
            ..
        })
    ));
    assert_eq!(runtime.sink().commands, [command]);
}

#[test]
fn unsupported_domains_are_reported_and_never_reach_the_sink() {
    let compiled = table(vec![
        cv_binding(
            500,
            ControlScope::Global,
            ControlTarget::MixerCue {
                parameter: CueParameter::Gain,
            },
        ),
        trigger_binding(
            501,
            ControlScope::MixerPage,
            TriggerBehavior::Momentary,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        ),
        trigger_binding(
            502,
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
            TriggerBehavior::Invoke,
            ControlTarget::Scene {
                scene_id: SCENE_ID.to_owned(),
                action: SceneAction::Launch,
            },
        ),
    ]);
    let mut runtime = ControlRuntime::try_new(compiled, Sink::default()).unwrap();

    assert_eq!(
        runtime
            .statuses()
            .iter()
            .map(|status| status.availability())
            .collect::<Vec<_>>(),
        [
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::CueOutputUnavailable
            ),
            ControlBindingAvailability::Live,
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::SceneAudioNotPrepared
            ),
        ]
    );
    assert_eq!(
        runtime
            .dispatch(
                ControlEvent::Cv {
                    input_index: 0,
                    value: 0,
                },
                ControlContext::default(),
            )
            .unwrap(),
        ControlDispatchOutcome::Unavailable {
            binding_index: 0,
            reason: ControlBindingUnavailableReason::CueOutputUnavailable,
        }
    );
    assert!(runtime.sink().commands.is_empty());
}

#[test]
fn momentary_bindings_use_the_configured_edge_for_activation_and_the_opposite_for_release() {
    let compiled = table(vec![
        trigger_binding_from(
            500,
            ControlSource::ClockInput { edge: Edge::Rising },
            ControlScope::Global,
            TriggerBehavior::Momentary,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        ),
        trigger_binding_from(
            501,
            ControlSource::ResetInput {
                edge: Edge::Falling,
            },
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
            TriggerBehavior::Momentary,
            ControlTarget::MixerChannel {
                track_id: TRACK_ID.to_owned(),
                parameter: ChannelParameter::Solo,
            },
        ),
    ]);
    let mut runtime = ControlRuntime::try_new(compiled, Sink::default()).unwrap();

    for event in [
        ControlEvent::Clock { edge: Edge::Rising },
        ControlEvent::Clock {
            edge: Edge::Falling,
        },
    ] {
        assert!(matches!(
            runtime.dispatch(event, ControlContext::default()),
            Ok(ControlDispatchOutcome::Sent { .. })
        ));
    }
    for event in [
        ControlEvent::Reset {
            edge: Edge::Falling,
        },
        ControlEvent::Reset { edge: Edge::Rising },
    ] {
        assert!(matches!(
            runtime.dispatch(event, ControlContext::new(false, Some(0), None)),
            Ok(ControlDispatchOutcome::Sent { .. })
        ));
    }

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::MainMute { muted: true },
            RuntimeControlCommand::MainMute { muted: false },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: true,
            },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: false,
            },
        ]
    );
}

#[test]
fn scene_launch_dispatches_one_prevalidated_numeric_command() {
    let compiled = table(vec![trigger_binding(
        700,
        ControlScope::Global,
        TriggerBehavior::Invoke,
        ControlTarget::Scene {
            scene_id: SCENE_ID.to_owned(),
            action: SceneAction::Launch,
        },
    )]);
    let mut runtime = ControlRuntime::try_new(
        compiled,
        Sink {
            scene_ready: true,
            ..Sink::default()
        },
    )
    .unwrap();
    assert_eq!(
        runtime.statuses()[0].availability(),
        ControlBindingAvailability::Live
    );
    assert_eq!(
        runtime
            .dispatch(
                ControlEvent::Clock { edge: Edge::Rising },
                ControlContext::default(),
            )
            .unwrap(),
        ControlDispatchOutcome::Sent {
            binding_index: 0,
            command: RuntimeControlCommand::SceneLaunch { scene_index: 0 },
        }
    );
}

#[test]
fn momentary_release_keeps_cycle_ownership_across_context_changes() {
    let compiled = table(vec![
        trigger_binding_from(
            500,
            ControlSource::ClockInput { edge: Edge::Rising },
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
            TriggerBehavior::Momentary,
            ControlTarget::MixerChannel {
                track_id: TRACK_ID.to_owned(),
                parameter: ChannelParameter::Solo,
            },
        ),
        trigger_binding_from(
            501,
            ControlSource::ClockInput {
                edge: Edge::Falling,
            },
            ControlScope::MixerPage,
            TriggerBehavior::Toggle,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        ),
    ]);
    let mut runtime = ControlRuntime::try_new(
        compiled,
        Sink {
            booleans: vec![(BooleanControlTarget::MainMute, false)],
            ..Sink::default()
        },
    )
    .unwrap();

    runtime
        .dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::new(false, Some(0), None),
        )
        .unwrap();
    runtime
        .dispatch(
            ControlEvent::Clock {
                edge: Edge::Falling,
            },
            ControlContext::new(true, None, None),
        )
        .unwrap();
    runtime
        .dispatch(
            ControlEvent::Clock {
                edge: Edge::Falling,
            },
            ControlContext::new(true, None, None),
        )
        .unwrap();

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: true,
            },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: false,
            },
            RuntimeControlCommand::MainMute { muted: true },
        ]
    );
}

#[test]
fn momentary_capture_starts_and_clears_only_after_successful_delivery() {
    let compiled = table(vec![trigger_binding(
        500,
        ControlScope::Track {
            track_id: TRACK_ID.to_owned(),
        },
        TriggerBehavior::Momentary,
        ControlTarget::MixerChannel {
            track_id: TRACK_ID.to_owned(),
            parameter: ChannelParameter::Mute,
        },
    )]);
    let mut runtime = ControlRuntime::try_new(
        compiled,
        Sink {
            reject_on_attempt: Some(2),
            ..Sink::default()
        },
    )
    .unwrap();
    let release = ControlEvent::Clock {
        edge: Edge::Falling,
    };

    assert_eq!(
        runtime
            .dispatch(release, ControlContext::new(false, Some(0), None))
            .unwrap(),
        ControlDispatchOutcome::NoMatch
    );
    runtime
        .dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::new(false, Some(0), None),
        )
        .unwrap();
    assert!(matches!(
        runtime.dispatch(release, ControlContext::default()),
        Err(ControlDispatchError::Sink {
            binding_index: 0,
            source: SinkError::Rejected,
            ..
        })
    ));
    runtime
        .dispatch(release, ControlContext::default())
        .unwrap();
    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: false,
            },
        ]
    );
}

#[test]
fn simultaneous_active_scopes_are_ambiguous_without_invented_precedence() {
    let compiled = table(vec![
        cv_binding(
            500,
            ControlScope::Global,
            ControlTarget::MixerMain {
                parameter: MainParameter::Gain,
            },
        ),
        cv_binding(
            501,
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
            ControlTarget::MixerChannel {
                track_id: TRACK_ID.to_owned(),
                parameter: ChannelParameter::Pan,
            },
        ),
    ]);
    let mut runtime = ControlRuntime::try_new(compiled, Sink::default()).unwrap();

    let error = runtime
        .dispatch(
            ControlEvent::Cv {
                input_index: 0,
                value: 0,
            },
            ControlContext::new(false, Some(0), None),
        )
        .unwrap_err();

    assert!(matches!(
        error,
        ControlDispatchError::Ambiguous {
            first_binding_index: 0,
            second_binding_index: 1,
            matching_binding_count: 2,
        }
    ));
    assert!(runtime.sink().commands.is_empty());
}

#[test]
fn toggle_shadows_advance_only_after_successful_delivery() {
    let compiled = table(vec![trigger_binding(
        500,
        ControlScope::Global,
        TriggerBehavior::Toggle,
        ControlTarget::MixerSend {
            send_id: SEND_ID.to_owned(),
            parameter: SendParameter::Enabled,
        },
    )]);
    let target = BooleanControlTarget::SendEnabled {
        channel_index: 0,
        send_index: 0,
    };
    let sink = Sink {
        booleans: vec![(target, false)],
        reject_on_attempt: Some(2),
        ..Sink::default()
    };
    let mut runtime = ControlRuntime::try_new(compiled, sink).unwrap();
    let event = ControlEvent::Clock { edge: Edge::Rising };

    runtime.dispatch(event, ControlContext::default()).unwrap();
    assert!(matches!(
        runtime.dispatch(event, ControlContext::default()),
        Err(ControlDispatchError::Sink {
            source: SinkError::Rejected,
            ..
        })
    ));
    runtime.dispatch(event, ControlContext::default()).unwrap();

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::SendEnabled {
                channel_index: 0,
                send_index: 0,
                enabled: true,
            },
            RuntimeControlCommand::SendEnabled {
                channel_index: 0,
                send_index: 0,
                enabled: false,
            },
        ]
    );
}

#[test]
fn missing_toggle_state_and_invalid_bound_targets_fail_closed() {
    let compiled = table(vec![trigger_binding(
        500,
        ControlScope::Global,
        TriggerBehavior::Toggle,
        ControlTarget::MixerMain {
            parameter: MainParameter::Mute,
        },
    )]);
    let mut runtime = ControlRuntime::try_new(compiled, Sink::default()).unwrap();
    assert!(matches!(
        runtime.dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::default()
        ),
        Err(ControlDispatchError::StateUnavailable { binding_index: 0 })
    ));

    let compiled = table(vec![cv_binding(
        500,
        ControlScope::Global,
        ControlTarget::MixerChannel {
            track_id: TRACK_ID.to_owned(),
            parameter: ChannelParameter::Gain,
        },
    )]);
    let sink = Sink {
        invalid_target: Some(RuntimeControlTarget::Channel {
            channel_index: 0,
            parameter: ChannelParameter::Gain,
        }),
        ..Sink::default()
    };
    let error = ControlRuntime::try_new(compiled, sink).unwrap_err();
    assert_eq!(error.binding_index(), Some(0));
    assert!(matches!(
        error.reason(),
        ControlRuntimeBuildErrorReason::Target {
            binding_index: 0,
            source: SinkError::InvalidTarget,
        }
    ));
    let (table, sink) = error.into_authorities();
    assert_eq!(table.bindings().len(), 1);
    assert!(sink.commands.is_empty());
}

#[test]
fn missing_toggle_state_is_refreshed_after_recovery_becomes_available() {
    let compiled = table(vec![trigger_binding(
        500,
        ControlScope::Global,
        TriggerBehavior::Toggle,
        ControlTarget::MixerMain {
            parameter: MainParameter::Mute,
        },
    )]);
    let state_ready = Arc::new(AtomicBool::new(false));
    let sink = Sink {
        booleans: vec![(BooleanControlTarget::MainMute, false)],
        state_ready: Some(Arc::clone(&state_ready)),
        ..Sink::default()
    };
    let mut runtime = ControlRuntime::try_new(compiled, sink).unwrap();
    let event = ControlEvent::Clock { edge: Edge::Rising };

    assert!(matches!(
        runtime.dispatch(event, ControlContext::default()),
        Err(ControlDispatchError::StateUnavailable { binding_index: 0 })
    ));
    state_ready.store(true, Ordering::Release);
    assert_eq!(
        runtime.dispatch(event, ControlContext::default()),
        Ok(ControlDispatchOutcome::Sent {
            binding_index: 0,
            command: RuntimeControlCommand::MainMute { muted: true },
        })
    );
}

#[test]
fn transport_toggle_uses_owned_shadow_between_audio_cycles() {
    let compiled = table(vec![trigger_binding(
        500,
        ControlScope::Global,
        TriggerBehavior::Invoke,
        ControlTarget::Transport {
            action: TransportAction::Toggle,
        },
    )]);
    let sink = Sink {
        transport_rolling: Some(false),
        ..Sink::default()
    };
    let mut runtime = ControlRuntime::try_new(compiled, sink).unwrap();
    let event = ControlEvent::Clock { edge: Edge::Rising };

    runtime.dispatch(event, ControlContext::default()).unwrap();
    runtime.dispatch(event, ControlContext::default()).unwrap();

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::TransportStart,
            RuntimeControlCommand::TransportStop,
        ]
    );
}

#[test]
fn direct_software_commands_share_and_rollback_mapping_shadows() {
    let compiled = table(vec![
        trigger_binding(
            500,
            ControlScope::MixerPage,
            TriggerBehavior::Toggle,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        ),
        trigger_binding(
            501,
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
            TriggerBehavior::Invoke,
            ControlTarget::Transport {
                action: TransportAction::Toggle,
            },
        ),
    ]);
    let sink = Sink {
        booleans: vec![(BooleanControlTarget::MainMute, false)],
        transport_rolling: Some(false),
        reject_on_attempt: Some(2),
        ..Sink::default()
    };
    let mut runtime = ControlRuntime::try_new(compiled, sink).unwrap();

    runtime
        .try_send_direct(RuntimeControlCommand::MainMute { muted: true })
        .unwrap();
    assert_eq!(
        runtime.try_send_direct(RuntimeControlCommand::TransportStart),
        Err(SinkError::Rejected)
    );
    runtime
        .dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::new(true, None, None),
        )
        .unwrap();
    runtime
        .dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::new(false, Some(0), None),
        )
        .unwrap();
    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::MainMute { muted: true },
            RuntimeControlCommand::MainMute { muted: false },
            RuntimeControlCommand::TransportStart,
        ]
    );
}

#[test]
fn dispatch_paths_perform_zero_heap_operations() {
    let mut live = ControlRuntime::try_new(
        table(vec![trigger_binding(
            500,
            ControlScope::Global,
            TriggerBehavior::Toggle,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        )]),
        Sink {
            booleans: vec![(BooleanControlTarget::MainMute, false)],
            commands: Vec::with_capacity(1),
            ..Sink::default()
        },
    )
    .unwrap();
    let mut unavailable = ControlRuntime::try_new(
        table(vec![cv_binding(
            501,
            ControlScope::Global,
            ControlTarget::MixerCue {
                parameter: CueParameter::Gain,
            },
        )]),
        Sink::default(),
    )
    .unwrap();
    let mut ambiguous = ControlRuntime::try_new(
        table(vec![
            cv_binding(
                502,
                ControlScope::Global,
                ControlTarget::MixerMain {
                    parameter: MainParameter::Gain,
                },
            ),
            cv_binding(
                503,
                ControlScope::MixerPage,
                ControlTarget::MixerChannel {
                    track_id: TRACK_ID.to_owned(),
                    parameter: ChannelParameter::Pan,
                },
            ),
        ]),
        Sink::default(),
    )
    .unwrap();
    let measurement = HeapMeasurement::start_verified();
    assert!(matches!(
        live.dispatch(
            ControlEvent::Clock { edge: Edge::Rising },
            ControlContext::default(),
        ),
        Ok(ControlDispatchOutcome::Sent { .. })
    ));
    assert_eq!(
        live.dispatch(
            ControlEvent::Reset { edge: Edge::Rising },
            ControlContext::default(),
        ),
        Ok(ControlDispatchOutcome::NoMatch)
    );
    assert!(matches!(
        unavailable.dispatch(
            ControlEvent::Cv {
                input_index: 0,
                value: 0,
            },
            ControlContext::default(),
        ),
        Ok(ControlDispatchOutcome::Unavailable { .. })
    ));
    assert!(matches!(
        ambiguous.dispatch(
            ControlEvent::Cv {
                input_index: 0,
                value: 0,
            },
            ControlContext::new(true, None, None),
        ),
        Err(ControlDispatchError::Ambiguous { .. })
    ));
    let heap_operations = measurement.finish();

    assert_eq!(heap_operations, 0);
}

#[test]
fn momentary_dispatch_performs_zero_heap_operations() {
    let mut runtime = ControlRuntime::try_new(
        table(vec![trigger_binding(
            504,
            ControlScope::Global,
            TriggerBehavior::Momentary,
            ControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        )]),
        Sink {
            commands: Vec::with_capacity(2),
            ..Sink::default()
        },
    )
    .unwrap();

    let measurement = HeapMeasurement::start_verified();
    for edge in [Edge::Rising, Edge::Falling] {
        assert!(matches!(
            runtime.dispatch(ControlEvent::Clock { edge }, ControlContext::default()),
            Ok(ControlDispatchOutcome::Sent { .. })
        ));
    }
    let heap_operations = measurement.finish();

    assert_eq!(heap_operations, 0);
}

fn panel_descriptor(encoders: u8, buttons: u8) -> PanelDescriptor {
    PanelDescriptor {
        firmware: FirmwareVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 7,
        },
        boot_id: 0x00AB_CDEF,
        encoders,
        buttons,
        leds: 0,
        rings: 0,
        ring_segments: 0,
    }
}

fn connect(
    runtime: &mut ControlRuntime<Sink>,
    descriptor: PanelDescriptor,
    bindings: &[PanelBinding],
) {
    assert!(
        runtime
            .connect_panel(descriptor, bindings)
            .unwrap()
            .is_fully_installed(),
        "every binding in this fixture is expected to install"
    );
}

fn panel_runtime(sink: Sink) -> ControlRuntime<Sink> {
    ControlRuntime::try_new(table(Vec::new()), sink).unwrap()
}

fn step(millionths: u32) -> NonZeroU32 {
    NonZeroU32::new(millionths).unwrap()
}

fn turn(control_id: u16, delta: i16) -> Message {
    Message::EncoderDelta {
        control_id,
        delta,
        timestamp_us: 0,
        modifiers: 0,
    }
}

fn press(control_id: u16) -> Message {
    Message::ButtonPress {
        control_id,
        timestamp_us: 0,
        modifiers: 0,
    }
}

fn release(control_id: u16) -> Message {
    Message::ButtonRelease {
        control_id,
        timestamp_us: 0,
        modifiers: 0,
    }
}

fn encoder(index: u8, target: ValueControlTarget, millionths: u32) -> PanelBinding {
    PanelBinding::Encoder {
        index,
        target,
        step_millionths: step(millionths),
    }
}

fn toggle(index: u8, target: BooleanControlTarget) -> PanelBinding {
    PanelBinding::Button {
        index,
        target,
        behavior: PanelButtonBehavior::Toggle,
    }
}

fn momentary(index: u8, target: BooleanControlTarget) -> PanelBinding {
    PanelBinding::Button {
        index,
        target,
        behavior: PanelButtonBehavior::Momentary,
    }
}

const MAIN_GAIN: ValueControlTarget = ValueControlTarget::MainGain;
const CHANNEL_PAN: ValueControlTarget = ValueControlTarget::ChannelPan { channel_index: 0 };
const MAIN_MUTE: BooleanControlTarget = BooleanControlTarget::MainMute;

#[test]
fn connect_refuses_bindings_the_panel_cannot_serve_and_installs_the_rest() {
    let mut runtime = panel_runtime(Sink {
        invalid_target: Some(RuntimeControlTarget::Channel {
            channel_index: 3,
            parameter: ChannelParameter::Gain,
        }),
        values: vec![(MAIN_GAIN, 1_000_000)],
        ..Sink::default()
    });

    let connection = runtime
        .connect_panel(
            panel_descriptor(2, 2),
            &[
                encoder(0, MAIN_GAIN, 50_000),
                encoder(2, MAIN_GAIN, 50_000),
                toggle(2, MAIN_MUTE),
                encoder(0, CHANNEL_PAN, 1),
                encoder(1, ValueControlTarget::ChannelGain { channel_index: 3 }, 1),
            ],
        )
        .unwrap();

    let reported: Vec<usize> = connection
        .refused()
        .iter()
        .map(hyper_control_runtime::PanelBindingRefusal::binding_index)
        .collect();
    assert_eq!(reported, [1, 2, 3, 4]);
    assert!(!connection.is_fully_installed());
    assert_eq!(connection.installed(), 1);
    assert!(matches!(
        connection.refused()[0].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert!(matches!(
        connection.refused()[1].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert!(matches!(
        connection.refused()[2].reason(),
        PanelBindingRefusalReason::ControlAlreadyBound {
            first_binding_index: 0
        }
    ));
    assert!(matches!(
        connection.refused()[3].reason(),
        PanelBindingRefusalReason::TargetRejected(SinkError::InvalidTarget)
    ));

    assert!(runtime.panel_connected());
    assert_eq!(
        runtime.dispatch_panel(turn(2, 1)),
        Ok(PanelDispatchOutcome::Unbound(UnboundReason::NoBinding {
            control: PanelControl::Encoder { index: 2 }
        }))
    );
    assert_eq!(
        runtime.dispatch_panel(press(2)),
        Ok(PanelDispatchOutcome::Unbound(UnboundReason::NoBinding {
            control: PanelControl::Button { index: 2 }
        }))
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 1_050_000,
            },
        })
    );
}

#[test]
fn encoder_deltas_move_the_bound_target_in_the_turned_direction_and_stop_at_both_limits() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(CHANNEL_PAN, 0)],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 0),
        &[encoder(0, CHANNEL_PAN, 250_000)],
    );

    let control = PanelControl::Encoder { index: 0 };
    let moved_to = |pan_millionths| {
        Ok(PanelDispatchOutcome::Sent {
            control,
            command: RuntimeControlCommand::ChannelPan {
                channel_index: 0,
                pan_millionths,
            },
        })
    };

    assert_eq!(runtime.dispatch_panel(turn(0, 3)), moved_to(750_000));
    assert_eq!(runtime.dispatch_panel(turn(0, -1)), moved_to(500_000));
    assert_eq!(runtime.dispatch_panel(turn(0, -8)), moved_to(-1_000_000));
    assert_eq!(
        runtime.dispatch_panel(turn(0, -1)),
        Ok(PanelDispatchOutcome::AtLimit { control })
    );
    assert_eq!(runtime.dispatch_panel(turn(0, 12)), moved_to(1_000_000));
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::AtLimit { control })
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 0)),
        Ok(PanelDispatchOutcome::Idle { control })
    );
    assert_eq!(runtime.sink().commands.len(), 4);
}

#[test]
fn an_encoder_saturates_at_the_gain_ceiling_the_sink_declares() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(MAIN_GAIN, 1_000_000)],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 0),
        &[encoder(0, MAIN_GAIN, 400_000)],
    );

    let control = PanelControl::Encoder { index: 0 };
    assert_eq!(
        runtime.dispatch_panel(turn(0, 9)),
        Ok(PanelDispatchOutcome::Sent {
            control,
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 2_000_000,
            },
        })
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::AtLimit { control })
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, -20)),
        Ok(PanelDispatchOutcome::Sent {
            control,
            command: RuntimeControlCommand::MainGain { gain_millionths: 0 },
        })
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, -1)),
        Ok(PanelDispatchOutcome::AtLimit { control })
    );
}

#[test]
fn a_momentary_and_a_toggle_button_answer_the_same_press_release_pair_differently() {
    let mut runtime = panel_runtime(Sink {
        booleans: vec![
            (
                BooleanControlTarget::ChannelMute { channel_index: 0 },
                false,
            ),
            (
                BooleanControlTarget::ChannelSolo { channel_index: 0 },
                false,
            ),
        ],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 2),
        &[
            momentary(0, BooleanControlTarget::ChannelMute { channel_index: 0 }),
            toggle(1, BooleanControlTarget::ChannelSolo { channel_index: 0 }),
        ],
    );

    for control_id in [0, 1] {
        assert!(matches!(
            runtime.dispatch_panel(press(control_id)),
            Ok(PanelDispatchOutcome::Sent { .. })
        ));
    }
    assert!(matches!(
        runtime.dispatch_panel(release(0)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert_eq!(
        runtime.dispatch_panel(release(1)),
        Ok(PanelDispatchOutcome::Idle {
            control: PanelControl::Button { index: 1 }
        })
    );

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: true,
            },
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: false,
            },
        ]
    );
}

#[test]
fn action_buttons_invoke_on_press_and_command_nothing_on_release() {
    let mut runtime = panel_runtime(Sink {
        transport_rolling: Some(false),
        scene_ready: true,
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 5),
        &[
            PanelBinding::ActionButton {
                index: 0,
                action: PanelActionTarget::TransportToggle,
            },
            PanelBinding::ActionButton {
                index: 1,
                action: PanelActionTarget::TransportLocateStart,
            },
            PanelBinding::ActionButton {
                index: 2,
                action: PanelActionTarget::SceneLaunch { scene_index: 0 },
            },
            PanelBinding::ActionButton {
                index: 3,
                action: PanelActionTarget::TransportStop,
            },
            PanelBinding::ActionButton {
                index: 4,
                action: PanelActionTarget::TransportStart,
            },
        ],
    );

    assert!(matches!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert_eq!(
        runtime.dispatch_panel(release(0)),
        Ok(PanelDispatchOutcome::Idle {
            control: PanelControl::Button { index: 0 }
        })
    );
    for control_id in [0, 1, 2, 3, 4] {
        assert!(matches!(
            runtime.dispatch_panel(press(control_id)),
            Ok(PanelDispatchOutcome::Sent { .. })
        ));
    }

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::TransportStart,
            RuntimeControlCommand::TransportStop,
            RuntimeControlCommand::TransportLocateStartPreserve,
            RuntimeControlCommand::SceneLaunch { scene_index: 0 },
            RuntimeControlCommand::TransportStop,
            RuntimeControlCommand::TransportStart,
        ]
    );
}

#[test]
fn send_bindings_carry_both_the_channel_and_the_send_index_to_the_command() {
    let send_gain = ValueControlTarget::SendGain {
        channel_index: 2,
        send_index: 1,
    };
    let send_pan = ValueControlTarget::SendPan {
        channel_index: 1,
        send_index: 2,
    };
    let send_enabled = BooleanControlTarget::SendEnabled {
        channel_index: 3,
        send_index: 4,
    };
    let mut runtime = panel_runtime(Sink {
        values: vec![(send_gain, 1_000_000), (send_pan, 0)],
        booleans: vec![(send_enabled, false)],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(2, 1),
        &[
            encoder(0, send_gain, 100_000),
            encoder(1, send_pan, 100_000),
            toggle(0, send_enabled),
        ],
    );

    assert!(matches!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert!(matches!(
        runtime.dispatch_panel(turn(1, -1)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert!(matches!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::SendGain {
                channel_index: 2,
                send_index: 1,
                gain_millionths: 1_100_000,
            },
            RuntimeControlCommand::SendPan {
                channel_index: 1,
                send_index: 2,
                pan_millionths: -100_000,
            },
            RuntimeControlCommand::SendEnabled {
                channel_index: 3,
                send_index: 4,
                enabled: true,
            },
        ]
    );
}

#[test]
fn bindings_survive_a_reconnect_only_while_the_new_descriptor_still_has_the_control() {
    let channel_gain = ValueControlTarget::ChannelGain { channel_index: 0 };
    let bindings = [
        encoder(0, channel_gain, 50_000),
        toggle(3, BooleanControlTarget::ChannelMute { channel_index: 0 }),
    ];
    let mut runtime = panel_runtime(Sink {
        values: vec![(channel_gain, 1_000_000)],
        booleans: vec![(
            BooleanControlTarget::ChannelMute { channel_index: 0 },
            false,
        )],
        ..Sink::default()
    });

    connect(&mut runtime, panel_descriptor(4, 8), &bindings);
    assert_eq!(
        runtime.connect_panel(panel_descriptor(4, 8), &bindings),
        Err(PanelConnectError::AlreadyConnected)
    );
    assert!(matches!(
        runtime.dispatch_panel(press(3)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert!(matches!(
        runtime.dispatch_panel(turn(0, 2)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));

    assert!(runtime.disconnect_panel().is_empty());
    assert!(!runtime.panel_connected());
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Unbound(
            UnboundReason::NoPanelConnected
        ))
    );

    // Something outside the panel moves both targets while it is away.
    runtime.sink_mut().values = vec![(channel_gain, 400_000)];
    runtime.sink_mut().booleans = vec![(
        BooleanControlTarget::ChannelMute { channel_index: 0 },
        false,
    )];

    let connection = runtime
        .connect_panel(panel_descriptor(1, 2), &bindings)
        .unwrap();
    assert_eq!(connection.refused().len(), 1);
    assert_eq!(connection.refused()[0].binding_index(), 1);
    assert!(matches!(
        connection.refused()[0].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert_eq!(
        runtime.dispatch_panel(press(3)),
        Ok(PanelDispatchOutcome::Unbound(UnboundReason::NoBinding {
            control: PanelControl::Button { index: 3 }
        }))
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 450_000,
            },
        })
    );
}

#[test]
fn a_reconnected_toggle_reads_its_state_from_the_sink_again_rather_than_from_a_stale_shadow() {
    let bindings = [toggle(0, MAIN_MUTE)];
    let mut runtime = panel_runtime(Sink {
        booleans: vec![(MAIN_MUTE, false)],
        ..Sink::default()
    });
    connect(&mut runtime, panel_descriptor(0, 1), &bindings);

    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Button { index: 0 },
            command: RuntimeControlCommand::MainMute { muted: true },
        })
    );
    assert!(runtime.disconnect_panel().is_empty());

    // The target is un-muted by something else while the panel is away.
    runtime.sink_mut().booleans = vec![(MAIN_MUTE, false)];

    connect(&mut runtime, panel_descriptor(0, 1), &bindings);
    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Button { index: 0 },
            command: RuntimeControlCommand::MainMute { muted: true },
        })
    );
}

#[test]
fn disconnecting_releases_only_the_momentary_control_still_held() {
    let mut runtime = panel_runtime(Sink::default());
    connect(
        &mut runtime,
        panel_descriptor(0, 2),
        &[
            momentary(0, BooleanControlTarget::ChannelMute { channel_index: 0 }),
            momentary(1, BooleanControlTarget::ChannelSolo { channel_index: 0 }),
        ],
    );

    for message in [press(0), release(0), press(1)] {
        assert!(matches!(
            runtime.dispatch_panel(message),
            Ok(PanelDispatchOutcome::Sent { .. })
        ));
    }
    assert!(runtime.disconnect_panel().is_empty());

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            RuntimeControlCommand::ChannelMute {
                channel_index: 0,
                muted: false,
            },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: true,
            },
            RuntimeControlCommand::ChannelSolo {
                channel_index: 0,
                solo: false,
            },
        ]
    );
}

#[test]
fn a_refused_release_is_reported_and_keeps_the_panel_so_a_later_disconnect_retries_it() {
    let mut runtime = panel_runtime(Sink {
        reject_on_attempt: Some(2),
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 1),
        &[momentary(0, MAIN_MUTE)],
    );

    assert!(matches!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));

    let failures = runtime.disconnect_panel();
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].control(), PanelControl::Button { index: 0 });
    assert_eq!(
        failures[0].command(),
        RuntimeControlCommand::MainMute { muted: false }
    );
    assert_eq!(*failures[0].source(), SinkError::Rejected);
    assert!(
        runtime.panel_connected(),
        "a target still latched is not a disconnected panel"
    );
    assert_eq!(
        runtime.sink().commands,
        [RuntimeControlCommand::MainMute { muted: true }]
    );

    assert!(runtime.disconnect_panel().is_empty());
    assert!(!runtime.panel_connected());
    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::MainMute { muted: true },
            RuntimeControlCommand::MainMute { muted: false },
        ]
    );
}

#[test]
fn a_relative_move_fails_closed_until_the_target_value_can_be_read() {
    let ready = Arc::new(AtomicBool::new(false));
    let mut runtime = panel_runtime(Sink {
        values: vec![(MAIN_GAIN, 1_000_000)],
        state_ready: Some(Arc::clone(&ready)),
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 0),
        &[encoder(0, MAIN_GAIN, 50_000)],
    );

    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Err(PanelDispatchError::StateUnavailable {
            control: PanelControl::Encoder { index: 0 }
        })
    );
    assert!(runtime.sink().commands.is_empty());

    ready.store(true, Ordering::Release);
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 1_050_000,
            },
        })
    );
}

#[test]
fn a_toggle_fails_closed_until_its_own_state_can_be_read() {
    let ready = Arc::new(AtomicBool::new(false));
    let mut runtime = panel_runtime(Sink {
        booleans: vec![(MAIN_MUTE, false)],
        state_ready: Some(Arc::clone(&ready)),
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 1),
        &[toggle(0, MAIN_MUTE)],
    );

    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Err(PanelDispatchError::StateUnavailable {
            control: PanelControl::Button { index: 0 }
        })
    );
    assert!(runtime.sink().commands.is_empty());

    ready.store(true, Ordering::Release);
    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Button { index: 0 },
            command: RuntimeControlCommand::MainMute { muted: true },
        })
    );
}

#[test]
fn a_transport_toggle_fails_closed_rather_than_guessing_the_transport_state() {
    let mut runtime = panel_runtime(Sink {
        transport_rolling: None,
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 1),
        &[PanelBinding::ActionButton {
            index: 0,
            action: PanelActionTarget::TransportToggle,
        }],
    );

    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Err(PanelDispatchError::StateUnavailable {
            control: PanelControl::Button { index: 0 }
        })
    );
    assert!(runtime.sink().commands.is_empty());
}

#[test]
fn a_rejected_send_is_reported_and_leaves_the_shadow_where_the_target_actually_is() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(MAIN_GAIN, 1_000_000)],
        reject_on_attempt: Some(1),
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 0),
        &[encoder(0, MAIN_GAIN, 50_000)],
    );

    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Err(PanelDispatchError::Sink {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 1_050_000,
            },
            source: SinkError::Rejected,
        })
    );
    assert!(runtime.sink().commands.is_empty());

    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 1_050_000,
            },
        }),
        "a refused command must not advance the shadow past a value the sink never took"
    );
}

#[test]
fn an_unavailable_target_is_reported_and_starts_working_once_the_sink_is_ready() {
    let mut runtime = panel_runtime(Sink {
        scene_ready: false,
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(0, 1),
        &[PanelBinding::ActionButton {
            index: 0,
            action: PanelActionTarget::SceneLaunch { scene_index: 0 },
        }],
    );

    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Unavailable {
            control: PanelControl::Button { index: 0 },
            reason: ControlBindingUnavailableReason::SceneAudioNotPrepared,
        })
    );
    assert!(runtime.sink().commands.is_empty());

    runtime.sink_mut().scene_ready = true;
    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Button { index: 0 },
            command: RuntimeControlCommand::SceneLaunch { scene_index: 0 },
        })
    );
}

#[test]
fn a_direct_edit_made_elsewhere_moves_the_value_the_next_detent_starts_from() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(MAIN_GAIN, 1_000_000)],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 0),
        &[encoder(0, MAIN_GAIN, 50_000)],
    );

    assert!(matches!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));

    runtime
        .try_send_direct(RuntimeControlCommand::MainGain {
            gain_millionths: 1_500_000,
        })
        .unwrap();

    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::MainGain {
                gain_millionths: 1_550_000,
            },
        }),
        "the encoder edits the target's position, not one it accumulated itself"
    );
}

#[test]
fn panel_messages_that_bind_nothing_command_nothing_and_say_why() {
    let mut runtime = panel_runtime(Sink::default());
    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Unbound(
            UnboundReason::NoPanelConnected
        ))
    );

    connect(
        &mut runtime,
        panel_descriptor(1, 1),
        &[PanelBinding::ActionButton {
            index: 0,
            action: PanelActionTarget::TransportStart,
        }],
    );

    for message in [
        Message::EncoderPress {
            control_id: 0,
            timestamp_us: 0,
            modifiers: 0,
        },
        Message::ButtonLongPress {
            control_id: 0,
            held_ms: 900,
            timestamp_us: 0,
            modifiers: 0,
        },
        Message::ButtonDoublePress {
            control_id: 0,
            timestamp_us: 0,
            modifiers: 0,
        },
        Message::Heartbeat {
            uptime_ms: 1,
            last_received_sequence: 1,
        },
    ] {
        assert_eq!(
            runtime.dispatch_panel(message),
            Ok(PanelDispatchOutcome::Unbound(
                UnboundReason::MessageBindsNothing
            ))
        );
    }

    for control_id in [256, 300, u16::MAX] {
        assert_eq!(
            runtime.dispatch_panel(press(control_id)),
            Ok(PanelDispatchOutcome::Unbound(
                UnboundReason::ControlOutOfRange
            )),
            "a control id no descriptor can name is not the same as an unbound control"
        );
    }

    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Unbound(UnboundReason::NoBinding {
            control: PanelControl::Encoder { index: 0 }
        }))
    );
    assert!(runtime.sink().commands.is_empty());
}

#[test]
fn panel_dispatch_performs_zero_heap_operations() {
    let mut runtime = panel_runtime(Sink {
        commands: Vec::with_capacity(2),
        booleans: vec![(MAIN_MUTE, false)],
        values: vec![(MAIN_GAIN, 1_000_000)],
        ..Sink::default()
    });
    connect(
        &mut runtime,
        panel_descriptor(1, 1),
        &[encoder(0, MAIN_GAIN, 50_000), toggle(0, MAIN_MUTE)],
    );
    // Fill both shadows so the measured dispatches do no first-use reads.
    assert!(matches!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert!(matches!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    runtime.sink_mut().commands.clear();

    let measurement = HeapMeasurement::start_verified();
    for message in [turn(0, 1), press(0)] {
        assert!(matches!(
            runtime.dispatch_panel(message),
            Ok(PanelDispatchOutcome::Sent { .. })
        ));
    }
    let heap_operations = measurement.finish();

    assert_eq!(heap_operations, 0);
}

struct PanelPeers {
    session: PanelSession,
    panel: PanelSimulator,
    buffer: [u8; 1024],
}

impl PanelPeers {
    fn new(descriptor: PanelDescriptor) -> Self {
        Self {
            session: PanelSession::new(LinkConfig::new(50, 200).unwrap()),
            panel: PanelSimulator::new(PanelSimulatorConfig {
                descriptor,
                bootloader_available: false,
            })
            .unwrap(),
            buffer: [0; 1024],
        }
    }

    fn exchange(&mut self, now_ms: u64) -> Vec<PanelEvent> {
        let outbound = self.session.take_transmit();
        if !outbound.is_empty() {
            self.panel.receive_runtime_bytes(&outbound, now_ms);
        }
        self.panel.poll(now_ms).unwrap();
        let read = self.panel.read_panel_bytes(&mut self.buffer);
        if read == 0 {
            return Vec::new();
        }
        let inbound = self.buffer[..read].to_vec();
        self.session.receive(&inbound, now_ms)
    }

    /// Drives one exchange the way a runtime binary must: input is dispatched,
    /// and every failure event takes the panel's bindings down rather than
    /// being discarded. Returns whether the link is still usable.
    fn drive(&mut self, runtime: &mut ControlRuntime<Sink>, now_ms: u64) -> bool {
        let mut live = true;
        for event in self.exchange(now_ms) {
            match event {
                PanelEvent::Input(message) => {
                    // A panel can outrun the first mixer snapshot, so an
                    // unreadable target is an ordinary startup race, not a fault.
                    match runtime.dispatch_panel(message) {
                        Ok(_) | Err(PanelDispatchError::StateUnavailable { .. }) => {}
                        Err(error) => panic!("the sink refused panel input: {error:?}"),
                    }
                }
                PanelEvent::Fault(_)
                | PanelEvent::TimedOut
                | PanelEvent::VersionRejected
                | PanelEvent::Undecodable(_) => {
                    assert!(runtime.disconnect_panel().is_empty());
                    live = false;
                }
                PanelEvent::Connected(_) => {}
            }
        }
        live
    }
}

fn simulator_descriptor() -> PanelDescriptor {
    PanelDescriptor {
        firmware: FirmwareVersion {
            major: 1,
            minor: 0,
            patch: 0,
            build: 7,
        },
        boot_id: 0x00AB_CDEF,
        encoders: 4,
        buttons: 8,
        leds: 8,
        rings: 2,
        ring_segments: 16,
    }
}

fn connected_peers(runtime: &mut ControlRuntime<Sink>, bindings: &[PanelBinding]) -> PanelPeers {
    let mut peers = PanelPeers::new(simulator_descriptor());
    peers.session.start(0x5EED, 0).unwrap();
    let events = peers.exchange(1);
    let Some(PanelEvent::Connected(found)) = events
        .iter()
        .find(|event| matches!(event, PanelEvent::Connected(_)))
        .copied()
    else {
        panic!("expected the handshake to complete, saw {events:?}");
    };
    let connection = runtime.connect_panel(found, bindings).unwrap();
    assert!(connection.is_fully_installed());
    peers
}

#[test]
fn a_real_panel_link_reaches_the_bound_targets_through_encoded_frames() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(MAIN_GAIN, 1_000_000)],
        booleans: vec![(MAIN_MUTE, false)],
        transport_rolling: Some(false),
        ..Sink::default()
    });
    let mut peers = connected_peers(
        &mut runtime,
        &[
            encoder(0, MAIN_GAIN, 50_000),
            toggle(1, MAIN_MUTE),
            PanelBinding::ActionButton {
                index: 2,
                action: PanelActionTarget::TransportStart,
            },
        ],
    );

    peers.panel.encoder_delta(0, 3, 2).unwrap();
    assert!(peers.drive(&mut runtime, 2));
    peers.panel.button_press(1, 3).unwrap();
    assert!(peers.drive(&mut runtime, 3));
    peers.panel.button_release(1, 4).unwrap();
    assert!(peers.drive(&mut runtime, 4));
    peers.panel.button_press(2, 5).unwrap();
    assert!(peers.drive(&mut runtime, 5));

    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::MainGain {
                gain_millionths: 1_150_000,
            },
            RuntimeControlCommand::MainMute { muted: true },
            RuntimeControlCommand::TransportStart,
        ]
    );
    assert!(!peers.session.is_faulted());
}

#[test]
fn a_link_lost_while_a_control_is_held_releases_it_over_the_real_link() {
    let mut runtime = panel_runtime(Sink::default());
    let mut peers = connected_peers(&mut runtime, &[momentary(1, MAIN_MUTE)]);

    peers.panel.button_press(1, 2).unwrap();
    assert!(peers.drive(&mut runtime, 2));
    assert_eq!(
        runtime.sink().commands,
        [RuntimeControlCommand::MainMute { muted: true }]
    );

    peers.panel.disconnect();
    let mut now_ms = 10_u64;
    while now_ms < 2_000 && runtime.panel_connected() {
        let _ = peers.session.take_transmit();
        if matches!(peers.session.poll(now_ms), Some(PanelEvent::TimedOut)) {
            assert!(runtime.disconnect_panel().is_empty());
        }
        now_ms += 50;
    }

    assert!(
        !runtime.panel_connected(),
        "a panel that stopped answering is disconnected"
    );
    assert_eq!(
        runtime.sink().commands,
        [
            RuntimeControlCommand::MainMute { muted: true },
            RuntimeControlCommand::MainMute { muted: false },
        ],
        "the held control is released rather than left latched"
    );
}

#[test]
fn a_send_binding_is_validated_against_the_parameter_it_actually_drives() {
    let send_gain = ValueControlTarget::SendGain {
        channel_index: 2,
        send_index: 1,
    };
    let mut runtime = panel_runtime(Sink {
        invalid_target: Some(RuntimeControlTarget::Send {
            channel_index: 2,
            send_index: 1,
            parameter: SendParameter::Gain,
        }),
        values: vec![(send_gain, 1_000_000)],
        ..Sink::default()
    });

    let connection = runtime
        .connect_panel(panel_descriptor(1, 0), &[encoder(0, send_gain, 100_000)])
        .unwrap();

    assert_eq!(
        connection.refused().len(),
        1,
        "a send gain binding is offered to the sink as a send gain target, not as some other send parameter"
    );
    assert!(matches!(
        connection.refused()[0].reason(),
        PanelBindingRefusalReason::TargetRejected(SinkError::InvalidTarget)
    ));
}

#[test]
fn a_value_domain_maps_its_whole_range_onto_a_display() {
    let gain = ValueDomain::new(0, 2_000_000);
    assert_eq!(gain.normalize(0), 0);
    assert_eq!(gain.normalize(2_000_000), u16::MAX);
    assert_eq!(gain.normalize(1_000_000), u16::MAX / 2);

    // A domain whose minimum is negative is where spanning the maximum rather
    // than the range goes wrong, and gain alone would never show it.
    let pan = ValueDomain::new(-1_000_000, 1_000_000);
    assert_eq!(pan.normalize(-1_000_000), 0);
    assert_eq!(pan.normalize(1_000_000), u16::MAX);
    assert_eq!(
        pan.normalize(0),
        u16::MAX / 2,
        "centre reads as half scale, not as the bottom of the range"
    );
}

#[test]
fn a_position_outside_its_domain_is_shown_at_the_nearest_edge() {
    let pan = ValueDomain::new(-1_000_000, 1_000_000);

    assert_eq!(pan.normalize(-5_000_000), 0);
    assert_eq!(pan.normalize(5_000_000), u16::MAX);
    assert_eq!(pan.normalize(i32::MIN), 0);
    assert_eq!(pan.normalize(i32::MAX), u16::MAX);
}
