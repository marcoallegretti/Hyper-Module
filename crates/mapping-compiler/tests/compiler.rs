use hyper_control_mappings::{
    ChannelParameter, ContinuousControlValue, ControlMappingBindingErrorKind,
    ControlMappingDocument, ControlMappingProject, ControlTransform, CueParameter, Edge,
    MainParameter, SceneAction, SendParameter, TransportAction, TriggerBehavior,
};
use hyper_mapping_compiler::{
    CompiledControlScope, CompiledControlSource, CompiledControlTarget, ControlMappingCompileError,
    ControlMappingReference, ControlMappingResolver, MixerSendAddress, compile_control_mapping,
};
use serde_json::{Value, json};

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334400";
const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334401";
const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334402";
const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334403";
const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334404";
const BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334405";
const SECOND_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-101122334406";

#[derive(Default)]
struct Resolver {
    omit_channel_address: bool,
}

impl ControlMappingProject for Resolver {
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

    fn contains_nested_entity(&self, _id: &str) -> bool {
        false
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

impl ControlMappingResolver for Resolver {
    fn track_index(&self, track_id: &str) -> Option<usize> {
        (track_id == TRACK_ID).then_some(2)
    }

    fn scene_index(&self, scene_id: &str) -> Option<usize> {
        (scene_id == SCENE_ID).then_some(3)
    }

    fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
        (track_id == TRACK_ID && !self.omit_channel_address).then_some(4)
    }

    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress> {
        (send_id == SEND_ID).then_some(MixerSendAddress::new(4, 1))
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one exhaustive table keeps every accepted target variant and its numeric result auditable"
)]
fn compiles_every_version_one_target_to_numeric_runtime_addresses() {
    let cases = [
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "track", "track_id": TRACK_ID}),
            json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "gain"}),
            CompiledControlTarget::MixerChannel {
                channel_index: 4,
                parameter: ChannelParameter::Gain,
            },
        ),
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "mixer-page"}),
            json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "pan"}),
            CompiledControlTarget::MixerChannel {
                channel_index: 4,
                parameter: ChannelParameter::Pan,
            },
        ),
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "global"}),
            json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "gain"}),
            CompiledControlTarget::MixerSend {
                address: MixerSendAddress::new(4, 1),
                parameter: SendParameter::Gain,
            },
        ),
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "mixer-page"}),
            json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "pan"}),
            CompiledControlTarget::MixerSend {
                address: MixerSendAddress::new(4, 1),
                parameter: SendParameter::Pan,
            },
        ),
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "global"}),
            json!({"kind": "mixer-main", "parameter": "gain"}),
            CompiledControlTarget::MixerMain {
                parameter: MainParameter::Gain,
            },
        ),
        (
            cv_source(),
            linear_transform(),
            json!({"kind": "scene", "scene_id": SCENE_ID}),
            json!({"kind": "mixer-cue", "parameter": "gain"}),
            CompiledControlTarget::MixerCue {
                parameter: CueParameter::Gain,
            },
        ),
        (
            clock_source("rising"),
            trigger_transform("toggle"),
            json!({"kind": "global"}),
            json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "mute"}),
            CompiledControlTarget::MixerChannel {
                channel_index: 4,
                parameter: ChannelParameter::Mute,
            },
        ),
        (
            reset_source("falling"),
            trigger_transform("momentary"),
            json!({"kind": "global"}),
            json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "solo"}),
            CompiledControlTarget::MixerChannel {
                channel_index: 4,
                parameter: ChannelParameter::Solo,
            },
        ),
        (
            clock_source("falling"),
            trigger_transform("toggle"),
            json!({"kind": "global"}),
            json!({"kind": "mixer-send", "send_id": SEND_ID, "parameter": "enabled"}),
            CompiledControlTarget::MixerSend {
                address: MixerSendAddress::new(4, 1),
                parameter: SendParameter::Enabled,
            },
        ),
        (
            reset_source("rising"),
            trigger_transform("toggle"),
            json!({"kind": "global"}),
            json!({"kind": "mixer-main", "parameter": "mute"}),
            CompiledControlTarget::MixerMain {
                parameter: MainParameter::Mute,
            },
        ),
        (
            clock_source("rising"),
            trigger_transform("momentary"),
            json!({"kind": "global"}),
            json!({"kind": "mixer-cue", "parameter": "mute"}),
            CompiledControlTarget::MixerCue {
                parameter: CueParameter::Mute,
            },
        ),
        (
            reset_source("rising"),
            trigger_transform("invoke"),
            json!({"kind": "global"}),
            json!({"kind": "scene", "scene_id": SCENE_ID, "action": "launch"}),
            CompiledControlTarget::Scene {
                scene_index: 3,
                action: SceneAction::Launch,
            },
        ),
    ];

    for (source, transform, scope, target, expected) in cases {
        let document = document(&[binding(BINDING_ID, true, source, scope, transform, target)]);
        let table = compile_control_mapping(&document, MAPPING_ID, &Resolver::default()).unwrap();
        assert_eq!(table.bindings().len(), 1);
        assert_eq!(table.bindings()[0].target(), expected);
    }

    for action in [
        TransportAction::Play,
        TransportAction::Stop,
        TransportAction::Toggle,
        TransportAction::Reset,
    ] {
        let action_name = match action {
            TransportAction::Play => "play",
            TransportAction::Stop => "stop",
            TransportAction::Toggle => "toggle",
            TransportAction::Reset => "reset",
        };
        let document = document(&[binding(
            BINDING_ID,
            true,
            clock_source("rising"),
            json!({"kind": "global"}),
            trigger_transform("invoke"),
            json!({"kind": "transport", "action": action_name}),
        )]);
        let table = compile_control_mapping(&document, MAPPING_ID, &Resolver::default()).unwrap();
        assert_eq!(
            table.bindings()[0].target(),
            CompiledControlTarget::Transport { action }
        );
    }
}

