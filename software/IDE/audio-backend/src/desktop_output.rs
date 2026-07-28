use core::error::Error;
use core::fmt;

#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{
    PipeWireOutputConfig, PipeWireOutputTarget, PipeWireTargetDiscoveryError,
    discover_pipewire_output_targets,
};
#[cfg(target_os = "windows")]
use super::{
    WasapiEndpointDiscoveryError, WasapiOutputConfig, WasapiOutputEndpoint,
    discover_wasapi_output_endpoints,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopOutputTarget {
    routing_id: String,
    display_name: String,
}

impl DesktopOutputTarget {
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
impl From<&PipeWireOutputTarget> for DesktopOutputTarget {
    fn from(target: &PipeWireOutputTarget) -> Self {
        Self {
            routing_id: target.routing_id().to_owned(),
            display_name: target.display_name().to_owned(),
        }
    }
}

#[cfg(target_os = "windows")]
impl From<&WasapiOutputEndpoint> for DesktopOutputTarget {
    fn from(target: &WasapiOutputEndpoint) -> Self {
        Self {
            routing_id: target.routing_id().to_owned(),
            display_name: target.display_name().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DesktopOutputTargets {
    targets: Vec<DesktopOutputTarget>,
}

impl DesktopOutputTargets {
    #[must_use]
    pub fn targets(&self) -> &[DesktopOutputTarget] {
        &self.targets
    }

    #[must_use]
    pub fn target_by_routing_id(&self, routing_id: &str) -> Option<&DesktopOutputTarget> {
        self.targets
            .iter()
            .find(|target| target.routing_id == routing_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopOutputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyRoutingId,
    RoutingIdContainsNul,
}

impl fmt::Display for DesktopOutputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => {
                formatter.write_str("desktop output sample rate must be nonzero")
            }
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "desktop output maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyRoutingId => {
                formatter.write_str("desktop output routing identifier must not be empty")
            }
            Self::RoutingIdContainsNul => {
                formatter.write_str("desktop output routing identifier contains an embedded NUL")
            }
        }
    }
}

impl Error for DesktopOutputConfigError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopOutputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    routing_id: Option<String>,
}

impl DesktopOutputConfig {
    /// Creates a stereo `f32` desktop output configuration.
    ///
    /// No routing identifier delegates selection to the operating-system audio
    /// policy. Recovery requires an explicitly pinned target.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputConfigError`] for an invalid sample rate or
    /// callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, DesktopOutputConfigError> {
        if sample_rate_hz == 0 {
            return Err(DesktopOutputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(DesktopOutputConfigError::InvalidMaximumBlockFrames {
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

    /// Pins playback to one opaque routing identifier returned by discovery.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputConfigError`] for an empty identifier or an
    /// embedded NUL.
    pub fn with_routing_id(mut self, routing_id: &str) -> Result<Self, DesktopOutputConfigError> {
        if routing_id.is_empty() {
            return Err(DesktopOutputConfigError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(DesktopOutputConfigError::RoutingIdContainsNul);
        }
        self.routing_id = Some(routing_id.to_owned());
        Ok(self)
    }

    /// Pins playback to one target returned by desktop discovery.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopOutputConfigError`] if its routing identity is invalid.
    pub fn with_output_target(
        self,
        target: &DesktopOutputTarget,
    ) -> Result<Self, DesktopOutputConfigError> {
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

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    pub(crate) fn native(&self) -> PipeWireOutputConfig {
        PipeWireOutputConfig::from_desktop(
            self.sample_rate_hz,
            self.maximum_block_frames,
            self.routing_id.clone(),
        )
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn native(&self) -> WasapiOutputConfig {
        WasapiOutputConfig::from_desktop(
            self.sample_rate_hz,
            self.maximum_block_frames,
            self.routing_id.clone(),
        )
    }
}

#[derive(Debug)]
pub struct DesktopOutputDiscoveryError {
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    source: PipeWireTargetDiscoveryError,
    #[cfg(target_os = "windows")]
    source: WasapiEndpointDiscoveryError,
}

impl fmt::Display for DesktopOutputDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for DesktopOutputDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Returns one deterministic snapshot of routable desktop audio outputs.
///
/// # Errors
///
/// Returns [`DesktopOutputDiscoveryError`] while preserving the native error
/// as its source.
pub fn discover_desktop_output_targets() -> Result<DesktopOutputTargets, DesktopOutputDiscoveryError>
{
    #[cfg(all(target_os = "linux", target_endian = "little"))]
    let native = discover_pipewire_output_targets()
        .map_err(|source| DesktopOutputDiscoveryError { source })?;
    #[cfg(target_os = "windows")]
    let native = discover_wasapi_output_endpoints()
        .map_err(|source| DesktopOutputDiscoveryError { source })?;

    let targets = native
        .targets()
        .iter()
        .map(DesktopOutputTarget::from)
        .collect();
    Ok(DesktopOutputTargets { targets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_config_has_one_cross_platform_routing_contract() {
        let target = DesktopOutputTarget {
            routing_id: "opaque-route".to_owned(),
            display_name: "Studio Output".to_owned(),
        };
        let config = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_output_target(&target)
            .unwrap();

        assert_eq!(target.display_name(), "Studio Output");
        assert_eq!(config.sample_rate_hz(), 48_000);
        assert_eq!(config.maximum_block_frames(), 256);
        assert_eq!(config.routing_id(), Some("opaque-route"));
        assert_eq!(
            DesktopOutputConfig::try_new(0, 256),
            Err(DesktopOutputConfigError::ZeroSampleRate)
        );
    }
}
