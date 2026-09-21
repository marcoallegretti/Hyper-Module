//! Runtime-side ownership of one explicit native-device-chain handback.
//!
//! The controller stays with the sole Runtime playback owner. It maps an
//! operator-selected immutable chain index to verified source topology, waits
//! for a native-device fence, then projects one coherent recovery snapshot
//! into a state-only artifact. It never serializes session handles or writes a
//! project package.

use core::error::Error;
use core::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hyper_desktop_audio::{DesktopDirectControlError, DesktopPlaybackSession};
use hyper_mixer_runtime::{NativeDeviceStateSnapshot, PlaybackRecoverySnapshot};
use hyper_native_device::{DeviceChain, DeviceChainError, NativeDeviceState};
use hyper_playback_json::CompiledMixerGraph;
use hyper_project_loader::PreparedProjectPlayback;
use hyper_project_model::ResourceRole;
use hyper_runtime_native_device_update::{
    RuntimeNativeDeviceUpdate, RuntimeNativeDeviceUpdateDevice, RuntimeNativeDeviceUpdateDirectory,
    RuntimeNativeDeviceUpdateError, RuntimeNativeDeviceUpdateSource,
};
use hyper_storage_engine::StorageError;
use uuid::Uuid;

/// One coherent native-device observation used to complete an explicit export.
///
/// The production adapter constructs this directly from a single recovery
/// snapshot. Fence acknowledgement and positional live state must remain in
/// this one value so they cannot accidentally be read from different cycles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NativeDeviceRecovery {
    committed_native_device_fence: u64,
    native_device_state: Option<NativeDeviceStateSnapshot>,
}

impl From<PlaybackRecoverySnapshot> for NativeDeviceRecovery {
    fn from(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            committed_native_device_fence: snapshot.committed_native_device_fence(),
            native_device_state: snapshot.native_device_state(),
        }
    }
}

/// Minimal Runtime session authority needed for an explicit native-device handback.
///
/// The production adapter keeps `DesktopPlaybackSession` as the only audio
/// owner. The narrow interface makes the fence/recovery boundary testable
/// without exposing a fence as an operator or mapped control command.
pub(super) trait RuntimeNativeDeviceUpdateSession {
    fn native_device_recovery(&self) -> NativeDeviceRecovery;

    fn try_send_native_device_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError>;
}

impl RuntimeNativeDeviceUpdateSession for DesktopPlaybackSession {
    fn native_device_recovery(&self) -> NativeDeviceRecovery {
        self.recovery_checkpoint().snapshot().into()
    }

    fn try_send_native_device_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        DesktopPlaybackSession::try_send_native_device_fence(self)
    }
}

/// Immutable display facts for one exportable source chain.
///
/// `slot` remains Runtime-private selection evidence. It is never included in
/// the artifact and is not supplied by the operator UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeNativeDeviceUpdateChainCatalog {
    pub(super) slot: usize,
    pub(super) track_id: String,
    pub(super) device_count: usize,
}

struct RuntimeNativeDeviceUpdateChainConfiguration {
    slot: usize,
    source_chain: DeviceChain,
    source_chain_bytes: Vec<u8>,
    source_chain_sha256: String,
}

/// Immutable Runtime export facts prepared before native output starts.
pub(super) struct RuntimeNativeDeviceUpdateConfiguration {
    source: RuntimeNativeDeviceUpdateSource,
    chains: Vec<RuntimeNativeDeviceUpdateChainConfiguration>,
    output_directory: Option<RuntimeNativeDeviceUpdateDirectory>,
}

impl RuntimeNativeDeviceUpdateConfiguration {
    /// Builds one export configuration from a structurally verified project.
    ///
    /// Chain order is compiled-mixer slot order, not manifest order or display
    /// order. Each retained chain is paired with exact source bytes and its
    /// descriptor digest before the playback session owns the native layout.
    pub(super) fn try_new(
        source: RuntimeNativeDeviceUpdateSource,
        project: &PreparedProjectPlayback,
        output_directory: Option<RuntimeNativeDeviceUpdateDirectory>,
    ) -> Result<Self, RuntimeNativeDeviceUpdateExportError> {
        let (_, mixer, _, _) = project.session().parts();
        if mixer.project_id() != source.project_id() {
            return Err(RuntimeNativeDeviceUpdateExportError::ProjectCatalogUnavailable);
        }
        let chains = collect_chain_configurations(project, mixer)?;
        Ok(Self {
            source,
            chains,
            output_directory,
        })
    }

