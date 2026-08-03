use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_audio_engine::{LinearGain, Pan};
use hyper_audio_source::{DecodeError, DecodeLimits};
use hyper_control_mappings::ControlMappingBindingErrorKind;
use hyper_mapping_compiler::{CompiledControlScope, CompiledControlSource, CompiledControlTarget};
use hyper_media_cache::CacheLimits;
use hyper_playback_json::{
    LoaderLimits, MixerBaseState, MixerChannelBaseState, serialize_mixer_base_state,
};
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::{
    ProjectPlaybackError, ProjectPlaybackLimits, RecordingBindingErrorKind,
    prepare_project_playback,
};
use hyper_project_model::ResourceRole;
use hyper_storage_engine::{
    ProjectResourcePublicationRequest, publish_project_resource, verify_project_package,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
const TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
const CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
const ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334459";
const MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445a";
const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445b";
const PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445c";
const MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445d";
const SECOND_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445e";
const SECOND_MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-00112233445f";
const SECOND_ARRANGEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334460";
const RECORDING_METADATA_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334461";
const FOREIGN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334462";
const SECOND_MAPPING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334463";
const BINDING_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334464";
const AUTOMATION_TAKE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334465";

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    #[allow(
        clippy::too_many_lines,
        reason = "one constructor keeps the coherent package graph and its exact descriptor bytes visible together"
    )]
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hyper-project-loader-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let resources = [
            (
                TIMELINE_ID,
                "timeline",
                "timeline/timeline.json",
                json!({
                    "format": "hyper-timeline",
                    "format_version": 1,
                    "id": TIMELINE_ID,
                    "project_id": PROJECT_ID,
                    "ticks_per_quarter": 1920,
                    "tempo_events": [{
                        "tick": 0,
                        "microseconds_per_quarter": 1_000_000
                    }],
                    "meter_events": [{
                        "tick": 0,
                        "numerator": 4,
                        "denominator": 4
                    }],
                    "extensions": {}
                }),
            ),
            (
                TRACK_ID,
                "track",
                "tracks/audio.json",
                json!({
                    "format": "hyper-track",
                    "format_version": 1,
                    "id": TRACK_ID,
                    "project_id": PROJECT_ID,
                    "name": "Audio",
                    "kind": "audio",
                    "order": 0,
                    "active": true,
                    "clip_ids": [CLIP_ID],
                    "extensions": {}
                }),
            ),
            (
                CLIP_ID,
                "clip",
                "clips/clip.json",
                json!({
                    "format": "hyper-audio-clip",
                    "format_version": 1,
                    "id": CLIP_ID,
                    "project_id": PROJECT_ID,
                    "track_id": TRACK_ID,
                    "name": "Clip",
                    "enabled": true,
                    "duration_ticks": 4,
                    "source": {
                        "asset_id": ASSET_ID,
                        "start_frame": 0,
                        "frame_count": 100
                    },
                    "loop": {
                        "enabled": false,
                        "start_tick": 0,
                        "end_tick": 4
                    },
                    "playback": {
                        "gain_decibels": 0,
                        "reverse": false,
                        "transpose_cents": 0
                    },
                    "fades": {
                        "curve": "linear-amplitude",
                        "in_ticks": 0,
                        "out_ticks": 0
                    },
                    "warp_markers": [
                        {"tick": 0, "source_frame_offset": 0},
                        {"tick": 4, "source_frame_offset": 100}
                    ],
                    "extensions": {}
                }),
            ),
            (
                ARRANGEMENT_ID,
                "arrangement",
                "arrangement/main.json",
                json!({
                    "format": "hyper-arrangement",
                    "format_version": 1,
                    "id": ARRANGEMENT_ID,
                    "project_id": PROJECT_ID,
                    "name": "Main",
                    "enabled": true,
                    "placements": [{
                        "id": PLACEMENT_ID,
                        "track_id": TRACK_ID,
                        "clip_id": CLIP_ID,
                        "start_tick": 0,
                        "clip_start_tick": 0,
                        "duration_ticks": 4,
                        "enabled": true
                    }],
                    "scene_launches": [],
                    "extensions": {}
                }),
            ),
            (
                MIXER_ID,
                "mixer",
                "mixer/main.json",
                json!({
                    "format": "hyper-mixer",
                    "format_version": 1,
                    "id": MIXER_ID,
                    "project_id": PROJECT_ID,
                    "channels": [{
                        "track_id": TRACK_ID,
                        "gain_millionths": 1_000_000,
                        "pan_millionths": 0,
                        "mute": false,
                        "solo": false,
                        "destination": {"kind": "main"},
                        "sends": []
                    }],
                    "return_buses": [],
                    "main": {"gain_millionths": 1_000_000, "mute": false},
                    "cue": {
                        "source": "main",
                        "gain_millionths": 1_000_000,
                        "mute": false
                    },
                    "extensions": {}
                }),
            ),
        ];

        let mut manifest_resources = Vec::new();
        for (id, role, path, value) in resources {
            let bytes = canonical(&value);
            write_file(&root, path, &bytes);
            manifest_resources.push(descriptor(id, role, path, &bytes));
        }
        let audio = pcm16_wav(&[0; 100]);
        write_file(&root, "assets/samples/sample.wav", &audio);
        let manifest = json!({
            "format": "hyper-project",
            "format_version": 1,
            "project_id": PROJECT_ID,
            "revision": 1,
            "name": "Native playback package",
            "sample_rate_hz": 48_000,
            "required_capabilities": {
                "format": "hyper-hardware-capabilities",
                "format_version": 1,
                "audio_inputs": 2,
                "audio_outputs": 2,
                "cv_inputs": 0,
                "cv_outputs": 0,
                "gate_outputs": 0,
                "has_clock_input": false,
                "has_reset_input": false,
                "has_clock_output": false,
                "usb_mode": "none",
                "has_ethernet": true,
                "extensions": {}
            },
            "resources": manifest_resources,
            "assets": [{
                "id": ASSET_ID,
                "role": "sample",
                "path": "assets/samples/sample.wav",
                "media_type": "audio/wav",
                "byte_length": audio.len(),
                "sha256": sha256(&audio),
                "extensions": {}
            }],
            "extensions": {}
        });
        fs::write(root.join("project.json"), canonical(&manifest)).unwrap();
        Self { root }
    }

    fn replace_resource(&self, path: &str, value: &Value) {
        let bytes = canonical(value);
        write_file(&self.root, path, &bytes);
        let mut manifest = self.manifest();
        let record = manifest["resources"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|record| record["path"] == path)
            .unwrap();
        record["byte_length"] = bytes.len().into();
        record["sha256"] = sha256(&bytes).into();
        self.write_manifest(&manifest);
    }

    fn add_resource(&self, id: &str, role: &str, path: &str, value: &Value) {
        let bytes = canonical(value);
        write_file(&self.root, path, &bytes);
        let mut manifest = self.manifest();
        manifest["resources"]
            .as_array_mut()
            .unwrap()
            .push(descriptor(id, role, path, &bytes));
        self.write_manifest(&manifest);
    }

    fn remove_mixer(&self) {
        fs::remove_file(self.root.join("mixer/main.json")).unwrap();
        fs::remove_dir(self.root.join("mixer")).unwrap();
        let mut manifest = self.manifest();
        manifest["resources"]
            .as_array_mut()
            .unwrap()
            .retain(|record| record["role"] != "mixer");
        self.write_manifest(&manifest);
    }

    fn replace_asset_bytes(&self, bytes: &[u8]) {
        write_file(&self.root, "assets/samples/sample.wav", bytes);
        let mut manifest = self.manifest();
        let asset = &mut manifest["assets"][0];
        asset["byte_length"] = bytes.len().into();
        asset["sha256"] = sha256(bytes).into();
        self.write_manifest(&manifest);
    }

    fn add_sample_asset(&self) {
        let bytes = fs::read(self.root.join("assets/samples/sample.wav")).unwrap();
        write_file(&self.root, "assets/samples/second.wav", &bytes);
        let mut manifest = self.manifest();
        manifest["assets"].as_array_mut().unwrap().push(json!({
            "id": SECOND_ASSET_ID,
            "role": "sample",
            "path": "assets/samples/second.wav",
            "media_type": "audio/wav",
            "byte_length": bytes.len(),
            "sha256": sha256(&bytes),
            "extensions": {}
        }));
        self.write_manifest(&manifest);
    }

    fn make_asset_a_recording(&self) {
        let old_path = self.root.join("assets/samples/sample.wav");
        let new_path = self.root.join("assets/recordings/recording.wav");
        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::rename(old_path, new_path).unwrap();
        fs::remove_dir(self.root.join("assets/samples")).unwrap();
        let mut manifest = self.manifest();
        manifest["assets"][0]["role"] = "recording".into();
        manifest["assets"][0]["path"] = "assets/recordings/recording.wav".into();
        self.write_manifest(&manifest);
    }

    fn add_recording_metadata(&self, value: &Value) {
        self.add_resource(
            RECORDING_METADATA_ID,
            "recording-metadata",
            "assets/recordings/metadata.json",
            value,
        );
    }

    fn set_audio_inputs(&self, inputs: u8) {
        let mut manifest = self.manifest();
        manifest["required_capabilities"]["audio_inputs"] = inputs.into();
        self.write_manifest(&manifest);
    }

    fn set_cv_inputs(&self, inputs: u8) {
        let mut manifest = self.manifest();
        manifest["required_capabilities"]["cv_inputs"] = inputs.into();
        self.write_manifest(&manifest);
    }

    fn set_clock_input(&self, available: bool) {
        let mut manifest = self.manifest();
        manifest["required_capabilities"]["has_clock_input"] = available.into();
        self.write_manifest(&manifest);
    }

    fn set_reset_input(&self, available: bool) {
        let mut manifest = self.manifest();
        manifest["required_capabilities"]["has_reset_input"] = available.into();
        self.write_manifest(&manifest);
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&fs::read(self.root.join("project.json")).unwrap()).unwrap()
    }

    fn write_manifest(&self, manifest: &Value) {
        fs::write(self.root.join("project.json"), canonical(manifest)).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn prepares_one_verified_owned_playback_session() {
    let fixture = Fixture::new();
    let prepared = prepare_project_playback(&fixture.root, limits()).unwrap();

    assert_eq!(prepared.package().manifest().project_id(), PROJECT_ID);
    assert_eq!(prepared.session().project_id(), PROJECT_ID);
    assert_eq!(prepared.session().media_asset_count(), 1);
    assert!(!prepared.session().has_automation());
    assert!(prepared.control_mapping().is_none());
    assert!(prepared.compiled_control_mapping().is_none());
    assert!(prepared.recording_metadata().is_empty());
    prepared.package().confirm_unchanged().unwrap();
}

#[test]
fn serialized_mixer_publication_reopens_as_the_next_project_revision() {
    let fixture = Fixture::new();
    let verified = verify_project_package(&fixture.root).unwrap();
    let descriptor = verified
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
        .unwrap();
    let source = verified.read_resource(descriptor.id()).unwrap();
    let channel_gain = LinearGain::from_millionths(625_000).unwrap();
    let channel_pan = Pan::from_millionths(-250_000).unwrap();
    let main_gain = LinearGain::from_millionths(750_000).unwrap();
    let serialized = serialize_mixer_base_state(
        &source,
        PROJECT_ID,
        MIXER_ID,
        &MixerBaseState {
            channels: vec![MixerChannelBaseState {
                track_id: TRACK_ID.to_owned(),
                gain: channel_gain,
                pan: channel_pan,
                muted: true,
                solo: true,
                sends: Vec::new(),
            }],
            main_gain,
            main_muted: true,
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();

    let outcome = publish_project_resource(
        &fixture.root,
        ProjectResourcePublicationRequest {
            expected_project_sha256: *verified.manifest_sha256(),
            resource_id: MIXER_ID,
            expected_role: ResourceRole::Mixer,
            resource_bytes: &serialized,
        },
    )
    .unwrap();

    assert!(outcome.changed());
    assert_eq!(outcome.project_revision(), 2);
    let reopened = prepare_project_playback(&fixture.root, limits()).unwrap();
    assert_eq!(reopened.package().manifest().revision(), 2);
    assert_eq!(
        reopened.package().manifest_sha256(),
        outcome.project_identity()
    );
    let (_, mixer, _, _) = reopened.session().parts();
    let channel = &mixer.slots()[0];
    assert_eq!(channel.parameters().gain, channel_gain);
    assert_eq!(channel.parameters().pan, channel_pan);
    assert!(channel.muted());
    assert!(channel.solo());
    assert_eq!(mixer.main_gain(), main_gain);
    assert!(mixer.main_muted());
}

#[test]
fn shared_playback_domain_cases_match_the_public_preparation_boundary() {
    let cases: Value = serde_json::from_str(include_str!(
        "../../../contracts/fixtures/playback-domain-cases.json"
    ))
    .unwrap();
    for test_case in cases.as_array().unwrap() {
        let fixture = Fixture::new();
        if let Some(role) = test_case.get("resource_role").and_then(Value::as_str) {
            let manifest = fixture.manifest();
            let path = manifest["resources"]
                .as_array()
                .unwrap()
                .iter()
                .find(|record| record["role"] == role)
                .and_then(|record| record["path"].as_str())
                .unwrap()
                .to_owned();
            let mut resource: Value =
                serde_json::from_slice(&fs::read(fixture.root.join(&path)).unwrap()).unwrap();
            let pointer = test_case["pointer"].as_str().unwrap();
            let mut replacement = test_case["value"].clone();
            replace_case_symbols(&mut replacement);
            *resource.pointer_mut(pointer).unwrap() = replacement;
            fixture.replace_resource(&path, &resource);
        }

        let accepted = test_case["accepted"].as_bool().unwrap();
        let result = prepare_project_playback(&fixture.root, limits());
        assert_eq!(
            result.is_ok(),
            accepted,
            "shared playback-domain case {} disagreed with its expected result",
            test_case["name"].as_str().unwrap()
        );
    }
}

fn replace_case_symbols(value: &mut Value) {
    match value {
        Value::String(text) if text == "$asset_id" => ASSET_ID.clone_into(text),
        Value::String(text) if text == "$arrangement_event_id" => {
            PLACEMENT_ID.clone_into(text);
        }
        Value::Array(items) => items.iter_mut().for_each(replace_case_symbols),
        Value::Object(properties) => properties.values_mut().for_each(replace_case_symbols),
        _ => {}
    }
}

#[test]
fn binds_recording_metadata_to_verified_media_and_audio_tracks() {
    let fixture = Fixture::new();
    fixture.make_asset_a_recording();
    fixture.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[0], 1, 100));

    let prepared = prepare_project_playback(&fixture.root, limits()).unwrap();

    assert_eq!(prepared.recording_metadata().len(), 1);
    assert_eq!(prepared.recording_metadata()[0].asset_id(), ASSET_ID);
    assert_eq!(prepared.session().media_asset_count(), 1);
}

#[test]
fn rejects_semantic_identity_failure_without_publishing_a_session() {
    let fixture = Fixture::new();
    fixture.replace_resource(
        "timeline/timeline.json",
        &json!({
            "format": "hyper-timeline",
            "format_version": 1,
            "id": TIMELINE_ID,
            "project_id": "018f47a2-6b2d-7c11-8a44-0011223344ff",
            "ticks_per_quarter": 1920,
            "tempo_events": [{"tick": 0, "microseconds_per_quarter": 1_000_000}],
            "meter_events": [{"tick": 0, "numerator": 4, "denominator": 4}],
            "extensions": {}
        }),
    );

    assert!(matches!(
        prepare_project_playback(&fixture.root, limits()),
        Err(ProjectPlaybackError::Assembly(_))
    ));
}

#[test]
fn rejects_a_playback_document_id_that_does_not_match_its_descriptor() {
    let fixture = Fixture::new();
    fixture.replace_resource(
        "timeline/timeline.json",
        &json!({
            "format": "hyper-timeline",
            "format_version": 1,
            "id": FOREIGN_ID,
            "project_id": PROJECT_ID,
            "ticks_per_quarter": 1920,
            "tempo_events": [{"tick": 0, "microseconds_per_quarter": 1_000_000}],
            "meter_events": [{"tick": 0, "numerator": 4, "denominator": 4}],
            "extensions": {}
        }),
    );

    assert!(matches!(
        prepare_project_playback(&fixture.root, limits()),
        Err(ProjectPlaybackError::ResourceIdMismatch {
            descriptor_id,
            document_id: Some(document_id),
        }) if descriptor_id == TIMELINE_ID && document_id == FOREIGN_ID
    ));
}

#[test]
fn recording_metadata_id_cannot_collide_with_a_nested_playback_identity() {
    let fixture = Fixture::new();
    fixture.replace_resource(
        "arrangement/main.json",
        &json!({
            "format": "hyper-arrangement",
            "format_version": 1,
            "id": ARRANGEMENT_ID,
            "project_id": PROJECT_ID,
            "name": "Main",
            "enabled": true,
            "placements": [{
                "id": RECORDING_METADATA_ID,
                "track_id": TRACK_ID,
                "clip_id": CLIP_ID,
                "start_tick": 0,
                "clip_start_tick": 0,
                "duration_ticks": 4,
                "enabled": true
            }],
            "scene_launches": [],
            "extensions": {}
        }),
    );
    fixture.make_asset_a_recording();
    fixture.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[0], 1, 100));

    assert_binding_kind(
        prepare_project_playback(&fixture.root, limits()),
        RecordingBindingErrorKind::IdentityCollision,
    );
}

