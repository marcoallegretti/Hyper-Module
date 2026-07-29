use core::error::Error;
use core::fmt;

#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{
    PipeWireInputConfig, PipeWireInputTarget, PipeWireTargetDiscoveryError,
    discover_pipewire_input_targets,
};
#[cfg(target_os = "windows")]
use super::{
    WasapiEndpointDiscoveryError, WasapiInputConfig, WasapiInputEndpoint,
    discover_wasapi_input_endpoints,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopInputTarget {
    routing_id: String,
    display_name: String,
}

impl DesktopInputTarget {
    #[must_use]
    pub fn routing_id(&self) -> &str {
        &self.routing_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
impl From<&PipeWireInputTarget> for DesktopInputTarget {
    fn from(target: &PipeWireInputTarget) -> Self {
        Self {
            routing_id: target.routing_id().to_owned(),
            display_name: target.display_name().to_owned(),
        }
    }
}

#[cfg(target_os = "windows")]
impl From<&WasapiInputEndpoint> for DesktopInputTarget {
    fn from(target: &WasapiInputEndpoint) -> Self {
        Self {
            routing_id: target.routing_id().to_owned(),
            display_name: target.display_name().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopInputTargets {
    targets: Vec<DesktopInputTarget>,
}

impl DesktopInputTargets {
    #[must_use]
    pub fn targets(&self) -> &[DesktopInputTarget] {
        &self.targets
    }

    #[must_use]
    pub fn target_by_routing_id(&self, routing_id: &str) -> Option<&DesktopInputTarget> {
        self.targets
            .iter()
            .find(|target| target.routing_id == routing_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopInputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyRoutingId,
    RoutingIdContainsNul,
}

impl fmt::Display for DesktopInputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => {
                formatter.write_str("desktop input sample rate must be nonzero")
            }
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "desktop input maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyRoutingId => {
                formatter.write_str("desktop input routing identifier must not be empty")
            }
            Self::RoutingIdContainsNul => {
                formatter.write_str("desktop input routing identifier contains an embedded NUL")
            }
        }
    }
}

impl Error for DesktopInputConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopInputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    routing_id: Option<String>,
}

impl DesktopInputConfig {
    /// Creates a stereo `f32` desktop input configuration.
    ///
    /// No routing identifier delegates selection to the operating-system audio
    /// policy.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopInputConfigError`] for an invalid sample rate or
    /// callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, DesktopInputConfigError> {
        if sample_rate_hz == 0 {
            return Err(DesktopInputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(DesktopInputConfigError::InvalidMaximumBlockFrames {
                actual: maximum_block_frames,
                maximum: super::MAXIMUM_BLOCK_FRAMES,
            });
        }
        Ok(Self {
            sample_rate_hz,
            maximum_block_frames,
            routing_id: None,
        })
    }

    /// Pins capture to one opaque routing identifier returned by discovery.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopInputConfigError`] for an empty identifier or an
    /// embedded NUL.
    pub fn with_routing_id(mut self, routing_id: &str) -> Result<Self, DesktopInputConfigError> {
        if routing_id.is_empty() {
            return Err(DesktopInputConfigError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(DesktopInputConfigError::RoutingIdContainsNul);
        }
        self.routing_id = Some(routing_id.to_owned());
        Ok(self)
    }

    /// Pins capture to one target returned by desktop discovery.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopInputConfigError`] if its routing identity is invalid.
    pub fn with_input_target(
        self,
        target: &DesktopInputTarget,
    ) -> Result<Self, DesktopInputConfigError> {
        self.with_routing_id(target.routing_id())
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn maximum_block_frames(&self) -> usize {
        self.maximum_block_frames
    }

    #[must_use]
    pub fn routing_id(&self) -> Option<&str> {
        self.routing_id.as_deref()
    }
}

#[cfg(all(target_os = "linux", target_endian = "little"))]
impl From<&DesktopInputConfig> for PipeWireInputConfig {
    fn from(config: &DesktopInputConfig) -> Self {
        Self::from_desktop(
            config.sample_rate_hz,
            config.maximum_block_frames,
            config.routing_id.clone(),
        )
    }
}

#[cfg(target_os = "windows")]
impl From<&DesktopInputConfig> for WasapiInputConfig {
    fn from(config: &DesktopInputConfig) -> Self {
        Self::from_desktop(
            config.sample_rate_hz,
            config.maximum_block_frames,
            config.routing_id.clone(),
        )
    }
}

#[derive(Debug)]
pub struct DesktopInputDiscoveryError {
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    source: PipeWireTargetDiscoveryError,
    #[cfg(target_os = "windows")]
    source: WasapiEndpointDiscoveryError,
}

impl fmt::Display for DesktopInputDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for DesktopInputDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Returns one deterministic snapshot of routable desktop audio inputs.
///
/// # Errors
///
/// Returns [`DesktopInputDiscoveryError`] while preserving the native error as
/// its source.
pub fn discover_desktop_input_targets() -> Result<DesktopInputTargets, DesktopInputDiscoveryError> {
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    let native = discover_pipewire_input_targets()
        .map_err(|source| DesktopInputDiscoveryError { source })?;
    #[cfg(target_os = "windows")]
    let native = discover_wasapi_input_endpoints()
        .map_err(|source| DesktopInputDiscoveryError { source })?;

    let targets = native
        .targets()
        .iter()
        .map(DesktopInputTarget::from)
        .collect();
    Ok(DesktopInputTargets { targets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_config_has_one_cross_platform_routing_contract() {
        let target = DesktopInputTarget {
            routing_id: "opaque-route".to_owned(),
            display_name: "Studio Input".to_owned(),
        };
        let config = DesktopInputConfig::try_new(48_000, 256)
            .unwrap()
            .with_input_target(&target)
            .unwrap();

        assert_eq!(target.display_name(), "Studio Input");
        assert_eq!(config.sample_rate_hz(), 48_000);
        assert_eq!(config.maximum_block_frames(), 256);
        assert_eq!(config.routing_id(), Some("opaque-route"));
        let targets = DesktopInputTargets {
            targets: vec![target],
        };
        assert_eq!(
            targets
                .target_by_routing_id("opaque-route")
                .map(DesktopInputTarget::display_name),
            Some("Studio Input")
        );
        #[cfg(all(target_os = "linux", target_endian = "little"))]
        {
            let native = PipeWireInputConfig::from(&config);
            assert_eq!(native.sample_rate_hz(), 48_000);
            assert_eq!(native.maximum_block_frames(), 256);
            assert_eq!(native.target(), Some("opaque-route"));
        }
        #[cfg(target_os = "windows")]
        {
            let native = WasapiInputConfig::from(&config);
            assert_eq!(native.sample_rate_hz(), 48_000);
            assert_eq!(native.maximum_block_frames(), 256);
            assert_eq!(native.device_id(), Some("opaque-route"));
        }
        assert_eq!(
            DesktopInputConfig::try_new(0, 256),
            Err(DesktopInputConfigError::ZeroSampleRate)
        );
        assert!(matches!(
            DesktopInputConfig::try_new(48_000, super::super::MAXIMUM_BLOCK_FRAMES + 1),
            Err(DesktopInputConfigError::InvalidMaximumBlockFrames { .. })
        ));
        assert_eq!(
            DesktopInputConfig::try_new(48_000, 256)
                .unwrap()
                .with_routing_id(""),
            Err(DesktopInputConfigError::EmptyRoutingId)
        );
        assert_eq!(
            DesktopInputConfig::try_new(48_000, 256)
                .unwrap()
                .with_routing_id("invalid\0route"),
            Err(DesktopInputConfigError::RoutingIdContainsNul)
        );
    }
}
