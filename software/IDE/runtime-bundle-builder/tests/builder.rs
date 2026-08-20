use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use hyper_audio_source::DecodeLimits;
use hyper_media_cache::CacheLimits;
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_runtime_bundle::prepare_runtime_bundle;
use hyper_runtime_bundle_builder::{RuntimeBundleBuildErrorKind, build_project_runtime_bundle};
use hyper_storage_engine::{ProjectCreationRequest, create_project};
use sha2::{Digest, Sha256};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    parent: PathBuf,
    project: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let parent = std::env::temp_dir().join(format!(
            "hyper-runtime-builder-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&parent).unwrap();
        let project = create_project(
            &parent,
            ProjectCreationRequest {
                directory_name: "Project",
                project_name: "Builder test",
            },
        )
        .unwrap()
        .project()
        .root()
        .to_owned();
        Self { parent, project }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.parent).unwrap();
    }
}

#[test]
fn deterministic_build_is_runtime_verified_and_idempotent() {
    let fixture = Fixture::new();
    let before = project_files(&fixture.project);
    let first_path = fixture.parent.join("first.runtime");
    let second_path = fixture.parent.join("second.runtime");

    let first =
        build_project_runtime_bundle(&fixture.project, &first_path, "1.0.0", 1 << 20, limits())
            .unwrap();
    let second =
        build_project_runtime_bundle(&fixture.project, &second_path, "1.0.0", 1 << 20, limits())
            .unwrap();
    let repeated =
        build_project_runtime_bundle(&fixture.project, &first_path, "1.0.0", 1 << 20, limits())
            .unwrap();

    assert_eq!(first.bundle_sha256(), second.bundle_sha256());
    assert_eq!(first.bundle_byte_length(), second.bundle_byte_length());
    assert_eq!(
        fs::read(first_path.join("manifest.json")).unwrap(),
        fs::read(second_path.join("manifest.json")).unwrap()
    );
    assert!(!first.reused());
    assert!(repeated.reused());
    assert_eq!(repeated.output_path(), first_path);
    assert_eq!(project_files(&fixture.project), before);

    let verified = prepare_runtime_bundle(&first_path, "1.0.0", 1 << 20, limits()).unwrap();
    assert_eq!(verified.bundle_sha256(), first.bundle_sha256());
    assert_eq!(verified.bundle_byte_length(), first.bundle_byte_length());
    assert!(staging_entries(&fixture.parent).is_empty());
}

#[test]
fn collision_preserves_existing_output_and_cleans_staging() {
    let fixture = Fixture::new();
    let output = fixture.parent.join("occupied.runtime");
    fs::create_dir(&output).unwrap();
    fs::write(output.join("owner.txt"), b"unrelated").unwrap();

    let error = build_project_runtime_bundle(&fixture.project, &output, "1.0.0", 1 << 20, limits())
        .unwrap_err();

    assert_eq!(error.kind(), RuntimeBundleBuildErrorKind::OutputConflict);
    assert_eq!(fs::read(output.join("owner.txt")).unwrap(), b"unrelated");
    assert!(staging_entries(&fixture.parent).is_empty());
}

#[test]
fn invalid_request_and_tight_bound_create_no_output() {
    let fixture = Fixture::new();
    let invalid = fixture.parent.join("invalid.runtime");
    let bounded = fixture.parent.join("bounded.runtime");

    let error =
        build_project_runtime_bundle(&fixture.project, &invalid, "latest", 1 << 20, limits())
            .unwrap_err();
    assert_eq!(error.kind(), RuntimeBundleBuildErrorKind::InvalidRequest);
    assert!(!invalid.exists());

    let error =
        build_project_runtime_bundle(&fixture.project, &bounded, "1.0.0", 1, limits()).unwrap_err();
    assert_eq!(error.kind(), RuntimeBundleBuildErrorKind::Limit);
    assert!(!bounded.exists());
    assert!(staging_entries(&fixture.parent).is_empty());
}

#[test]
#[ignore = "invoked by the JavaScript contract suite with an external fixture"]
fn builds_a_contract_project_for_cross_language_comparison() {
    let project = std::env::var_os("HYPER_CONTRACT_RUNTIME_PROJECT")
        .map(PathBuf::from)
        .expect("HYPER_CONTRACT_RUNTIME_PROJECT is required");
    let output = std::env::var_os("HYPER_CONTRACT_NATIVE_RUNTIME_BUNDLE")
        .map(PathBuf::from)
        .expect("HYPER_CONTRACT_NATIVE_RUNTIME_BUNDLE is required");

    build_project_runtime_bundle(&project, output, "1.0.0", 1 << 30, contract_limits()).unwrap();
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

fn contract_limits() -> ProjectPlaybackLimits {
    ProjectPlaybackLimits {
        max_total_resource_bytes: 1 << 30,
        audio: DecodeLimits::new(512 * 1_024 * 1_024, 43_200_000, 1 << 30).unwrap(),
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: LoaderLimits::VERSION_1.max_clip_documents,
                max_frames_per_asset: 43_200_000,
                max_decoded_sample_bytes: 1 << 30,
            },
        },
    }
}

fn project_files(root: &Path) -> Vec<(PathBuf, [u8; 32])> {
    fn visit(root: &Path, current: &Path, files: &mut Vec<(PathBuf, [u8; 32])>) {
        let mut entries: Vec<_> = fs::read_dir(current).unwrap().map(Result::unwrap).collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root).unwrap().to_owned(),
                    Sha256::digest(fs::read(path).unwrap()).into(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files
}

fn staging_entries(parent: &Path) -> Vec<PathBuf> {
    fs::read_dir(parent)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".runtime-staging-")
        })
        .map(|entry| entry.path())
        .collect()
}
