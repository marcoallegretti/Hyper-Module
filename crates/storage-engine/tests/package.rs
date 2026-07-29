use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_storage_engine::{StorageErrorKind, verify_project_package};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hyper-project-package-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("timeline")).unwrap();
        fs::create_dir_all(root.join("assets/samples")).unwrap();
        let timeline = br#"{"format":"fixture-timeline"}"#;
        let sample = b"RIFFstructural-fixture";
        fs::write(root.join("timeline/timeline.json"), timeline).unwrap();
        fs::write(root.join("assets/samples/sample.wav"), sample).unwrap();
        let manifest = json!({
            "format": "hyper-project",
            "format_version": 1,
            "project_id": "018f47a2-6b2d-7c11-8a44-001122334455",
            "revision": 1,
            "name": "Native package fixture",
            "sample_rate_hz": 48000,
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
            "resources": [{
                "id": "018f47a2-6b2d-7c11-8a44-001122334456",
                "role": "timeline",
                "path": "timeline/timeline.json",
                "media_type": "application/json",
                "byte_length": timeline.len(),
                "sha256": sha256(timeline),
                "extensions": {}
            }],
            "assets": [{
                "id": "018f47a2-6b2d-7c11-8a44-001122334457",
                "role": "sample",
                "path": "assets/samples/sample.wav",
                "media_type": "audio/wav",
                "byte_length": sample.len(),
                "sha256": sha256(sample),
                "extensions": {}
            }],
            "extensions": {}
        });
        write_canonical(&root.join("project.json"), &manifest);
        Self { root }
    }

    fn rewrite_resource(&self, bytes: &[u8]) {
        let path = self.root.join("timeline/timeline.json");
        fs::write(path, bytes).unwrap();
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(self.root.join("project.json")).unwrap()).unwrap();
        manifest["resources"][0]["byte_length"] = bytes.len().into();
        manifest["resources"][0]["sha256"] = sha256(bytes).into();
        write_canonical(&self.root.join("project.json"), &manifest);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn verifies_owned_canonical_bytes_and_allows_local_state() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("cache/waveforms")).unwrap();
    fs::create_dir_all(fixture.root.join("backups")).unwrap();
    fs::write(
        fixture.root.join("cache/waveforms/track.bin"),
        b"regenerable",
    )
    .unwrap();
    fs::write(fixture.root.join("backups/project.previous"), b"backup").unwrap();

    let verified = verify_project_package(&fixture.root).unwrap();
    assert_eq!(
        verified.manifest().project_id(),
        "018f47a2-6b2d-7c11-8a44-001122334455"
    );
    assert_eq!(
        verified.manifest_sha256(),
        &<[u8; 32]>::from(Sha256::digest(
            fs::read(fixture.root.join("project.json")).unwrap()
        ))
    );
    assert!(verified.root().is_absolute());
}

#[test]
fn rejects_noncanonical_manifest_and_resource_json() {
    let manifest_fixture = Fixture::new();
    let manifest_path = manifest_fixture.root.join("project.json");
    let value: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    fs::write(&manifest_path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
    assert_eq!(
        verify_project_package(&manifest_fixture.root)
            .unwrap_err()
            .kind(),
        StorageErrorKind::NonCanonicalJson
    );

    let resource_fixture = Fixture::new();
    resource_fixture.rewrite_resource(b"{ \"format\": \"fixture-timeline\" }\n");
    let error = verify_project_package(&resource_fixture.root).unwrap_err();
    assert_eq!(error.kind(), StorageErrorKind::NonCanonicalJson);
    assert_eq!(error.path(), "timeline/timeline.json");
}

#[test]
fn rejects_corruption_missing_payloads_and_undeclared_entries() {
    let corrupt = Fixture::new();
    fs::write(
        corrupt.root.join("timeline/timeline.json"),
        br#"{"format":"corrupt-timeline"}"#,
    )
    .unwrap();
    assert_eq!(
        verify_project_package(&corrupt.root).unwrap_err().kind(),
        StorageErrorKind::Sha256Mismatch
    );

    let missing = Fixture::new();
    fs::remove_file(missing.root.join("assets/samples/sample.wav")).unwrap();
    assert_eq!(
        verify_project_package(&missing.root).unwrap_err().kind(),
        StorageErrorKind::ManagedTreeMismatch
    );

    let undeclared = Fixture::new();
    fs::write(undeclared.root.join("undeclared"), b"foreign").unwrap();
    assert_eq!(
        verify_project_package(&undeclared.root).unwrap_err().kind(),
        StorageErrorKind::UndeclaredEntry
    );

    for reserved_directory in ["cache", "backups"] {
        let reserved_file = Fixture::new();
        fs::write(reserved_file.root.join(reserved_directory), b"foreign").unwrap();
        assert_eq!(
            verify_project_package(&reserved_file.root)
                .unwrap_err()
                .kind(),
            StorageErrorKind::UndeclaredEntry
        );
    }
}

#[test]
fn rejects_links_in_the_owned_tree_when_the_platform_can_create_one() {
    let fixture = Fixture::new();
    let target = fixture.root.join("timeline/timeline.json");
    let link = fixture.root.join("linked.json");
    if let Err(error) = create_file_symlink(&target, &link) {
        if error.kind() == std::io::ErrorKind::PermissionDenied
            || error.raw_os_error() == Some(1314)
        {
            return;
        }
        panic!("could not create link fixture: {error}");
    }
    let error = verify_project_package(&fixture.root).unwrap_err();
    assert_eq!(error.kind(), StorageErrorKind::LinkForbidden);
    assert_eq!(error.path(), "linked.json");
}

#[test]
fn rejects_non_directory_roots_and_excessive_local_depth() {
    let fixture = Fixture::new();
    assert_eq!(
        verify_project_package(fixture.root.join("project.json"))
            .unwrap_err()
            .kind(),
        StorageErrorKind::RootNotDirectory
    );

    let mut deep = fixture.root.join("cache");
    for _ in 0..65 {
        deep.push("d");
    }
    fs::create_dir_all(deep).unwrap();
    assert_eq!(
        verify_project_package(&fixture.root).unwrap_err().kind(),
        StorageErrorKind::LimitExceeded
    );
}

fn sha256(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
}

fn write_canonical(path: &Path, value: &Value) {
    fs::write(path, serde_json_canonicalizer::to_vec(value).unwrap()).unwrap();
}

#[cfg(unix)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_file_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}
