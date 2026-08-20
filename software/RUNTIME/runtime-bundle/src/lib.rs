#![doc = "Native verification and preparation of deployable Runtime bundles."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

use hyper_project_loader::{
    PreparedProjectPlayback, ProjectPlaybackError, ProjectPlaybackLimits,
    prepare_verified_project_playback,
};
use hyper_project_model::{
    AudioMediaType, HardwareCapabilities, ProjectManifest, UsbMode, is_valid_entity_id,
    is_valid_extension_key,
};
use hyper_storage_engine::{
    RuntimeBundleManifestFile, StorageError, verify_runtime_bundle_project_with_limits,
};
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const MANIFEST_NAME: &str = "manifest.json";
const PROJECT_NAME: &str = "project.json";
const MAX_MANIFEST_BYTES: usize = 1_048_576;
const MAX_FILES: usize = 4_096;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeBundleErrorKind {
    Io,
    JsonInvalid,
    NonCanonicalJson,
    ManifestInvalid,
    RuntimeIncompatible,
    BundleLimit,
    Storage,
    Playback,
}

#[derive(Debug)]
pub struct RuntimeBundleError {
    kind: RuntimeBundleErrorKind,
    message: String,
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl RuntimeBundleError {
    #[must_use]
    pub const fn kind(&self) -> RuntimeBundleErrorKind {
        self.kind
    }
}

impl fmt::Display for RuntimeBundleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for RuntimeBundleError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

#[derive(Debug)]
pub struct VerifiedRuntimeBundle {
    project: PreparedProjectPlayback,
    bundle_sha256: [u8; 32],
    bundle_byte_length: u64,
}

impl VerifiedRuntimeBundle {
    #[must_use]
    pub const fn project(&self) -> &PreparedProjectPlayback {
        &self.project
    }

    #[must_use]
    pub const fn bundle_sha256(&self) -> &[u8; 32] {
        &self.bundle_sha256
    }

    #[must_use]
    pub const fn bundle_byte_length(&self) -> u64 {
        self.bundle_byte_length
    }

    #[must_use]
    pub fn into_project(self) -> PreparedProjectPlayback {
        self.project
    }
}

/// Verifies one complete Runtime bundle and prepares its embedded project for
/// playback without publishing a partial authority.
///
/// `runtime_version` is the executing Runtime's semantic version. The bundle
/// is rejected before project loading when it requires a newer Runtime.
///
/// # Errors
///
/// Returns a classified rejection for malformed or noncanonical metadata,
/// unsupported domains, incompatibility, capacity, storage integrity, project
/// semantics, media decoding, or concurrent filesystem change.
pub fn prepare_runtime_bundle(
    root: impl AsRef<Path>,
    runtime_version: &str,
    maximum_bundle_bytes: u64,
    playback_limits: ProjectPlaybackLimits,
) -> Result<VerifiedRuntimeBundle, RuntimeBundleError> {
    if maximum_bundle_bytes == 0 {
        return Err(reject(
            RuntimeBundleErrorKind::BundleLimit,
            "maximum bundle bytes must be positive",
        ));
    }
    let runtime_version = Version::parse(runtime_version).map_err(|source| RuntimeBundleError {
        kind: RuntimeBundleErrorKind::ManifestInvalid,
        message: "executing Runtime version is invalid".to_owned(),
        source: Some(Box::new(source)),
    })?;
    let manifest_bytes = read_manifest(root.as_ref(), maximum_bundle_bytes)?;
    let value: Value =
        hyper_strict_json::parse(&manifest_bytes, MAX_MANIFEST_BYTES).map_err(|source| {
            RuntimeBundleError {
                kind: RuntimeBundleErrorKind::JsonInvalid,
                message: "runtime manifest is not strict JSON".to_owned(),
                source: Some(Box::new(source)),
            }
        })?;
    let canonical =
        serde_json_canonicalizer::to_vec(&value).map_err(|source| RuntimeBundleError {
            kind: RuntimeBundleErrorKind::ManifestInvalid,
            message: "runtime manifest cannot be canonicalized".to_owned(),
            source: Some(Box::new(source)),
        })?;
    if manifest_bytes != canonical {
        return Err(reject(
            RuntimeBundleErrorKind::NonCanonicalJson,
            "runtime manifest is not canonical RFC 8785 JSON",
        ));
    }
    let raw: RawManifest = serde_json::from_value(value).map_err(|source| RuntimeBundleError {
        kind: RuntimeBundleErrorKind::ManifestInvalid,
        message: "runtime manifest shape is invalid".to_owned(),
        source: Some(Box::new(source)),
    })?;
    let manifest = validate_manifest(raw)?;
    if runtime_version < manifest.minimum_runtime_version {
        return Err(reject(
            RuntimeBundleErrorKind::RuntimeIncompatible,
            "bundle requires a newer Runtime version",
        ));
    }

    let bundle_byte_length = manifest.files.iter().try_fold(
        u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX),
        |total, file| total.checked_add(file.byte_length),
    );
    let Some(bundle_byte_length) = bundle_byte_length else {
        return Err(reject(
            RuntimeBundleErrorKind::BundleLimit,
            "bundle byte accounting overflowed",
        ));
    };
    if bundle_byte_length > maximum_bundle_bytes {
        return Err(reject(
            RuntimeBundleErrorKind::BundleLimit,
            "bundle exceeds the configured byte limit",
        ));
    }

