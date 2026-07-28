use core::error::Error;
use core::fmt;
use std::collections::HashSet;
use std::thread;
use wasapi::{Direction, Role};

/// One active Windows render endpoint available for explicit selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasapiOutputEndpoint {
    device_id: String,
    display_name: String,
    default: bool,
}

impl WasapiOutputEndpoint {
    #[must_use]
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.default
    }
}

/// A consistent snapshot of active Windows render endpoints.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WasapiOutputEndpoints {
    endpoints: Vec<WasapiOutputEndpoint>,
}

impl WasapiOutputEndpoints {
    #[must_use]
    pub fn endpoints(&self) -> &[WasapiOutputEndpoint] {
        &self.endpoints
    }

    #[must_use]
    pub fn default_endpoint(&self) -> Option<&WasapiOutputEndpoint> {
        self.endpoints.iter().find(|endpoint| endpoint.default)
    }

    #[must_use]
    pub fn endpoint_by_id(&self, device_id: &str) -> Option<&WasapiOutputEndpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.device_id == device_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasapiEndpointDiscoveryStage {
    Com,
    Enumerator,
    Collection,
    Count,
    DefaultDevice,
    DefaultIdentifier,
    Device,
    Identifier,
    DisplayName,
    Allocation,
    Snapshot,
}

impl fmt::Display for WasapiEndpointDiscoveryStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Com => "COM initialization",
            Self::Enumerator => "device enumeration",
            Self::Collection => "render endpoint collection",
            Self::Count => "render endpoint count",
            Self::DefaultDevice => "default render endpoint",
            Self::DefaultIdentifier => "default endpoint identifier",
            Self::Device => "render endpoint",
            Self::Identifier => "endpoint identifier",
            Self::DisplayName => "endpoint display name",
            Self::Allocation => "endpoint snapshot allocation",
            Self::Snapshot => "endpoint snapshot validation",
        })
    }
}

#[derive(Debug)]
pub enum WasapiEndpointDiscoveryError {
    ThreadSpawn(std::io::Error),
    ThreadPanicked,
    Backend {
        stage: WasapiEndpointDiscoveryStage,
        endpoint_index: Option<u32>,
        detail: String,
    },
}

impl fmt::Display for WasapiEndpointDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(source) => {
                write!(formatter, "cannot start WASAPI discovery thread: {source}")
            }
            Self::ThreadPanicked => formatter.write_str("WASAPI discovery thread panicked"),
            Self::Backend {
                stage,
                endpoint_index: Some(index),
                detail,
            } => write!(
                formatter,
                "WASAPI {stage} failed for endpoint {index}: {detail}"
            ),
            Self::Backend {
                stage,
                endpoint_index: None,
                detail,
            } => write!(formatter, "WASAPI {stage} failed: {detail}"),
        }
    }
}

impl Error for WasapiEndpointDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(source) => Some(source),
            Self::ThreadPanicked | Self::Backend { .. } => None,
        }
    }
}

/// Discovers active Windows render endpoints on a dedicated COM thread.
///
/// Endpoint IDs are opaque routing keys suitable for
/// [`crate::WasapiOutputConfig::with_device_id`]. Display names are not unique
/// and must not be used as identifiers.
///
/// # Errors
///
/// Returns [`WasapiEndpointDiscoveryError`] if the discovery thread, COM,
/// endpoint enumeration, endpoint properties, allocation, or snapshot
/// validation fails.
pub fn discover_wasapi_output_endpoints()
-> Result<WasapiOutputEndpoints, WasapiEndpointDiscoveryError> {
    thread::Builder::new()
        .name("hyper-wasapi-discovery".to_owned())
        .spawn(discover_on_com_thread)
        .map_err(WasapiEndpointDiscoveryError::ThreadSpawn)?
        .join()
        .map_err(|_| WasapiEndpointDiscoveryError::ThreadPanicked)?
}

fn discover_on_com_thread() -> Result<WasapiOutputEndpoints, WasapiEndpointDiscoveryError> {
    let _com = ComGuard::initialize()?;
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|source| backend_error(WasapiEndpointDiscoveryStage::Enumerator, None, source))?;
    let collection = enumerator
        .get_device_collection(&Direction::Render)
        .map_err(|source| backend_error(WasapiEndpointDiscoveryStage::Collection, None, source))?;
    let count = collection
        .get_nbr_devices()
        .map_err(|source| backend_error(WasapiEndpointDiscoveryStage::Count, None, source))?;
    if count == 0 {
        return Ok(WasapiOutputEndpoints::default());
    }

    let default = enumerator
        .get_default_device_for_role(&Direction::Render, &Role::Console)
        .map_err(|source| {
            backend_error(WasapiEndpointDiscoveryStage::DefaultDevice, None, source)
        })?;
    let default_id = default.get_id().map_err(|source| {
        backend_error(
            WasapiEndpointDiscoveryStage::DefaultIdentifier,
            None,
            source,
        )
    })?;

    let capacity = usize::try_from(count).map_err(|_| WasapiEndpointDiscoveryError::Backend {
        stage: WasapiEndpointDiscoveryStage::Allocation,
        endpoint_index: None,
        detail: "endpoint count does not fit the platform address space".to_owned(),
    })?;
    let mut records = Vec::new();
    records
        .try_reserve_exact(capacity)
        .map_err(|_| WasapiEndpointDiscoveryError::Backend {
            stage: WasapiEndpointDiscoveryStage::Allocation,
            endpoint_index: None,
            detail: "endpoint snapshot capacity is unavailable".to_owned(),
        })?;
    for index in 0..count {
        let device = collection.get_device_at_index(index).map_err(|source| {
            backend_error(WasapiEndpointDiscoveryStage::Device, Some(index), source)
        })?;
        let device_id = device.get_id().map_err(|source| {
            backend_error(
                WasapiEndpointDiscoveryStage::Identifier,
                Some(index),
                source,
            )
        })?;
        let display_name = device.get_friendlyname().map_err(|source| {
            backend_error(
                WasapiEndpointDiscoveryStage::DisplayName,
                Some(index),
                source,
            )
        })?;
        records.push(EndpointRecord {
            device_id,
            display_name,
        });
    }
    assemble_snapshot(records, Some(&default_id))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EndpointRecord {
    device_id: String,
    display_name: String,
}

