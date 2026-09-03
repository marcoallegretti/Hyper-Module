#![doc = "Strict Native Device state and bounded shared DSP for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::f64::consts::PI;
use core::fmt;
use std::collections::{BTreeMap, BTreeSet};

use hyper_project_model::{is_valid_entity_id, is_valid_extension_key};
use hyper_strict_json::{StrictJsonError, StrictJsonErrorKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const FORMAT: &str = "hyper-device-chain";
const LEGACY_FORMAT_VERSION: u8 = 1;
const CURRENT_FORMAT_VERSION: u8 = 2;
const MAX_DOCUMENT_BYTES: usize = 1_048_576;
pub const MAX_DEVICES_PER_CHAIN: usize = 8;
pub const PROJECT_SAMPLE_RATE_HZ: u32 = 48_000;
pub const MAX_PROCESS_FRAMES: usize = 65_536;
pub const NATIVE_FILTER_DEVICE_ID: &str = "org.hyper-module.native-filter";
pub const NATIVE_FILTER_STATE_VERSION: u16 = 1;
pub const NATIVE_FILTER_DSP_VERSION: u16 = 1;
pub const NATIVE_FILTER_CUTOFF_PARAMETER: &str = "org.hyper-module.native-filter/cutoff";
pub const NATIVE_FILTER_RESONANCE_PARAMETER: &str = "org.hyper-module.native-filter/resonance";
pub const NATIVE_FILTER_MODE_PARAMETER: &str = "org.hyper-module.native-filter/mode";
pub const NATIVE_FILTER_ENABLED_PARAMETER: &str = "org.hyper-module.native-filter/enabled";
pub const MIN_FILTER_CUTOFF_MILLIHERTZ: u32 = 20_000;
pub const MAX_FILTER_CUTOFF_MILLIHERTZ: u32 = 20_000_000;
pub const MIN_FILTER_RESONANCE_MILLIONTHS: u32 = 100_000;
pub const MAX_FILTER_RESONANCE_MILLIONTHS: u32 = 20_000_000;
pub const NATIVE_COMPRESSOR_DEVICE_ID: &str = "org.hyper-module.native-compressor";
pub const NATIVE_COMPRESSOR_STATE_VERSION: u16 = 1;
pub const NATIVE_COMPRESSOR_DSP_VERSION: u16 = 1;
pub const NATIVE_COMPRESSOR_THRESHOLD_PARAMETER: &str =
    "org.hyper-module.native-compressor/threshold";
pub const NATIVE_COMPRESSOR_RATIO_PARAMETER: &str = "org.hyper-module.native-compressor/ratio";
pub const NATIVE_COMPRESSOR_ATTACK_PARAMETER: &str = "org.hyper-module.native-compressor/attack";
pub const NATIVE_COMPRESSOR_RELEASE_PARAMETER: &str = "org.hyper-module.native-compressor/release";
pub const NATIVE_COMPRESSOR_MAKEUP_PARAMETER: &str = "org.hyper-module.native-compressor/makeup";
pub const NATIVE_COMPRESSOR_ENABLED_PARAMETER: &str = "org.hyper-module.native-compressor/enabled";
pub const MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS: i32 = -80_000;
pub const MAX_COMPRESSOR_THRESHOLD_MILLIDECIBELS: i32 = 0;
pub const MIN_COMPRESSOR_RATIO_MILLIONTHS: u32 = 1_000_000;
pub const MAX_COMPRESSOR_RATIO_MILLIONTHS: u32 = 100_000_000;
pub const MIN_COMPRESSOR_ATTACK_MICROSECONDS: u32 = 100;
pub const MAX_COMPRESSOR_ATTACK_MICROSECONDS: u32 = 500_000;
pub const MIN_COMPRESSOR_RELEASE_MICROSECONDS: u32 = 100;
pub const MAX_COMPRESSOR_RELEASE_MICROSECONDS: u32 = 2_000_000;
pub const MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS: i32 = -24_000;
pub const MAX_COMPRESSOR_MAKEUP_MILLIDECIBELS: i32 = 24_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceChainErrorKind {
    Json(StrictJsonErrorKind),
    InvalidDomain,
    CapacityUnavailable,
}

#[derive(Debug, Eq, PartialEq)]
pub struct DeviceChainError {
    kind: DeviceChainErrorKind,
    message: String,
}

impl DeviceChainError {
    #[must_use]
    pub const fn kind(&self) -> DeviceChainErrorKind {
        self.kind
    }
}

impl fmt::Display for DeviceChainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for DeviceChainError {}

impl From<StrictJsonError> for DeviceChainError {
    fn from(error: StrictJsonError) -> Self {
        Self {
            kind: DeviceChainErrorKind::Json(error.kind),
            message: error.message,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NativeFilterMode {
    LowPass,
    HighPass,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeFilterState {
    mode: NativeFilterMode,
    cutoff_millihertz: u32,
    resonance_millionths: u32,
}

impl NativeFilterState {
    /// Creates one persisted filter state in the version-1 parameter domain.
    ///
    /// # Errors
    ///
    /// Returns a domain error when cutoff or resonance is outside the declared
    /// device range.
    pub fn try_new(
        mode: NativeFilterMode,
        cutoff_millihertz: u32,
        resonance_millionths: u32,
    ) -> Result<Self, DeviceChainError> {
        let state = Self {
            mode,
            cutoff_millihertz,
            resonance_millionths,
        };
        validate_filter_state(state)?;
        Ok(state)
    }

    #[must_use]
    pub const fn mode(self) -> NativeFilterMode {
        self.mode
    }

    #[must_use]
    pub const fn cutoff_millihertz(self) -> u32 {
        self.cutoff_millihertz
    }

    #[must_use]
    pub const fn resonance_millionths(self) -> u32 {
        self.resonance_millionths
    }
}

/// Persisted state for the fixed, zero-latency linked stereo compressor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCompressorState {
    threshold_millidecibels: i32,
    ratio_millionths: u32,
    attack_microseconds: u32,
    release_microseconds: u32,
    makeup_millidecibels: i32,
}

impl NativeCompressorState {
    /// Creates one persisted compressor state in the version-1 parameter domain.
    ///
    /// # Errors
    ///
    /// Returns a domain error when a control is outside the declared device range.
    pub fn try_new(
        threshold_millidecibels: i32,
        ratio_millionths: u32,
        attack_microseconds: u32,
        release_microseconds: u32,
        makeup_millidecibels: i32,
    ) -> Result<Self, DeviceChainError> {
        let state = Self {
            threshold_millidecibels,
            ratio_millionths,
            attack_microseconds,
            release_microseconds,
            makeup_millidecibels,
        };
        validate_compressor_state(state)?;
        Ok(state)
    }

    #[must_use]
    pub const fn threshold_millidecibels(self) -> i32 {
        self.threshold_millidecibels
    }

    #[must_use]
    pub const fn ratio_millionths(self) -> u32 {
        self.ratio_millionths
    }

    #[must_use]
    pub const fn attack_microseconds(self) -> u32 {
        self.attack_microseconds
    }

    #[must_use]
    pub const fn release_microseconds(self) -> u32 {
        self.release_microseconds
    }

    #[must_use]
    pub const fn makeup_millidecibels(self) -> i32 {
        self.makeup_millidecibels
    }
}

/// The state discriminant stored by a version-2 device-chain document.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "parameters", rename_all = "kebab-case")]
pub enum NativeDeviceState {
    Filter(NativeFilterState),
    Compressor(NativeCompressorState),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeDeviceInstance {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeDeviceState,
    extensions: BTreeMap<String, Value>,
}

impl NativeDeviceInstance {
    /// Creates one supported Native Filter instance.
    ///
    /// # Errors
    ///
    /// Returns a domain error for an invalid identity or state.
    pub fn try_new_filter(
        id: String,
        enabled: bool,
        state: NativeFilterState,
    ) -> Result<Self, DeviceChainError> {
        let instance = Self {
            id,
            device_id: NATIVE_FILTER_DEVICE_ID.to_owned(),
            state_version: NATIVE_FILTER_STATE_VERSION,
            dsp_version: NATIVE_FILTER_DSP_VERSION,
            enabled,
            state: NativeDeviceState::Filter(state),
            extensions: BTreeMap::new(),
        };
        validate_device(&instance)?;
        Ok(instance)
    }

    /// Creates one supported Native Compressor instance.
    ///
    /// # Errors
    ///
    /// Returns a domain error for an invalid identity or state.
    pub fn try_new_compressor(
        id: String,
        enabled: bool,
        state: NativeCompressorState,
    ) -> Result<Self, DeviceChainError> {
        let instance = Self {
            id,
            device_id: NATIVE_COMPRESSOR_DEVICE_ID.to_owned(),
            state_version: NATIVE_COMPRESSOR_STATE_VERSION,
            dsp_version: NATIVE_COMPRESSOR_DSP_VERSION,
            enabled,
            state: NativeDeviceState::Compressor(state),
            extensions: BTreeMap::new(),
        };
        validate_device(&instance)?;
        Ok(instance)
    }

    /// Returns this supported device with a new stable instance identity.
    ///
    /// This preserves the device kind, version, enabled state, parameter
    /// state, and extension data for durable processor duplication.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the replacement identity is invalid.
    pub fn try_with_id(&self, id: String) -> Result<Self, DeviceChainError> {
        let mut device = self.clone();
        device.id = id;
        validate_device(&device)?;
        Ok(device)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    #[must_use]
    pub const fn state_version(&self) -> u16 {
        self.state_version
    }

    #[must_use]
    pub const fn dsp_version(&self) -> u16 {
        self.dsp_version
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn state(&self) -> &NativeDeviceState {
        &self.state
    }

    #[must_use]
    pub const fn filter_state(&self) -> Option<NativeFilterState> {
        match self.state {
            NativeDeviceState::Filter(state) => Some(state),
            NativeDeviceState::Compressor(_) => None,
        }
    }

    #[must_use]
    pub const fn compressor_state(&self) -> Option<NativeCompressorState> {
        match self.state {
            NativeDeviceState::Filter(_) => None,
            NativeDeviceState::Compressor(state) => Some(state),
        }
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceChain {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    devices: Vec<NativeDeviceInstance>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct DeviceChainHeader {
    format: String,
    format_version: u8,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeviceChainV1 {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    devices: Vec<NativeDeviceInstanceV1>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeDeviceInstanceV1 {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeFilterState,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DeviceChainV2 {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    devices: Vec<NativeDeviceInstanceV2>,
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeDeviceInstanceV2 {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeDeviceState,
    extensions: BTreeMap<String, Value>,
}

impl DeviceChain {
    /// Parses and validates one complete device-chain document.
    ///
    /// # Errors
    ///
    /// Returns a classified strict-JSON or device-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, DeviceChainError> {
        let header: DeviceChainHeader = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
        let chain = match (header.format.as_str(), header.format_version) {
            (FORMAT, LEGACY_FORMAT_VERSION) => {
                let legacy: DeviceChainV1 = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
                Self {
                    format: legacy.format,
                    format_version: legacy.format_version,
                    id: legacy.id,
                    project_id: legacy.project_id,
                    track_id: legacy.track_id,
                    devices: legacy
                        .devices
                        .into_iter()
                        .map(|device| NativeDeviceInstance {
                            id: device.id,
                            device_id: device.device_id,
                            state_version: device.state_version,
                            dsp_version: device.dsp_version,
                            enabled: device.enabled,
                            state: NativeDeviceState::Filter(device.state),
                            extensions: device.extensions,
                        })
                        .collect(),
                    extensions: legacy.extensions,
                }
            }
            (FORMAT, CURRENT_FORMAT_VERSION) => {
                let current: DeviceChainV2 = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
                Self {
                    format: current.format,
                    format_version: current.format_version,
                    id: current.id,
                    project_id: current.project_id,
                    track_id: current.track_id,
                    devices: current
                        .devices
                        .into_iter()
                        .map(|device| NativeDeviceInstance {
                            id: device.id,
                            device_id: device.device_id,
                            state_version: device.state_version,
                            dsp_version: device.dsp_version,
                            enabled: device.enabled,
                            state: device.state,
                            extensions: device.extensions,
                        })
                        .collect(),
                    extensions: current.extensions,
                }
            }
            _ => return Err(invalid_domain("device chain format is unsupported")),
        };
        validate_chain(&chain)?;
        Ok(chain)
    }

    /// Creates one non-empty validated device chain.
    ///
    /// # Errors
    ///
    /// Returns a domain error for invalid ownership, identities, devices, or
    /// declared bounds.
    pub fn try_new(
        id: String,
        project_id: String,
        track_id: String,
        devices: Vec<NativeDeviceInstance>,
    ) -> Result<Self, DeviceChainError> {
        let format_version = if devices
            .iter()
            .all(|device| matches!(device.state(), NativeDeviceState::Filter(_)))
        {
            LEGACY_FORMAT_VERSION
        } else {
            CURRENT_FORMAT_VERSION
        };
        let chain = Self {
            format: FORMAT.to_owned(),
            format_version,
            id,
            project_id,
            track_id,
            devices,
            extensions: BTreeMap::new(),
        };
        validate_chain(&chain)?;
        Ok(chain)
    }

    /// Serializes this validated chain as RFC 8785 canonical JSON.
    ///
    /// # Errors
    ///
    /// Returns a capacity error if serialization cannot allocate its output.
    pub fn to_canonical_vec(&self) -> Result<Vec<u8>, DeviceChainError> {
        let result = match self.format_version {
            LEGACY_FORMAT_VERSION => {
                let devices = self
                    .devices
                    .iter()
                    .map(|device| {
                        let state = device.filter_state().ok_or_else(|| {
                            invalid_domain("version-one device chains may contain only filters")
                        })?;
                        Ok(NativeDeviceInstanceV1 {
                            id: device.id.clone(),
                            device_id: device.device_id.clone(),
                            state_version: device.state_version,
                            dsp_version: device.dsp_version,
                            enabled: device.enabled,
                            state,
                            extensions: device.extensions.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, DeviceChainError>>()?;
                serde_json_canonicalizer::to_vec(&DeviceChainV1 {
                    format: self.format.clone(),
                    format_version: self.format_version,
                    id: self.id.clone(),
                    project_id: self.project_id.clone(),
                    track_id: self.track_id.clone(),
                    devices,
                    extensions: self.extensions.clone(),
                })
            }
            CURRENT_FORMAT_VERSION => serde_json_canonicalizer::to_vec(&DeviceChainV2 {
                format: self.format.clone(),
                format_version: self.format_version,
                id: self.id.clone(),
                project_id: self.project_id.clone(),
                track_id: self.track_id.clone(),
                devices: self
                    .devices
                    .iter()
                    .map(|device| NativeDeviceInstanceV2 {
                        id: device.id.clone(),
                        device_id: device.device_id.clone(),
                        state_version: device.state_version,
                        dsp_version: device.dsp_version,
                        enabled: device.enabled,
                        state: device.state,
                        extensions: device.extensions.clone(),
                    })
                    .collect(),
                extensions: self.extensions.clone(),
            }),
            _ => return Err(invalid_domain("device chain format is unsupported")),
        };
        result.map_err(|_| DeviceChainError {
            kind: DeviceChainErrorKind::CapacityUnavailable,
            message: "device chain serialization capacity is unavailable".to_owned(),
        })
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    #[must_use]
    pub fn track_id(&self) -> &str {
        &self.track_id
    }

    #[must_use]
    pub fn devices(&self) -> &[NativeDeviceInstance] {
        &self.devices
    }

    #[must_use]
    pub const fn format_version(&self) -> u8 {
        self.format_version
    }

    /// Returns an independently addressable copy for another track.
    ///
    /// The caller supplies a fresh chain identity, destination track identity,
    /// and exactly one fresh identity for every device instance. Device order,
    /// format version, and validated state are retained. Unknown extension
    /// payloads refuse the copy because their identity semantics are not known
    /// at this boundary.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the supplied identities are invalid, the
    /// source declares extensions with unknown copy semantics, the
    /// device-identity count differs from this chain, or the copied chain is
    /// not valid.
    pub fn try_clone_for_track(
        &self,
        chain_id: String,
        track_id: String,
        device_instance_ids: &[String],
    ) -> Result<Self, DeviceChainError> {
        if !self.extensions.is_empty()
            || self
                .devices
                .iter()
                .any(|device| !device.extensions.is_empty())
        {
            return Err(invalid_domain(
                "native device clone refuses unknown extension identity semantics",
            ));
        }
        if device_instance_ids.len() != self.devices.len() {
            return Err(invalid_domain(
                "native device clone identities do not match the source chain",
            ));
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(self.devices.len())
            .map_err(|_| DeviceChainError {
                kind: DeviceChainErrorKind::CapacityUnavailable,
                message: "native device clone capacity is unavailable".to_owned(),
            })?;
        for (source, id) in self.devices.iter().zip(device_instance_ids) {
            devices.push(source.try_with_id(id.clone())?);
        }
        let copied = Self {
            format: self.format.clone(),
            format_version: self.format_version,
            id: chain_id,
            project_id: self.project_id.clone(),
            track_id,
            devices,
            extensions: BTreeMap::new(),
        };
        validate_chain(&copied)?;
        Ok(copied)
    }

    /// Returns a copy with one validated Native Filter appended in processing
    /// order.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the instance identity is duplicated or the
    /// version-1 chain bound would be exceeded.
    pub fn try_with_appended_filter(
        &self,
        id: String,
        enabled: bool,
        state: NativeFilterState,
    ) -> Result<Self, DeviceChainError> {
        let mut devices = self.devices.clone();
        devices.push(NativeDeviceInstance::try_new_filter(id, enabled, state)?);
        self.try_with_devices(devices)
    }

    /// Returns a copy with one validated Native Compressor appended in processing
    /// order. A legacy filter-only chain upgrades to the explicit version-2
    /// discriminated representation.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the instance identity is duplicated or the
    /// device-count bound would be exceeded.
    pub fn try_with_appended_compressor(
        &self,
        id: String,
        enabled: bool,
        state: NativeCompressorState,
    ) -> Result<Self, DeviceChainError> {
        let mut devices = self.devices.clone();
        devices.push(NativeDeviceInstance::try_new_compressor(
            id, enabled, state,
        )?);
        self.try_with_devices(devices)
    }

    /// Returns a copy with one existing Native Filter state replaced without
    /// changing chain order or device identity.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the device identity is absent, belongs to a
    /// different native-device kind, or the state is outside the filter domain.
    pub fn try_with_filter_state(
        &self,
        device_instance_id: &str,
        enabled: bool,
        state: NativeFilterState,
    ) -> Result<Self, DeviceChainError> {
        let mut devices = self.devices.clone();
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.id() == device_instance_id)
        else {
            return Err(invalid_domain(
                "native device instance does not exist in this chain",
            ));
        };
        if device.filter_state().is_none() {
            return Err(invalid_domain(
                "native device kind does not match the requested edit",
            ));
        }
        device.enabled = enabled;
        device.state = NativeDeviceState::Filter(state);
        validate_device(device)?;
        self.try_with_devices(devices)
    }

    /// Returns a copy with one existing Native Compressor state replaced without
    /// changing chain order or device identity.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the instance identity is absent, belongs to a
    /// different native-device kind, or contains an invalid state.
    pub fn try_with_compressor_state(
        &self,
        device_instance_id: &str,
        enabled: bool,
        state: NativeCompressorState,
    ) -> Result<Self, DeviceChainError> {
        let mut devices = self.devices.clone();
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.id() == device_instance_id)
        else {
            return Err(invalid_domain(
                "native device instance does not exist in this chain",
            ));
        };
        if device.compressor_state().is_none() {
            return Err(invalid_domain(
                "native device kind does not match the requested edit",
            ));
        }
        device.enabled = enabled;
        device.state = NativeDeviceState::Compressor(state);
        validate_device(device)?;
        self.try_with_devices(devices)
    }

    /// Returns a copy with one existing device moved to a new processing index.
    ///
    /// The device instance and every other device remain unchanged; only the
    /// explicit processing order changes.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the device identity is absent, the target
    /// index is outside this chain, or the requested position is unchanged.
    pub fn try_with_moved_device(
        &self,
        device_instance_id: &str,
        destination_index: usize,
    ) -> Result<Self, DeviceChainError> {
        let Some(source_index) = self
            .devices
            .iter()
            .position(|device| device.id() == device_instance_id)
        else {
            return Err(invalid_domain(
                "native device instance does not exist in this chain",
            ));
        };
        if destination_index >= self.devices.len() {
            return Err(invalid_domain(
                "native device destination is outside this chain",
            ));
        }
        if source_index == destination_index {
            return Err(invalid_domain(
                "native device is already at the requested processing position",
            ));
        }
        let mut devices = self.devices.clone();
        let device = devices.remove(source_index);
        devices.insert(destination_index, device);
        self.try_with_devices(devices)
    }

    /// Returns a copy with one existing device duplicated immediately after
    /// the source device.
    ///
    /// The new instance receives the supplied identity while preserving the
    /// source kind, version, state, enabled flag, and extension data.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the source is absent, the new identity is
    /// invalid or already present, or the resulting chain exceeds its bound.
    pub fn try_with_duplicated_device(
        &self,
        source_device_instance_id: &str,
        duplicate_device_instance_id: String,
    ) -> Result<Self, DeviceChainError> {
        let Some(source_index) = self
            .devices
            .iter()
            .position(|device| device.id() == source_device_instance_id)
        else {
            return Err(invalid_domain(
                "native device instance does not exist in this chain",
            ));
        };
        let duplicate = self.devices[source_index].try_with_id(duplicate_device_instance_id)?;
        let mut devices = self.devices.clone();
        devices.insert(source_index + 1, duplicate);
        self.try_with_devices(devices)
    }

    /// Returns a copy with a validated device from another chain inserted
    /// immediately after one existing device in this chain.
    ///
    /// The inserted device receives the supplied identity while preserving the
    /// copied device's supported kind, version, state, enabled flag, and
    /// extension data.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the target is absent, the new identity is
    /// invalid or already present, or the resulting chain exceeds its bound.
    pub fn try_with_copied_device_after(
        &self,
        source: &NativeDeviceInstance,
        target_device_instance_id: &str,
        copied_device_instance_id: String,
    ) -> Result<Self, DeviceChainError> {
        let Some(target_index) = self
            .devices
            .iter()
            .position(|device| device.id() == target_device_instance_id)
        else {
            return Err(invalid_domain(
                "native device insertion target does not exist in this chain",
            ));
        };
        let copied = source.try_with_id(copied_device_instance_id)?;
        let mut devices = self.devices.clone();
        devices.insert(target_index + 1, copied);
        self.try_with_devices(devices)
    }

    /// Returns a copy without one device, or `None` when removing it empties
    /// the chain and therefore requires deleting the chain resource.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the device identity is absent.
    pub fn try_without_device(
        &self,
        device_instance_id: &str,
    ) -> Result<Option<Self>, DeviceChainError> {
        let Some(index) = self
            .devices
            .iter()
            .position(|device| device.id() == device_instance_id)
        else {
            return Err(DeviceChainError {
                kind: DeviceChainErrorKind::InvalidDomain,
                message: "native device instance does not exist in this chain".to_owned(),
            });
        };
        if self.devices.len() == 1 {
            return Ok(None);
        }
        let mut devices = self.devices.clone();
        devices.remove(index);
        Ok(Some(self.try_with_devices(devices)?))
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }

    fn try_with_devices(
        &self,
        devices: Vec<NativeDeviceInstance>,
    ) -> Result<Self, DeviceChainError> {
        let format_version = match self.format_version {
            LEGACY_FORMAT_VERSION
                if devices
                    .iter()
                    .all(|device| matches!(device.state(), NativeDeviceState::Filter(_))) =>
            {
                LEGACY_FORMAT_VERSION
            }
            _ => CURRENT_FORMAT_VERSION,
        };
        let chain = Self {
            format: self.format.clone(),
            format_version,
            id: self.id.clone(),
            project_id: self.project_id.clone(),
            track_id: self.track_id.clone(),
            devices,
            extensions: self.extensions.clone(),
        };
        validate_chain(&chain)?;
        Ok(chain)
    }
}

fn validate_chain(chain: &DeviceChain) -> Result<(), DeviceChainError> {
    require(
        chain.format == FORMAT
            && matches!(
                chain.format_version,
                LEGACY_FORMAT_VERSION | CURRENT_FORMAT_VERSION
            ),
        "device chain format is unsupported",
    )?;
    require(
        is_valid_entity_id(&chain.id)
            && is_valid_entity_id(&chain.project_id)
            && is_valid_entity_id(&chain.track_id),
        "device chain identity or ownership is invalid",
    )?;
    require(
        (1..=MAX_DEVICES_PER_CHAIN).contains(&chain.devices.len()),
        "device chain count is outside the declared bound",
    )?;
    require(
        chain
            .extensions
            .keys()
            .all(|key| is_valid_extension_key(key)),
        "device chain contains an invalid extension key",
    )?;
    let mut identities = BTreeSet::new();
    for device in &chain.devices {
        validate_device(device)?;
        if chain.format_version == LEGACY_FORMAT_VERSION {
            require(
                matches!(device.state(), NativeDeviceState::Filter(_)),
                "version-1 device chains may contain only native filters",
            )?;
        }
        require(
            identities.insert(device.id()),
            "device chain contains a duplicate device identity",
        )?;
    }
    Ok(())
}

fn validate_device(device: &NativeDeviceInstance) -> Result<(), DeviceChainError> {
    require(
        is_valid_entity_id(&device.id),
        "native device identity is invalid",
    )?;
    match device.state() {
        NativeDeviceState::Filter(state) => {
            require(
                device.device_id == NATIVE_FILTER_DEVICE_ID
                    && device.state_version == NATIVE_FILTER_STATE_VERSION
                    && device.dsp_version == NATIVE_FILTER_DSP_VERSION,
                "native filter kind or version is unsupported",
            )?;
            validate_filter_state(*state)?;
        }
        NativeDeviceState::Compressor(state) => {
            require(
                device.device_id == NATIVE_COMPRESSOR_DEVICE_ID
                    && device.state_version == NATIVE_COMPRESSOR_STATE_VERSION
                    && device.dsp_version == NATIVE_COMPRESSOR_DSP_VERSION,
                "native compressor kind or version is unsupported",
            )?;
            validate_compressor_state(*state)?;
        }
    }
    require(
        device
            .extensions
            .keys()
            .all(|key| is_valid_extension_key(key)),
        "native device contains an invalid extension key",
    )?;
    Ok(())
}

fn validate_filter_state(state: NativeFilterState) -> Result<(), DeviceChainError> {
    require(
        (MIN_FILTER_CUTOFF_MILLIHERTZ..=MAX_FILTER_CUTOFF_MILLIHERTZ)
            .contains(&state.cutoff_millihertz),
        "native filter cutoff is outside 20..=20000 Hz",
    )?;
    require(
        (MIN_FILTER_RESONANCE_MILLIONTHS..=MAX_FILTER_RESONANCE_MILLIONTHS)
            .contains(&state.resonance_millionths),
        "native filter resonance is outside 0.1..=20 Q",
    )
}

fn validate_compressor_state(state: NativeCompressorState) -> Result<(), DeviceChainError> {
    require(
        (MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS..=MAX_COMPRESSOR_THRESHOLD_MILLIDECIBELS)
            .contains(&state.threshold_millidecibels),
        "native compressor threshold is outside -80..=0 dB",
    )?;
    require(
        (MIN_COMPRESSOR_RATIO_MILLIONTHS..=MAX_COMPRESSOR_RATIO_MILLIONTHS)
            .contains(&state.ratio_millionths),
        "native compressor ratio is outside 1..=100",
    )?;
    require(
        (MIN_COMPRESSOR_ATTACK_MICROSECONDS..=MAX_COMPRESSOR_ATTACK_MICROSECONDS)
            .contains(&state.attack_microseconds),
        "native compressor attack is outside 0.1..=500 ms",
    )?;
    require(
        (MIN_COMPRESSOR_RELEASE_MICROSECONDS..=MAX_COMPRESSOR_RELEASE_MICROSECONDS)
            .contains(&state.release_microseconds),
        "native compressor release is outside 0.1..=2000 ms",
    )?;
    require(
        (MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS..=MAX_COMPRESSOR_MAKEUP_MILLIDECIBELS)
            .contains(&state.makeup_millidecibels),
        "native compressor makeup is outside -24..=24 dB",
    )
}

fn require(condition: bool, message: &'static str) -> Result<(), DeviceChainError> {
    if condition {
        Ok(())
    } else {
        Err(DeviceChainError {
            kind: DeviceChainErrorKind::InvalidDomain,
            message: message.to_owned(),
        })
    }
}

fn invalid_domain(message: &'static str) -> DeviceChainError {
    DeviceChainError {
        kind: DeviceChainErrorKind::InvalidDomain,
        message: message.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct BiquadState {
    z1: f64,
    z2: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BiquadCoefficients {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

const PARAMETER_TRANSITION_FRAMES: u16 = 128;

#[derive(Clone, Copy, Debug)]
struct FilterTransition {
    coefficients: BiquadCoefficients,
    channels: [BiquadState; 2],
    frames_remaining: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceProcessError {
    ChannelLengthMismatch { left: usize, right: usize },
    BlockTooLarge { actual: usize, maximum: usize },
    NonFiniteInput,
    NonFiniteOutput,
}

impl fmt::Display for NativeDeviceProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelLengthMismatch { left, right } => {
                write!(
                    formatter,
                    "native device channel lengths differ: left={left}, right={right}"
                )
            }
            Self::BlockTooLarge { actual, maximum } => write!(
                formatter,
                "native device block has {actual} frames; maximum is {maximum}"
            ),
            Self::NonFiniteInput => formatter.write_str("native device input is non-finite"),
            Self::NonFiniteOutput => formatter.write_str("native device output became non-finite"),
        }
    }
}

impl Error for NativeDeviceProcessError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeDeviceCreationError {
    UnsupportedSampleRate { actual: u32, required: u32 },
    CapacityUnavailable,
}

impl fmt::Display for NativeDeviceCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSampleRate { actual, required } => write!(
                formatter,
                "native device sample rate is {actual} Hz; required rate is {required} Hz"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("native device runtime capacity is unavailable")
            }
        }
    }
}

impl Error for NativeDeviceCreationError {}

#[derive(Debug)]
pub struct NativeFilterProcessor {
    enabled: bool,
    state: NativeFilterState,
    coefficients: BiquadCoefficients,
    channels: [BiquadState; 2],
    transition: Option<FilterTransition>,
}

impl NativeFilterProcessor {
    /// Compiles one filter into fixed callback state.
    ///
    /// # Errors
    ///
    /// Returns an error unless the configured sample rate is the project rate.
    pub fn try_new(
        state: NativeFilterState,
        enabled: bool,
        sample_rate_hz: u32,
    ) -> Result<Self, NativeDeviceCreationError> {
        if sample_rate_hz != PROJECT_SAMPLE_RATE_HZ {
            return Err(NativeDeviceCreationError::UnsupportedSampleRate {
                actual: sample_rate_hz,
                required: PROJECT_SAMPLE_RATE_HZ,
            });
        }
        Ok(Self {
            enabled,
            state,
            coefficients: coefficients(state, sample_rate_hz),
            channels: [BiquadState::default(); 2],
            transition: None,
        })
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn state(&self) -> NativeFilterState {
        self.state
    }

    /// Changes bypass state without allocating.
    ///
    /// A state transition clears filter history so samples from an earlier
    /// enabled period cannot reappear when the processor is enabled again.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.reset();
        }
    }

    /// Retunes one prepared filter without allocation or processor recreation.
    ///
    /// Enabled processors preserve their current biquad histories and crossfade
    /// the old and new stable coefficient paths for a fixed number of frames.
    /// This prevents a direct coefficient discontinuity from becoming an
    /// audible control step. Disabled processors update their next enabled
    /// state directly.
    pub fn set_state(&mut self, state: NativeFilterState) {
        if self.state == state {
            return;
        }
        if self.enabled {
            self.transition = Some(FilterTransition {
                coefficients: self.coefficients,
                channels: self.channels,
                frames_remaining: PARAMETER_TRANSITION_FRAMES,
            });
        }
        self.state = state;
        self.coefficients = coefficients(state, PROJECT_SAMPLE_RATE_HZ);
    }

    pub fn reset(&mut self) {
        self.channels = [BiquadState::default(); 2];
        self.transition = None;
    }

    /// Processes one planar stereo block in place without allocation.
    ///
    /// # Errors
    ///
    /// Returns before processing for an invalid shape or non-finite input.
    /// A non-finite calculated output resets filter history before returning.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the finite f64 processor result is range-checked after conversion to the f32 audio domain"
    )]
    pub fn process_in_place(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), NativeDeviceProcessError> {
        validate_block(left, right)?;
        if !self.enabled {
            return Ok(());
        }
        let mut states = self.channels;
        let mut transition = self.transition;
        for (left, right) in left.iter_mut().zip(right) {
            let left_input = f64::from(*left);
            let right_input = f64::from(*right);
            let (next_left, next_right) = if let Some(active) = &mut transition {
                let old_left =
                    process_biquad_sample(active.coefficients, &mut active.channels[0], left_input);
                let old_right = process_biquad_sample(
                    active.coefficients,
                    &mut active.channels[1],
                    right_input,
                );
                let new_left = process_biquad_sample(self.coefficients, &mut states[0], left_input);
                let new_right =
                    process_biquad_sample(self.coefficients, &mut states[1], right_input);
                let progress = f64::from(PARAMETER_TRANSITION_FRAMES - active.frames_remaining + 1)
                    / f64::from(PARAMETER_TRANSITION_FRAMES);
                active.frames_remaining -= 1;
                if active.frames_remaining == 0 {
                    transition = None;
                }
                (
                    old_left.mul_add(1.0 - progress, new_left * progress),
                    old_right.mul_add(1.0 - progress, new_right * progress),
                )
            } else {
                (
                    process_biquad_sample(self.coefficients, &mut states[0], left_input),
                    process_biquad_sample(self.coefficients, &mut states[1], right_input),
                )
            };
            let next_left = next_left as f32;
            let next_right = next_right as f32;
            if !next_left.is_finite()
                || !next_right.is_finite()
                || !states
                    .iter()
                    .all(|state| state.z1.is_finite() && state.z2.is_finite())
                || transition.is_some_and(|active| {
                    active
                        .channels
                        .iter()
                        .any(|state| !state.z1.is_finite() || !state.z2.is_finite())
                })
            {
                self.reset();
                return Err(NativeDeviceProcessError::NonFiniteOutput);
            }
            *left = next_left;
            *right = next_right;
        }
        self.channels = states;
        self.transition = transition;
        Ok(())
    }
}

fn process_biquad_sample(
    coefficients: BiquadCoefficients,
    state: &mut BiquadState,
    input: f64,
) -> f64 {
    let output = coefficients.b0.mul_add(input, state.z1);
    state.z1 = coefficients
        .b1
        .mul_add(input, (-coefficients.a1).mul_add(output, state.z2));
    state.z2 = coefficients.b2.mul_add(input, -coefficients.a2 * output);
    output
}

fn validate_block(left: &[f32], right: &[f32]) -> Result<(), NativeDeviceProcessError> {
    if left.len() != right.len() {
        return Err(NativeDeviceProcessError::ChannelLengthMismatch {
            left: left.len(),
            right: right.len(),
        });
    }
    if left.len() > MAX_PROCESS_FRAMES {
        return Err(NativeDeviceProcessError::BlockTooLarge {
            actual: left.len(),
            maximum: MAX_PROCESS_FRAMES,
        });
    }
    if left.iter().chain(right).any(|sample| !sample.is_finite()) {
        return Err(NativeDeviceProcessError::NonFiniteInput);
    }
    Ok(())
}

fn coefficients(state: NativeFilterState, sample_rate_hz: u32) -> BiquadCoefficients {
    let cutoff_hz = f64::from(state.cutoff_millihertz) / 1_000.0;
    let resonance = f64::from(state.resonance_millionths) / 1_000_000.0;
    let omega = 2.0 * PI * cutoff_hz / f64::from(sample_rate_hz);
    let sine = omega.sin();
    let cosine = omega.cos();
    let alpha = sine / (2.0 * resonance);
    let a0 = 1.0 + alpha;
    let (b0, b1, b2) = match state.mode {
        NativeFilterMode::LowPass => {
            let half = f64::midpoint(1.0, -cosine);
            (half, 1.0 - cosine, half)
        }
        NativeFilterMode::HighPass => {
            let half = f64::midpoint(1.0, cosine);
            (half, -(1.0 + cosine), half)
        }
    };
    BiquadCoefficients {
        b0: b0 / a0,
        b1: b1 / a0,
        b2: b2 / a0,
        a1: (-2.0 * cosine) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Zero-latency stereo-linked peak compressor runtime state.
#[derive(Debug)]
pub struct NativeCompressorProcessor {
    enabled: bool,
    state: NativeCompressorState,
    threshold_decibels: f64,
    ratio: f64,
    attack_coefficient: f64,
    release_coefficient: f64,
    makeup_gain: f64,
    gain_reduction_decibels: f64,
    transition: Option<CompressorTransition>,
}

#[derive(Clone, Copy, Debug)]
struct CompressorTransition {
    threshold_decibels: f64,
    ratio: f64,
    attack_coefficient: f64,
    release_coefficient: f64,
    makeup_gain: f64,
    gain_reduction_decibels: f64,
    frames_remaining: u16,
}

impl NativeCompressorProcessor {
    /// Compiles one compressor into fixed callback state.
    ///
    /// # Errors
    ///
    /// Returns an error unless the configured sample rate is the project rate.
    pub fn try_new(
        state: NativeCompressorState,
        enabled: bool,
        sample_rate_hz: u32,
    ) -> Result<Self, NativeDeviceCreationError> {
        if sample_rate_hz != PROJECT_SAMPLE_RATE_HZ {
            return Err(NativeDeviceCreationError::UnsupportedSampleRate {
                actual: sample_rate_hz,
                required: PROJECT_SAMPLE_RATE_HZ,
            });
        }
        let rate = f64::from(sample_rate_hz);
        let attack_seconds = f64::from(state.attack_microseconds) / 1_000_000.0;
        let release_seconds = f64::from(state.release_microseconds) / 1_000_000.0;
        let makeup_decibels = f64::from(state.makeup_millidecibels) / 1_000.0;
        Ok(Self {
            enabled,
            state,
            threshold_decibels: f64::from(state.threshold_millidecibels) / 1_000.0,
            ratio: f64::from(state.ratio_millionths) / 1_000_000.0,
            attack_coefficient: (-1.0 / (attack_seconds * rate)).exp(),
            release_coefficient: (-1.0 / (release_seconds * rate)).exp(),
            makeup_gain: 10.0_f64.powf(makeup_decibels / 20.0),
            gain_reduction_decibels: 0.0,
            transition: None,
        })
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn state(&self) -> NativeCompressorState {
        self.state
    }

    /// Changes bypass state without allocating.
    ///
    /// A state transition clears detector history so an old gain reduction
    /// cannot affect a later enabled period.
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.enabled = enabled;
            self.reset();
        }
    }

    /// Updates one prepared compressor without allocating or rebuilding it.
    ///
    /// Detector history remains authoritative. The old and new complete
    /// compressor paths crossfade for a fixed number of frames, so every
    /// exposed control changes without an instantaneous output step.
    pub fn set_state(&mut self, state: NativeCompressorState) {
        if self.state == state {
            return;
        }
        let rate = f64::from(PROJECT_SAMPLE_RATE_HZ);
        let attack_seconds = f64::from(state.attack_microseconds()) / 1_000_000.0;
        let release_seconds = f64::from(state.release_microseconds()) / 1_000_000.0;
        let makeup_gain = 10.0_f64.powf(f64::from(state.makeup_millidecibels()) / 1_000.0 / 20.0);
        if self.enabled {
            self.transition = Some(CompressorTransition {
                threshold_decibels: self.threshold_decibels,
                ratio: self.ratio,
                attack_coefficient: self.attack_coefficient,
                release_coefficient: self.release_coefficient,
                makeup_gain: self.makeup_gain,
                gain_reduction_decibels: self.gain_reduction_decibels,
                frames_remaining: PARAMETER_TRANSITION_FRAMES,
            });
        }
        self.state = state;
        self.threshold_decibels = f64::from(state.threshold_millidecibels()) / 1_000.0;
        self.ratio = f64::from(state.ratio_millionths()) / 1_000_000.0;
        self.attack_coefficient = (-1.0 / (attack_seconds * rate)).exp();
        self.release_coefficient = (-1.0 / (release_seconds * rate)).exp();
        self.makeup_gain = makeup_gain;
    }

    pub fn reset(&mut self) {
        self.gain_reduction_decibels = 0.0;
        self.transition = None;
    }

    /// Processes one planar stereo block in place without allocation.
    ///
    /// The detector is peak-linked across both channels; each sample pair gets
    /// exactly one shared gain, so the compressor cannot move the stereo image.
    ///
    /// # Errors
    ///
    /// Returns before processing for an invalid shape or non-finite input. A
    /// non-finite calculated output resets detector history before returning.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the finite f64 processor result is range-checked after conversion to the f32 audio domain"
    )]
    pub fn process_in_place(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), NativeDeviceProcessError> {
        validate_block(left, right)?;
        if !self.enabled {
            return Ok(());
        }
        let mut gain_reduction_decibels = self.gain_reduction_decibels;
        let mut transition = self.transition;
        for (left, right) in left.iter_mut().zip(right) {
            let left_input = f64::from(*left);
            let right_input = f64::from(*right);
            let (next_left, next_right) = if let Some(active) = &mut transition {
                let (old_left, old_right) = process_compressor_sample(
                    left_input,
                    right_input,
                    active.threshold_decibels,
                    active.ratio,
                    active.attack_coefficient,
                    active.release_coefficient,
                    active.makeup_gain,
                    &mut active.gain_reduction_decibels,
                );
                let (new_left, new_right) = process_compressor_sample(
                    left_input,
                    right_input,
                    self.threshold_decibels,
                    self.ratio,
                    self.attack_coefficient,
                    self.release_coefficient,
                    self.makeup_gain,
                    &mut gain_reduction_decibels,
                );
                let progress = f64::from(PARAMETER_TRANSITION_FRAMES - active.frames_remaining + 1)
                    / f64::from(PARAMETER_TRANSITION_FRAMES);
                active.frames_remaining -= 1;
                if active.frames_remaining == 0 {
                    transition = None;
                }
                (
                    old_left.mul_add(1.0 - progress, new_left * progress),
                    old_right.mul_add(1.0 - progress, new_right * progress),
                )
            } else {
                process_compressor_sample(
                    left_input,
                    right_input,
                    self.threshold_decibels,
                    self.ratio,
                    self.attack_coefficient,
                    self.release_coefficient,
                    self.makeup_gain,
                    &mut gain_reduction_decibels,
                )
            };
            let next_left = next_left as f32;
            let next_right = next_right as f32;
            if !next_left.is_finite()
                || !next_right.is_finite()
                || !gain_reduction_decibels.is_finite()
                || transition.is_some_and(|active| !active.gain_reduction_decibels.is_finite())
            {
                self.reset();
                return Err(NativeDeviceProcessError::NonFiniteOutput);
            }
            *left = next_left;
            *right = next_right;
        }
        self.gain_reduction_decibels = gain_reduction_decibels;
        self.transition = transition;
        Ok(())
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the callback path keeps both crossfade states explicit and allocation-free"
)]
fn process_compressor_sample(
    left: f64,
    right: f64,
    threshold_decibels: f64,
    ratio: f64,
    attack_coefficient: f64,
    release_coefficient: f64,
    makeup_gain: f64,
    gain_reduction_decibels: &mut f64,
) -> (f64, f64) {
    let peak = left.abs().max(right.abs());
    let level_decibels = 20.0 * peak.max(f64::MIN_POSITIVE).log10();
    let target_gain_reduction = if level_decibels > threshold_decibels {
        (level_decibels - threshold_decibels) * (1.0 - 1.0 / ratio)
    } else {
        0.0
    };
    let coefficient = if target_gain_reduction > *gain_reduction_decibels {
        attack_coefficient
    } else {
        release_coefficient
    };
    *gain_reduction_decibels = coefficient.mul_add(
        *gain_reduction_decibels,
        (1.0 - coefficient) * target_gain_reduction,
    );
    let gain = 10.0_f64.powf(-*gain_reduction_decibels / 20.0) * makeup_gain;
    (left * gain, right * gain)
}

#[derive(Debug)]
enum NativeDeviceProcessor {
    Filter(NativeFilterProcessor),
    Compressor(NativeCompressorProcessor),
}

impl NativeDeviceProcessor {
    const fn enabled(&self) -> bool {
        match self {
            Self::Filter(processor) => processor.enabled(),
            Self::Compressor(processor) => processor.enabled(),
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        match self {
            Self::Filter(processor) => processor.set_enabled(enabled),
            Self::Compressor(processor) => processor.set_enabled(enabled),
        }
    }

    const fn state(&self) -> NativeDeviceState {
        match self {
            Self::Filter(processor) => NativeDeviceState::Filter(processor.state()),
            Self::Compressor(processor) => NativeDeviceState::Compressor(processor.state()),
        }
    }

    fn set_state(&mut self, state: NativeDeviceState) -> bool {
        match (self, state) {
            (Self::Filter(processor), NativeDeviceState::Filter(state)) => {
                processor.set_state(state);
                true
            }
            (Self::Compressor(processor), NativeDeviceState::Compressor(state)) => {
                processor.set_state(state);
                true
            }
            (Self::Filter(_), NativeDeviceState::Compressor(_))
            | (Self::Compressor(_), NativeDeviceState::Filter(_)) => false,
        }
    }

    fn reset(&mut self) {
        match self {
            Self::Filter(processor) => processor.reset(),
            Self::Compressor(processor) => processor.reset(),
        }
    }

    fn process_in_place(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), NativeDeviceProcessError> {
        match self {
            Self::Filter(processor) => processor.process_in_place(left, right),
            Self::Compressor(processor) => processor.process_in_place(left, right),
        }
    }
}

#[derive(Debug)]
pub struct NativeDeviceChainProcessor {
    devices: Vec<NativeDeviceProcessor>,
}

impl NativeDeviceChainProcessor {
    /// Compiles one validated chain into ordered fixed processors.
    ///
    /// # Errors
    ///
    /// Returns a sample-rate or allocation error before callback ownership.
    pub fn try_new(
        chain: &DeviceChain,
        sample_rate_hz: u32,
    ) -> Result<Self, NativeDeviceCreationError> {
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(chain.devices.len())
            .map_err(|_| NativeDeviceCreationError::CapacityUnavailable)?;
        for device in &chain.devices {
            devices.push(match device.state() {
                NativeDeviceState::Filter(state) => NativeDeviceProcessor::Filter(
                    NativeFilterProcessor::try_new(*state, device.enabled, sample_rate_hz)?,
                ),
                NativeDeviceState::Compressor(state) => NativeDeviceProcessor::Compressor(
                    NativeCompressorProcessor::try_new(*state, device.enabled, sample_rate_hz)?,
                ),
            });
        }
        Ok(Self { devices })
    }

    #[must_use]
    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Returns whether one prepared device is enabled.
    #[must_use]
    pub fn device_enabled(&self, index: usize) -> Option<bool> {
        self.devices.get(index).map(NativeDeviceProcessor::enabled)
    }

    /// Returns one prepared device's current validated parameter state.
    #[must_use]
    pub fn device_state(&self, index: usize) -> Option<NativeDeviceState> {
        self.devices.get(index).map(NativeDeviceProcessor::state)
    }

    /// Changes one prepared device's bypass state without allocating.
    ///
    /// Returns `false` when `index` is outside this prepared chain.
    pub fn set_device_enabled(&mut self, index: usize, enabled: bool) -> bool {
        let Some(device) = self.devices.get_mut(index) else {
            return false;
        };
        device.set_enabled(enabled);
        true
    }

    /// Updates one prepared device's complete typed parameter state without
    /// allocating. Returns `false` for an invalid index or mismatched kind.
    pub fn set_device_state(&mut self, index: usize, state: NativeDeviceState) -> bool {
        self.devices
            .get_mut(index)
            .is_some_and(|device| device.set_state(state))
    }

    pub fn reset(&mut self) {
        for device in &mut self.devices {
            device.reset();
        }
    }

    /// Processes the complete ordered chain without allocation.
    ///
    /// # Errors
    ///
    /// Returns the first processor failure after resetting the entire chain.
    pub fn process_in_place(
        &mut self,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Result<(), NativeDeviceProcessError> {
        for device in &mut self.devices {
            if let Err(error) = device.process_in_place(left, right) {
                self.reset();
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334490";
    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const DEVICE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334491";

    fn state(mode: NativeFilterMode) -> NativeFilterState {
        NativeFilterState::try_new(mode, 1_000_000, 707_107).unwrap()
    }

    fn compressor_state() -> NativeCompressorState {
        NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 100_000, 0).unwrap()
    }

    fn chain(enabled: bool) -> DeviceChain {
        DeviceChain::try_new(
            CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![
                NativeDeviceInstance::try_new_filter(
                    DEVICE_ID.to_owned(),
                    enabled,
                    state(NativeFilterMode::LowPass),
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    fn compressor_chain(enabled: bool) -> DeviceChain {
        DeviceChain::try_new(
            CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![
                NativeDeviceInstance::try_new_compressor(
                    DEVICE_ID.to_owned(),
                    enabled,
                    compressor_state(),
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    #[test]
    fn canonical_chain_round_trips_with_stable_device_identity() {
        let chain = chain(true);
        let bytes = chain.to_canonical_vec().unwrap();
        assert_eq!(DeviceChain::parse(&bytes).unwrap(), chain);
        assert_eq!(chain.format_version(), LEGACY_FORMAT_VERSION);
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(wire["devices"][0]["state"].get("kind").is_none());
        assert_eq!(chain.devices()[0].device_id(), NATIVE_FILTER_DEVICE_ID);
        assert_eq!(chain.devices()[0].state_version(), 1);
        assert_eq!(chain.devices()[0].dsp_version(), 1);
        assert_eq!(
            [
                NATIVE_FILTER_CUTOFF_PARAMETER,
                NATIVE_FILTER_RESONANCE_PARAMETER,
                NATIVE_FILTER_MODE_PARAMETER,
                NATIVE_FILTER_ENABLED_PARAMETER,
            ],
            [
                "org.hyper-module.native-filter/cutoff",
                "org.hyper-module.native-filter/resonance",
                "org.hyper-module.native-filter/mode",
                "org.hyper-module.native-filter/enabled",
            ]
        );
    }

    #[test]
    fn compressor_chain_uses_explicit_v2_state_and_preserves_v1_filters_on_upgrade() {
        let compressor = compressor_chain(true);
        let bytes = compressor.to_canonical_vec().unwrap();
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(compressor.format_version(), CURRENT_FORMAT_VERSION);
        assert_eq!(wire["format_version"], CURRENT_FORMAT_VERSION);
        assert_eq!(wire["devices"][0]["state"]["kind"], "compressor");
        assert_eq!(DeviceChain::parse(&bytes).unwrap(), compressor);

        let upgraded = chain(true)
            .try_with_appended_compressor(
                "018f47a2-6b2d-7c11-8a44-001122338005".to_owned(),
                false,
                compressor_state(),
            )
            .unwrap();
        assert_eq!(upgraded.format_version(), CURRENT_FORMAT_VERSION);
        assert_eq!(
            upgraded.devices()[0].filter_state(),
            Some(state(NativeFilterMode::LowPass))
        );
        assert_eq!(
            upgraded.devices()[1].compressor_state(),
            Some(compressor_state())
        );
        let upgraded_wire: Value =
            serde_json::from_slice(&upgraded.to_canonical_vec().unwrap()).unwrap();
        assert_eq!(upgraded_wire["devices"][0]["state"]["kind"], "filter");
        assert_eq!(upgraded_wire["devices"][1]["state"]["kind"], "compressor");
    }

    #[test]
    fn chain_edits_preserve_order_identity_and_non_empty_ownership() {
        let original = chain(true);
        let second_id = "018f47a2-6b2d-7c11-8a44-001122338004";
        let two = original
            .try_with_appended_filter(
                second_id.to_owned(),
                false,
                state(NativeFilterMode::HighPass),
            )
            .unwrap();
        assert_eq!(two.devices().len(), 2);
        assert_eq!(two.devices()[0].id(), DEVICE_ID);
        assert_eq!(two.devices()[1].id(), second_id);

        let changed = two
            .try_with_filter_state(second_id, true, state(NativeFilterMode::LowPass))
            .unwrap();
        assert!(changed.devices()[1].enabled());
        assert_eq!(
            changed.devices()[1].filter_state().unwrap().mode(),
            NativeFilterMode::LowPass
        );
        let reduced = changed.try_without_device(DEVICE_ID).unwrap().unwrap();
        assert_eq!(reduced.devices()[0].id(), second_id);
        assert!(reduced.try_without_device(second_id).unwrap().is_none());
        assert!(reduced.try_without_device(DEVICE_ID).is_err());
    }

    #[test]
    fn moving_a_device_changes_only_processing_order() {
        let second_id = "018f47a2-6b2d-7c11-8a44-001122338004";
        let chain = chain(true)
            .try_with_appended_compressor(second_id.to_owned(), false, compressor_state())
            .unwrap();
        let moved = chain.try_with_moved_device(DEVICE_ID, 1).unwrap();
        assert_eq!(
            moved
                .devices()
                .iter()
                .map(NativeDeviceInstance::id)
                .collect::<Vec<_>>(),
            [second_id, DEVICE_ID]
        );
        assert_eq!(
            moved.devices()[0].compressor_state(),
            Some(compressor_state())
        );
        assert!(!moved.devices()[0].enabled());
        assert_eq!(
            moved.devices()[1].filter_state(),
            Some(state(NativeFilterMode::LowPass))
        );
        assert!(moved.devices()[1].enabled());
        assert_eq!(
            moved.to_canonical_vec().unwrap(),
            DeviceChain::parse(&moved.to_canonical_vec().unwrap())
                .unwrap()
                .to_canonical_vec()
                .unwrap()
        );
        let restored = moved.try_with_moved_device(DEVICE_ID, 0).unwrap();
        assert_eq!(
            restored.to_canonical_vec().unwrap(),
            chain.to_canonical_vec().unwrap()
        );
        assert!(chain.try_with_moved_device(second_id, 1).is_err());
        assert!(chain.try_with_moved_device("missing", 0).is_err());
        assert!(chain.try_with_moved_device(second_id, 2).is_err());
    }

    #[test]
    fn duplicating_a_device_preserves_state_and_requires_a_new_identity() {
        let source_id = "018f47a2-6b2d-7c11-8a44-001122338004";
        let duplicate_id = "018f47a2-6b2d-7c11-8a44-001122338005";
        let source_chain = chain(true)
            .try_with_appended_compressor(source_id.to_owned(), false, compressor_state())
            .unwrap();
        let mut source_wire: Value =
            serde_json::from_slice(&source_chain.to_canonical_vec().unwrap()).unwrap();
        source_wire["devices"][1]["extensions"] = serde_json::json!({
            "org.example.fixture": { "retained": true }
        });
        let source_chain = DeviceChain::parse(&serde_json::to_vec(&source_wire).unwrap()).unwrap();
        let source_bytes = source_chain.to_canonical_vec().unwrap();

        let duplicated = source_chain
            .try_with_duplicated_device(source_id, duplicate_id.to_owned())
            .unwrap();
        assert_eq!(
            duplicated
                .devices()
                .iter()
                .map(NativeDeviceInstance::id)
                .collect::<Vec<_>>(),
            [DEVICE_ID, source_id, duplicate_id]
        );
        assert_eq!(
            duplicated.devices()[1].device_id(),
            duplicated.devices()[2].device_id()
        );
        assert_eq!(
            duplicated.devices()[1].state(),
            duplicated.devices()[2].state()
        );
        assert_eq!(
            duplicated.devices()[1].enabled(),
            duplicated.devices()[2].enabled()
        );
        assert_eq!(
            duplicated.devices()[1].extensions(),
            duplicated.devices()[2].extensions()
        );
        assert_eq!(
            duplicated.devices()[2].extensions()["org.example.fixture"],
            serde_json::json!({ "retained": true })
        );
        assert_eq!(duplicated.format_version(), CURRENT_FORMAT_VERSION);
        assert_eq!(
            DeviceChain::parse(&duplicated.to_canonical_vec().unwrap()).unwrap(),
            duplicated
        );
        assert!(
            source_chain
                .try_with_duplicated_device(source_id, source_id.to_owned())
                .is_err()
        );
        assert_eq!(source_chain.to_canonical_vec().unwrap(), source_bytes);
        assert_eq!(
            source_chain
                .try_with_duplicated_device(source_id, "not a device identity".to_owned())
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
        assert_eq!(source_chain.to_canonical_vec().unwrap(), source_bytes);
        assert!(
            source_chain
                .try_with_duplicated_device("missing", duplicate_id.to_owned())
                .is_err()
        );

        let mut full = chain(true);
        for id in [
            "018f47a2-6b2d-7c11-8a44-001122338006",
            "018f47a2-6b2d-7c11-8a44-001122338007",
            "018f47a2-6b2d-7c11-8a44-001122338008",
            "018f47a2-6b2d-7c11-8a44-001122338009",
            "018f47a2-6b2d-7c11-8a44-001122338010",
            "018f47a2-6b2d-7c11-8a44-001122338011",
            "018f47a2-6b2d-7c11-8a44-001122338012",
        ] {
            full = full
                .try_with_appended_filter(id.to_owned(), true, state(NativeFilterMode::LowPass))
                .unwrap();
        }
        assert_eq!(full.devices().len(), MAX_DEVICES_PER_CHAIN);
        assert!(
            full.try_with_duplicated_device(
                DEVICE_ID,
                "018f47a2-6b2d-7c11-8a44-001122338013".to_owned(),
            )
            .is_err()
        );
    }

    #[test]
    fn cloning_a_chain_for_another_track_preserves_processing_state() {
        let source = chain(true)
            .try_with_appended_compressor(
                "018f47a2-6b2d-7c11-8a44-001122338040".to_owned(),
                false,
                compressor_state(),
            )
            .unwrap();
        let source_bytes = source.to_canonical_vec().unwrap();
        let cloned = source
            .try_clone_for_track(
                "018f47a2-6b2d-7c11-8a44-001122338041".to_owned(),
                "018f47a2-6b2d-7c11-8a44-001122338042".to_owned(),
                &[
                    "018f47a2-6b2d-7c11-8a44-001122338043".to_owned(),
                    "018f47a2-6b2d-7c11-8a44-001122338044".to_owned(),
                ],
            )
            .unwrap();

        assert_eq!(cloned.id(), "018f47a2-6b2d-7c11-8a44-001122338041");
        assert_eq!(cloned.track_id(), "018f47a2-6b2d-7c11-8a44-001122338042");
        assert_eq!(
            cloned
                .devices()
                .iter()
                .map(NativeDeviceInstance::id)
                .collect::<Vec<_>>(),
            [
                "018f47a2-6b2d-7c11-8a44-001122338043",
                "018f47a2-6b2d-7c11-8a44-001122338044",
            ]
        );
        assert_eq!(cloned.format_version(), source.format_version());
        assert_eq!(cloned.devices()[1].state(), source.devices()[1].state());
        assert_eq!(
            DeviceChain::parse(&cloned.to_canonical_vec().unwrap()).unwrap(),
            cloned
        );
        assert_eq!(source.to_canonical_vec().unwrap(), source_bytes);
        assert_eq!(
            source
                .try_clone_for_track(
                    "018f47a2-6b2d-7c11-8a44-001122338041".to_owned(),
                    "018f47a2-6b2d-7c11-8a44-001122338042".to_owned(),
                    &["018f47a2-6b2d-7c11-8a44-001122338043".to_owned()],
                )
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );

        let mut extended_wire: Value = serde_json::from_slice(&source_bytes).unwrap();
        extended_wire["extensions"] = serde_json::json!({"org.example.chain": {"retained": true}});
        let extended =
            DeviceChain::parse(&serde_json_canonicalizer::to_vec(&extended_wire).unwrap()).unwrap();
        assert_eq!(
            extended
                .try_clone_for_track(
                    "018f47a2-6b2d-7c11-8a44-001122338041".to_owned(),
                    "018f47a2-6b2d-7c11-8a44-001122338042".to_owned(),
                    &[
                        "018f47a2-6b2d-7c11-8a44-001122338043".to_owned(),
                        "018f47a2-6b2d-7c11-8a44-001122338044".to_owned(),
                    ],
                )
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );

        let mut extended_device_wire: Value = serde_json::from_slice(&source_bytes).unwrap();
        extended_device_wire["devices"][0]["extensions"] =
            serde_json::json!({"org.example.device": {"retained": true}});
        let extended_device =
            DeviceChain::parse(&serde_json_canonicalizer::to_vec(&extended_device_wire).unwrap())
                .unwrap();
        assert_eq!(
            extended_device
                .try_clone_for_track(
                    "018f47a2-6b2d-7c11-8a44-001122338041".to_owned(),
                    "018f47a2-6b2d-7c11-8a44-001122338042".to_owned(),
                    &[
                        "018f47a2-6b2d-7c11-8a44-001122338043".to_owned(),
                        "018f47a2-6b2d-7c11-8a44-001122338044".to_owned(),
                    ],
                )
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
    }

    #[test]
    fn copying_a_device_after_an_existing_target_preserves_the_source_state() {
        let source_id = "018f47a2-6b2d-7c11-8a44-001122338014";
        let copied_id = "018f47a2-6b2d-7c11-8a44-001122338015";
        let source_chain = chain(true)
            .try_with_appended_compressor(source_id.to_owned(), false, compressor_state())
            .unwrap();
        let mut source_wire: Value =
            serde_json::from_slice(&source_chain.to_canonical_vec().unwrap()).unwrap();
        source_wire["devices"][1]["extensions"] = serde_json::json!({
            "org.example.fixture": { "copied": true }
        });
        let source_chain = DeviceChain::parse(&serde_json::to_vec(&source_wire).unwrap()).unwrap();
        let source = &source_chain.devices()[1];

        let target = DeviceChain::try_new(
            "018f47a2-6b2d-7c11-8a44-001122338016".to_owned(),
            PROJECT_ID.to_owned(),
            "018f47a2-6b2d-7c11-8a44-001122338017".to_owned(),
            vec![
                NativeDeviceInstance::try_new_filter(
                    "018f47a2-6b2d-7c11-8a44-001122338018".to_owned(),
                    true,
                    state(NativeFilterMode::HighPass),
                )
                .unwrap(),
            ],
        )
        .unwrap();
        let target_bytes = target.to_canonical_vec().unwrap();
        let copied = target
            .try_with_copied_device_after(source, target.devices()[0].id(), copied_id.to_owned())
            .unwrap();
        assert_eq!(
            copied
                .devices()
                .iter()
                .map(NativeDeviceInstance::id)
                .collect::<Vec<_>>(),
            [target.devices()[0].id(), copied_id]
        );
        assert_eq!(copied.devices()[1].device_id(), source.device_id());
        assert_eq!(copied.devices()[1].state(), source.state());
        assert_eq!(copied.devices()[1].enabled(), source.enabled());
        assert_eq!(copied.devices()[1].extensions(), source.extensions());
        assert_eq!(
            DeviceChain::parse(&copied.to_canonical_vec().unwrap()).unwrap(),
            copied
        );
        assert_eq!(
            target
                .try_with_copied_device_after(source, "missing", copied_id.to_owned())
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
        assert_eq!(target.to_canonical_vec().unwrap(), target_bytes);

        let full_target = (0..7).fold(target, |chain, ordinal| {
            chain
                .try_with_appended_filter(
                    format!("018f47a2-6b2d-7c11-8a44-{ordinal:012x}"),
                    true,
                    state(NativeFilterMode::LowPass),
                )
                .unwrap()
        });
        let full_target_bytes = full_target.to_canonical_vec().unwrap();
        assert_eq!(full_target.devices().len(), MAX_DEVICES_PER_CHAIN);
        assert_eq!(
            full_target
                .try_with_copied_device_after(
                    source,
                    full_target.devices()[0].id(),
                    "018f47a2-6b2d-7c11-8a44-001122338030".to_owned(),
                )
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
        assert_eq!(full_target.to_canonical_vec().unwrap(), full_target_bytes);
    }

    #[test]
    fn state_replacement_preserves_chain_and_device_extensions() {
        let compressor_id = "018f47a2-6b2d-7c11-8a44-001122338019";
        let source = chain(true)
            .try_with_appended_compressor(compressor_id.to_owned(), true, compressor_state())
            .unwrap();
        let mut wire: Value = serde_json::from_slice(&source.to_canonical_vec().unwrap()).unwrap();
        wire["extensions"] = serde_json::json!({"org.example.chain": {"retained": true}});
        wire["devices"][0]["extensions"] =
            serde_json::json!({"org.example.filter": {"retained": true}});
        wire["devices"][1]["extensions"] =
            serde_json::json!({"org.example.compressor": {"retained": true}});
        let source = DeviceChain::parse(&serde_json_canonicalizer::to_vec(&wire).unwrap()).unwrap();

        let retuned_filter = source
            .try_with_filter_state(DEVICE_ID, false, state(NativeFilterMode::HighPass))
            .unwrap();
        assert_eq!(retuned_filter.extensions(), source.extensions());
        assert_eq!(
            retuned_filter.devices()[0].extensions(),
            source.devices()[0].extensions()
        );
        assert_eq!(retuned_filter.devices()[1], source.devices()[1]);
        assert!(!retuned_filter.devices()[0].enabled());
        assert_eq!(
            retuned_filter.devices()[0].filter_state().unwrap().mode(),
            NativeFilterMode::HighPass
        );
        assert_eq!(
            DeviceChain::parse(&retuned_filter.to_canonical_vec().unwrap()).unwrap(),
            retuned_filter
        );

        let retuned_compressor = source
            .try_with_compressor_state(compressor_id, false, compressor_state())
            .unwrap();
        assert_eq!(retuned_compressor.extensions(), source.extensions());
        assert_eq!(retuned_compressor.devices()[0], source.devices()[0]);
        assert_eq!(
            retuned_compressor.devices()[1].extensions(),
            source.devices()[1].extensions()
        );
        assert!(!retuned_compressor.devices()[1].enabled());
        assert_eq!(
            DeviceChain::parse(&retuned_compressor.to_canonical_vec().unwrap()).unwrap(),
            retuned_compressor
        );
    }

    #[test]
    fn chain_edits_reject_cross_kind_replacements_without_changing_canonical_state() {
        let filter = chain(true);
        let compressor = compressor_chain(true);
        let filter_bytes = filter.to_canonical_vec().unwrap();
        let compressor_bytes = compressor.to_canonical_vec().unwrap();

        assert_eq!(
            filter
                .try_with_compressor_state(DEVICE_ID, true, compressor_state())
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
        assert_eq!(filter.to_canonical_vec().unwrap(), filter_bytes);
        assert_eq!(
            compressor
                .try_with_filter_state(DEVICE_ID, true, state(NativeFilterMode::LowPass))
                .unwrap_err()
                .kind(),
            DeviceChainErrorKind::InvalidDomain
        );
        assert_eq!(compressor.to_canonical_vec().unwrap(), compressor_bytes);
    }

    #[test]
    fn parser_rejects_unknown_device_versions_domains_and_duplicate_instances() {
        let canonical = chain(true).to_canonical_vec().unwrap();
        let base: Value = serde_json::from_slice(&canonical).unwrap();
        let mut cases = Vec::new();
        for (pointer, value) in [
            ("/format_version", serde_json::json!(3)),
            ("/devices/0/device_id", serde_json::json!("unknown.device")),
            ("/devices/0/state_version", serde_json::json!(2)),
            ("/devices/0/dsp_version", serde_json::json!(2)),
            (
                "/devices/0/state/cutoff_millihertz",
                serde_json::json!(19_999),
            ),
            (
                "/devices/0/state/resonance_millionths",
                serde_json::json!(20_000_001),
            ),
        ] {
            let mut changed = base.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            cases.push(changed);
        }
        let mut duplicate = base.clone();
        let repeated = duplicate["devices"][0].clone();
        duplicate["devices"].as_array_mut().unwrap().push(repeated);
        cases.push(duplicate);

        for value in cases {
            let bytes = serde_json_canonicalizer::to_vec(&value).unwrap();
            assert_eq!(
                DeviceChain::parse(&bytes).unwrap_err().kind(),
                DeviceChainErrorKind::InvalidDomain
            );
        }
    }

    #[test]
    fn v2_parser_rejects_cross_kind_versions_and_compressor_domains() {
        let canonical = compressor_chain(true).to_canonical_vec().unwrap();
        let base: Value = serde_json::from_slice(&canonical).unwrap();
        let mut cases = Vec::new();
        for (pointer, value) in [
            (
                "/devices/0/device_id",
                serde_json::json!(NATIVE_FILTER_DEVICE_ID),
            ),
            ("/devices/0/state_version", serde_json::json!(2)),
            (
                "/devices/0/state/parameters/threshold_millidecibels",
                serde_json::json!(-80_001),
            ),
            (
                "/devices/0/state/parameters/ratio_millionths",
                serde_json::json!(100_000_001),
            ),
            (
                "/devices/0/state/parameters/attack_microseconds",
                serde_json::json!(99),
            ),
            (
                "/devices/0/state/parameters/release_microseconds",
                serde_json::json!(2_000_001),
            ),
            (
                "/devices/0/state/parameters/makeup_millidecibels",
                serde_json::json!(24_001),
            ),
        ] {
            let mut changed = base.clone();
            *changed.pointer_mut(pointer).unwrap() = value;
            cases.push(changed);
        }
        for value in cases {
            let bytes = serde_json_canonicalizer::to_vec(&value).unwrap();
            assert_eq!(
                DeviceChain::parse(&bytes).unwrap_err().kind(),
                DeviceChainErrorKind::InvalidDomain
            );
        }

        let mut cross_kind = base.clone();
        cross_kind["devices"][0]["state"] = serde_json::json!({
            "kind": "filter",
            "parameters": {
                "mode": "low-pass",
                "cutoff_millihertz": 1_000_000,
                "resonance_millionths": 707_107
            }
        });
        let cross_kind_bytes = serde_json_canonicalizer::to_vec(&cross_kind).unwrap();
        assert_eq!(
            DeviceChain::parse(&cross_kind_bytes).unwrap_err().kind(),
            DeviceChainErrorKind::InvalidDomain
        );

        let mut legacy =
            serde_json::from_slice::<Value>(&chain(true).to_canonical_vec().unwrap()).unwrap();
        legacy["devices"][0]["state"] = base["devices"][0]["state"].clone();
        let legacy_bytes = serde_json_canonicalizer::to_vec(&legacy).unwrap();
        assert!(matches!(
            DeviceChain::parse(&legacy_bytes).unwrap_err().kind(),
            DeviceChainErrorKind::Json(StrictJsonErrorKind::TypedDocumentMismatch)
        ));
    }

    #[test]
    fn bypass_is_bit_exact_and_shape_failures_do_not_touch_audio() {
        let mut processor = NativeFilterProcessor::try_new(
            state(NativeFilterMode::LowPass),
            false,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut left = [0.25_f32, -0.5, 0.75];
        let mut right = [-0.125_f32, 0.375, -0.625];
        let expected_left = left;
        let expected_right = right;
        processor.process_in_place(&mut left, &mut right).unwrap();
        assert_eq!(left.map(f32::to_bits), expected_left.map(f32::to_bits));
        assert_eq!(right.map(f32::to_bits), expected_right.map(f32::to_bits));

        let mut mismatched = [1.0_f32, 2.0];
        let mut short = [3.0_f32];
        assert!(matches!(
            processor.process_in_place(&mut mismatched, &mut short),
            Err(NativeDeviceProcessError::ChannelLengthMismatch { .. })
        ));
        assert_eq!(
            mismatched.map(f32::to_bits),
            [1.0_f32, 2.0].map(f32::to_bits)
        );
        assert_eq!(short.map(f32::to_bits), [3.0_f32].map(f32::to_bits));
    }

    #[test]
    fn prepared_chain_bypass_is_exact_and_resets_transient_processor_state() {
        let filter = NativeDeviceInstance::try_new_filter(
            DEVICE_ID.to_owned(),
            true,
            state(NativeFilterMode::LowPass),
        )
        .unwrap();
        let compressor = NativeDeviceInstance::try_new_compressor(
            "018f47a2-6b2d-7c11-8a44-001122334492".to_owned(),
            true,
            compressor_state(),
        )
        .unwrap();
        let chain = DeviceChain::try_new(
            CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![filter, compressor],
        )
        .unwrap();
        let mut processor =
            NativeDeviceChainProcessor::try_new(&chain, PROJECT_SAMPLE_RATE_HZ).unwrap();

        assert_eq!(processor.device_enabled(0), Some(true));
        assert_eq!(processor.device_enabled(1), Some(true));
        assert_eq!(processor.device_enabled(2), None);
        assert!(processor.set_device_enabled(0, false));
        assert!(processor.set_device_enabled(1, false));
        assert!(!processor.set_device_enabled(2, true));
        assert_eq!(processor.device_enabled(0), Some(false));
        assert_eq!(processor.device_enabled(1), Some(false));

        let expected_left = [0.25_f32, -0.5, 0.75];
        let expected_right = [-0.125_f32, 0.375, -0.625];
        let mut left = expected_left;
        let mut right = expected_right;
        processor.process_in_place(&mut left, &mut right).unwrap();
        assert_eq!(left.map(f32::to_bits), expected_left.map(f32::to_bits));
        assert_eq!(right.map(f32::to_bits), expected_right.map(f32::to_bits));

        assert!(processor.set_device_enabled(0, true));
        assert!(processor.set_device_enabled(1, true));
        let mut replay_left = [0.0_f32; 64];
        replay_left[0] = 1.0;
        let mut replay_right = replay_left;
        processor
            .process_in_place(&mut replay_left, &mut replay_right)
            .unwrap();

        let mut fresh =
            NativeDeviceChainProcessor::try_new(&chain, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let mut fresh_left = [0.0_f32; 64];
        fresh_left[0] = 1.0;
        let mut fresh_right = fresh_left;
        fresh
            .process_in_place(&mut fresh_left, &mut fresh_right)
            .unwrap();
        assert_eq!(replay_left.map(f32::to_bits), fresh_left.map(f32::to_bits));
        assert_eq!(
            replay_right.map(f32::to_bits),
            fresh_right.map(f32::to_bits)
        );
    }

    #[test]
    fn prepared_filter_retarget_preserves_history_and_finishes_a_bounded_crossfade() {
        let initial = state(NativeFilterMode::LowPass);
        let target =
            NativeFilterState::try_new(NativeFilterMode::HighPass, 4_000_000, 1_500_000).unwrap();
        let mut processor =
            NativeFilterProcessor::try_new(initial, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let mut left = vec![1.0; usize::from(PARAMETER_TRANSITION_FRAMES) + 1];
        let mut right = vec![-0.5; usize::from(PARAMETER_TRANSITION_FRAMES) + 1];
        processor
            .process_in_place(&mut left[..1], &mut right[..1])
            .unwrap();
        processor.set_state(target);
        assert_eq!(processor.state(), target);
        assert!(processor.transition.is_some());

        processor
            .process_in_place(&mut left[1..], &mut right[1..])
            .unwrap();

        assert!(left.iter().all(|sample| sample.is_finite()));
        assert!(right.iter().all(|sample| sample.is_finite()));
        assert!(processor.transition.is_none());
        assert_eq!(processor.state(), target);
    }

    #[test]
    fn prepared_compressor_retarget_uses_a_bounded_crossfade() {
        let initial =
            NativeCompressorState::try_new(-18_000, 1_000_000, 10_000, 100_000, 0).unwrap();
        let target =
            NativeCompressorState::try_new(-36_000, 4_000_000, 10_000, 100_000, 6_020).unwrap();
        let mut processor =
            NativeCompressorProcessor::try_new(initial, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
        processor.set_state(target);
        let mut left = vec![1.0; usize::from(PARAMETER_TRANSITION_FRAMES)];
        let mut right = vec![1.0; usize::from(PARAMETER_TRANSITION_FRAMES)];
        let mut expected_left = left.clone();
        let mut expected_right = right.clone();
        let mut target_processor =
            NativeCompressorProcessor::try_new(target, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
        target_processor
            .process_in_place(&mut expected_left, &mut expected_right)
            .unwrap();

        processor
            .process_in_place(&mut left[..1], &mut right[..1])
            .unwrap();
        assert!(left[0] > 1.0 && left[0] < expected_left[0]);
        assert!((left[0] - right[0]).abs() < f32::EPSILON);
        processor
            .process_in_place(&mut left[1..], &mut right[1..])
            .unwrap();

        assert!(processor.transition.is_none());
        assert_eq!(
            left.last().map(|sample| sample.to_bits()),
            expected_left.last().map(|sample| sample.to_bits())
        );
        assert_eq!(
            right.last().map(|sample| sample.to_bits()),
            expected_right.last().map(|sample| sample.to_bits())
        );
        assert_eq!(processor.state(), target);
    }

    #[test]
    fn prepared_chain_rejects_cross_kind_retarget_without_mutation() {
        let filter = NativeDeviceInstance::try_new_filter(
            DEVICE_ID.to_owned(),
            true,
            state(NativeFilterMode::LowPass),
        )
        .unwrap();
        let chain = DeviceChain::try_new(
            CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![filter],
        )
        .unwrap();
        let mut processor =
            NativeDeviceChainProcessor::try_new(&chain, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let before = processor.device_state(0);

        assert!(!processor.set_device_state(0, NativeDeviceState::Compressor(compressor_state()),));

        assert_eq!(processor.device_state(0), before);
    }

    #[test]
    fn filter_is_block_split_equivalent_and_preserves_channel_separation() {
        let mut whole = NativeFilterProcessor::try_new(
            state(NativeFilterMode::LowPass),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut split = NativeFilterProcessor::try_new(
            state(NativeFilterMode::LowPass),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut whole_left = [0.0_f32; 128];
        whole_left[0] = 1.0;
        let mut whole_right = [0.0_f32; 128];
        let mut split_left = whole_left;
        let mut split_right = whole_right;
        whole
            .process_in_place(&mut whole_left, &mut whole_right)
            .unwrap();
        split
            .process_in_place(&mut split_left[..37], &mut split_right[..37])
            .unwrap();
        split
            .process_in_place(&mut split_left[37..], &mut split_right[37..])
            .unwrap();
        assert_eq!(whole_left.map(f32::to_bits), split_left.map(f32::to_bits));
        assert_eq!(whole_right.map(f32::to_bits), split_right.map(f32::to_bits));
        assert!(whole_left.iter().any(|sample| *sample != 0.0));
        assert!(whole_right.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn low_pass_converges_to_dc_and_high_pass_rejects_it() {
        let mut low = NativeFilterProcessor::try_new(
            state(NativeFilterMode::LowPass),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut high = NativeFilterProcessor::try_new(
            state(NativeFilterMode::HighPass),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut low_left = [1.0_f32; 4_096];
        let mut low_right = low_left;
        let mut high_left = low_left;
        let mut high_right = low_left;
        low.process_in_place(&mut low_left, &mut low_right).unwrap();
        high.process_in_place(&mut high_left, &mut high_right)
            .unwrap();
        assert!((low_left[4_095] - 1.0).abs() < 0.000_01);
        assert!(high_left[4_095].abs() < 0.000_01);
        assert_eq!(low_left.map(f32::to_bits), low_right.map(f32::to_bits));
        assert_eq!(high_left.map(f32::to_bits), high_right.map(f32::to_bits));
    }

    #[test]
    fn reset_replays_the_same_impulse_and_extreme_states_remain_finite() {
        for state in [
            NativeFilterState::try_new(
                NativeFilterMode::LowPass,
                MIN_FILTER_CUTOFF_MILLIHERTZ,
                MAX_FILTER_RESONANCE_MILLIONTHS,
            )
            .unwrap(),
            NativeFilterState::try_new(
                NativeFilterMode::HighPass,
                MAX_FILTER_CUTOFF_MILLIHERTZ,
                MIN_FILTER_RESONANCE_MILLIONTHS,
            )
            .unwrap(),
        ] {
            let mut processor =
                NativeFilterProcessor::try_new(state, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
            let mut first = [0.0_f32; 512];
            first[0] = 1.0;
            let mut first_right = first;
            processor
                .process_in_place(&mut first, &mut first_right)
                .unwrap();
            assert!(
                first
                    .iter()
                    .chain(&first_right)
                    .all(|sample| sample.is_finite())
            );
            processor.reset();
            let mut second = [0.0_f32; 512];
            second[0] = 1.0;
            let mut second_right = second;
            processor
                .process_in_place(&mut second, &mut second_right)
                .unwrap();
            assert_eq!(first.map(f32::to_bits), second.map(f32::to_bits));
            assert_eq!(
                first_right.map(f32::to_bits),
                second_right.map(f32::to_bits)
            );
        }
    }

    #[test]
    fn compressor_is_block_split_equivalent_stereo_linked_and_resettable() {
        let state = NativeCompressorState::try_new(-24_000, 4_000_000, 5_000, 80_000, 0).unwrap();
        let mut whole =
            NativeCompressorProcessor::try_new(state, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let mut split =
            NativeCompressorProcessor::try_new(state, true, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let mut whole_left = [1.0_f32; 256];
        let mut whole_right = [0.25_f32; 256];
        let mut split_left = whole_left;
        let mut split_right = whole_right;
        whole
            .process_in_place(&mut whole_left, &mut whole_right)
            .unwrap();
        split
            .process_in_place(&mut split_left[..71], &mut split_right[..71])
            .unwrap();
        split
            .process_in_place(&mut split_left[71..], &mut split_right[71..])
            .unwrap();
        assert_eq!(whole_left.map(f32::to_bits), split_left.map(f32::to_bits));
        assert_eq!(whole_right.map(f32::to_bits), split_right.map(f32::to_bits));
        assert!(whole_left[255] < 1.0);
        assert!((f64::from(whole_left[255]) / f64::from(whole_right[255]) - 4.0).abs() < 0.000_001);

        whole.reset();
        let mut replay_left = [1.0_f32; 256];
        let mut replay_right = [0.25_f32; 256];
        whole
            .process_in_place(&mut replay_left, &mut replay_right)
            .unwrap();
        assert_eq!(whole_left.map(f32::to_bits), replay_left.map(f32::to_bits));
        assert_eq!(
            whole_right.map(f32::to_bits),
            replay_right.map(f32::to_bits)
        );
    }

    #[test]
    fn compressor_bypass_and_unity_ratio_are_bit_exact() {
        let state = compressor_state();
        let mut bypass =
            NativeCompressorProcessor::try_new(state, false, PROJECT_SAMPLE_RATE_HZ).unwrap();
        let mut unity = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-80_000, 1_000_000, 100, 100, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let expected_left = [0.25_f32, -0.5, 1.0];
        let expected_right = [-0.125_f32, 0.375, -0.75];
        let mut bypass_left = expected_left;
        let mut bypass_right = expected_right;
        let mut unity_left = expected_left;
        let mut unity_right = expected_right;
        bypass
            .process_in_place(&mut bypass_left, &mut bypass_right)
            .unwrap();
        unity
            .process_in_place(&mut unity_left, &mut unity_right)
            .unwrap();
        assert_eq!(
            bypass_left.map(f32::to_bits),
            expected_left.map(f32::to_bits)
        );
        assert_eq!(
            bypass_right.map(f32::to_bits),
            expected_right.map(f32::to_bits)
        );
        assert_eq!(
            unity_left.map(f32::to_bits),
            expected_left.map(f32::to_bits)
        );
        assert_eq!(
            unity_right.map(f32::to_bits),
            expected_right.map(f32::to_bits)
        );
    }

    fn compressor_threshold_and_ratio_are_directional() {
        let mut lower_threshold = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-36_000, 4_000_000, 100, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut higher_threshold = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-6_000, 4_000_000, 100, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut higher_ratio = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-18_000, 16_000_000, 100, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut lower_ratio = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-18_000, 2_000_000, 100, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut source = [1.0_f32; 512];
        let mut right = source;
        lower_threshold
            .process_in_place(&mut source, &mut right)
            .unwrap();
        let mut source = [1.0_f32; 512];
        let mut right = source;
        higher_threshold
            .process_in_place(&mut source, &mut right)
            .unwrap();
        assert!(lower_threshold.gain_reduction_decibels > higher_threshold.gain_reduction_decibels);
        let mut source = [1.0_f32; 512];
        let mut right = source;
        higher_ratio
            .process_in_place(&mut source, &mut right)
            .unwrap();
        let mut source = [1.0_f32; 512];
        let mut right = source;
        lower_ratio
            .process_in_place(&mut source, &mut right)
            .unwrap();
        assert!(higher_ratio.gain_reduction_decibels > lower_ratio.gain_reduction_decibels);
    }

    fn compressor_attack_and_release_are_directional() {
        let mut fast_attack = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-24_000, 8_000_000, 100, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut slow_attack = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-24_000, 8_000_000, 500_000, 100_000, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut fast_attack_left = [1.0_f32];
        let mut fast_attack_right = fast_attack_left;
        fast_attack
            .process_in_place(&mut fast_attack_left, &mut fast_attack_right)
            .unwrap();
        let mut slow_attack_left = [1.0_f32];
        let mut slow_attack_right = slow_attack_left;
        slow_attack
            .process_in_place(&mut slow_attack_left, &mut slow_attack_right)
            .unwrap();
        assert!(fast_attack_left[0] < slow_attack_left[0]);

        let release_state = |release_microseconds| {
            NativeCompressorProcessor::try_new(
                NativeCompressorState::try_new(-24_000, 8_000_000, 100, release_microseconds, 0)
                    .unwrap(),
                true,
                PROJECT_SAMPLE_RATE_HZ,
            )
            .unwrap()
        };
        let mut fast_release = release_state(100);
        let mut slow_release = release_state(2_000_000);
        let mut high_left = [1.0_f32; 512];
        let mut high_right = high_left;
        fast_release
            .process_in_place(&mut high_left, &mut high_right)
            .unwrap();
        let mut high_left = [1.0_f32; 512];
        let mut high_right = high_left;
        slow_release
            .process_in_place(&mut high_left, &mut high_right)
            .unwrap();
        let mut fast_release_left = [0.01_f32; 32];
        let mut fast_release_right = fast_release_left;
        fast_release
            .process_in_place(&mut fast_release_left, &mut fast_release_right)
            .unwrap();
        let mut slow_release_left = [0.01_f32; 32];
        let mut slow_release_right = slow_release_left;
        slow_release
            .process_in_place(&mut slow_release_left, &mut slow_release_right)
            .unwrap();
        assert!(fast_release_left[31] > slow_release_left[31]);
    }

    fn compressor_makeup_changes_unity_gain() {
        let mut no_makeup = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-80_000, 1_000_000, 100, 100, 0).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut makeup = NativeCompressorProcessor::try_new(
            NativeCompressorState::try_new(-80_000, 1_000_000, 100, 100, 6_000).unwrap(),
            true,
            PROJECT_SAMPLE_RATE_HZ,
        )
        .unwrap();
        let mut no_makeup_left = [0.25_f32];
        let mut no_makeup_right = no_makeup_left;
        no_makeup
            .process_in_place(&mut no_makeup_left, &mut no_makeup_right)
            .unwrap();
        let mut makeup_left = [0.25_f32];
        let mut makeup_right = makeup_left;
        makeup
            .process_in_place(&mut makeup_left, &mut makeup_right)
            .unwrap();
        assert!(makeup_left[0] > no_makeup_left[0]);
    }

    #[test]
    fn compressor_controls_have_independent_directional_effects() {
        compressor_threshold_and_ratio_are_directional();
        compressor_attack_and_release_are_directional();
        compressor_makeup_changes_unity_gain();
    }
}