    /// Returns stable display facts in the exact private selection order.
    pub(super) fn chain_catalog(
        &self,
    ) -> Result<Vec<RuntimeNativeDeviceUpdateChainCatalog>, RuntimeNativeDeviceUpdateExportError>
    {
        let mut catalog = Vec::new();
        catalog
            .try_reserve_exact(self.chains.len())
            .map_err(|_| RuntimeNativeDeviceUpdateExportError::CapacityUnavailable)?;
        for chain in &self.chains {
            catalog.push(RuntimeNativeDeviceUpdateChainCatalog {
                slot: chain.slot,
                track_id: chain.source_chain.track_id().to_owned(),
                device_count: chain.source_chain.devices().len(),
            });
        }
        Ok(catalog)
    }

    /// Attaches immutable facts to Runtime's sole playback owner.
    #[must_use]
    pub(super) fn attach(self) -> RuntimeNativeDeviceUpdateController {
        let state = if self.output_directory.is_none() {
            "Native-device update unavailable: no external native-device-update directory"
                .to_owned()
        } else if self.chains.is_empty() {
            "Native-device update unavailable: no active device chains".to_owned()
        } else {
            "Waiting for committed Runtime native-device recovery".to_owned()
        };
        RuntimeNativeDeviceUpdateController {
            source: self.source,
            chains: self.chains,
            output_directory: self.output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeNativeDeviceUpdateObservation {
                state,
                export_enabled: false,
            })),
        }
    }
}

fn collect_chain_configurations(
    project: &PreparedProjectPlayback,
    mixer: &CompiledMixerGraph,
) -> Result<Vec<RuntimeNativeDeviceUpdateChainConfiguration>, RuntimeNativeDeviceUpdateExportError>
{
    let package = project.package();
    let source_chains = project.device_chains();
    let mut configurations = Vec::new();
    configurations
        .try_reserve_exact(source_chains.len())
        .map_err(|_| RuntimeNativeDeviceUpdateExportError::CapacityUnavailable)?;
    for (slot, compiled_slot) in mixer.slots().iter().enumerate() {
        let mut matching = source_chains
            .iter()
            .filter(|chain| chain.track_id() == compiled_slot.track_id());
        let Some(source_chain) = matching.next() else {
            continue;
        };
        if matching.next().is_some() {
            return Err(RuntimeNativeDeviceUpdateExportError::DeviceChainTrackAmbiguous);
        }
        let mut descriptors = package.manifest().resources().iter().filter(|descriptor| {
            descriptor.role() == ResourceRole::DeviceChain && descriptor.id() == source_chain.id()
        });
        let descriptor = descriptors
            .next()
            .ok_or(RuntimeNativeDeviceUpdateExportError::DeviceChainResourceMissing)?;
        if descriptors.next().is_some() {
            return Err(RuntimeNativeDeviceUpdateExportError::DeviceChainResourceAmbiguous);
        }
        let source_chain_bytes = package
            .read_resource(descriptor.id())
            .map_err(RuntimeNativeDeviceUpdateExportError::SourceChain)?;
        let canonical_source_chain = source_chain
            .to_canonical_vec()
            .map_err(RuntimeNativeDeviceUpdateExportError::SourceChainSerialization)?;
        if canonical_source_chain != source_chain_bytes {
            return Err(RuntimeNativeDeviceUpdateExportError::ProjectCatalogUnavailable);
        }
        configurations.push(RuntimeNativeDeviceUpdateChainConfiguration {
            slot,
            source_chain: source_chain.clone(),
            source_chain_bytes,
            source_chain_sha256: descriptor.sha256().to_owned(),
        });
    }
    if configurations.len() != source_chains.len() {
        return Err(RuntimeNativeDeviceUpdateExportError::ProjectCatalogUnavailable);
    }
    Ok(configurations)
}