#[test]
fn requires_complete_playback_resource_ownership() {
    let missing_mixer = Fixture::new();
    missing_mixer.remove_mixer();
    assert!(matches!(
        prepare_project_playback(&missing_mixer.root, limits()),
        Err(ProjectPlaybackError::MissingMixer)
    ));

    let recording = Fixture::new();
    recording.make_asset_a_recording();
    assert_binding_kind(
        prepare_project_playback(&recording.root, limits()),
        RecordingBindingErrorKind::CountMismatch {
            metadata: 0,
            assets: 1,
        },
    );

    let duplicate_mixer = Fixture::new();
    duplicate_mixer.add_resource(SECOND_MIXER_ID, "mixer", "mixer/second.json", &json!({}));
    assert!(matches!(
        prepare_project_playback(&duplicate_mixer.root, limits()),
        Err(ProjectPlaybackError::MultipleMixers)
    ));

    let duplicate_arrangement = Fixture::new();
    duplicate_arrangement.add_resource(
        SECOND_ARRANGEMENT_ID,
        "arrangement",
        "arrangement/second.json",
        &json!({}),
    );
    assert!(matches!(
        prepare_project_playback(&duplicate_arrangement.root, limits()),
        Err(ProjectPlaybackError::MultipleArrangements)
    ));
}