#[test]
fn preserves_sources_scopes_transforms_and_exact_value_mapping() {
    let resolver = Resolver::default();
    let cv = document(&[binding(
        BINDING_ID,
        true,
        cv_source(),
        json!({"kind": "track", "track_id": TRACK_ID}),
        linear_transform(),
        json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "gain"}),
    )]);
    let compiled = compile_control_mapping(&cv, MAPPING_ID, &resolver)
        .unwrap()
        .bindings()[0];
    assert_eq!(
        compiled.source(),
        CompiledControlSource::CvInput { input_index: 0 }
    );
    assert_eq!(
        compiled.scope(),
        CompiledControlScope::Track { track_index: 2 }
    );
    assert_eq!(
        compiled.transform(),
        ControlTransform::Linear {
            input_min: -1_000_000,
            input_max: 1_000_000,
            output_min: 0,
            output_max: 1_000_000,
        }
    );
    assert_eq!(
        compiled.map_continuous(0),
        Some(ContinuousControlValue::GainMillionths(1_000_000))
    );
    assert_eq!(compiled.trigger_behavior(), None);

    let trigger = document(&[binding(
        BINDING_ID,
        true,
        reset_source("falling"),
        json!({"kind": "scene", "scene_id": SCENE_ID}),
        trigger_transform("invoke"),
        json!({"kind": "transport", "action": "stop"}),
    )]);
    let compiled = compile_control_mapping(&trigger, MAPPING_ID, &resolver)
        .unwrap()
        .bindings()[0];
    assert_eq!(
        compiled.source(),
        CompiledControlSource::ResetInput {
            edge: Edge::Falling,
        }
    );
    assert_eq!(
        compiled.scope(),
        CompiledControlScope::Scene { scene_index: 3 }
    );
    assert_eq!(compiled.trigger_behavior(), Some(TriggerBehavior::Invoke));
    assert_eq!(compiled.map_continuous(0), None);
}

#[test]
fn excludes_disabled_bindings_and_keeps_source_ordinals() {
    let document = document(&[
        binding(
            BINDING_ID,
            false,
            cv_source(),
            json!({"kind": "global"}),
            linear_transform(),
            json!({"kind": "mixer-main", "parameter": "gain"}),
        ),
        binding(
            SECOND_BINDING_ID,
            true,
            clock_source("rising"),
            json!({"kind": "global"}),
            trigger_transform("invoke"),
            json!({"kind": "transport", "action": "play"}),
        ),
    ]);

    let table = compile_control_mapping(&document, MAPPING_ID, &Resolver::default()).unwrap();
    assert_eq!(table.bindings().len(), 1);
    assert_eq!(table.bindings()[0].source_binding_index(), 1);
}

#[test]
fn validates_before_compilation_and_fails_on_resolver_divergence() {
    let document = document(&[binding(
        BINDING_ID,
        true,
        cv_source(),
        json!({"kind": "global"}),
        linear_transform(),
        json!({"kind": "mixer-channel", "track_id": TRACK_ID, "parameter": "gain"}),
    )]);
    let error = compile_control_mapping(&document, PROJECT_ID, &Resolver::default()).unwrap_err();
    assert!(matches!(
        error,
        ControlMappingCompileError::Binding(source)
            if source.kind() == ControlMappingBindingErrorKind::DescriptorIdMismatch
    ));

    let error = compile_control_mapping(
        &document,
        MAPPING_ID,
        &Resolver {
            omit_channel_address: true,
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ControlMappingCompileError::ResolverInconsistent {
            binding_index: 0,
            reference: ControlMappingReference::MixerChannel,
        }
    ));
}

#[test]
fn runtime_bindings_have_no_owned_or_drop_state() {
    assert!(!core::mem::needs_drop::<
        hyper_mapping_compiler::CompiledControlBinding,
    >());
}

fn document(bindings: &[Value]) -> ControlMappingDocument {
    ControlMappingDocument::parse(
        &serde_json::to_vec(&json!({
            "format": "hyper-control-mappings",
            "format_version": 1,
            "id": MAPPING_ID,
            "project_id": PROJECT_ID,
            "bindings": bindings,
            "extensions": {}
        }))
        .unwrap(),
    )
    .unwrap()
}

fn binding(
    id: &str,
    enabled: bool,
    source: Value,
    scope: Value,
    transform: Value,
    target: Value,
) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("id".to_owned(), Value::String(id.to_owned()));
    object.insert("enabled".to_owned(), Value::Bool(enabled));
    object.insert("source".to_owned(), source);
    object.insert("scope".to_owned(), scope);
    object.insert("transform".to_owned(), transform);
    object.insert("target".to_owned(), target);
    Value::Object(object)
}

fn cv_source() -> Value {
    json!({"kind": "cv-input", "input_index": 0})
}

fn clock_source(edge: &str) -> Value {
    json!({"kind": "clock-input", "edge": edge})
}

fn reset_source(edge: &str) -> Value {
    json!({"kind": "reset-input", "edge": edge})
}

fn linear_transform() -> Value {
    json!({
        "kind": "linear",
        "input_min": -1_000_000,
        "input_max": 1_000_000,
        "output_min": 0,
        "output_max": 1_000_000
    })
}

fn trigger_transform(behavior: &str) -> Value {
    json!({"kind": "trigger", "behavior": behavior})
}