/// A classified refusal or terminal error while exporting a native-device update.
#[derive(Debug)]
pub(super) enum RuntimeNativeDeviceUpdateExportError {
    OutputDirectoryNotConfigured,
    ExportPending,
    NativeDeviceFenceUnavailable,
    NativeDeviceStateUnavailable,
    NativeDeviceChainUnavailable,
    DeviceChainResourceMissing,
    DeviceChainResourceAmbiguous,
    DeviceChainTrackAmbiguous,
    ProjectCatalogUnavailable,
    RecoveryTopologyMismatch,
    CapacityUnavailable,
    SourceChain(StorageError),
    SourceChainSerialization(DeviceChainError),
    Transport(DesktopDirectControlError),
    Update(RuntimeNativeDeviceUpdateError),
}

impl fmt::Display for RuntimeNativeDeviceUpdateExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputDirectoryNotConfigured => formatter.write_str(
                "Runtime was not configured with an external native-device-update output directory",
            ),
            Self::ExportPending => formatter.write_str(
                "a Runtime native-device-update export is already awaiting a fence",
            ),
            Self::NativeDeviceFenceUnavailable => formatter.write_str(
                "Runtime cannot allocate a native-device fence for this export",
            ),
            Self::NativeDeviceStateUnavailable => {
                formatter.write_str("committed Runtime native-device state is unavailable")
            }
            Self::NativeDeviceChainUnavailable => {
                formatter.write_str("the selected Runtime native-device chain is unavailable")
            }
            Self::DeviceChainResourceMissing => {
                formatter.write_str("verified Runtime project is missing a native-device resource")
            }
            Self::DeviceChainResourceAmbiguous => formatter.write_str(
                "verified Runtime project has multiple resources for one native-device chain",
            ),
            Self::DeviceChainTrackAmbiguous => formatter.write_str(
                "verified Runtime project has multiple native-device chains for one mixer track",
            ),
            Self::ProjectCatalogUnavailable => formatter.write_str(
                "verified Runtime project has inconsistent native-device export facts",
            ),
            Self::RecoveryTopologyMismatch => formatter.write_str(
                "committed Runtime native-device state does not match the verified selected chain topology",
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("Runtime native-device-update export capacity is unavailable")
            }
            Self::SourceChain(source) => source.fmt(formatter),
            Self::SourceChainSerialization(source) => source.fmt(formatter),
            Self::Transport(source) => source.fmt(formatter),
            Self::Update(source) => source.fmt(formatter),
        }
    }
}

impl Error for RuntimeNativeDeviceUpdateExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceChain(source) => Some(source),
            Self::SourceChainSerialization(source) => Some(source),
            Self::Transport(source) => Some(source),
            Self::Update(source) => Some(source),
            Self::OutputDirectoryNotConfigured
            | Self::ExportPending
            | Self::NativeDeviceFenceUnavailable
            | Self::NativeDeviceStateUnavailable
            | Self::NativeDeviceChainUnavailable
            | Self::DeviceChainResourceMissing
            | Self::DeviceChainResourceAmbiguous
            | Self::DeviceChainTrackAmbiguous
            | Self::ProjectCatalogUnavailable
            | Self::RecoveryTopologyMismatch
            | Self::CapacityUnavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingRuntimeNativeDeviceUpdate {
    chain_index: usize,
    fence_sequence: u64,
}

/// Runtime-owned presentation state for explicit per-chain operator controls.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RuntimeNativeDeviceUpdateObservation {
    pub(super) state: String,
    pub(super) export_enabled: bool,
}

/// One durable native-device update visible to the IDE's explicit import action.
#[derive(Debug)]
pub(super) struct RuntimeNativeDeviceUpdateExportSuccess {
    pub(super) path: PathBuf,
}

/// Terminal result reported by one Runtime owner turn.
#[derive(Debug)]
pub(super) enum RuntimeNativeDeviceUpdatePoll {
    Saved(RuntimeNativeDeviceUpdateExportSuccess),
    NoChanges,
    Failed(RuntimeNativeDeviceUpdateExportError),
}

/// Runtime-side controller for one-at-a-time explicit per-chain exports.
pub(super) struct RuntimeNativeDeviceUpdateController {
    source: RuntimeNativeDeviceUpdateSource,
    chains: Vec<RuntimeNativeDeviceUpdateChainConfiguration>,
    output_directory: Option<RuntimeNativeDeviceUpdateDirectory>,
    pending: Option<PendingRuntimeNativeDeviceUpdate>,
    last_status: Option<String>,
    observation: Arc<Mutex<RuntimeNativeDeviceUpdateObservation>>,
}