#[test]
fn loads_and_binds_one_verified_control_mapping() {
    let fixture = Fixture::new();
    fixture.set_cv_inputs(1);
    fixture.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(
            MAPPING_ID,
            BINDING_ID,
            TRACK_ID,
            json!({"kind": "track", "track_id": TRACK_ID}),
        ),
    );

    let prepared = prepare_project_playback(&fixture.root, limits()).unwrap();
    let mapping = prepared.control_mapping().unwrap();
    assert_eq!(mapping.id(), MAPPING_ID);
    assert_eq!(mapping.project_id(), PROJECT_ID);
    assert_eq!(mapping.bindings().len(), 1);
    let compiled = prepared.compiled_control_mapping().unwrap();
    assert_eq!(compiled.bindings().len(), 1);
    let compiled_binding = compiled.bindings()[0];
    assert_eq!(compiled_binding.source_binding_index(), 0);
    assert_eq!(
        compiled_binding.source(),
        CompiledControlSource::CvInput { input_index: 0 }
    );
    assert_eq!(
        compiled_binding.scope(),
        CompiledControlScope::Track { track_index: 0 }
    );
    assert!(matches!(
        compiled_binding.target(),
        CompiledControlTarget::MixerChannel {
            channel_index: 0,
            parameter: hyper_control_mappings::ChannelParameter::Gain,
        }
    ));
    let (package, session, mapping, compiled, recording_metadata) = prepared.into_parts();
    assert_eq!(package.manifest().project_id(), PROJECT_ID);
    assert_eq!(session.parts().0.project_id(), PROJECT_ID);
    assert_eq!(mapping.unwrap().id(), MAPPING_ID);
    assert_eq!(compiled.unwrap().bindings().len(), 1);
    assert!(recording_metadata.is_empty());
}

