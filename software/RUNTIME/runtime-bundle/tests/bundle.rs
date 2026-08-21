use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_audio_source::DecodeLimits;
use hyper_fs_authority::open_syncable_directory_ambient;
use hyper_media_cache::CacheLimits;
use hyper_native_device::{DeviceChain, NativeDeviceInstance, NativeFilterMode, NativeFilterState};
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_project_model::{AudioMediaType, ResourceRole, UsbMode};
use hyper_runtime_bundle::{
    RuntimeBundleErrorKind, prepare_runtime_bundle, prepare_runtime_bundle_at,
};
use hyper_storage_engine::{
    DeviceChainPublicationRequest, ProjectCreationRequest, create_project, publish_device_chain,
    verify_project_package,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    manifest: Value,
}

impl Fixture {
    fn new() -> Self {
        Self::new_with_native_filter(false)
    }

    fn with_native_filter() -> Self {
        Self::new_with_native_filter(true)
    }

    fn new_with_native_filter(include_native_filter: bool) -> Self {
        let parent = std::env::temp_dir().join(format!(
            "hyper-runtime-bundle-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&parent).unwrap();
        let creation = create_project(
            &parent,
            ProjectCreationRequest {
                directory_name: "Bundle",
                project_name: "Bundle",
            },
        )
        .unwrap();
        let source_root = creation.project().root().to_owned();
        drop(creation);
        if include_native_filter {
            publish_native_filter(&source_root);
        }
        let project = verify_project_package(&source_root).unwrap();
        let project_bytes = fs::read(source_root.join("project.json")).unwrap();
        let project_sha256 = digest(&project_bytes);
        let capabilities = project.manifest().required_capabilities();
        let root = parent.join("RuntimeBundle");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("project.json"), &project_bytes).unwrap();
        let mut files = vec![json!({
            "path": "project.json",
            "role": "runtime-project",
            "media_type": "application/json",
            "byte_length": project_bytes.len(),
            "sha256": project_sha256
        })];
        for resource in project.manifest().resources() {
            copy_payload(&source_root, &root, resource.path());
            files.push(json!({
                "path": resource.path(),
                "role": "runtime-data",
                "media_type": "application/json",
                "byte_length": resource.byte_length(),
                "sha256": resource.sha256()
            }));
        }
        for asset in project.manifest().assets() {
            copy_payload(&source_root, &root, asset.path());
            files.push(json!({
                "path": asset.path(),
                "role": "audio",
                "media_type": match asset.media_type() {
                    AudioMediaType::AudioFlac => "audio/flac",
                    AudioMediaType::AudioWav => "audio/wav",
                },
                "byte_length": asset.byte_length(),
                "sha256": asset.sha256()
            }));
        }
        let manifest = json!({
            "format": "hyper-runtime-bundle",
            "format_version": 1,
            "project_id": project.manifest().project_id(),
            "project_revision": project.manifest().revision(),
            "source_project_sha256": project_sha256,
            "minimum_runtime_version": "0.1.0",
            "builder": { "id": "hyper-bundle-builder", "version": "0.1.0" },
            "entrypoint": "project.json",
            "required_capabilities": {
                "format": "hyper-hardware-capabilities",
                "format_version": 1,
                "audio_inputs": capabilities.audio_inputs(),
                "audio_outputs": capabilities.audio_outputs(),
                "cv_inputs": capabilities.cv_inputs(),
                "cv_outputs": capabilities.cv_outputs(),
                "gate_outputs": capabilities.gate_outputs(),
                "has_clock_input": capabilities.has_clock_input(),
                "has_reset_input": capabilities.has_reset_input(),
                "has_clock_output": capabilities.has_clock_output(),
                "usb_mode": usb_mode(capabilities.usb_mode()),
                "has_ethernet": capabilities.has_ethernet(),
                "extensions": capabilities.extensions()
            },
            "files": files,
            "extensions": {}
        });
        drop(project);
        write_manifest(&root, &manifest);
        Self { root, manifest }
    }

    fn rewrite(&self, manifest: &Value) {
        write_manifest(&self.root, manifest);
    }
}

fn publish_native_filter(source_root: &Path) {
    let project = verify_project_package(source_root).unwrap();
    let project_id = project.manifest().project_id().to_owned();
    let track = project
        .manifest()
        .resources()
        .iter()
        .find(|resource| resource.role() == ResourceRole::Track)
        .unwrap();
    let track_document: Value =
        serde_json::from_slice(&project.read_resource(track.id()).unwrap()).unwrap();
    let track_id = track_document["id"].as_str().unwrap().to_owned();
    let expected_project_sha256 = *project.manifest_sha256();
    drop(project);
    let chain = DeviceChain::try_new(
        "018f47a2-6b2d-7c11-8a44-0011223375a0".to_owned(),
        project_id,
        track_id,
        vec![NativeDeviceInstance::try_new_filter(
            "018f47a2-6b2d-7c11-8a44-0011223375a1".to_owned(),
            true,
            NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_000).unwrap(),
        )
        .unwrap()],
    )
    .unwrap();
    let chain_bytes = chain.to_canonical_vec().unwrap();
    publish_device_chain(
        source_root,
        DeviceChainPublicationRequest {
            expected_project_sha256,
            chain_id: chain.id(),
            chain_bytes: &chain_bytes,
        },
    )
    .unwrap();
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
    }
}

