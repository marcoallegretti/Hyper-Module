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
    PanelBindingRefusalReason, PanelButtonBehavior, PanelControl, PanelDispatchError,
    PanelDispatchOutcome, RuntimeControlCommand, RuntimeControlSink, RuntimeControlTarget,
    ValueControlTarget,
};
use hyper_mapping_compiler::{ControlMappingResolver, MixerSendAddress, compile_control_mapping};
use hyper_panel_protocol::{FirmwareVersion, Message, PanelDescriptor};
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

    fn try_send(&mut self, command: RuntimeControlCommand) -> Result<(), Self::Error> {
        self.attempts += 1;
        if self.reject_on_attempt == Some(self.attempts) {
            Err(SinkError::Rejected)
        } else {
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
            RuntimeControlCommand::ChannelGain { .. }
            | RuntimeControlCommand::ChannelPan { .. }
            | RuntimeControlCommand::ChannelMute { .. }
            | RuntimeControlCommand::ChannelSolo { .. }
            | RuntimeControlCommand::SendGain { .. }
            | RuntimeControlCommand::SendPan { .. }
            | RuntimeControlCommand::SendEnabled { .. }
            | RuntimeControlCommand::MainGain { .. }
            | RuntimeControlCommand::TransportLocateStartPreserve
            | RuntimeControlCommand::SceneLaunch { .. } => DirectControlStateUpdate::Unchanged,
        })
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

#[test]
fn connect_refuses_bindings_the_panel_cannot_serve_and_installs_the_rest() {
    let mut runtime = panel_runtime(Sink {
        invalid_target: Some(RuntimeControlTarget::Channel {
            channel_index: 3,
            parameter: ChannelParameter::Gain,
        }),
        values: vec![(
            ValueControlTarget::ChannelGain { channel_index: 0 },
            1_000_000,
        )],
        ..Sink::default()
    });

    let refusals = runtime
        .connect_panel(
            panel_descriptor(2, 2),
            &[
                PanelBinding::Encoder {
                    index: 0,
                    target: ValueControlTarget::ChannelGain { channel_index: 0 },
                    step_millionths: step(50_000),
                },
                PanelBinding::Encoder {
                    index: 2,
                    target: ValueControlTarget::ChannelGain { channel_index: 0 },
                    step_millionths: step(50_000),
                },
                PanelBinding::Button {
                    index: 2,
                    target: BooleanControlTarget::ChannelMute { channel_index: 0 },
                    behavior: PanelButtonBehavior::Toggle,
                },
                PanelBinding::Encoder {
                    index: 0,
                    target: ValueControlTarget::ChannelPan { channel_index: 0 },
                    step_millionths: step(1),
                },
                PanelBinding::Encoder {
                    index: 1,
                    target: ValueControlTarget::ChannelGain { channel_index: 3 },
                    step_millionths: step(1),
                },
            ],
        )
        .unwrap();

    let reported: Vec<usize> = refusals
        .iter()
        .map(hyper_control_runtime::PanelBindingRefusal::binding_index)
        .collect();
    assert_eq!(reported, [1, 2, 3, 4]);
    assert!(matches!(
        refusals[0].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert!(matches!(
        refusals[1].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert!(matches!(
        refusals[2].reason(),
        PanelBindingRefusalReason::ControlAlreadyBound {
            first_binding_index: 0
        }
    ));
    assert!(matches!(
        refusals[3].reason(),
        PanelBindingRefusalReason::TargetRejected(SinkError::InvalidTarget)
    ));

    assert_eq!(
        runtime.dispatch_panel(turn(2, 1)),
        Ok(PanelDispatchOutcome::Unbound)
    );
    assert_eq!(
        runtime.dispatch_panel(press(2)),
        Ok(PanelDispatchOutcome::Unbound)
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 1_050_000,
            },
        })
    );
}

#[test]
fn encoder_deltas_move_the_bound_target_in_the_turned_direction_and_stop_at_its_limit() {
    let mut runtime = panel_runtime(Sink {
        values: vec![(ValueControlTarget::ChannelPan { channel_index: 0 }, 0)],
        ..Sink::default()
    });
    runtime
        .connect_panel(
            panel_descriptor(1, 0),
            &[PanelBinding::Encoder {
                index: 0,
                target: ValueControlTarget::ChannelPan { channel_index: 0 },
                step_millionths: step(250_000),
            }],
        )
        .unwrap();

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
        Ok(PanelDispatchOutcome::Idle { control })
    );
    assert_eq!(runtime.sink().commands.len(), 3);
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
    runtime
        .connect_panel(
            panel_descriptor(0, 2),
            &[
                PanelBinding::Button {
                    index: 0,
                    target: BooleanControlTarget::ChannelMute { channel_index: 0 },
                    behavior: PanelButtonBehavior::Momentary,
                },
                PanelBinding::Button {
                    index: 1,
                    target: BooleanControlTarget::ChannelSolo { channel_index: 0 },
                    behavior: PanelButtonBehavior::Toggle,
                },
            ],
        )
        .unwrap();

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
    runtime
        .connect_panel(
            panel_descriptor(0, 3),
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
            ],
        )
        .unwrap();

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
    for control_id in [0, 1, 2] {
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
        ]
    );
}