#[test]
fn rejects_invalid_or_ambiguous_control_mapping_resources() {
    let malformed = Fixture::new();
    malformed.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &json!({}),
    );
    assert!(matches!(
        prepare_project_playback(&malformed.root, limits()),
        Err(ProjectPlaybackError::ControlMapping { resource_id, .. })
            if resource_id == MAPPING_ID
    ));

    let duplicate = Fixture::new();
    duplicate.set_cv_inputs(1);
    duplicate.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null),
    );
    let mut second = control_mapping(SECOND_MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null);
    second["bindings"] = json!([]);
    duplicate.add_resource(
        SECOND_MAPPING_ID,
        "mapping",
        "mappings/second.json",
        &second,
    );
    assert!(matches!(
        prepare_project_playback(&duplicate.root, limits()),
        Err(ProjectPlaybackError::MultipleControlMappings)
    ));
}

#[test]
fn rejects_mapping_document_ownership_mismatches() {
    let descriptor = Fixture::new();
    descriptor.set_cv_inputs(1);
    descriptor.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(FOREIGN_ID, BINDING_ID, TRACK_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&descriptor.root, limits()),
        ControlMappingBindingErrorKind::DescriptorIdMismatch,
        None,
    );

    let project = Fixture::new();
    project.set_cv_inputs(1);
    let mut mapping = control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null);
    mapping["project_id"] = FOREIGN_ID.into();
    project.add_resource(MAPPING_ID, "mapping", "mappings/controller.json", &mapping);
    assert_mapping_binding_kind(
        prepare_project_playback(&project.root, limits()),
        ControlMappingBindingErrorKind::ProjectIdMismatch,
        None,
    );
}