#[test]
fn prepares_a_contract_shaped_bundle_and_retains_its_identity() {
    let fixture = Fixture::new();
    let manifest_bytes = fs::read(fixture.root.join("manifest.json")).unwrap();
    let expected_sha256: [u8; 32] = Sha256::digest(&manifest_bytes).into();
    let expected_byte_length = manifest_bytes.len() as u64
        + fixture.manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["byte_length"].as_u64().unwrap())
            .sum::<u64>();
    let bundle = prepare_runtime_bundle(&fixture.root, "0.1.0", 1 << 30, limits()).unwrap();

    assert_eq!(bundle.bundle_sha256(), &expected_sha256);
    assert_eq!(bundle.bundle_byte_length(), expected_byte_length);
    assert_eq!(bundle.project().session().parts().0.track_count(), 1);
    bundle.project().package().confirm_unchanged().unwrap();
}

#[test]
fn prepared_bundle_retains_a_native_filter_for_runtime_loading() {
    let fixture = Fixture::with_native_filter();
    let bundle = prepare_runtime_bundle(&fixture.root, "0.1.0", 1 << 30, limits()).unwrap();

    assert_eq!(bundle.project().device_chains().len(), 1);
    let chain = &bundle.project().device_chains()[0];
    assert_eq!(chain.devices().len(), 1);
    assert!(chain.devices()[0].enabled());
    assert_eq!(chain.devices()[0].state().mode(), NativeFilterMode::LowPass);
    bundle.project().package().confirm_unchanged().unwrap();
}

#[test]
fn retained_bundle_authority_survives_an_ambient_rename() {
    let fixture = Fixture::new();
    let retained = open_syncable_directory_ambient(&fixture.root).unwrap();
    let moved = fixture.root.parent().unwrap().join("MovedRuntimeBundle");
    fs::rename(&fixture.root, &moved).unwrap();

    let bundle =
        prepare_runtime_bundle_at(&fixture.root, retained, "0.1.0", 1 << 30, limits()).unwrap();

    assert_eq!(bundle.project().session().parts().0.track_count(), 1);
    bundle.project().package().confirm_unchanged().unwrap();
}

#[test]
fn compares_runtime_versions_by_semver_precedence() {
    let fixture = Fixture::new();
    for (runtime, minimum, compatible) in [
        ("1.0.0", "1.0.0", true),
        ("1.0.1", "1.0.0", true),
        ("1.0.0", "1.0.1", false),
        ("1.0.0-alpha.2", "1.0.0-alpha.1", true),
        ("1.0.0-alpha.1", "1.0.0-alpha.2", false),
        ("1.0.0+aaa", "1.0.0+zzz", true),
        ("1.0.0+zzz", "1.0.0+aaa", true),
    ] {
        let mut manifest = fixture.manifest.clone();
        manifest["minimum_runtime_version"] = minimum.into();
        fixture.rewrite(&manifest);
        let result = prepare_runtime_bundle(&fixture.root, runtime, 1 << 30, limits());
        if compatible {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                RuntimeBundleErrorKind::RuntimeIncompatible,
                "Runtime {runtime} must reject minimum {minimum}"
            );
        }
    }
}