    let bundle_sha256: [u8; 32] = Sha256::digest(&manifest_bytes).into();
    let package_limits = playback_limits
        .package_verification_limits()
        .map_err(playback_error)?;
    let package = verify_runtime_bundle_project_with_limits(
        root,
        package_limits,
        RuntimeBundleManifestFile {
            byte_length: u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX),
            sha256: bundle_sha256,
        },
    )
    .map_err(storage_error)?;
    bind_project(
        &manifest,
        package.manifest(),
        package.manifest_byte_length(),
        package.manifest_sha256(),
    )?;
    let project =
        prepare_verified_project_playback(package, playback_limits).map_err(playback_error)?;
    Ok(VerifiedRuntimeBundle {
        project,
        bundle_sha256,
        bundle_byte_length,
    })
}

fn read_manifest(root: &Path, maximum_bundle_bytes: u64) -> Result<Vec<u8>, RuntimeBundleError> {
    let path = root.join(MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|source| io_error("inspect runtime manifest", source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(reject(
            RuntimeBundleErrorKind::ManifestInvalid,
            "runtime manifest must be a regular non-link file",
        ));
    }
    let maximum = maximum_bundle_bytes.min(MAX_MANIFEST_BYTES as u64);
    if metadata.len() > maximum {
        return Err(reject(
            RuntimeBundleErrorKind::BundleLimit,
            "runtime manifest exceeds its byte limit",
        ));
    }
    let file = File::open(path).map_err(|source| io_error("open runtime manifest", source))?;
    let capacity = usize::try_from(metadata.len()).map_err(|_| {
        reject(
            RuntimeBundleErrorKind::BundleLimit,
            "runtime manifest cannot fit in memory",
        )
    })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|_| {
        reject(
            RuntimeBundleErrorKind::BundleLimit,
            "runtime manifest capacity is unavailable",
        )
    })?;
    file.take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read runtime manifest", source))?;
    if bytes.len() as u64 > maximum {
        return Err(reject(
            RuntimeBundleErrorKind::BundleLimit,
            "runtime manifest grew beyond its byte limit",
        ));
    }
    Ok(bytes)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    format: String,
    format_version: u8,
    project_id: String,
    project_revision: u64,
    source_project_sha256: String,
    minimum_runtime_version: String,
    builder: RawBuilder,
    entrypoint: String,
    required_capabilities: RawCapabilities,
    files: Vec<RawFile>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBuilder {
    id: String,
    version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "this wire representation exactly mirrors the published capability contract"
)]
struct RawCapabilities {
    format: String,
    format_version: u8,
    audio_inputs: u8,
    audio_outputs: u8,
    cv_inputs: u8,
    cv_outputs: u8,
    gate_outputs: u8,
    has_clock_input: bool,
    has_reset_input: bool,
    has_clock_output: bool,
    usb_mode: UsbMode,
    has_ethernet: bool,
    extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    role: FileRole,
    media_type: FileMediaType,
    byte_length: u64,
    sha256: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum FileRole {
    RuntimeProject,
    RuntimeData,
    Audio,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
enum FileMediaType {
    #[serde(rename = "application/json")]
    Json,
    #[serde(rename = "application/octet-stream")]
    OctetStream,
    #[serde(rename = "audio/flac")]
    AudioFlac,
    #[serde(rename = "audio/wav")]
    AudioWav,
}

struct RuntimeManifest {
    project_id: String,
    project_revision: u64,
    source_project_sha256: String,
    minimum_runtime_version: Version,
    required_capabilities: RawCapabilities,
    files: Vec<RawFile>,
}

fn validate_manifest(raw: RawManifest) -> Result<RuntimeManifest, RuntimeBundleError> {
    require(
        raw.format == "hyper-runtime-bundle",
        "runtime bundle format is invalid",
    )?;
    require(
        raw.format_version == 1,
        "runtime bundle version is unsupported",
    )?;
    require(
        is_valid_entity_id(&raw.project_id),
        "bundle project identifier is invalid",
    )?;
    require(
        raw.project_revision <= MAX_SAFE_INTEGER,
        "bundle project revision is invalid",
    )?;
    require(
        valid_sha256(&raw.source_project_sha256),
        "source project digest is invalid",
    )?;
    let minimum_runtime_version = Version::parse(&raw.minimum_runtime_version)
        .map_err(|_| invalid("minimum Runtime version is invalid"))?;
    require(
        valid_builder_id(&raw.builder.id),
        "bundle builder identifier is invalid",
    )?;
    Version::parse(&raw.builder.version)
        .map_err(|_| invalid("bundle builder version is invalid"))?;
    require(
        raw.entrypoint == PROJECT_NAME,
        "version 1 entrypoint must be project.json",
    )?;
    validate_capabilities(&raw.required_capabilities)?;
    validate_extensions(&raw.extensions, "runtime manifest")?;
    require(
        !raw.files.is_empty() && raw.files.len() <= MAX_FILES,
        "runtime file count is invalid",
    )?;

    let mut folded = BTreeSet::new();
    let mut entrypoints = 0_usize;
    for file in &raw.files {
        require(
            valid_relative_path(&file.path),
            "runtime file path is invalid",
        )?;
        require(
            file.path != MANIFEST_NAME,
            "runtime manifest cannot describe itself",
        )?;
        require(
            folded.insert(file.path.to_ascii_lowercase()),
            "runtime paths are not unique after case folding",
        )?;
        require(
            file.byte_length <= MAX_SAFE_INTEGER,
            "runtime file byte length is invalid",
        )?;
        require(valid_sha256(&file.sha256), "runtime file digest is invalid")?;
        match file.role {
            FileRole::RuntimeProject | FileRole::RuntimeData => require(
                file.media_type == FileMediaType::Json,
                "runtime JSON role has an invalid media type",
            )?,
            FileRole::Audio => require(
                matches!(
                    file.media_type,
                    FileMediaType::AudioFlac | FileMediaType::AudioWav
                ),
                "runtime audio role has an invalid media type",
            )?,
        }
        if file.path == raw.entrypoint {
            entrypoints += 1;
            require(
                file.role == FileRole::RuntimeProject,
                "runtime entrypoint role is invalid",
            )?;
        }
    }
    require(
        entrypoints == 1,
        "runtime entrypoint must be listed exactly once",
    )?;
    Ok(RuntimeManifest {
        project_id: raw.project_id,
        project_revision: raw.project_revision,
        source_project_sha256: raw.source_project_sha256,
        minimum_runtime_version,
        required_capabilities: raw.required_capabilities,
        files: raw.files,
    })
}

fn bind_project(
    bundle: &RuntimeManifest,
    project: &ProjectManifest,
    project_byte_length: u64,
    project_sha256: &[u8; 32],
) -> Result<(), RuntimeBundleError> {
    require(
        bundle.project_id == project.project_id(),
        "bundle project identifier does not match project",
    )?;
    require(
        bundle.project_revision == project.revision(),
        "bundle revision does not match project",
    )?;
    require(
        bundle
            .required_capabilities
            .matches(project.required_capabilities()),
        "bundle capabilities do not match project",
    )?;
    let project_digest = hex_digest(project_sha256);
    require(
        bundle.source_project_sha256 == project_digest,
        "bundle source digest does not match project",
    )?;

    let by_path = bundle
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let project_file = by_path
        .get(PROJECT_NAME)
        .ok_or_else(|| invalid("runtime project descriptor is absent"))?;
    require(
        project_file.sha256 == project_digest,
        "runtime project descriptor digest does not match project",
    )?;
    require(
        project_file.byte_length == project_byte_length,
        "runtime project descriptor length does not match project",
    )?;
    require(
        project_file.role == FileRole::RuntimeProject
            && project_file.media_type == FileMediaType::Json,
        "runtime project descriptor type is invalid",
    )?;
    for resource in project.resources() {
        let file = by_path
            .get(resource.path())
            .ok_or_else(|| invalid("project resource is absent from runtime manifest"))?;
        require(
            file.role == FileRole::RuntimeData && file.media_type == FileMediaType::Json,
            "project resource runtime role is invalid",
        )?;
        require(
            file.byte_length == resource.byte_length() && file.sha256 == resource.sha256(),
            "project resource descriptor does not match runtime manifest",
        )?;
    }
    for asset in project.assets() {
        let file = by_path
            .get(asset.path())
            .ok_or_else(|| invalid("project asset is absent from runtime manifest"))?;
        let media_type = match asset.media_type() {
            AudioMediaType::AudioFlac => FileMediaType::AudioFlac,
            AudioMediaType::AudioWav => FileMediaType::AudioWav,
        };
        require(
            file.role == FileRole::Audio && file.media_type == media_type,
            "project asset runtime role is invalid",
        )?;
        require(
            file.byte_length == asset.byte_length() && file.sha256 == asset.sha256(),
            "project asset descriptor does not match runtime manifest",
        )?;
    }
    require(
        by_path.len() == 1 + project.resources().len() + project.assets().len(),
        "runtime manifest contains a file outside the project",
    )
}

impl RawCapabilities {
    fn matches(&self, value: &HardwareCapabilities) -> bool {
        self.audio_inputs == value.audio_inputs()
            && self.audio_outputs == value.audio_outputs()
            && self.cv_inputs == value.cv_inputs()
            && self.cv_outputs == value.cv_outputs()
            && self.gate_outputs == value.gate_outputs()
            && self.has_clock_input == value.has_clock_input()
            && self.has_reset_input == value.has_reset_input()
            && self.has_clock_output == value.has_clock_output()
            && self.usb_mode == value.usb_mode()
            && self.has_ethernet == value.has_ethernet()
            && self.extensions == *value.extensions()
    }
}

fn validate_capabilities(value: &RawCapabilities) -> Result<(), RuntimeBundleError> {
    require(
        value.format == "hyper-hardware-capabilities" && value.format_version == 1,
        "hardware capability format is invalid",
    )?;
    require(
        value.audio_inputs <= 2
            && value.audio_outputs <= 2
            && value.cv_inputs <= 4
            && value.cv_outputs <= 4
            && value.gate_outputs <= 4,
        "hardware capability count exceeds version 1",
    )?;
    validate_extensions(&value.extensions, "hardware capabilities")
}

fn validate_extensions(
    values: &BTreeMap<String, Value>,
    label: &str,
) -> Result<(), RuntimeBundleError> {
    require(
        values.keys().all(|key| is_valid_extension_key(key)),
        &format!("{label} extensions contain an invalid key"),
    )
}

fn valid_builder_id(value: &str) -> bool {
    !value.is_empty()
        && value.split(['.', '_', '-']).all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 240
        && value.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex_digest(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn require(condition: bool, message: &str) -> Result<(), RuntimeBundleError> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

fn invalid(message: &str) -> RuntimeBundleError {
    reject(RuntimeBundleErrorKind::ManifestInvalid, message)
}

fn reject(kind: RuntimeBundleErrorKind, message: &str) -> RuntimeBundleError {
    RuntimeBundleError {
        kind,
        message: message.to_owned(),
        source: None,
    }
}

fn io_error(action: &str, source: io::Error) -> RuntimeBundleError {
    RuntimeBundleError {
        kind: RuntimeBundleErrorKind::Io,
        message: action.to_owned(),
        source: Some(Box::new(source)),
    }
}

fn storage_error(source: StorageError) -> RuntimeBundleError {
    RuntimeBundleError {
        kind: RuntimeBundleErrorKind::Storage,
        message: "bundled project failed structural verification".to_owned(),
        source: Some(Box::new(source)),
    }
}

fn playback_error(source: ProjectPlaybackError) -> RuntimeBundleError {
    RuntimeBundleError {
        kind: RuntimeBundleErrorKind::Playback,
        message: "bundled project is not playback-ready".to_owned(),
        source: Some(Box::new(source)),
    }
}