#[test]
fn rejects_unavailable_edge_sources_and_graph_target_kinds() {
    let clock = Fixture::new();
    clock.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &trigger_control_mapping(
            "clock-input",
            json!({"kind": "transport", "action": "play"}),
        ),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&clock.root, limits()),
        ControlMappingBindingErrorKind::ClockInputUnavailable,
        Some(BINDING_ID),
    );

    let reset = Fixture::new();
    reset.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &trigger_control_mapping(
            "reset-input",
            json!({"kind": "transport", "action": "reset"}),
        ),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&reset.root, limits()),
        ControlMappingBindingErrorKind::ResetInputUnavailable,
        Some(BINDING_ID),
    );

    let send = Fixture::new();
    send.set_cv_inputs(1);
    let mut mapping = control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null);
    mapping["bindings"][0]["target"] =
        json!({"kind": "mixer-send", "send_id": FOREIGN_ID, "parameter": "gain"});
    send.add_resource(MAPPING_ID, "mapping", "mappings/controller.json", &mapping);
    assert_mapping_binding_kind(
        prepare_project_playback(&send.root, limits()),
        ControlMappingBindingErrorKind::MixerSendMissing,
        Some(BINDING_ID),
    );

    let scene = Fixture::new();
    scene.set_clock_input(true);
    scene.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &trigger_control_mapping(
            "clock-input",
            json!({"kind": "scene", "scene_id": FOREIGN_ID, "action": "launch"}),
        ),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&scene.root, limits()),
        ControlMappingBindingErrorKind::SceneTargetMissing,
        Some(BINDING_ID),
    );

    let available_reset = Fixture::new();
    available_reset.set_reset_input(true);
    available_reset.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &trigger_control_mapping(
            "reset-input",
            json!({"kind": "transport", "action": "reset"}),
        ),
    );
    assert!(prepare_project_playback(&available_reset.root, limits()).is_ok());
}

