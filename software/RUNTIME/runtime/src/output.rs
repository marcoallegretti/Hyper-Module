//! Runtime startup selection for the existing desktop-output contract.
//!
//! Native audio ownership is intentionally created only after a requested
//! route has been found in one current discovery snapshot. The Runtime never
//! treats an absent pinned route as permission to fall back to a different
//! output.

use core::error::Error;
use core::fmt;

use hyper_desktop_audio::{
    DesktopOutputConfig, DesktopOutputConfigError, DesktopOutputDiscoveryError,
    DesktopOutputTarget, discover_desktop_output_targets,
};

/// The route policy that was verified before Runtime audio ownership began.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RuntimeOutputRoute {
    SystemDefault,
    Pinned { display_name: String },
}

impl fmt::Display for RuntimeOutputRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SystemDefault => formatter.write_str("System default"),
            Self::Pinned { display_name } => write!(formatter, "Pinned: {display_name}"),
        }
    }
}

/// A preflighted desktop output configuration and the operator-facing route
/// fact derived from the same discovery snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeOutputConfiguration {
    config: DesktopOutputConfig,
    route: RuntimeOutputRoute,
}

impl RuntimeOutputConfiguration {
    #[must_use]
    pub(super) fn into_parts(self) -> (DesktopOutputConfig, RuntimeOutputRoute) {
        (self.config, self.route)
    }
}

/// Why Runtime could not prepare an output before native ownership started.
#[derive(Debug)]
pub(super) enum RuntimeOutputConfigurationError {
    Config(DesktopOutputConfigError),
    Discovery(DesktopOutputDiscoveryError),
    CapacityUnavailable,
    EmptyRoutingId,
    RoutingIdContainsNul,
    EmptyDisplayName,
    DisplayNameContainsNul,
    DuplicateRoutingId { output_index: usize },
    RequestedOutputUnavailable,
}

impl fmt::Display for RuntimeOutputConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => source.fmt(formatter),
            Self::Discovery(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime output-target inventory capacity is unavailable")
            }
            Self::EmptyRoutingId => {
                formatter.write_str("Runtime output discovery returned an empty routing identity")
            }
            Self::RoutingIdContainsNul => formatter.write_str(
                "Runtime output discovery returned a routing identity with an embedded NUL",
            ),
            Self::EmptyDisplayName => {
                formatter.write_str("Runtime output discovery returned an empty display name")
            }
            Self::DisplayNameContainsNul => formatter
                .write_str("Runtime output discovery returned a display name with an embedded NUL"),
            Self::DuplicateRoutingId { output_index } => write!(
                formatter,
                "Runtime output discovery entry {output_index} duplicates an earlier routing identity"
            ),
            Self::RequestedOutputUnavailable => {
                formatter.write_str("the requested Runtime audio output is unavailable")
            }
        }
    }
}

impl Error for RuntimeOutputConfigurationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::Discovery(source) => Some(source),
            Self::CapacityUnavailable
            | Self::EmptyRoutingId
            | Self::RoutingIdContainsNul
            | Self::EmptyDisplayName
            | Self::DisplayNameContainsNul
            | Self::DuplicateRoutingId { .. }
            | Self::RequestedOutputUnavailable => None,
        }
    }
}

/// One owned row from the current desktop output inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeOutputTarget {
    routing_id: String,
    display_name: String,
}

impl RuntimeOutputTarget {
    #[must_use]
    pub(super) fn routing_id(&self) -> &str {
        &self.routing_id
    }

    #[must_use]
    pub(super) fn display_name(&self) -> &str {
        &self.display_name
    }