fn assemble_snapshot(
    records: Vec<EndpointRecord>,
    default_id: Option<&str>,
) -> Result<WasapiOutputEndpoints, WasapiEndpointDiscoveryError> {
    {
        let mut identifiers = HashSet::new();
        identifiers.try_reserve(records.len()).map_err(|_| {
            WasapiEndpointDiscoveryError::Backend {
                stage: WasapiEndpointDiscoveryStage::Allocation,
                endpoint_index: None,
                detail: "endpoint identifier capacity is unavailable".to_owned(),
            }
        })?;
        for record in &records {
            if record.device_id.is_empty() {
                return Err(snapshot_error("endpoint identifier is empty"));
            }
            if record.display_name.is_empty() {
                return Err(snapshot_error("endpoint display name is empty"));
            }
            if !identifiers.insert(record.device_id.as_str()) {
                return Err(snapshot_error("endpoint identifier is duplicated"));
            }
        }
        if default_id.is_some_and(|identifier| !identifiers.contains(identifier)) {
            return Err(snapshot_error(
                "default endpoint is absent from the active render collection",
            ));
        }
    }

    let mut endpoints = Vec::new();
    endpoints.try_reserve_exact(records.len()).map_err(|_| {
        WasapiEndpointDiscoveryError::Backend {
            stage: WasapiEndpointDiscoveryStage::Allocation,
            endpoint_index: None,
            detail: "validated endpoint capacity is unavailable".to_owned(),
        }
    })?;
    for record in records {
        endpoints.push(WasapiOutputEndpoint {
            default: default_id == Some(record.device_id.as_str()),
            device_id: record.device_id,
            display_name: record.display_name,
        });
    }
    endpoints.sort_unstable_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left.device_id.cmp(&right.device_id))
    });
    Ok(WasapiOutputEndpoints { endpoints })
}

fn backend_error(
    stage: WasapiEndpointDiscoveryStage,
    endpoint_index: Option<u32>,
    source: impl fmt::Display,
) -> WasapiEndpointDiscoveryError {
    WasapiEndpointDiscoveryError::Backend {
        stage,
        endpoint_index,
        detail: source.to_string(),
    }
}

fn snapshot_error(detail: &str) -> WasapiEndpointDiscoveryError {
    WasapiEndpointDiscoveryError::Backend {
        stage: WasapiEndpointDiscoveryStage::Snapshot,
        endpoint_index: None,
        detail: detail.to_owned(),
    }
}

struct ComGuard;

impl ComGuard {
    fn initialize() -> Result<Self, WasapiEndpointDiscoveryError> {
        let result = wasapi::initialize_mta();
        if result.is_err() {
            return Err(WasapiEndpointDiscoveryError::Backend {
                stage: WasapiEndpointDiscoveryStage::Com,
                endpoint_index: None,
                detail: format!("{result:?}"),
            });
        }
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(device_id: &str, display_name: &str) -> EndpointRecord {
        EndpointRecord {
            device_id: device_id.to_owned(),
            display_name: display_name.to_owned(),
        }
    }

    #[test]
    fn snapshot_uses_opaque_ids_and_deterministic_presentation_order() {
        let snapshot = assemble_snapshot(
            vec![
                record("endpoint-b", "Speakers"),
                record("endpoint-c", "Headphones"),
                record("endpoint-a", "Speakers"),
            ],
            Some("endpoint-b"),
        )
        .unwrap();

        assert_eq!(
            snapshot
                .endpoints()
                .iter()
                .map(|endpoint| (
                    endpoint.device_id(),
                    endpoint.display_name(),
                    endpoint.is_default()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("endpoint-c", "Headphones", false),
                ("endpoint-a", "Speakers", false),
                ("endpoint-b", "Speakers", true),
            ]
        );
        assert_eq!(
            snapshot
                .default_endpoint()
                .map(WasapiOutputEndpoint::device_id),
            Some("endpoint-b")
        );
        assert_eq!(
            snapshot
                .endpoint_by_id("endpoint-a")
                .map(WasapiOutputEndpoint::display_name),
            Some("Speakers")
        );
    }

    #[test]
    fn snapshot_rejects_ambiguous_or_inconsistent_identity() {
        let duplicate =
            assemble_snapshot(vec![record("same", "A"), record("same", "B")], Some("same"));
        assert!(matches!(
            duplicate,
            Err(WasapiEndpointDiscoveryError::Backend {
                stage: WasapiEndpointDiscoveryStage::Snapshot,
                detail,
                ..
            }) if detail == "endpoint identifier is duplicated"
        ));

        let missing_default =
            assemble_snapshot(vec![record("endpoint-a", "A")], Some("endpoint-b"));
        assert!(matches!(
            missing_default,
            Err(WasapiEndpointDiscoveryError::Backend {
                stage: WasapiEndpointDiscoveryStage::Snapshot,
                detail,
                ..
            }) if detail == "default endpoint is absent from the active render collection"
        ));
    }

    #[test]
    fn empty_snapshot_has_no_default() {
        let snapshot = assemble_snapshot(Vec::new(), None).unwrap();

        assert!(snapshot.endpoints().is_empty());
        assert!(snapshot.default_endpoint().is_none());
    }
}