#[test]
fn rejects_unavailable_mapping_sources_and_unresolved_graph_references() {
    let unavailable = Fixture::new();
    unavailable.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&unavailable.root, limits()),
        ControlMappingBindingErrorKind::CvInputUnavailable {
            index: 0,
            available: 0,
        },
        Some(BINDING_ID),
    );

    let target = Fixture::new();
    target.set_cv_inputs(1);
    target.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(MAPPING_ID, BINDING_ID, FOREIGN_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&target.root, limits()),
        ControlMappingBindingErrorKind::MixerChannelMissing,
        Some(BINDING_ID),
    );

    let scope = Fixture::new();
    scope.set_cv_inputs(1);
    scope.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(
            MAPPING_ID,
            BINDING_ID,
            TRACK_ID,
            json!({"kind": "track", "track_id": FOREIGN_ID}),
        ),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&scope.root, limits()),
        ControlMappingBindingErrorKind::TrackScopeMissing,
        Some(BINDING_ID),
    );
}

#[test]
fn rejects_mapping_binding_identity_collisions() {
    let fixture = Fixture::new();
    fixture.set_cv_inputs(1);
    fixture.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(MAPPING_ID, PLACEMENT_ID, TRACK_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&fixture.root, limits()),
        ControlMappingBindingErrorKind::IdentityCollision,
        Some(PLACEMENT_ID),
    );
}

#[test]
fn rejects_mapping_resource_identity_collisions() {
    let arrangement = Fixture::new();
    arrangement.set_cv_inputs(1);
    arrangement.add_resource(
        PLACEMENT_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(PLACEMENT_ID, BINDING_ID, TRACK_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&arrangement.root, limits()),
        ControlMappingBindingErrorKind::DocumentIdentityCollision,
        None,
    );

    let automation = Fixture::new();
    automation.set_cv_inputs(1);
    automation.add_resource(
        AUTOMATION_TAKE_ID,
        "automation-take",
        "takes/take.json",
        &json!({
            "format": "hyper-automation-take",
            "format_version": 1,
            "id": AUTOMATION_TAKE_ID,
            "project_id": PROJECT_ID,
            "name": "Collision",
            "enabled": true,
            "captured_at_utc": "2026-08-03T00:00:00.000Z",
            "start_tick": 0,
            "end_tick": 4,
            "lanes": [{
                "id": MAPPING_ID,
                "target": {
                    "kind": "mixer-channel",
                    "track_id": TRACK_ID,
                    "parameter": "gain"
                },
                "interpolation": "linear",
                "points": [
                    {"tick": 0, "value": 0},
                    {"tick": 3, "value": 1_000_000}
                ]
            }],
            "extensions": {}
        }),
    );
    automation.add_resource(
        MAPPING_ID,
        "mapping",
        "mappings/controller.json",
        &control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null),
    );
    assert_mapping_binding_kind(
        prepare_project_playback(&automation.root, limits()),
        ControlMappingBindingErrorKind::DocumentIdentityCollision,
        None,
    );
}

