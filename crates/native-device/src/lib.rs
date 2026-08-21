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
const FORMAT_VERSION: u8 = 1;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDeviceInstance {
    id: String,
    device_id: String,
    state_version: u16,
    dsp_version: u16,
    enabled: bool,
    state: NativeFilterState,
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
            state,
            extensions: BTreeMap::new(),
        };
        validate_device(&instance)?;
        Ok(instance)
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
    pub const fn state(&self) -> NativeFilterState {
        self.state
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceChain {
    format: String,
    format_version: u8,
    id: String,
    project_id: String,
    track_id: String,
    devices: Vec<NativeDeviceInstance>,
    extensions: BTreeMap<String, Value>,
}

impl DeviceChain {
    /// Parses and validates one complete version-1 device chain.
    ///
    /// # Errors
    ///
    /// Returns a classified strict-JSON or device-domain rejection.
    pub fn parse(bytes: &[u8]) -> Result<Self, DeviceChainError> {
        let chain: Self = hyper_strict_json::parse(bytes, MAX_DOCUMENT_BYTES)?;
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
        let chain = Self {
            format: FORMAT.to_owned(),
            format_version: FORMAT_VERSION,
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
        serde_json_canonicalizer::to_vec(self).map_err(|_| DeviceChainError {
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
        Self::try_new(
            self.id.clone(),
            self.project_id.clone(),
            self.track_id.clone(),
            devices,
        )
    }

    /// Returns a copy with one existing Native Filter state replaced without
    /// changing chain order or device identity.
    ///
    /// # Errors
    ///
    /// Returns a domain error when the device identity is absent or the state
    /// is outside the version-1 filter domain.
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
            return Err(DeviceChainError {
                kind: DeviceChainErrorKind::InvalidDomain,
                message: "native device instance does not exist in this chain".to_owned(),
            });
        };
        *device = NativeDeviceInstance::try_new_filter(
            device_instance_id.to_owned(),
            enabled,
            state,
        )?;
        Self::try_new(
            self.id.clone(),
            self.project_id.clone(),
            self.track_id.clone(),
            devices,
        )
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
        Ok(Some(Self::try_new(
            self.id.clone(),
            self.project_id.clone(),
            self.track_id.clone(),
            devices,
        )?))
    }

    #[must_use]
    pub const fn extensions(&self) -> &BTreeMap<String, Value> {
        &self.extensions
    }
}

fn validate_chain(chain: &DeviceChain) -> Result<(), DeviceChainError> {
    require(
        chain.format == FORMAT && chain.format_version == FORMAT_VERSION,
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
        "device chain count is outside the version-1 bound",
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
    require(
        device.device_id == NATIVE_FILTER_DEVICE_ID
            && device.state_version == NATIVE_FILTER_STATE_VERSION
            && device.dsp_version == NATIVE_FILTER_DSP_VERSION,
        "native device kind or version is unsupported",
    )?;
    require(
        device
            .extensions
            .keys()
            .all(|key| is_valid_extension_key(key)),
        "native device contains an invalid extension key",
    )?;
    validate_filter_state(device.state)
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
    coefficients: BiquadCoefficients,
    channels: [BiquadState; 2],
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
            coefficients: coefficients(state, sample_rate_hz),
            channels: [BiquadState::default(); 2],
        })
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn reset(&mut self) {
        self.channels = [BiquadState::default(); 2];
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
        let coefficients = self.coefficients;
        let mut states = self.channels;
        for (channel, state) in [left, right].into_iter().zip(&mut states) {
            for sample in channel {
                let input = f64::from(*sample);
                let output = coefficients.b0.mul_add(input, state.z1);
                state.z1 = coefficients
                    .b1
                    .mul_add(input, (-coefficients.a1).mul_add(output, state.z2));
                state.z2 = coefficients.b2.mul_add(input, -coefficients.a2 * output);
                let output = output as f32;
                if !output.is_finite() || !state.z1.is_finite() || !state.z2.is_finite() {
                    self.reset();
                    return Err(NativeDeviceProcessError::NonFiniteOutput);
                }
                *sample = output;
            }
        }
        self.channels = states;
        Ok(())
    }
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

#[derive(Debug)]
pub struct NativeDeviceChainProcessor {
    filters: Vec<NativeFilterProcessor>,
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
        let mut filters = Vec::new();
        filters
            .try_reserve_exact(chain.devices.len())
            .map_err(|_| NativeDeviceCreationError::CapacityUnavailable)?;
        for device in &chain.devices {
            filters.push(NativeFilterProcessor::try_new(
                device.state,
                device.enabled,
                sample_rate_hz,
            )?);
        }
        Ok(Self { filters })
    }

    #[must_use]
    pub fn device_count(&self) -> usize {
        self.filters.len()
    }

    pub fn reset(&mut self) {
        for filter in &mut self.filters {
            filter.reset();
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
        for filter in &mut self.filters {
            if let Err(error) = filter.process_in_place(left, right) {
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

    #[test]
    fn canonical_chain_round_trips_with_stable_device_identity() {
        let chain = chain(true);
        let bytes = chain.to_canonical_vec().unwrap();
        assert_eq!(DeviceChain::parse(&bytes).unwrap(), chain);
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
            changed.devices()[1].state().mode(),
            NativeFilterMode::LowPass
        );
        let reduced = changed.try_without_device(DEVICE_ID).unwrap().unwrap();
        assert_eq!(reduced.devices()[0].id(), second_id);
        assert!(reduced.try_without_device(second_id).unwrap().is_none());
        assert!(reduced.try_without_device(DEVICE_ID).is_err());
    }

    #[test]
    fn parser_rejects_unknown_device_versions_domains_and_duplicate_instances() {
        let canonical = chain(true).to_canonical_vec().unwrap();
        let base: Value = serde_json::from_slice(&canonical).unwrap();
        let mut cases = Vec::new();
        for (pointer, value) in [
            ("/format_version", serde_json::json!(2)),
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
}