impl RuntimeNativeDeviceUpdateController {
    /// Returns the presentation snapshot observed by the operator UI.
    #[must_use]
    pub(super) fn observation(&self) -> Arc<Mutex<RuntimeNativeDeviceUpdateObservation>> {
        Arc::clone(&self.observation)
    }

    /// Returns whether playback-owner replacement can occur without losing a
    /// requested native-device fence or its eventual publication outcome.
    pub(super) const fn is_idle(&self) -> bool {
        self.pending.is_none()
    }

    /// Queues one explicit boundary after all earlier accepted native-device commands.
    ///
    /// The `chain_index` refers only to this controller's immutable startup
    /// catalog. It neither reads state nor creates a file. Publication waits
    /// for a later coherent recovery snapshot with the matching fence.
    pub(super) fn request_export<S: RuntimeNativeDeviceUpdateSession>(
        &mut self,
        session: &mut S,
        chain_index: usize,
    ) -> Result<u64, RuntimeNativeDeviceUpdateExportError> {
        if self.pending.is_some() {
            return Err(RuntimeNativeDeviceUpdateExportError::ExportPending);
        }
        if self.output_directory.is_none() {
            return Err(RuntimeNativeDeviceUpdateExportError::OutputDirectoryNotConfigured);
        }
        if self.chains.get(chain_index).is_none() {
            return Err(RuntimeNativeDeviceUpdateExportError::NativeDeviceChainUnavailable);
        }
        let fence_sequence = session
            .try_send_native_device_fence()
            .map_err(RuntimeNativeDeviceUpdateExportError::Transport)?
            .ok_or(RuntimeNativeDeviceUpdateExportError::NativeDeviceFenceUnavailable)?;
        self.pending = Some(PendingRuntimeNativeDeviceUpdate {
            chain_index,
            fence_sequence,
        });
        self.last_status = None;
        self.refresh_observation(session);
        Ok(fence_sequence)
    }

    /// Finalizes the pending export only after one coherent fence/state view.
    pub(super) fn poll<S: RuntimeNativeDeviceUpdateSession>(
        &mut self,
        session: &S,
    ) -> Option<RuntimeNativeDeviceUpdatePoll> {
        let recovery = session.native_device_recovery();
        let Some(pending) = self.pending else {
            self.refresh_observation_from_recovery(&recovery);
            return None;
        };
        if recovery.committed_native_device_fence < pending.fence_sequence {
            self.refresh_observation_from_recovery(&recovery);
            return None;
        }

        self.pending = None;
        let poll = if let Some(state) = recovery.native_device_state {
            match self.finish_from_state(pending.chain_index, &state) {
                Ok(Some(saved)) => {
                    self.last_status = Some("Native-device update published".to_owned());
                    RuntimeNativeDeviceUpdatePoll::Saved(saved)
                }
                Ok(None) => {
                    self.last_status = Some("No native-device changes to export".to_owned());
                    RuntimeNativeDeviceUpdatePoll::NoChanges
                }
                Err(error) => {
                    self.last_status = Some(format!("Native-device update failed: {error}"));
                    RuntimeNativeDeviceUpdatePoll::Failed(error)
                }
            }
        } else {
            let error = RuntimeNativeDeviceUpdateExportError::NativeDeviceStateUnavailable;
            self.last_status = Some(format!("Native-device update failed: {error}"));
            RuntimeNativeDeviceUpdatePoll::Failed(error)
        };
        self.refresh_observation_from_recovery(&recovery);
        Some(poll)
    }