#[test]
fn rejects_recording_metadata_that_does_not_match_project_or_decoded_media() {
    let sample_rate = Fixture::new();
    sample_rate.replace_asset_bytes(&pcm16_wav_at_rate(&[0; 100], 44_100));
    sample_rate.make_asset_a_recording();
    sample_rate.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[0], 1, 100));
    assert!(matches!(
        prepare_project_playback(&sample_rate.root, limits()),
        Err(ProjectPlaybackError::Audio {
            source: DecodeError::SampleRateMismatch {
                expected: 48_000,
                actual: 44_100
            },
            ..
        })
    ));

    let channels = Fixture::new();
    channels.make_asset_a_recording();
    channels.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[0, 1], 2, 100));
    assert_binding_kind(
        prepare_project_playback(&channels.root, limits()),
        RecordingBindingErrorKind::ChannelMismatch {
            metadata: 2,
            audio: 1,
        },
    );

    let frames = Fixture::new();
    frames.make_asset_a_recording();
    frames.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[0], 1, 99));
    assert_binding_kind(
        prepare_project_playback(&frames.root, limits()),
        RecordingBindingErrorKind::FrameCountMismatch {
            metadata: 99,
            audio: 100,
        },
    );

    let input = Fixture::new();
    input.make_asset_a_recording();
    input.set_audio_inputs(1);
    input.add_recording_metadata(&recording_metadata(PROJECT_ID, TRACK_ID, &[1], 1, 100));
    assert_binding_kind(
        prepare_project_playback(&input.root, limits()),
        RecordingBindingErrorKind::InputUnavailable {
            index: 1,
            available: 1,
        },
    );

    let track = Fixture::new();
    track.make_asset_a_recording();
    track.add_recording_metadata(&recording_metadata(PROJECT_ID, MIXER_ID, &[0], 1, 100));
    assert_binding_kind(
        prepare_project_playback(&track.root, limits()),
        RecordingBindingErrorKind::TrackNotAudio,
    );

    let project = Fixture::new();
    project.make_asset_a_recording();
    project.add_recording_metadata(&recording_metadata(FOREIGN_ID, TRACK_ID, &[0], 1, 100));
    assert_binding_kind(
        prepare_project_playback(&project.root, limits()),
        RecordingBindingErrorKind::ProjectIdMismatch,
    );
}

#[test]
fn rejects_resource_budget_and_mismatched_audio_container() {
    let budget = Fixture::new();
    fs::write(
        budget.root.join("timeline/timeline.json"),
        br#"{"format":"corrupt"}"#,
    )
    .unwrap();
    let mut tiny = limits();
    tiny.max_total_resource_bytes = 1;
    assert!(matches!(
        prepare_project_playback(&budget.root, tiny),
        Err(ProjectPlaybackError::ResourceBytesLimit { .. })
    ));

    let frames = Fixture::new();
    let mut frame_limited = limits();
    frame_limited.session.media_cache.max_frames_per_asset = 99;
    assert!(matches!(
        prepare_project_playback(&frames.root, frame_limited),
        Err(ProjectPlaybackError::AssetFrameLimit { .. })
    ));

    let decoded_bytes = Fixture::new();
    let mut byte_limited = limits();
    byte_limited.session.media_cache.max_decoded_sample_bytes = 399;
    assert!(matches!(
        prepare_project_playback(&decoded_bytes.root, byte_limited),
        Err(ProjectPlaybackError::DecodedBytesLimit { .. })
    ));

    let asset_count = Fixture::new();
    asset_count.add_sample_asset();
    fs::write(asset_count.root.join("undeclared"), b"must not be scanned").unwrap();
    assert!(matches!(
        prepare_project_playback(&asset_count.root, limits()),
        Err(ProjectPlaybackError::AssetCountLimit {
            actual: 2,
            maximum: 1
        })
    ));

    let encoded_bytes = Fixture::new();
    fs::write(
        encoded_bytes.root.join("undeclared"),
        b"must not be scanned",
    )
    .unwrap();
    let mut encoded_limited = limits();
    encoded_limited.audio = DecodeLimits::new(1, 100, 800).unwrap();
    assert!(matches!(
        prepare_project_playback(&encoded_bytes.root, encoded_limited),
        Err(ProjectPlaybackError::Audio {
            source: DecodeError::FileTooLarge {
                actual: 244,
                maximum: 1
            },
            ..
        })
    ));

    let media = Fixture::new();
    media.replace_asset_bytes(b"fLaCnot-a-wave-container");
    assert!(matches!(
        prepare_project_playback(&media.root, limits()),
        Err(ProjectPlaybackError::Audio {
            source: DecodeError::ContainerMismatch { .. },
            ..
        })
    ));
}

