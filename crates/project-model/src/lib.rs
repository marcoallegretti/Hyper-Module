#![doc = "Validated, platform-neutral Hyper Module project manifests."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use hyper_strict_json::{StrictJsonError, StrictJsonErrorKind};
use serde::Deserialize;
use serde_json::Value;

const FORMAT: &str = "hyper-project";
const FORMAT_VERSION: u8 = 1;
const SAMPLE_RATE_HZ: u32 = 48_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const MAX_MANIFEST_BYTES: usize = 1_048_576;
const MAX_RECORDS: usize = 4_096;

/// Returns whether a project name belongs to the version-1 manifest domain.
///
/// Importers must preserve every name accepted by the format. Interactive
/// creation may impose additional presentation or portability policy.
#[must_use]
pub fn is_valid_project_name(name: &str) -> bool {
    (1..=120).contains(&name.chars().count())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestErrorKind {
    Json(StrictJsonErrorKind),
    InvalidDomain,
}

#[derive(Debug, Eq, PartialEq)]
pub struct ManifestError {
    kind: ManifestErrorKind,
    line: usize,
    column: usize,
    message: String,
}

impl ManifestError {
    #[must_use]
    pub const fn kind(&self) -> ManifestErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn line(&self) -> usize {
        self.line
    }

    #[must_use]
    pub const fn column(&self) -> usize {
        self.column
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ManifestError {}

impl From<StrictJsonError> for ManifestError {
    fn from(error: StrictJsonError) -> Self {
        Self {
            kind: ManifestErrorKind::Json(error.kind),
            line: error.line,
            column: error.column,
            message: error.message,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceRole {
    Timeline,
    Track,
    Clip,
    Scene,
    Arrangement,
    Mixer,
    Mapping,
    AutomationTake,
    RecordingMetadata,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AssetRole {
    Sample,
    Recording,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub enum AudioMediaType {
    #[serde(rename = "audio/flac")]
    AudioFlac,
    #[serde(rename = "audio/wav")]
    AudioWav,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum UsbMode {
    None,
    Host,
    Service,
    HostAndService,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the published capability contract has four independent binary hardware facts"
)]
pub struct HardwareCapabilities {
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

impl HardwareCapabilities {
    #[must_use]
    pub const fn audio_inputs(&self) -> u8 {
        self.audio_inputs
    }

    #[must_use]
    pub const fn audio_outputs(&self) -> u8 {
        self.audio_outputs
    }

    #[must_use]
    pub const fn cv_inputs(&self) -> u8 {
        self.cv_inputs
    }

    #[must_use]
    pub const fn cv_outputs(&self) -> u8 {
        self.cv_outputs
    }

    #[must_use]
    pub const fn gate_outputs(&self) -> u8 {
        self.gate_outputs
    }

    #[must_use]
    pub const fn has_clock_input(&self) -> bool {
        self.has_clock_input
    }

    #[must_use]
    pub const fn has_reset_input(&self) -> bool {
        self.has_reset_input
    }

    #[must_use]
    pub const fn has_clock_output(&self) -> bool {
        self.has_clock_output
    }

    #[must_use]
    pub const fn usb_mode(&self) -> UsbMode {
        self.usb_mode
    }

    #[must_use]
    pub const fn has_ethernet(&self) -> bool {
        self.has_ethernet
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceDescriptor {
    id: String,
    role: ResourceRole,
    path: String,
    byte_length: u64,
    sha256: String,
    extensions: BTreeMap<String, Value>,
}

impl ResourceDescriptor {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn role(&self) -> ResourceRole {
        self.role
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetDescriptor {
    id: String,
    role: AssetRole,
    path: String,
    media_type: AudioMediaType,
    byte_length: u64,
    sha256: String,
    extensions: BTreeMap<String, Value>,
}

impl AssetDescriptor {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub const fn role(&self) -> AssetRole {
        self.role
    }

    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub const fn media_type(&self) -> AudioMediaType {
        self.media_type
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectManifest {
    project_id: String,
    revision: u64,
    name: String,
    required_capabilities: HardwareCapabilities,
    resources: Vec<ResourceDescriptor>,
    assets: Vec<AssetDescriptor>,
    extensions: BTreeMap<String, Value>,
}

impl ProjectManifest {
    /// Parses and semantically validates a complete version-1 project
    /// manifest. File-system integrity remains the storage layer's concern.
    ///
    /// # Errors
    ///
    /// Returns a classified JSON or project-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, ManifestError> {
        let raw: RawManifest = hyper_strict_json::parse(bytes, MAX_MANIFEST_BYTES)?;
        validate_manifest(raw)
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        SAMPLE_RATE_HZ
    }

    #[must_use]
    pub const fn required_capabilities(&self) -> &HardwareCapabilities {
        &self.required_capabilities
    }

    #[must_use]
    pub fn resources(&self) -> &[ResourceDescriptor] {
        &self.resources
    }

    #[must_use]
    pub fn assets(&self) -> &[AssetDescriptor] {
        &self.assets
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    format: String,
    format_version: u8,
    project_id: String,
    revision: u64,
    name: String,
    sample_rate_hz: u32,
    required_capabilities: RawHardwareCapabilities,
    resources: Vec<RawResourceDescriptor>,
    assets: Vec<RawAssetDescriptor>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "this wire representation exactly mirrors the published capability contract"
)]
struct RawHardwareCapabilities {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawResourceDescriptor {
    id: String,
    role: ResourceRole,
    path: String,
    media_type: String,
    byte_length: u64,
    sha256: String,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAssetDescriptor {
    id: String,
    role: AssetRole,
    path: String,
    media_type: AudioMediaType,
    byte_length: u64,
    sha256: String,
    extensions: BTreeMap<String, Value>,
}

fn validate_manifest(raw: RawManifest) -> Result<ProjectManifest, ManifestError> {
    require(raw.format == FORMAT, "project format is invalid")?;
    require(
        raw.format_version == FORMAT_VERSION,
        "project format version is unsupported",
    )?;
    require(
        is_valid_entity_id(&raw.project_id),
        "project identifier is invalid",
    )?;
    require(
        raw.revision <= MAX_SAFE_INTEGER,
        "project revision exceeds the interoperable integer range",
    )?;
    require(
        is_valid_project_name(&raw.name),
        "project name length is invalid",
    )?;
    require(
        raw.sample_rate_hz == SAMPLE_RATE_HZ,
        "project sample rate is unsupported",
    )?;
    validate_extensions(&raw.extensions, "project extensions")?;
    require(
        raw.resources.len() <= MAX_RECORDS,
        "project resource count exceeds the version-1 limit",
    )?;
    require(
        raw.assets.len() <= MAX_RECORDS,
        "project asset count exceeds the version-1 limit",
    )?;

    let required_capabilities = validate_capabilities(raw.required_capabilities)?;
    let mut identities = BTreeSet::new();
    let mut folded_paths = BTreeSet::new();
    let resources = validate_resources(raw.resources, &mut identities, &mut folded_paths)?;
    let assets = validate_assets(raw.assets, &mut identities, &mut folded_paths)?;

    Ok(ProjectManifest {
        project_id: raw.project_id,
        revision: raw.revision,
        name: raw.name,
        required_capabilities,
        resources,
        assets,
        extensions: raw.extensions,
    })
}

fn validate_resources(
    raw_resources: Vec<RawResourceDescriptor>,
    identities: &mut BTreeSet<String>,
    folded_paths: &mut BTreeSet<String>,
) -> Result<Vec<ResourceDescriptor>, ManifestError> {
    let mut timeline_count = 0_usize;
    let mut resources = Vec::new();
    resources
        .try_reserve_exact(raw_resources.len())
        .map_err(|_| domain_error("project resource capacity is unavailable"))?;
    for raw in raw_resources {
        validate_record(
            &raw.id,
            &raw.path,
            raw.byte_length,
            &raw.sha256,
            &raw.extensions,
            identities,
            folded_paths,
        )?;
        require(
            raw.media_type == "application/json",
            "project resource media type is invalid",
        )?;
        require(
            raw.path.starts_with(resource_root(raw.role)),
            "project resource path does not match its role",
        )?;
        timeline_count += usize::from(raw.role == ResourceRole::Timeline);
        resources.push(ResourceDescriptor {
            id: raw.id,
            role: raw.role,
            path: raw.path,
            byte_length: raw.byte_length,
            sha256: raw.sha256,
            extensions: raw.extensions,
        });
    }
    require(
        timeline_count == 1,
        "project must describe exactly one timeline",
    )?;
    Ok(resources)
}

fn validate_assets(
    raw_assets: Vec<RawAssetDescriptor>,
    identities: &mut BTreeSet<String>,
    folded_paths: &mut BTreeSet<String>,
) -> Result<Vec<AssetDescriptor>, ManifestError> {
    let mut assets = Vec::new();
    assets
        .try_reserve_exact(raw_assets.len())
        .map_err(|_| domain_error("project asset capacity is unavailable"))?;
    for raw in raw_assets {
        validate_record(
            &raw.id,
            &raw.path,
            raw.byte_length,
            &raw.sha256,
            &raw.extensions,
            identities,
            folded_paths,
        )?;
        require(
            raw.path.starts_with(asset_root(raw.role)),
            "project asset path does not match its role",
        )?;
        assets.push(AssetDescriptor {
            id: raw.id,
            role: raw.role,
            path: raw.path,
            media_type: raw.media_type,
            byte_length: raw.byte_length,
            sha256: raw.sha256,
            extensions: raw.extensions,
        });
    }
    Ok(assets)
}

fn validate_capabilities(
    raw: RawHardwareCapabilities,
) -> Result<HardwareCapabilities, ManifestError> {
    require(
        raw.format == "hyper-hardware-capabilities",
        "hardware capability format is invalid",
    )?;
    require(
        raw.format_version == FORMAT_VERSION,
        "hardware capability format version is unsupported",
    )?;
    require(
        raw.audio_inputs <= 2
            && raw.audio_outputs <= 2
            && raw.cv_inputs <= 4
            && raw.cv_outputs <= 4
            && raw.gate_outputs <= 4,
        "hardware capability count exceeds the version-1 product boundary",
    )?;
    validate_extensions(&raw.extensions, "hardware capability extensions")?;
    Ok(HardwareCapabilities {
        audio_inputs: raw.audio_inputs,
        audio_outputs: raw.audio_outputs,
        cv_inputs: raw.cv_inputs,
        cv_outputs: raw.cv_outputs,
        gate_outputs: raw.gate_outputs,
        has_clock_input: raw.has_clock_input,
        has_reset_input: raw.has_reset_input,
        has_clock_output: raw.has_clock_output,
        usb_mode: raw.usb_mode,
        has_ethernet: raw.has_ethernet,
        extensions: raw.extensions,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_record(
    id: &str,
    path: &str,
    byte_length: u64,
    sha256: &str,
    extensions: &BTreeMap<String, Value>,
    identities: &mut BTreeSet<String>,
    folded_paths: &mut BTreeSet<String>,
) -> Result<(), ManifestError> {
    require(
        is_valid_entity_id(id),
        "project record identifier is invalid",
    )?;
    require(
        identities.insert(id.to_owned()),
        "project record identifiers are not unique",
    )?;
    require(valid_relative_path(path), "project record path is invalid")?;
    require(
        folded_paths.insert(path.to_ascii_lowercase()),
        "project record paths are not unique after case folding",
    )?;
    require(
        byte_length <= MAX_SAFE_INTEGER,
        "project record byte length exceeds the interoperable integer range",
    )?;
    require(valid_sha256(sha256), "project record SHA-256 is invalid")?;
    validate_extensions(extensions, "project record extensions")
}

fn resource_root(role: ResourceRole) -> &'static str {
    match role {
        ResourceRole::Timeline => "timeline/",
        ResourceRole::Track => "tracks/",
        ResourceRole::Clip => "clips/",
        ResourceRole::Scene => "scenes/",
        ResourceRole::Arrangement => "arrangement/",
        ResourceRole::Mixer => "mixer/",
        ResourceRole::Mapping => "mappings/",
        ResourceRole::AutomationTake => "takes/",
        ResourceRole::RecordingMetadata => "assets/recordings/",
    }
}

fn asset_root(role: AssetRole) -> &'static str {
    match role {
        AssetRole::Sample => "assets/samples/",
        AssetRole::Recording => "assets/recordings/",
    }
}

/// Returns whether a string belongs to the version-1 entity identifier domain.
#[must_use]
pub fn is_valid_entity_id(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
        })
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_relative_path(value: &str) -> bool {
    if value.is_empty() || value.len() > 240 {
        return false;
    }
    value.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    })
}

fn validate_extensions(
    extensions: &BTreeMap<String, Value>,
    label: &str,
) -> Result<(), ManifestError> {
    require(
        extensions.keys().all(|key| is_valid_extension_key(key)),
        &format!("{label} contain an invalid key"),
    )
}

/// Returns whether a string is a namespaced version-1 extension key.
#[must_use]
pub fn is_valid_extension_key(value: &str) -> bool {
    let mut label_count = 0_usize;
    for label in value.split('.') {
        label_count += 1;
        let bytes = label.as_bytes();
        if bytes.is_empty()
            || !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit()
            || !bytes[bytes.len() - 1].is_ascii_lowercase()
                && !bytes[bytes.len() - 1].is_ascii_digit()
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        {
            return false;
        }
    }
    label_count >= 2
}

fn require(condition: bool, message: &str) -> Result<(), ManifestError> {
    if condition {
        Ok(())
    } else {
        Err(domain_error(message))
    }
}

fn domain_error(message: &str) -> ManifestError {
    ManifestError {
        kind: ManifestErrorKind::InvalidDomain,
        line: 0,
        column: 0,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_PROJECT: &[u8] = include_bytes!("../../../contracts/fixtures/valid/project.json");

    #[test]
    fn loads_the_published_version_one_fixture() {
        let project = ProjectManifest::parse(VALID_PROJECT).unwrap();
        assert_eq!(project.project_id(), "018f47a2-6b2d-7c11-8a44-001122334455");
        assert_eq!(project.revision(), 7);
        assert_eq!(project.sample_rate_hz(), 48_000);
        assert_eq!(project.resources().len(), 2);
        assert_eq!(project.assets().len(), 1);
        assert_eq!(project.required_capabilities().audio_inputs(), 2);
        assert!(project.extensions().contains_key("org.example.fixture"));
    }

    #[test]
    fn rejects_published_invalid_manifest_fixtures() {
        for bytes in [
            include_bytes!("../../../contracts/fixtures/invalid/project-missing-timeline.json")
                .as_slice(),
            include_bytes!("../../../contracts/fixtures/invalid/project-path-traversal.json")
                .as_slice(),
            include_bytes!("../../../contracts/fixtures/invalid/project-unknown-version.json")
                .as_slice(),
        ] {
            assert!(ProjectManifest::parse(bytes).is_err());
        }
    }

    #[test]
    fn rejects_unknown_duplicate_and_non_interoperable_json() {
        let unknown = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"extensions\": {",
            "\"unknown\":true,\"extensions\": {",
            1,
        );
        assert!(matches!(
            ProjectManifest::parse(unknown.as_bytes())
                .unwrap_err()
                .kind(),
            ManifestErrorKind::Json(StrictJsonErrorKind::TypedDocumentMismatch)
        ));

        let duplicate = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"revision\": 7,",
            "\"revision\":7,\"revision\":8,",
            1,
        );
        assert!(matches!(
            ProjectManifest::parse(duplicate.as_bytes())
                .unwrap_err()
                .kind(),
            ManifestErrorKind::Json(StrictJsonErrorKind::DuplicateObjectName)
        ));

        let unsafe_integer = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"revision\": 7,",
            "\"revision\":9007199254740992,",
            1,
        );
        assert!(matches!(
            ProjectManifest::parse(unsafe_integer.as_bytes())
                .unwrap_err()
                .kind(),
            ManifestErrorKind::Json(StrictJsonErrorKind::NonInteroperableInteger)
        ));
    }

    #[test]
    fn rejects_cross_collection_identity_and_case_folded_path_collisions() {
        let identity_collision = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"018f47a2-6b2d-7c11-8a44-001122334457\"",
            "\"018f47a2-6b2d-7c11-8a44-001122334456\"",
            1,
        );
        assert!(ProjectManifest::parse(identity_collision.as_bytes()).is_err());

        let path_collision = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"assets/samples/kick.flac\"",
            "\"TRACKS/018f47a2-6b2d-7c11-8a44-001122334456.json\"",
            1,
        );
        assert!(ProjectManifest::parse(path_collision.as_bytes()).is_err());
    }

    #[test]
    fn rejects_invalid_extension_keys_at_each_owned_boundary() {
        let invalid_project_extension = String::from_utf8(VALID_PROJECT.to_vec())
            .unwrap()
            .replacen("\"org.example.fixture\"", "\"invalid\"", 1);
        assert!(ProjectManifest::parse(invalid_project_extension.as_bytes()).is_err());

        let invalid_record_extension = String::from_utf8(VALID_PROJECT.to_vec()).unwrap().replacen(
            "\"extensions\": {}",
            "\"extensions\":{\"invalid\":true}",
            1,
        );
        assert!(ProjectManifest::parse(invalid_record_extension.as_bytes()).is_err());
    }
}