#[test]
fn bindings_survive_a_reconnect_only_while_the_new_descriptor_still_has_the_control() {
    let bindings = [
        PanelBinding::Encoder {
            index: 0,
            target: ValueControlTarget::ChannelGain { channel_index: 0 },
            step_millionths: step(50_000),
        },
        PanelBinding::Button {
            index: 3,
            target: BooleanControlTarget::ChannelMute { channel_index: 0 },
            behavior: PanelButtonBehavior::Toggle,
        },
    ];
    let mut runtime = panel_runtime(Sink {
        values: vec![(
            ValueControlTarget::ChannelGain { channel_index: 0 },
            1_000_000,
        )],
        booleans: vec![(
            BooleanControlTarget::ChannelMute { channel_index: 0 },
            false,
        )],
        ..Sink::default()
    });

    assert!(
        runtime
            .connect_panel(panel_descriptor(4, 8), &bindings)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        runtime.connect_panel(panel_descriptor(4, 8), &bindings),
        Err(hyper_control_runtime::PanelAlreadyConnected)
    );
    assert!(matches!(
        runtime.dispatch_panel(press(3)),
        Ok(PanelDispatchOutcome::Sent { .. })
    ));
    assert_eq!(
        runtime.dispatch_panel(turn(0, 2)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 1_100_000,
            },
        })
    );

    assert!(runtime.disconnect_panel().is_empty());
    assert!(!runtime.panel_connected());
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Unbound)
    );

    let refusals = runtime
        .connect_panel(panel_descriptor(1, 2), &bindings)
        .unwrap();
    assert_eq!(refusals.len(), 1);
    assert_eq!(refusals[0].binding_index(), 1);
    assert!(matches!(
        refusals[0].reason(),
        PanelBindingRefusalReason::ControlAbsent
    ));
    assert_eq!(
        runtime.dispatch_panel(press(3)),
        Ok(PanelDispatchOutcome::Unbound)
    );
    assert_eq!(
        runtime.dispatch_panel(turn(0, 1)),
        Ok(PanelDispatchOutcome::Sent {
            control: PanelControl::Encoder { index: 0 },
            command: RuntimeControlCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 1_050_000,
            },
        })
    );
}

#[test]
fn disconnecting_releases_only_the_momentary_control_still_held() {
    let mut runtime = panel_runtime(Sink { ..Sink::default() });
    runtime
        .connect_panel(
            panel_descriptor(0, 2),
            &[
                PanelBinding::Button {
                    index: 0,
                    target: BooleanControlTarget::ChannelMute { channel_index: 0 },
                    behavior: PanelButtonBehavior::Momentary,
                },
                PanelBinding::Button {
                    index: 1,
                    target: BooleanControlTarget::ChannelSolo { channel_index: 0 },
                    behavior: PanelButtonBehavior::Momentary,
                },
            ],
        )
        .unwrap();

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
fn a_relative_move_fails_closed_until_the_target_value_can_be_read() {
    let ready = Arc::new(AtomicBool::new(false));
    let mut runtime = panel_runtime(Sink {
        values: vec![(
            ValueControlTarget::ChannelGain { channel_index: 0 },
            1_000_000,
        )],
        state_ready: Some(Arc::clone(&ready)),
        ..Sink::default()
    });
    runtime
        .connect_panel(
            panel_descriptor(1, 0),
            &[PanelBinding::Encoder {
                index: 0,
                target: ValueControlTarget::ChannelGain { channel_index: 0 },
                step_millionths: step(50_000),
            }],
        )
        .unwrap();

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
            command: RuntimeControlCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 1_050_000,
            },
        })
    );
}

#[test]
fn panel_messages_that_bind_nothing_command_nothing() {
    let mut runtime = panel_runtime(Sink::default());
    assert_eq!(
        runtime.dispatch_panel(press(0)),
        Ok(PanelDispatchOutcome::Unbound)
    );

    runtime
        .connect_panel(
            panel_descriptor(1, 1),
            &[PanelBinding::ActionButton {
                index: 0,
                action: PanelActionTarget::TransportStart,
            }],
        )
        .unwrap();

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
        turn(0, 1),
    ] {
        assert_eq!(
            runtime.dispatch_panel(message),
            Ok(PanelDispatchOutcome::Unbound)
        );
    }
    assert!(runtime.sink().commands.is_empty());
}

#[test]
fn panel_dispatch_performs_zero_heap_operations() {
    let mut runtime = panel_runtime(Sink {
        commands: Vec::with_capacity(2),
        booleans: vec![(BooleanControlTarget::MainMute, false)],
        values: vec![(ValueControlTarget::MainGain, 1_000_000)],
        ..Sink::default()
    });
    runtime
        .connect_panel(
            panel_descriptor(1, 1),
            &[
                PanelBinding::Encoder {
                    index: 0,
                    target: ValueControlTarget::MainGain,
                    step_millionths: step(50_000),
                },
                PanelBinding::Button {
                    index: 0,
                    target: BooleanControlTarget::MainMute,
                    behavior: PanelButtonBehavior::Toggle,
                },
            ],
        )
        .unwrap();

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