fn limits() -> ProjectPlaybackLimits {
    ProjectPlaybackLimits {
        max_total_resource_bytes: 5 * 1_048_576,
        audio: DecodeLimits::new(1_048_576, 100, 800).unwrap(),
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 1,
                max_frames_per_asset: 100,
                max_decoded_sample_bytes: 400,
            },
        },
    }
}

fn descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> Value {
    json!({
        "id": id,
        "role": role,
        "path": path,
        "media_type": "application/json",
        "byte_length": bytes.len(),
        "sha256": sha256(bytes),
        "extensions": {}
    })
}

fn recording_metadata(
    project_id: &str,
    track_id: &str,
    source_input_indices: &[u8],
    channels: u8,
    frame_count: u64,
) -> Value {
    json!({
        "format": "hyper-recording-metadata",
        "format_version": 1,
        "id": RECORDING_METADATA_ID,
        "project_id": project_id,
        "asset_id": ASSET_ID,
        "track_id": track_id,
        "capture_kind": "new",
        "captured_at_utc": "2026-07-29T12:00:00.000Z",
        "timeline_start_tick": 0,
        "source_input_indices": source_input_indices,
        "sample_rate_hz": 48_000,
        "channels": channels,
        "frame_count": frame_count,
        "extensions": {}
    })
}

fn control_mapping(
    mapping_id: &str,
    binding_id: &str,
    target_track_id: &str,
    scope: Value,
) -> Value {
    json!({
        "format": "hyper-control-mappings",
        "format_version": 1,
        "id": mapping_id,
        "project_id": PROJECT_ID,
        "bindings": [{
            "id": binding_id,
            "enabled": true,
            "source": {"kind": "cv-input", "input_index": 0},
            "scope": if scope.is_null() { json!({"kind": "global"}) } else { scope },
            "transform": {
                "kind": "linear",
                "input_min": -1_000_000,
                "input_max": 1_000_000,
                "output_min": 0,
                "output_max": 1_000_000
            },
            "target": {
                "kind": "mixer-channel",
                "track_id": target_track_id,
                "parameter": "gain"
            }
        }],
        "extensions": {}
    })
}

fn trigger_control_mapping(source_kind: &str, target: Value) -> Value {
    let mut mapping = control_mapping(MAPPING_ID, BINDING_ID, TRACK_ID, Value::Null);
    mapping["bindings"][0]["source"] = json!({"kind": source_kind, "edge": "rising"});
    mapping["bindings"][0]["transform"] = json!({"kind": "trigger", "behavior": "invoke"});
    mapping["bindings"][0]["target"] = target;
    mapping
}

fn assert_binding_kind(
    result: Result<hyper_project_loader::PreparedProjectPlayback, ProjectPlaybackError>,
    expected: RecordingBindingErrorKind,
) {
    match result {
        Err(ProjectPlaybackError::RecordingBinding(error)) => {
            assert_eq!(error.kind(), expected);
        }
        other => panic!("expected recording binding error, got {other:?}"),
    }
}

fn assert_mapping_binding_kind(
    result: Result<hyper_project_loader::PreparedProjectPlayback, ProjectPlaybackError>,
    expected: ControlMappingBindingErrorKind,
    expected_binding_id: Option<&str>,
) {
    match result {
        Err(ProjectPlaybackError::ControlMappingBinding(error)) => {
            assert_eq!(error.kind(), expected);
            assert_eq!(error.binding_id(), expected_binding_id);
        }
        other => panic!("expected control mapping binding error, got {other:?}"),
    }
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn canonical(value: &Value) -> Vec<u8> {
    serde_json_canonicalizer::to_vec(value).unwrap()
}

fn sha256(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn pcm16_wav(samples: &[i16]) -> Vec<u8> {
    pcm16_wav_at_rate(samples, 48_000)
}

fn pcm16_wav_at_rate(samples: &[i16], sample_rate_hz: u32) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len() * 2).unwrap();
    let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate_hz * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}
