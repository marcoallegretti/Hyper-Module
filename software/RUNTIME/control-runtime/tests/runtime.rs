use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use hyper_control_mappings::{
    ChannelParameter, ControlBinding, ControlMappingDocument, ControlMappingProject, ControlScope,
    ControlSource, ControlTarget, ControlTransform, CueParameter, Edge, MainParameter, SceneAction,
    SendParameter, TransportAction, TriggerBehavior,
};
use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingAvailability, ControlBindingUnavailableReason,
    ControlContext, ControlDispatchError, ControlDispatchOutcome, ControlEvent, ControlRuntime,
    ControlRuntimeBuildErrorReason, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget,
};
use hyper_mapping_compiler::{ControlMappingResolver, MixerSendAddress, compile_control_mapping};

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
    state_ready: Option<Arc<AtomicBool>>,
    transport_rolling: Option<bool>,
    commands: Vec<RuntimeControlCommand>,
}

impl RuntimeControlSink for Sink {
    type Error = SinkError;

    fn validate_target(&self, target: RuntimeControlTarget) -> Result<(), Self::Error> {
        if self.invalid_target == Some(target) {
            Err(SinkError::InvalidTarget)
        } else {
            Ok(())
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

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        self.attempts += 1;
        if self.reject_on_attempt == Some(self.attempts) {
            Err(SinkError::Rejected)
        } else {
            self.commands.push(command);
            Ok(())
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
    ControlBinding::try_new(
        binding_id(index),
        true,
        ControlSource::ClockInput { edge: Edge::Rising },
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
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::MomentarySourceStateUnavailable
            ),
            ControlBindingAvailability::Unavailable(
                ControlBindingUnavailableReason::InteractiveSceneLaunchUnavailable
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
