use std::fs;
use std::path::{Path, PathBuf};

use hyper_audio_source::DecodeLimits;
use hyper_media_cache::CacheLimits;
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_runtime_deployment::{DeploymentStartupErrorKind, resolve_runtime_deployment};
use serde_json::Value;

#[test]
#[ignore = "executed by the JavaScript deployment contract suite"]
fn resolves_a_javascript_managed_deployment_target() {
    let root = std::env::var_os("HYPER_CONTRACT_DEPLOYMENT_TARGET")
        .map(PathBuf::from)
        .expect("the contract harness must supply its deployment target");
    let mode = std::env::var("HYPER_CONTRACT_DEPLOYMENT_MODE")
        .expect("the contract harness must supply its scenario");
    let before = read_state(&root);
    let before_active = digest(&before, "/active/bundle_sha256");
    let before_previous = before
        .pointer("/previous_bundle_sha256")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let resolved = resolve_runtime_deployment(&root, "1.0.0", 1 << 30, limits()).unwrap();
    let after = read_state(&root);

    assert_eq!(
        after["generation"].as_u64().unwrap(),
        before["generation"].as_u64().unwrap() + 1
    );
    assert_eq!(after["active"]["status"], "healthy");
    assert_eq!(
        digest(&after, "/active/bundle_sha256"),
        encode_digest(resolved.bundle().bundle_sha256())
    );
    assert_eq!(resolved.generation(), after["generation"].as_u64().unwrap());
    assert!(!root.join("deployment.lock").exists());
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".state-")
    }));
    resolved
        .bundle()
        .project()
        .package()
        .confirm_unchanged()
        .unwrap();

    match mode.as_str() {
        "promote" => {
            assert_eq!(digest(&after, "/active/bundle_sha256"), before_active);
            assert_eq!(resolved.recovered_from_bundle_sha256(), None);
        }
        "recover" => {
            let previous = before_previous.expect("recovery requires a previous bundle");
            assert_eq!(digest(&after, "/active/bundle_sha256"), previous);
            assert_eq!(digest(&after, "/previous_bundle_sha256"), before_active);
            assert_eq!(
                resolved.recovered_from_bundle_sha256(),
                Some(&parse_digest(&before_active))
            );
        }
        unexpected => panic!("unsupported contract scenario {unexpected}"),
    }
}

#[test]
#[ignore = "executed by the JavaScript deployment contract suite"]
fn rejects_an_exhausted_javascript_managed_deployment_atomically() {
    let root = std::env::var_os("HYPER_CONTRACT_DEPLOYMENT_TARGET")
        .map(PathBuf::from)
        .expect("the contract harness must supply its deployment target");
    let state_before = fs::read(root.join("state.json")).unwrap();

    let error = resolve_runtime_deployment(&root, "1.0.0", 1 << 30, limits()).unwrap_err();

    assert_eq!(error.kind(), DeploymentStartupErrorKind::Publication);
    assert_eq!(fs::read(root.join("state.json")).unwrap(), state_before);
    assert!(!root.join("deployment.lock").exists());
    assert!(fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".state-")
    }));
}

#[test]
#[ignore = "executed by the JavaScript deployment contract suite"]
fn leaves_a_healthy_javascript_managed_deployment_unchanged() {
    let root = contract_target();
    let state_before = fs::read(root.join("state.json")).unwrap();
    let before = read_state(&root);

    let resolved = resolve_runtime_deployment(&root, "1.0.0", 1 << 30, limits()).unwrap();

    assert_eq!(fs::read(root.join("state.json")).unwrap(), state_before);
    assert_eq!(
        resolved.generation(),
        before["generation"].as_u64().unwrap()
    );
    assert_eq!(resolved.recovered_from_bundle_sha256(), None);
    assert_clean_release(&root);
}

#[test]
#[ignore = "executed by the JavaScript deployment contract suite"]
fn preserves_both_javascript_candidate_failures() {
    let root = contract_target();
    let state_before = fs::read(root.join("state.json")).unwrap();

    let error = resolve_runtime_deployment(&root, "1.0.0", 1 << 30, limits()).unwrap_err();
    let diagnostic = error_chain(&error);

    assert_eq!(error.kind(), DeploymentStartupErrorKind::CandidateRejected);
    assert!(diagnostic.contains("active candidate:"));
    assert!(diagnostic.contains("previous candidate:"));
    assert_eq!(fs::read(root.join("state.json")).unwrap(), state_before);
    assert_clean_release(&root);
}

#[test]
#[ignore = "executed by the JavaScript deployment contract suite"]
fn refuses_a_javascript_owned_deployment_lock_without_mutation() {
    let root = contract_target();
    let state_before = fs::read(root.join("state.json")).unwrap();
    let owner_before = fs::read(root.join("deployment.lock/owner.json")).unwrap();

    let error = resolve_runtime_deployment(&root, "1.0.0", 1 << 30, limits()).unwrap_err();

    assert_eq!(error.kind(), DeploymentStartupErrorKind::Locked);
    assert_eq!(fs::read(root.join("state.json")).unwrap(), state_before);
    assert_eq!(
        fs::read(root.join("deployment.lock/owner.json")).unwrap(),
        owner_before
    );
}

fn contract_target() -> PathBuf {
    std::env::var_os("HYPER_CONTRACT_DEPLOYMENT_TARGET")
        .map(PathBuf::from)
        .expect("the contract harness must supply its deployment target")
}

fn assert_clean_release(root: &Path) {
    assert!(!root.join("deployment.lock").exists());
    assert!(fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".state-")
    }));
}

fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut output = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        use core::fmt::Write;
        write!(&mut output, ": {cause}").unwrap();
        source = cause.source();
    }
    output
}

fn read_state(root: &Path) -> Value {
    serde_json::from_slice(&fs::read(root.join("state.json")).unwrap()).unwrap()
}

fn digest(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .expect("deployment digest must be a string")
        .to_owned()
}

fn parse_digest(value: &str) -> [u8; 32] {
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        digest[index] = hexadecimal(pair[0]) * 16 + hexadecimal(pair[1]);
    }
    digest
}

const fn hexadecimal(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("invalid hexadecimal digest"),
    }
}

fn encode_digest(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
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
