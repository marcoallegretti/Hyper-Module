use std::collections::BTreeMap;

use hyper_control_mappings::{
    ChannelParameter, ControlBinding, ControlMappingDocument, ControlMappingProject, ControlScope,
    ControlSource, ControlTarget, ControlTransform, MAX_BINDINGS,
};
use hyper_mapping_compiler::{
    CompiledControlScope, CompiledControlTarget, ControlMappingResolver, MixerSendAddress,
};
use hyper_mapping_editor::{
    MappingConflictKind, MappingEditor, MappingEditorConfirmationError, MappingEditorMutationError,
    MappingEditorPreviewError,
};

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334400";
const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334401";
const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334402";
const SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334403";
const SEND_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334404";
const FIRST_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334405";
const SECOND_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334406";
const THIRD_BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-201122334407";

struct Resolver;

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
        (track_id == TRACK_ID).then_some(3)
    }

    fn scene_index(&self, scene_id: &str) -> Option<usize> {
        (scene_id == SCENE_ID).then_some(4)
    }

    fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
        (track_id == TRACK_ID).then_some(5)
    }

    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress> {
        (send_id == SEND_ID).then_some(MixerSendAddress::new(5, 1))
    }
}

#[test]
fn manual_edits_preview_as_compiled_canonical_content_and_confirm_explicitly() {
    let original = document(vec![binding(
        FIRST_BINDING_ID,
        true,
        0,
        ControlScope::Global,
    )]);
    let mut editor = MappingEditor::new(original.clone());
    assert!(!editor.is_dirty());

    editor
        .replace_binding(binding(FIRST_BINDING_ID, true, 1, ControlScope::Global))
        .unwrap();
    editor
        .insert_binding(binding(
            SECOND_BINDING_ID,
            true,
            2,
            ControlScope::Track {
                track_id: TRACK_ID.to_owned(),
            },
        ))
        .unwrap();
    let removed = editor
        .remove_binding(SECOND_BINDING_ID)
        .expect("the newly inserted binding must remain addressable");
    editor.insert_binding(removed).unwrap();
    assert!(editor.is_dirty());
    assert_eq!(editor.revision().value(), 4);

    let preview = editor.preview(&Resolver).unwrap();
    assert_eq!(preview.revision(), editor.revision());
    assert_eq!(preview.document().bindings().len(), 2);
    assert_eq!(preview.compiled().bindings().len(), 2);
    assert_eq!(
        preview.compiled().bindings()[1].scope(),
        CompiledControlScope::Track { track_index: 3 }
    );
    assert!(matches!(
        preview.compiled().bindings()[0].target(),
        CompiledControlTarget::MixerChannel {
            channel_index: 5,
            parameter: ChannelParameter::Gain,
        }
    ));
    assert_eq!(
        ControlMappingDocument::parse(preview.canonical_json()).unwrap(),
        *preview.document()
    );
    assert_eq!(original.bindings().len(), 1);

    let confirmed = editor.confirm(preview).unwrap();
    let (document, compiled, canonical_json) = confirmed.into_parts();
    assert_eq!(document.bindings().len(), 2);
    assert_eq!(compiled.bindings().len(), 2);
    assert_eq!(
        ControlMappingDocument::parse(&canonical_json).unwrap(),
        document
    );
}

#[test]
fn conflicts_are_bounded_deterministic_and_block_preview_without_touching_original() {
    let original = document(Vec::new());
    let mut editor = MappingEditor::new(original.clone());
    for id in [FIRST_BINDING_ID, SECOND_BINDING_ID, THIRD_BINDING_ID] {
        editor
            .insert_binding(binding(id, true, 0, ControlScope::Global))
            .unwrap();
    }

    let conflicts = editor.inspect_conflicts().unwrap();
    assert_eq!(conflicts.len(), 2);
    assert_eq!(
        conflicts[0].kind(),
        MappingConflictKind::SourceScopeCollision
    );
    assert_eq!(conflicts[0].first_binding_index(), 0);
    assert_eq!(conflicts[0].second_binding_index(), 1);
    assert_eq!(conflicts[1].first_binding_index(), 1);
    assert_eq!(conflicts[1].second_binding_index(), 2);
    assert!(matches!(
        editor.preview(&Resolver),
        Err(MappingEditorPreviewError::Conflict(found)) if found == conflicts
    ));

    assert_eq!(editor.cancel(), original);
}

