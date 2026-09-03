//! Runtime startup selection for optional desktop input monitoring.
//!
//! The Runtime only opens an input when an operator pins one opaque route from
//! a current discovery snapshot. Omitting the option means capture is disabled;
//! it never delegates input selection to the operating system.

use core::error::Error;
use core::fmt;

use hyper_desktop_audio::{
    DesktopInputConfig, DesktopInputConfigError, DesktopInputDiscoveryError, DesktopInputTarget,
    discover_desktop_input_targets,
};

/// The input route policy verified before Runtime audio ownership began.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RuntimeInputRoute {
    Disabled,
    Pinned { display_name: String },
}

impl fmt::Display for RuntimeInputRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => formatter.write_str("Disabled"),
            Self::Pinned { display_name } => write!(formatter, "Pinned: {display_name}"),
        }
    }
}

/// An optional, preflighted desktop input configuration and the operator-facing
/// route fact derived from the same discovery snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeInputConfiguration {
    config: Option<DesktopInputConfig>,
    route: RuntimeInputRoute,
}

impl RuntimeInputConfiguration {
    #[must_use]
    pub(super) fn into_parts(self) -> (Option<DesktopInputConfig>, RuntimeInputRoute) {
        (self.config, self.route)
    }
}

/// Why Runtime could not prepare optional input monitoring before native
/// ownership started.
#[derive(Debug)]
pub(super) enum RuntimeInputConfigurationError {
    Config(DesktopInputConfigError),
    Discovery(DesktopInputDiscoveryError),
    CapacityUnavailable,
    EmptyRoutingId,
    RoutingIdContainsNul,
    EmptyDisplayName,
    DisplayNameContainsNul,
    DuplicateRoutingId { input_index: usize },
    RequestedInputUnavailable,
}

impl fmt::Display for RuntimeInputConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => source.fmt(formatter),
            Self::Discovery(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime input-target inventory capacity is unavailable")
            }
            Self::EmptyRoutingId => {
                formatter.write_str("Runtime input discovery returned an empty routing identity")
            }
            Self::RoutingIdContainsNul => formatter.write_str(
                "Runtime input discovery returned a routing identity with an embedded NUL",
            ),
            Self::EmptyDisplayName => {
                formatter.write_str("Runtime input discovery returned an empty display name")
            }
            Self::DisplayNameContainsNul => formatter
                .write_str("Runtime input discovery returned a display name with an embedded NUL"),
            Self::DuplicateRoutingId { input_index } => write!(
                formatter,
                "Runtime input discovery entry {input_index} duplicates an earlier routing identity"
            ),
            Self::RequestedInputUnavailable => {
                formatter.write_str("the requested Runtime audio input is unavailable")
            }
        }
    }
}

impl Error for RuntimeInputConfigurationError {
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
            | Self::RequestedInputUnavailable => None,
        }
    }
}

/// One owned row from the current desktop input inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeInputTarget {
    routing_id: String,
    display_name: String,
}

impl RuntimeInputTarget {
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
    ) -> Result<Self, RuntimeInputConfigurationError> {
        if routing_id.is_empty() {
            return Err(RuntimeInputConfigurationError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(RuntimeInputConfigurationError::RoutingIdContainsNul);
        }
        if display_name.is_empty() {
            return Err(RuntimeInputConfigurationError::EmptyDisplayName);
        }
        if display_name.as_bytes().contains(&0) {
            return Err(RuntimeInputConfigurationError::DisplayNameContainsNul);
        }
        Ok(Self {
            routing_id: try_copy(routing_id)?,
            display_name: try_copy(display_name)?,
        })
    }

    fn from_desktop(target: &DesktopInputTarget) -> Result<Self, RuntimeInputConfigurationError> {
        Self::try_new(target.routing_id(), target.display_name())
    }
}

/// Discovers a complete owned inventory for the Runtime CLI before it opens a
/// native input stream.
///
/// # Errors
///
/// Returns a desktop discovery error or capacity failure without publishing a
/// partial inventory.
pub(super) fn discover_runtime_input_targets()
-> Result<Vec<RuntimeInputTarget>, RuntimeInputConfigurationError> {
    let discovered =
        discover_desktop_input_targets().map_err(RuntimeInputConfigurationError::Discovery)?;
    let mut targets = Vec::new();
    targets
        .try_reserve_exact(discovered.targets().len())
        .map_err(|_| RuntimeInputConfigurationError::CapacityUnavailable)?;
    for target in discovered.targets() {
        targets.push(RuntimeInputTarget::from_desktop(target)?);
    }
    validate_runtime_input_inventory(targets)
}

/// Builds the optional Runtime input-monitor policy from an opaque routing
/// identity.
///
/// Omitting the identity is the explicit disabled policy. A requested identity
/// is validated against a current discovery snapshot before it is allowed into
/// [`DesktopInputConfig`], so it can never silently fall back to an
/// operating-system default input.
///
/// # Errors
///
/// Returns an error before native input construction when configuration or
/// discovery fails, or when the requested route is no longer present.
pub(super) fn prepare_runtime_input(
    requested_routing_id: Option<&str>,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
) -> Result<RuntimeInputConfiguration, RuntimeInputConfigurationError> {
    configure_runtime_input(
        requested_routing_id,
        sample_rate_hz,
        maximum_block_frames,
        discover_runtime_input_targets,
    )
}