#[test]
fn accepts_the_exact_bundle_limit_and_rejects_one_byte_less() {
    let fixture = Fixture::new();
    let manifest_bytes = fs::read(fixture.root.join("manifest.json")).unwrap();
    let exact_byte_length = manifest_bytes.len() as u64
        + fixture.manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["byte_length"].as_u64().unwrap())
            .sum::<u64>();

    let bundle =
        prepare_runtime_bundle(&fixture.root, "0.1.0", exact_byte_length, limits()).unwrap();
    assert_eq!(bundle.bundle_byte_length(), exact_byte_length);
    drop(bundle);
    assert_eq!(
        prepare_runtime_bundle(&fixture.root, "0.1.0", exact_byte_length - 1, limits())
            .unwrap_err()
            .kind(),
        RuntimeBundleErrorKind::BundleLimit
    );
}

#[test]
fn rejects_descriptor_drift_from_the_embedded_project() {
    let fixture = Fixture::new();
    let mut manifest = fixture.manifest.clone();
    manifest["files"][1]["sha256"] = "a".repeat(64).into();
    fixture.rewrite(&manifest);

    assert_eq!(
        prepare_runtime_bundle(&fixture.root, "0.1.0", 1 << 30, limits())
            .unwrap_err()
            .kind(),
        RuntimeBundleErrorKind::ManifestInvalid
    );
}

#[test]
fn rejects_an_incorrect_project_descriptor_length() {
    let fixture = Fixture::new();
    let mut manifest = fixture.manifest.clone();
    manifest["files"][0]["byte_length"] = 1.into();
    fixture.rewrite(&manifest);

    assert_eq!(
        prepare_runtime_bundle(&fixture.root, "0.1.0", 1 << 30, limits())
            .unwrap_err()
            .kind(),
        RuntimeBundleErrorKind::ManifestInvalid
    );
}

#[test]
fn rejects_noncanonical_runtime_metadata() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("manifest.json"),
        serde_json::to_vec_pretty(&fixture.manifest).unwrap(),
    )
    .unwrap();

    assert_eq!(
        prepare_runtime_bundle(&fixture.root, "0.1.0", 1 << 30, limits())
            .unwrap_err()
            .kind(),
        RuntimeBundleErrorKind::NonCanonicalJson
    );
}

#[test]
#[ignore = "executed by contracts/scripts/test-native-runtime-bundle.mjs"]
fn consumes_a_javascript_built_contract_bundle() {
    let root = std::env::var_os("HYPER_CONTRACT_RUNTIME_BUNDLE")
        .map(PathBuf::from)
        .expect("the contract harness must supply its JavaScript-built bundle");
    let manifest_bytes = fs::read(root.join("manifest.json")).unwrap();
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let expected_byte_length = manifest_bytes.len() as u64
        + manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| file["byte_length"].as_u64().unwrap())
            .sum::<u64>();

    let bundle = prepare_runtime_bundle(&root, "1.0.0", expected_byte_length, limits()).unwrap();

    assert_eq!(bundle.bundle_byte_length(), expected_byte_length);
    assert_eq!(bundle.project().session().parts().0.track_count(), 0);
    bundle.project().package().confirm_unchanged().unwrap();
}

fn limits() -> ProjectPlaybackLimits {
    ProjectPlaybackLimits {
        max_total_resource_bytes: 1 << 20,
        audio: DecodeLimits::new(1 << 20, 48_000, 1 << 20).unwrap(),
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 8,
                max_frames_per_asset: 48_000,
                max_decoded_sample_bytes: 1 << 20,
            },
        },
    }
}

fn write_manifest(root: &Path, value: &Value) {
    fs::write(
        root.join("manifest.json"),
        serde_json_canonicalizer::to_vec(value).unwrap(),
    )
    .unwrap();
}

fn copy_payload(source_root: &Path, bundle_root: &Path, relative: &str) {
    let target = bundle_root.join(relative);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::copy(source_root.join(relative), target).unwrap();
}

fn digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

const fn usb_mode(value: UsbMode) -> &'static str {
    match value {
        UsbMode::None => "none",
        UsbMode::Host => "host",
        UsbMode::Service => "service",
        UsbMode::HostAndService => "host-and-service",
    }
}