    fn finish_from_state<V: NativeDeviceStateView>(
        &self,
        chain_index: usize,
        state: &V,
    ) -> Result<Option<RuntimeNativeDeviceUpdateExportSuccess>, RuntimeNativeDeviceUpdateExportError>
    {
        let chain = self
            .chains
            .get(chain_index)
            .ok_or(RuntimeNativeDeviceUpdateExportError::NativeDeviceChainUnavailable)?;
        let (candidate, devices) =
            project_live_device_state(&chain.source_chain, chain.slot, state)?;
        let candidate_bytes = candidate
            .to_canonical_vec()
            .map_err(RuntimeNativeDeviceUpdateExportError::SourceChainSerialization)?;
        if candidate_bytes == chain.source_chain_bytes {
            return Ok(None);
        }
        let update = RuntimeNativeDeviceUpdate::try_new(
            Uuid::new_v4().to_string(),
            self.source.clone(),
            chain.source_chain.id().to_owned(),
            chain.source_chain.track_id().to_owned(),
            chain.source_chain_sha256.clone(),
            devices,
        )
        .map_err(RuntimeNativeDeviceUpdateExportError::Update)?;
        let output_directory = self
            .output_directory
            .as_ref()
            .ok_or(RuntimeNativeDeviceUpdateExportError::OutputDirectoryNotConfigured)?;
        let path = output_directory
            .publish(&update)
            .map_err(RuntimeNativeDeviceUpdateExportError::Update)?;
        Ok(Some(RuntimeNativeDeviceUpdateExportSuccess { path }))
    }

    fn refresh_observation<S: RuntimeNativeDeviceUpdateSession>(&self, session: &S) {
        self.refresh_observation_from_recovery(&session.native_device_recovery());
    }

    fn refresh_observation_from_recovery(&self, recovery: &NativeDeviceRecovery) {
        let (state, export_enabled) = if let Some(pending) = self.pending {
            (
                format!(
                    "Native-device update queued at fence {}; awaiting a committed audio cycle",
                    pending.fence_sequence
                ),
                false,
            )
        } else if self.output_directory.is_none() {
            (
                "Native-device update unavailable: no external native-device-update directory"
                    .to_owned(),
                false,
            )
        } else if self.chains.is_empty() {
            (
                "Native-device update unavailable: no active device chains".to_owned(),
                false,
            )
        } else if !recovery
            .native_device_state
            .as_ref()
            .is_some_and(|state| self.all_chains_match_state(state))
        {
            (
                "Native-device update unavailable: committed native-device state does not match the verified project"
                    .to_owned(),
                false,
            )
        } else {
            (
                self.last_status.clone().unwrap_or_else(|| {
                    "Ready to export one committed native-device chain".to_owned()
                }),
                true,
            )
        };
        if let Ok(mut observation) = self.observation.lock() {
            *observation = RuntimeNativeDeviceUpdateObservation {
                state,
                export_enabled,
            };
        }
    }

    fn all_chains_match_state(&self, state: &NativeDeviceStateSnapshot) -> bool {
        self.chains
            .iter()
            .all(|chain| chain_matches_live_state(&chain.source_chain, chain.slot, state))
    }
}

trait NativeDeviceStateView {
    fn device_count(&self, slot: usize) -> Option<usize>;

    fn enabled(&self, slot: usize, device: usize) -> Option<bool>;

    fn state(&self, slot: usize, device: usize) -> Option<NativeDeviceState>;
}

impl NativeDeviceStateView for NativeDeviceStateSnapshot {
    fn device_count(&self, slot: usize) -> Option<usize> {
        self.device_count(slot)
    }

    fn enabled(&self, slot: usize, device: usize) -> Option<bool> {
        self.enabled(slot, device)
    }

    fn state(&self, slot: usize, device: usize) -> Option<NativeDeviceState> {
        self.state(slot, device)
    }
}

fn chain_matches_live_state<V: NativeDeviceStateView>(
    chain: &DeviceChain,
    slot: usize,
    state: &V,
) -> bool {
    state.device_count(slot) == Some(chain.devices().len())
        && chain.devices().iter().enumerate().all(|(index, device)| {
            matches!(
                (device.state(), state.state(slot, index)),
                (
                    NativeDeviceState::Filter(_),
                    Some(NativeDeviceState::Filter(_))
                ) | (
                    NativeDeviceState::Compressor(_),
                    Some(NativeDeviceState::Compressor(_))
                )
            ) && state.enabled(slot, index).is_some()
        })
}