    fn try_new(
        routing_id: &str,
        display_name: &str,
    ) -> Result<Self, RuntimeOutputConfigurationError> {
        if routing_id.is_empty() {
            return Err(RuntimeOutputConfigurationError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(RuntimeOutputConfigurationError::RoutingIdContainsNul);
        }
        if display_name.is_empty() {
            return Err(RuntimeOutputConfigurationError::EmptyDisplayName);
        }
        if display_name.as_bytes().contains(&0) {
            return Err(RuntimeOutputConfigurationError::DisplayNameContainsNul);
        }
        Ok(Self {
            routing_id: try_copy(routing_id)?,
            display_name: try_copy(display_name)?,
        })
    }

    fn from_desktop(target: &DesktopOutputTarget) -> Result<Self, RuntimeOutputConfigurationError> {
        Self::try_new(target.routing_id(), target.display_name())
    }
}

/// Discovers a complete owned inventory for the Runtime CLI before it opens a
/// native output stream.
///
/// # Errors
///
/// Returns a desktop discovery error or capacity failure without publishing a
/// partial inventory.
pub(super) fn discover_runtime_output_targets()
-> Result<Vec<RuntimeOutputTarget>, RuntimeOutputConfigurationError> {
    let discovered =
        discover_desktop_output_targets().map_err(RuntimeOutputConfigurationError::Discovery)?;
    let mut targets = Vec::new();
    targets
        .try_reserve_exact(discovered.targets().len())
        .map_err(|_| RuntimeOutputConfigurationError::CapacityUnavailable)?;
    for target in discovered.targets() {
        targets.push(RuntimeOutputTarget::from_desktop(target)?);
    }
    validate_runtime_output_inventory(targets)
}

/// Builds one Runtime output policy from an optional opaque routing identity.
///
/// A requested identity is validated against a current discovery snapshot
/// before it is allowed into [`DesktopOutputConfig`]. Omitting the identity is
/// the explicit system-default policy.
///
/// # Errors
///
/// Returns an error before native output construction when configuration or
/// discovery fails, or when the requested route is no longer present.
pub(super) fn prepare_runtime_output(
    requested_routing_id: Option<&str>,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
) -> Result<RuntimeOutputConfiguration, RuntimeOutputConfigurationError> {
    configure_runtime_output(
        requested_routing_id,
        sample_rate_hz,
        maximum_block_frames,
        discover_runtime_output_targets,
    )
}

fn configure_runtime_output(
    requested_routing_id: Option<&str>,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    discover: impl FnOnce() -> Result<Vec<RuntimeOutputTarget>, RuntimeOutputConfigurationError>,
) -> Result<RuntimeOutputConfiguration, RuntimeOutputConfigurationError> {
    let output = DesktopOutputConfig::try_new(sample_rate_hz, maximum_block_frames)
        .map_err(RuntimeOutputConfigurationError::Config)?;
    let Some(requested_routing_id) = requested_routing_id else {
        return Ok(RuntimeOutputConfiguration {
            config: output,
            route: RuntimeOutputRoute::SystemDefault,
        });
    };
    let output = output
        .with_routing_id(requested_routing_id)
        .map_err(RuntimeOutputConfigurationError::Config)?;
    let target = validate_runtime_output_inventory(discover()?)?
        .into_iter()
        .find(|target| target.routing_id == requested_routing_id)
        .ok_or(RuntimeOutputConfigurationError::RequestedOutputUnavailable)?;
    Ok(RuntimeOutputConfiguration {
        config: output,
        route: RuntimeOutputRoute::Pinned {
            display_name: target.display_name,
        },
    })
}

fn try_copy(value: &str) -> Result<String, RuntimeOutputConfigurationError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RuntimeOutputConfigurationError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

fn validate_runtime_output_inventory(
    targets: Vec<RuntimeOutputTarget>,
) -> Result<Vec<RuntimeOutputTarget>, RuntimeOutputConfigurationError> {
    for (output_index, target) in targets.iter().enumerate() {
        if targets[..output_index]
            .iter()
            .any(|earlier| earlier.routing_id == target.routing_id)
        {
            return Err(RuntimeOutputConfigurationError::DuplicateRoutingId { output_index });
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(routing_id: &str, display_name: &str) -> RuntimeOutputTarget {
        RuntimeOutputTarget::try_new(routing_id, display_name).unwrap()
    }

    #[test]
    fn system_default_does_not_require_discovery() {
        let configuration = configure_runtime_output(None, 48_000, 1_024, || {
            panic!("system-default Runtime output must not require discovery")
        })
        .unwrap();
        let (config, route) = configuration.into_parts();

        assert_eq!(config.routing_id(), None);
        assert_eq!(route, RuntimeOutputRoute::SystemDefault);
        assert_eq!(route.to_string(), "System default");
    }

    #[test]
    fn pinned_output_must_match_the_current_inventory_exactly() {
        let configuration = configure_runtime_output(Some("studio-right"), 48_000, 1_024, || {
            Ok(vec![
                target("studio-left", "Studio Left"),
                target("studio-right", "Studio Right"),
            ])
        })
        .unwrap();
        let (config, route) = configuration.into_parts();

        assert_eq!(config.routing_id(), Some("studio-right"));
        assert_eq!(
            route,
            RuntimeOutputRoute::Pinned {
                display_name: "Studio Right".to_owned(),
            }
        );
        assert_eq!(route.to_string(), "Pinned: Studio Right");
    }

    #[test]
    fn unavailable_or_invalid_pinned_output_never_falls_back() {
        assert!(matches!(
            configure_runtime_output(Some("missing"), 48_000, 1_024, || {
                Ok(vec![target("present", "Present")])
            }),
            Err(RuntimeOutputConfigurationError::RequestedOutputUnavailable)
        ));

        assert!(matches!(
            configure_runtime_output(Some(""), 48_000, 1_024, || {
                panic!("invalid routing identities fail before discovery")
            }),
            Err(RuntimeOutputConfigurationError::Config(
                DesktopOutputConfigError::EmptyRoutingId
            ))
        ));
    }

    #[test]
    fn discovery_inventory_rejects_ambiguous_or_malformed_rows() {
        assert!(matches!(
            configure_runtime_output(Some("shared"), 48_000, 1_024, || {
                Ok(vec![target("shared", "First"), target("shared", "Second")])
            }),
            Err(RuntimeOutputConfigurationError::DuplicateRoutingId { output_index: 1 })
        ));

        for (routing_id, display_name, expected) in [
            (
                "",
                "Output",
                RuntimeOutputConfigurationError::EmptyRoutingId,
            ),
            (
                "route\0id",
                "Output",
                RuntimeOutputConfigurationError::RoutingIdContainsNul,
            ),
            (
                "route",
                "",
                RuntimeOutputConfigurationError::EmptyDisplayName,
            ),
            (
                "route",
                "Output\0name",
                RuntimeOutputConfigurationError::DisplayNameContainsNul,
            ),
        ] {
            let actual = RuntimeOutputTarget::try_new(routing_id, display_name).unwrap_err();
            assert_eq!(actual.to_string(), expected.to_string());
        }
    }
}