fn configure_runtime_input(
    requested_routing_id: Option<&str>,
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    discover: impl FnOnce() -> Result<Vec<RuntimeInputTarget>, RuntimeInputConfigurationError>,
) -> Result<RuntimeInputConfiguration, RuntimeInputConfigurationError> {
    let Some(requested_routing_id) = requested_routing_id else {
        return Ok(RuntimeInputConfiguration {
            config: None,
            route: RuntimeInputRoute::Disabled,
        });
    };
    let input = DesktopInputConfig::try_new(sample_rate_hz, maximum_block_frames)
        .map_err(RuntimeInputConfigurationError::Config)?
        .with_routing_id(requested_routing_id)
        .map_err(RuntimeInputConfigurationError::Config)?;
    let target = validate_runtime_input_inventory(discover()?)?
        .into_iter()
        .find(|target| target.routing_id == requested_routing_id)
        .ok_or(RuntimeInputConfigurationError::RequestedInputUnavailable)?;
    Ok(RuntimeInputConfiguration {
        config: Some(input),
        route: RuntimeInputRoute::Pinned {
            display_name: target.display_name,
        },
    })
}

fn try_copy(value: &str) -> Result<String, RuntimeInputConfigurationError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| RuntimeInputConfigurationError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

fn validate_runtime_input_inventory(
    targets: Vec<RuntimeInputTarget>,
) -> Result<Vec<RuntimeInputTarget>, RuntimeInputConfigurationError> {
    for (input_index, target) in targets.iter().enumerate() {
        if targets[..input_index]
            .iter()
            .any(|earlier| earlier.routing_id == target.routing_id)
        {
            return Err(RuntimeInputConfigurationError::DuplicateRoutingId { input_index });
        }
    }
    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(routing_id: &str, display_name: &str) -> RuntimeInputTarget {
        RuntimeInputTarget::try_new(routing_id, display_name).unwrap()
    }

    #[test]
    fn disabled_input_does_not_require_discovery() {
        let configuration = configure_runtime_input(None, 48_000, 1_024, || {
            panic!("disabled Runtime input must not require discovery")
        })
        .unwrap();
        let (config, route) = configuration.into_parts();

        assert_eq!(config, None);
        assert_eq!(route, RuntimeInputRoute::Disabled);
        assert_eq!(route.to_string(), "Disabled");
    }

    #[test]
    fn pinned_input_requires_an_exact_current_target() {
        let configuration = configure_runtime_input(Some("studio-left"), 48_000, 1_024, || {
            Ok(vec![target("studio-left", "Studio Left")])
        })
        .unwrap();
        let (config, route) = configuration.into_parts();

        assert_eq!(
            config.as_ref().and_then(DesktopInputConfig::routing_id),
            Some("studio-left")
        );
        assert_eq!(
            route,
            RuntimeInputRoute::Pinned {
                display_name: "Studio Left".to_owned(),
            }
        );
        assert_eq!(route.to_string(), "Pinned: Studio Left");
    }

    #[test]
    fn unavailable_or_invalid_pinned_input_never_falls_back() {
        assert!(matches!(
            configure_runtime_input(Some("missing"), 48_000, 1_024, || {
                Ok(vec![target("present", "Present")])
            }),
            Err(RuntimeInputConfigurationError::RequestedInputUnavailable)
        ));

        assert!(matches!(
            configure_runtime_input(Some(""), 48_000, 1_024, || {
                panic!("invalid routing identities fail before discovery")
            }),
            Err(RuntimeInputConfigurationError::Config(
                DesktopInputConfigError::EmptyRoutingId
            ))
        ));
    }

    #[test]
    fn malformed_or_ambiguous_inventory_is_rejected() {
        assert!(matches!(
            configure_runtime_input(Some("shared"), 48_000, 1_024, || {
                Ok(vec![target("shared", "First"), target("shared", "Second")])
            }),
            Err(RuntimeInputConfigurationError::DuplicateRoutingId { input_index: 1 })
        ));

        for (routing_id, display_name, expected) in [
            ("", "Input", RuntimeInputConfigurationError::EmptyRoutingId),
            (
                "route\0id",
                "Input",
                RuntimeInputConfigurationError::RoutingIdContainsNul,
            ),
            (
                "route",
                "",
                RuntimeInputConfigurationError::EmptyDisplayName,
            ),
            (
                "route",
                "Input\0name",
                RuntimeInputConfigurationError::DisplayNameContainsNul,
            ),
        ] {
            let actual = RuntimeInputTarget::try_new(routing_id, display_name).unwrap_err();
            assert_eq!(actual.to_string(), expected.to_string());
        }
    }
}