fn project_live_device_state<V: NativeDeviceStateView>(
    source_chain: &DeviceChain,
    slot: usize,
    state: &V,
) -> Result<(DeviceChain, Vec<RuntimeNativeDeviceUpdateDevice>), RuntimeNativeDeviceUpdateExportError>
{
    if !chain_matches_live_state(source_chain, slot, state) {
        return Err(RuntimeNativeDeviceUpdateExportError::RecoveryTopologyMismatch);
    }
    let mut candidate = source_chain.clone();
    let mut devices = Vec::new();
    devices
        .try_reserve_exact(source_chain.devices().len())
        .map_err(|_| RuntimeNativeDeviceUpdateExportError::CapacityUnavailable)?;
    for (index, source_device) in source_chain.devices().iter().enumerate() {
        let enabled = state
            .enabled(slot, index)
            .ok_or(RuntimeNativeDeviceUpdateExportError::RecoveryTopologyMismatch)?;
        let live_state = state
            .state(slot, index)
            .ok_or(RuntimeNativeDeviceUpdateExportError::RecoveryTopologyMismatch)?;
        candidate = match (*source_device.state(), live_state) {
            (NativeDeviceState::Filter(_), NativeDeviceState::Filter(filter)) => candidate
                .try_with_filter_state(source_device.id(), enabled, filter)
                .map_err(RuntimeNativeDeviceUpdateExportError::SourceChainSerialization)?,
            (NativeDeviceState::Compressor(_), NativeDeviceState::Compressor(compressor)) => {
                candidate
                    .try_with_compressor_state(source_device.id(), enabled, compressor)
                    .map_err(RuntimeNativeDeviceUpdateExportError::SourceChainSerialization)?
            }
            _ => return Err(RuntimeNativeDeviceUpdateExportError::RecoveryTopologyMismatch),
        };
        devices.push(
            RuntimeNativeDeviceUpdateDevice::try_new(
                source_device.id().to_owned(),
                source_device.device_id().to_owned(),
                source_device.state_version(),
                source_device.dsp_version(),
                enabled,
                live_state,
            )
            .map_err(RuntimeNativeDeviceUpdateExportError::Update)?,
        );
    }
    Ok((candidate, devices))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use hyper_native_device::{
        NATIVE_COMPRESSOR_DEVICE_ID, NATIVE_COMPRESSOR_DSP_VERSION, NATIVE_FILTER_DEVICE_ID,
        NATIVE_FILTER_STATE_VERSION, NativeCompressorState, NativeFilterMode, NativeFilterState,
    };
    use hyper_runtime_native_device_update::read_runtime_native_device_update;

    use super::*;

    const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334455";
    const CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334456";
    const TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334457";
    const FILTER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334458";
    const COMPRESSOR_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334459";
    const SECOND_CHAIN_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334460";
    const SECOND_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334461";
    const SECOND_FILTER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122334462";
    const PROJECT_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BUNDLE_DIGEST: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const CHAIN_DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const SECOND_CHAIN_DIGEST: &str =
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    struct TemporaryDirectory {
        path: PathBuf,
    }

    impl TemporaryDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-runtime-native-device-controller-{}",
                Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TemporaryDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[derive(Clone)]
    struct FakeNativeDeviceState {
        slots: Vec<Vec<(bool, NativeDeviceState)>>,
    }

    impl NativeDeviceStateView for FakeNativeDeviceState {
        fn device_count(&self, slot: usize) -> Option<usize> {
            self.slots.get(slot).map(Vec::len)
        }

        fn enabled(&self, slot: usize, device: usize) -> Option<bool> {
            self.slots
                .get(slot)
                .and_then(|devices| devices.get(device))
                .map(|(enabled, _)| *enabled)
        }

        fn state(&self, slot: usize, device: usize) -> Option<NativeDeviceState> {
            self.slots
                .get(slot)
                .and_then(|devices| devices.get(device))
                .map(|(_, state)| *state)
        }
    }

    #[derive(Clone, Copy)]
    struct FakeSession {
        recovery: NativeDeviceRecovery,
        next_fence: Option<u64>,
        fence_request_count: usize,
    }

    impl RuntimeNativeDeviceUpdateSession for FakeSession {
        fn native_device_recovery(&self) -> NativeDeviceRecovery {
            self.recovery
        }

        fn try_send_native_device_fence(
            &mut self,
        ) -> Result<Option<u64>, DesktopDirectControlError> {
            self.fence_request_count = self.fence_request_count.saturating_add(1);
            Ok(self.next_fence)
        }
    }

    fn source() -> RuntimeNativeDeviceUpdateSource {
        RuntimeNativeDeviceUpdateSource::try_new(
            PROJECT_ID.to_owned(),
            7,
            PROJECT_DIGEST.to_owned(),
            BUNDLE_DIGEST.to_owned(),
            3,
        )
        .unwrap()
    }

    fn filter_state() -> NativeFilterState {
        NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_000).unwrap()
    }

    fn compressor_state() -> NativeCompressorState {
        NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 150_000, 1_500).unwrap()
    }

    fn source_chain() -> DeviceChain {
        DeviceChain::try_new(
            CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            TRACK_ID.to_owned(),
            vec![
                hyper_native_device::NativeDeviceInstance::try_new_filter(
                    FILTER_ID.to_owned(),
                    true,
                    filter_state(),
                )
                .unwrap(),
                hyper_native_device::NativeDeviceInstance::try_new_compressor(
                    COMPRESSOR_ID.to_owned(),
                    false,
                    compressor_state(),
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    fn second_source_chain() -> DeviceChain {
        DeviceChain::try_new(
            SECOND_CHAIN_ID.to_owned(),
            PROJECT_ID.to_owned(),
            SECOND_TRACK_ID.to_owned(),
            vec![
                hyper_native_device::NativeDeviceInstance::try_new_filter(
                    SECOND_FILTER_ID.to_owned(),
                    true,
                    filter_state(),
                )
                .unwrap(),
            ],
        )
        .unwrap()
    }

    fn state(
        filter: NativeFilterState,
        compressor: NativeCompressorState,
    ) -> FakeNativeDeviceState {
        FakeNativeDeviceState {
            slots: vec![vec![
                (true, NativeDeviceState::Filter(filter)),
                (false, NativeDeviceState::Compressor(compressor)),
            ]],
        }
    }

    fn controller(
        output_directory: Option<RuntimeNativeDeviceUpdateDirectory>,
    ) -> RuntimeNativeDeviceUpdateController {
        let source_chain = source_chain();
        let source_chain_bytes = source_chain.to_canonical_vec().unwrap();
        RuntimeNativeDeviceUpdateController {
            source: source(),
            chains: vec![RuntimeNativeDeviceUpdateChainConfiguration {
                slot: 0,
                source_chain,
                source_chain_bytes,
                source_chain_sha256: CHAIN_DIGEST.to_owned(),
            }],
            output_directory,
            pending: None,
            last_status: None,
            observation: Arc::new(Mutex::new(RuntimeNativeDeviceUpdateObservation {
                state: "fixture".to_owned(),
                export_enabled: false,
            })),
        }
    }

    #[test]
    fn projection_keeps_exact_source_topology_and_rejects_mismatch() {
        let source_chain = source_chain();
        let retuned =
            NativeFilterState::try_new(NativeFilterMode::HighPass, 1_500_000, 900_000).unwrap();
        let (candidate, devices) =
            project_live_device_state(&source_chain, 0, &state(retuned, compressor_state()))
                .unwrap();
        assert_eq!(candidate.id(), CHAIN_ID);
        assert_eq!(candidate.track_id(), TRACK_ID);
        assert_eq!(candidate.devices().len(), 2);
        assert_eq!(candidate.devices()[0].id(), FILTER_ID);
        assert_eq!(candidate.devices()[0].filter_state(), Some(retuned));
        assert_eq!(devices[0].device_id(), NATIVE_FILTER_DEVICE_ID);
        assert_eq!(devices[1].device_id(), NATIVE_COMPRESSOR_DEVICE_ID);
        assert_eq!(devices[0].state_version(), NATIVE_FILTER_STATE_VERSION);
        assert_eq!(devices[1].dsp_version(), NATIVE_COMPRESSOR_DSP_VERSION);

        let mismatch = FakeNativeDeviceState {
            slots: vec![vec![
                (true, NativeDeviceState::Compressor(compressor_state())),
                (false, NativeDeviceState::Compressor(compressor_state())),
            ]],
        };
        assert!(matches!(
            project_live_device_state(&source_chain, 0, &mismatch),
            Err(RuntimeNativeDeviceUpdateExportError::RecoveryTopologyMismatch)
        ));
    }

    #[test]
    fn controller_waits_for_fence_and_publishes_only_changed_selected_chain() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeNativeDeviceUpdateDirectory::open(temporary.path()).unwrap();
        let mut controller = controller(Some(output));
        let mut session = FakeSession {
            recovery: NativeDeviceRecovery {
                committed_native_device_fence: 0,
                native_device_state: None,
            },
            next_fence: Some(7),
            fence_request_count: 0,
        };

        assert_eq!(controller.request_export(&mut session, 0).unwrap(), 7);
        assert_eq!(session.fence_request_count, 1);
        assert!(controller.poll(&session).is_none());
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());

        session.recovery.committed_native_device_fence = 7;
        assert!(matches!(
            controller.poll(&session),
            Some(RuntimeNativeDeviceUpdatePoll::Failed(
                RuntimeNativeDeviceUpdateExportError::NativeDeviceStateUnavailable
            ))
        ));

        let retuned =
            NativeFilterState::try_new(NativeFilterMode::HighPass, 1_500_000, 900_000).unwrap();
        let saved = controller
            .finish_from_state(0, &state(retuned, compressor_state()))
            .unwrap()
            .expect("a changed source chain must publish one update");
        let update = read_runtime_native_device_update(&saved.path).unwrap();
        assert_eq!(update.source(), &source());
        assert_eq!(update.chain_id(), CHAIN_ID);
        assert_eq!(update.track_id(), TRACK_ID);
        assert_eq!(update.source_chain_sha256(), CHAIN_DIGEST);
        assert_eq!(
            update.devices()[0].state(),
            NativeDeviceState::Filter(retuned)
        );
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn selected_chain_index_uses_its_verified_mixer_slot_not_its_display_position() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeNativeDeviceUpdateDirectory::open(temporary.path()).unwrap();
        let mut export_controller = controller(Some(output));
        let source_chain = second_source_chain();
        let source_chain_bytes = source_chain.to_canonical_vec().unwrap();
        export_controller
            .chains
            .push(RuntimeNativeDeviceUpdateChainConfiguration {
                slot: 2,
                source_chain,
                source_chain_bytes,
                source_chain_sha256: SECOND_CHAIN_DIGEST.to_owned(),
            });
        let retuned =
            NativeFilterState::try_new(NativeFilterMode::HighPass, 1_250_000, 800_000).unwrap();
        let state = FakeNativeDeviceState {
            slots: vec![
                Vec::new(),
                Vec::new(),
                vec![(false, NativeDeviceState::Filter(retuned))],
            ],
        };

        let saved = export_controller
            .finish_from_state(1, &state)
            .unwrap()
            .expect("the second catalog entry must use its retained mixer slot");
        let update = read_runtime_native_device_update(&saved.path).unwrap();
        assert_eq!(update.chain_id(), SECOND_CHAIN_ID);
        assert_eq!(update.track_id(), SECOND_TRACK_ID);
        assert_eq!(update.source_chain_sha256(), SECOND_CHAIN_DIGEST);
        assert_eq!(update.devices().len(), 1);
        assert_eq!(update.devices()[0].id(), SECOND_FILTER_ID);
        assert!(!update.devices()[0].enabled());
        assert_eq!(
            update.devices()[0].state(),
            NativeDeviceState::Filter(retuned)
        );
    }

    #[test]
    fn unchanged_or_unavailable_chain_never_creates_an_artifact() {
        let temporary = TemporaryDirectory::new();
        let output = RuntimeNativeDeviceUpdateDirectory::open(temporary.path()).unwrap();
        let export_controller = controller(Some(output));
        assert!(
            export_controller
                .finish_from_state(0, &state(filter_state(), compressor_state()))
                .unwrap()
                .is_none()
        );
        assert!(fs::read_dir(temporary.path()).unwrap().next().is_none());
        assert!(matches!(
            export_controller.finish_from_state(1, &state(filter_state(), compressor_state())),
            Err(RuntimeNativeDeviceUpdateExportError::NativeDeviceChainUnavailable)
        ));

        let mut disabled = controller(None);
        let mut session = FakeSession {
            recovery: NativeDeviceRecovery {
                committed_native_device_fence: 0,
                native_device_state: None,
            },
            next_fence: Some(1),
            fence_request_count: 0,
        };
        assert!(matches!(
            disabled.request_export(&mut session, 0),
            Err(RuntimeNativeDeviceUpdateExportError::OutputDirectoryNotConfigured)
        ));
        assert_eq!(session.fence_request_count, 0);
    }
}