#[test]
fn stale_confirmation_returns_both_authorities_for_recovery() {
    let original = document(vec![binding(
        FIRST_BINDING_ID,
        true,
        0,
        ControlScope::Global,
    )]);
    let mut editor = MappingEditor::new(original);
    let stale = editor.preview(&Resolver).unwrap();
    editor.set_binding_enabled(FIRST_BINDING_ID, false).unwrap();

    let rejected = editor.confirm(stale).unwrap_err();
    assert_eq!(
        rejected.reason(),
        MappingEditorConfirmationError::StalePreview
    );
    let (editor, _stale) = rejected.into_parts();
    let current = editor.preview(&Resolver).unwrap();
    assert!(current.compiled().is_empty());
    let confirmed = editor.confirm(current).unwrap();
    assert!(!confirmed.document().bindings()[0].enabled());
}

#[test]
fn mutation_failures_and_no_ops_are_failure_atomic() {
    let existing = binding(FIRST_BINDING_ID, true, 0, ControlScope::Global);
    let mut editor = MappingEditor::new(document(vec![existing.clone()]));
    assert_eq!(
        editor.insert_binding(existing),
        Err(MappingEditorMutationError::DuplicateBindingId)
    );
    assert_eq!(editor.revision().value(), 0);
    assert_eq!(
        editor.remove_binding(SECOND_BINDING_ID),
        Err(MappingEditorMutationError::BindingMissing)
    );
    assert_eq!(
        editor.set_binding_enabled(SECOND_BINDING_ID, false),
        Err(MappingEditorMutationError::BindingMissing)
    );
    editor.set_binding_enabled(FIRST_BINDING_ID, true).unwrap();
    assert_eq!(editor.revision().value(), 0);
    assert!(!editor.is_dirty());
}

#[test]
fn binding_limit_is_enforced_before_mutation() {
    let mut editor = MappingEditor::new(document(Vec::new()));
    for index in 0..MAX_BINDINGS {
        let id = format!("018f47a2-6b2d-7c11-8a44-{index:012x}");
        editor
            .insert_binding(binding(&id, true, 0, ControlScope::Global))
            .unwrap();
    }
    let revision = editor.revision();
    assert_eq!(editor.bindings().len(), MAX_BINDINGS);
    assert_eq!(
        editor.insert_binding(binding(
            "018f47a2-6b2d-7c11-8a45-000000000000",
            true,
            0,
            ControlScope::Global,
        )),
        Err(MappingEditorMutationError::BindingLimit)
    );
    assert_eq!(editor.revision(), revision);
    assert_eq!(editor.bindings().len(), MAX_BINDINGS);
}

fn document(bindings: Vec<ControlBinding>) -> ControlMappingDocument {
    ControlMappingDocument::try_new(
        MAPPING_ID.to_owned(),
        PROJECT_ID.to_owned(),
        bindings,
        BTreeMap::new(),
    )
    .unwrap()
}

fn binding(id: &str, enabled: bool, input_index: u8, scope: ControlScope) -> ControlBinding {
    ControlBinding::try_new(
        id.to_owned(),
        enabled,
        ControlSource::CvInput { input_index },
        scope,
        ControlTransform::Linear {
            input_min: -1_000_000,
            input_max: 1_000_000,
            output_min: 0,
            output_max: 1_000_000,
        },
        ControlTarget::MixerChannel {
            track_id: TRACK_ID.to_owned(),
            parameter: ChannelParameter::Gain,
        },
    )
    .unwrap()
}
