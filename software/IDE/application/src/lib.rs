#![doc = "Single-owner desktop IDE application state above native audio playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hyper_audio_engine::{LinearGain, Pan, ParameterError};
use hyper_audio_source::{DecodeError, DecodeLimits};
pub use hyper_desktop_audio::DesktopSceneAvailability as IdeSceneAvailability;
use hyper_desktop_audio::{
    CaptureStreamSnapshot, DesktopDirectControlError, DesktopInputConfig, DesktopInputConfigError,
    DesktopInputDiscoveryError, DesktopInputStopError, DesktopMonitoringConfig,
    DesktopMonitoringConfigError, DesktopMonitoringControlError, DesktopOutputConfig,
    DesktopOutputConfigError, DesktopOutputDiscoveryError, DesktopOutputStopError,
    DesktopPlaybackRecovery, DesktopPlaybackRecoveryPendingStop,
    DesktopPlaybackRecoveryPendingStopWait, DesktopPlaybackSession, DesktopProjectOpenError,
    MonitoringSnapshot, OutputStreamSnapshot, OutputStreamState, StreamState,
    discover_desktop_input_targets, discover_desktop_output_targets,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{
    LatestOutputMeterSnapshot, MixerParameterCommand, MixerParameterResolveError,
    PlaybackRecoverySnapshot,
};
pub use hyper_mixer_runtime::{LocateState, PlaybackRecoveryTransportState, TransportCommand};
#[cfg(test)]
use hyper_mixer_runtime::{OutputMeterReceiver, output_meter_channel};
use hyper_playback_json::{
    CompiledMixerTap, CompiledMixerTrackKind, LoaderLimits, MixerBaseState, MixerChannelBaseState,
    MixerSendBaseState, MixerStateSerializationError, serialize_mixer_base_state,
};
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_project_model::ResourceRole;
use hyper_storage_engine::{
    ProjectResourcePublicationError, ProjectResourcePublicationOutcome,
    ProjectResourcePublicationRequest, StorageError, publish_project_resource,
};

mod worker;

pub use worker::{
    DesktopIdeWorker, DesktopIdeWorkerPendingShutdown, IdeClosedSnapshot, IdeMixerChannel,
    IdeMixerSend, IdeRunningObservation, IdeScene, IdeWorkerCommand, IdeWorkerCommandFailure,
    IdeWorkerCommandKind, IdeWorkerEvent, IdeWorkerEventReceiveError, IdeWorkerJoinError,
    IdeWorkerPendingShutdownWait, IdeWorkerSendError, IdeWorkerShutdownOutcome,
    IdeWorkerShutdownReport, IdeWorkerStartError, MAX_IDE_WORKER_COMMANDS, MAX_IDE_WORKER_EVENTS,
    desktop_ide_worker_supervisor_snapshot,
};

const DESKTOP_MAXIMUM_BLOCK_FRAMES: usize = 4_096;
const DESKTOP_MONITORING_TRANSPORT_FRAMES: usize = DESKTOP_MAXIMUM_BLOCK_FRAMES * 2;
const DESKTOP_MONITORING_TARGET_FRAMES: usize = 256;
const DESKTOP_MONITORING_CHUNK_FRAMES: usize = 64;
const MONITOR_GAIN_MILLIONTHS_MAXIMUM: u32 = 1_000_000;
const DESKTOP_MAX_AUDIO_FILE_BYTES: u64 = 512 * 1_024 * 1_024;
const DESKTOP_MAX_AUDIO_FRAMES: u64 = 43_200_000;
const DESKTOP_MAX_AUDIO_DECODED_BYTES: usize = 2 * 43_200_000 * size_of::<f32>();
const DESKTOP_MAX_PROJECT_RESOURCE_BYTES: usize = 1_024 * 1_024 * 1_024;
const DESKTOP_MAX_PROJECT_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
/// Maximum scene metadata rows published by the version-1 desktop IDE.
pub const MAX_IDE_SCENES: usize = LoaderLimits::VERSION_1.max_scene_documents;
/// Maximum channel rows published by the version-1 desktop IDE mixer.
pub const MAX_IDE_MIXER_CHANNELS: usize = hyper_mixer_runtime::MAX_MAIN_MIXER_SLOTS;

/// Builds the bounded playback policy used by the desktop IDE executable.
///
/// The policy accepts version-1 documents, at most fifteen minutes of 48 kHz
/// audio per decoded asset, and at most one GiB each of declared resources and
/// cached decoded sample planes. These are desktop software limits, not evidence
/// of embedded hardware capacity.
///
/// # Errors
///
/// Returns an invariant error if the fixed decode or output policy is invalid.
pub fn desktop_ide_application_config()
-> Result<IdeApplicationConfig, DesktopIdeApplicationConfigError> {
    let audio = DecodeLimits::new(
        DESKTOP_MAX_AUDIO_FILE_BYTES,
        DESKTOP_MAX_AUDIO_FRAMES,
        DESKTOP_MAX_AUDIO_DECODED_BYTES,
    )
    .map_err(DesktopIdeApplicationConfigError::Decode)?;
    IdeApplicationConfig::try_new(
        DESKTOP_MAXIMUM_BLOCK_FRAMES,
        ProjectPlaybackLimits {
            max_total_resource_bytes: DESKTOP_MAX_PROJECT_RESOURCE_BYTES,
            audio,
            session: PlaybackSessionAssemblyLimits {
                documents: LoaderLimits::VERSION_1,
                media_cache: CacheLimits {
                    max_assets: LoaderLimits::VERSION_1.max_clip_documents,
                    max_frames_per_asset: usize::try_from(DESKTOP_MAX_AUDIO_FRAMES)
                        .map_err(|_| DesktopIdeApplicationConfigError::FrameLimitUnavailable)?,
                    max_decoded_sample_bytes: DESKTOP_MAX_PROJECT_DECODED_BYTES,
                },
            },
        },
    )
    .map_err(DesktopIdeApplicationConfigError::Application)
}

#[derive(Debug)]
pub enum DesktopIdeApplicationConfigError {
    Decode(DecodeError),
    FrameLimitUnavailable,
    Application(IdeApplicationConfigError),
}

impl fmt::Display for DesktopIdeApplicationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(source) => source.fmt(formatter),
            Self::FrameLimitUnavailable => {
                formatter.write_str("desktop IDE audio frame limit is unavailable on this platform")
            }
            Self::Application(source) => source.fmt(formatter),
        }
    }
}

impl Error for DesktopIdeApplicationConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Decode(source) => Some(source),
            Self::Application(source) => Some(source),
            Self::FrameLimitUnavailable => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeAudioOutput {
    routing_id: String,
    display_name: String,
}

impl IdeAudioOutput {
    /// Creates one owned output descriptor from an opaque routing identity.
    ///
    /// # Errors
    ///
    /// Returns [`OutputInventoryError`] for an empty field or embedded NUL.
    pub fn try_new(routing_id: &str, display_name: &str) -> Result<Self, OutputInventoryError> {
        if routing_id.is_empty() {
            return Err(OutputInventoryError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(OutputInventoryError::RoutingIdContainsNul);
        }
        if display_name.is_empty() {
            return Err(OutputInventoryError::EmptyDisplayName);
        }
        if display_name.as_bytes().contains(&0) {
            return Err(OutputInventoryError::DisplayNameContainsNul);
        }
        Ok(Self {
            routing_id: try_owned(routing_id)?,
            display_name: try_owned(display_name)?,
        })
    }

    #[must_use]
    pub fn routing_id(&self) -> &str {
        &self.routing_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    fn try_to_owned(&self) -> Result<Self, OutputInventoryError> {
        Self::try_new(&self.routing_id, &self.display_name)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputInventoryError {
    CapacityUnavailable,
    EmptyRoutingId,
    RoutingIdContainsNul,
    EmptyDisplayName,
    DisplayNameContainsNul,
    DuplicateRoutingId { output_index: usize },
}

impl fmt::Display for OutputInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("audio output inventory capacity is unavailable")
            }
            Self::EmptyRoutingId => formatter.write_str("audio output routing identity is empty"),
            Self::RoutingIdContainsNul => {
                formatter.write_str("audio output routing identity contains an embedded NUL")
            }
            Self::EmptyDisplayName => formatter.write_str("audio output display name is empty"),
            Self::DisplayNameContainsNul => {
                formatter.write_str("audio output display name contains an embedded NUL")
            }
            Self::DuplicateRoutingId { output_index } => write!(
                formatter,
                "audio output {output_index} duplicates an earlier routing identity"
            ),
        }
    }
}

impl Error for OutputInventoryError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdeAudioOutputInventory {
    outputs: Vec<IdeAudioOutput>,
}

impl IdeAudioOutputInventory {
    /// Copies and validates one deterministic output snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`OutputInventoryError`] without publishing a partial inventory.
    pub fn try_new(outputs: &[(&str, &str)]) -> Result<Self, OutputInventoryError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(outputs.len())
            .map_err(|_| OutputInventoryError::CapacityUnavailable)?;
        for (output_index, (routing_id, display_name)) in outputs.iter().enumerate() {
            if owned
                .iter()
                .any(|output: &IdeAudioOutput| output.routing_id() == *routing_id)
            {
                return Err(OutputInventoryError::DuplicateRoutingId { output_index });
            }
            owned.push(IdeAudioOutput::try_new(routing_id, display_name)?);
        }
        Ok(Self { outputs: owned })
    }

    fn discover() -> Result<Self, IdeOutputDiscoveryError> {
        let discovered =
            discover_desktop_output_targets().map_err(IdeOutputDiscoveryError::Backend)?;
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(discovered.targets().len())
            .map_err(|_| {
                IdeOutputDiscoveryError::Inventory(OutputInventoryError::CapacityUnavailable)
            })?;
        for target in discovered.targets() {
            if outputs
                .iter()
                .any(|output: &IdeAudioOutput| output.routing_id() == target.routing_id())
            {
                return Err(IdeOutputDiscoveryError::Inventory(
                    OutputInventoryError::DuplicateRoutingId {
                        output_index: outputs.len(),
                    },
                ));
            }
            outputs.push(
                IdeAudioOutput::try_new(target.routing_id(), target.display_name())
                    .map_err(IdeOutputDiscoveryError::Inventory)?,
            );
        }
        Ok(Self { outputs })
    }

    #[must_use]
    pub fn outputs(&self) -> &[IdeAudioOutput] {
        &self.outputs
    }

    #[must_use]
    pub fn find(&self, routing_id: &str) -> Option<&IdeAudioOutput> {
        self.outputs
            .iter()
            .find(|output| output.routing_id == routing_id)
    }

    fn try_to_owned(&self) -> Result<Self, OutputInventoryError> {
        let mut outputs = Vec::new();
        outputs
            .try_reserve_exact(self.outputs.len())
            .map_err(|_| OutputInventoryError::CapacityUnavailable)?;
        for output in &self.outputs {
            outputs.push(output.try_to_owned()?);
        }
        Ok(Self { outputs })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeAudioInput {
    routing_id: String,
    display_name: String,
}

impl IdeAudioInput {
    /// Creates one owned input descriptor from an opaque routing identity.
    ///
    /// # Errors
    ///
    /// Returns [`InputInventoryError`] for an empty field or embedded NUL.
    pub fn try_new(routing_id: &str, display_name: &str) -> Result<Self, InputInventoryError> {
        if routing_id.is_empty() {
            return Err(InputInventoryError::EmptyRoutingId);
        }
        if routing_id.as_bytes().contains(&0) {
            return Err(InputInventoryError::RoutingIdContainsNul);
        }
        if display_name.is_empty() {
            return Err(InputInventoryError::EmptyDisplayName);
        }
        if display_name.as_bytes().contains(&0) {
            return Err(InputInventoryError::DisplayNameContainsNul);
        }
        Ok(Self {
            routing_id: try_input_owned(routing_id)?,
            display_name: try_input_owned(display_name)?,
        })
    }

    #[must_use]
    pub fn routing_id(&self) -> &str {
        &self.routing_id
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    fn try_to_owned(&self) -> Result<Self, InputInventoryError> {
        Self::try_new(&self.routing_id, &self.display_name)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputInventoryError {
    CapacityUnavailable,
    EmptyRoutingId,
    RoutingIdContainsNul,
    EmptyDisplayName,
    DisplayNameContainsNul,
    DuplicateRoutingId { input_index: usize },
}

impl fmt::Display for InputInventoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("audio input inventory capacity is unavailable")
            }
            Self::EmptyRoutingId => formatter.write_str("audio input routing identity is empty"),
            Self::RoutingIdContainsNul => {
                formatter.write_str("audio input routing identity contains an embedded NUL")
            }
            Self::EmptyDisplayName => formatter.write_str("audio input display name is empty"),
            Self::DisplayNameContainsNul => {
                formatter.write_str("audio input display name contains an embedded NUL")
            }
            Self::DuplicateRoutingId { input_index } => write!(
                formatter,
                "audio input {input_index} duplicates an earlier routing identity"
            ),
        }
    }
}

impl Error for InputInventoryError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdeAudioInputInventory {
    inputs: Vec<IdeAudioInput>,
}

impl IdeAudioInputInventory {
    /// Copies and validates one deterministic input snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`InputInventoryError`] without publishing a partial inventory.
    pub fn try_new(inputs: &[(&str, &str)]) -> Result<Self, InputInventoryError> {
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(inputs.len())
            .map_err(|_| InputInventoryError::CapacityUnavailable)?;
        for (input_index, (routing_id, display_name)) in inputs.iter().enumerate() {
            if owned
                .iter()
                .any(|input: &IdeAudioInput| input.routing_id() == *routing_id)
            {
                return Err(InputInventoryError::DuplicateRoutingId { input_index });
            }
            owned.push(IdeAudioInput::try_new(routing_id, display_name)?);
        }
        Ok(Self { inputs: owned })
    }

    fn discover() -> Result<Self, IdeInputDiscoveryError> {
        let discovered =
            discover_desktop_input_targets().map_err(IdeInputDiscoveryError::Backend)?;
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(discovered.targets().len())
            .map_err(|_| {
                IdeInputDiscoveryError::Inventory(InputInventoryError::CapacityUnavailable)
            })?;
        for target in discovered.targets() {
            if inputs
                .iter()
                .any(|input: &IdeAudioInput| input.routing_id() == target.routing_id())
            {
                return Err(IdeInputDiscoveryError::Inventory(
                    InputInventoryError::DuplicateRoutingId {
                        input_index: inputs.len(),
                    },
                ));
            }
            inputs.push(
                IdeAudioInput::try_new(target.routing_id(), target.display_name())
                    .map_err(IdeInputDiscoveryError::Inventory)?,
            );
        }
        Ok(Self { inputs })
    }

    #[must_use]
    pub fn inputs(&self) -> &[IdeAudioInput] {
        &self.inputs
    }

    #[must_use]
    pub fn find(&self, routing_id: &str) -> Option<&IdeAudioInput> {
        self.inputs
            .iter()
            .find(|input| input.routing_id == routing_id)
    }

    fn try_to_owned(&self) -> Result<Self, InputInventoryError> {
        let mut inputs = Vec::new();
        inputs
            .try_reserve_exact(self.inputs.len())
            .map_err(|_| InputInventoryError::CapacityUnavailable)?;
        for input in &self.inputs {
            inputs.push(input.try_to_owned()?);
        }
        Ok(Self { inputs })
    }
}

#[derive(Debug)]
pub enum IdeInputDiscoveryError {
    Backend(DesktopInputDiscoveryError),
    Inventory(InputInventoryError),
}

impl fmt::Display for IdeInputDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(source) => source.fmt(formatter),
            Self::Inventory(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeInputDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source),
            Self::Inventory(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeOutputDiscoveryError {
    Backend(DesktopOutputDiscoveryError),
    Inventory(OutputInventoryError),
}

impl fmt::Display for IdeOutputDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(source) => source.fmt(formatter),
            Self::Inventory(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeOutputDiscoveryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source),
            Self::Inventory(source) => Some(source),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeOutputSelection {
    SystemDefault,
    Routed(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeInputSelection {
    Disabled,
    SystemDefault,
    Routed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeApplicationConfigError {
    Output(DesktopOutputConfigError),
    Input(DesktopInputConfigError),
    Monitoring(DesktopMonitoringConfigError),
}

impl fmt::Display for IdeApplicationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output(source) => source.fmt(formatter),
            Self::Input(source) => source.fmt(formatter),
            Self::Monitoring(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeApplicationConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output(source) => Some(source),
            Self::Input(source) => Some(source),
            Self::Monitoring(source) => Some(source),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdeApplicationConfig {
    maximum_block_frames: usize,
    project_limits: ProjectPlaybackLimits,
}

impl IdeApplicationConfig {
    /// Creates the fixed 48 kHz desktop application policy.
    ///
    /// # Errors
    ///
    /// Returns [`IdeApplicationConfigError`] when callback capacity is invalid.
    pub fn try_new(
        maximum_block_frames: usize,
        project_limits: ProjectPlaybackLimits,
    ) -> Result<Self, IdeApplicationConfigError> {
        let output = DesktopOutputConfig::try_new(48_000, maximum_block_frames)
            .map_err(IdeApplicationConfigError::Output)?;
        let input = DesktopInputConfig::try_new(48_000, maximum_block_frames)
            .map_err(IdeApplicationConfigError::Input)?;
        DesktopMonitoringConfig::try_new(
            input,
            output,
            DESKTOP_MONITORING_TRANSPORT_FRAMES,
            DESKTOP_MONITORING_TARGET_FRAMES,
            DESKTOP_MONITORING_CHUNK_FRAMES,
        )
        .map_err(IdeApplicationConfigError::Monitoring)?;
        Ok(Self {
            maximum_block_frames,
            project_limits,
        })
    }

    #[must_use]
    pub const fn maximum_block_frames(self) -> usize {
        self.maximum_block_frames
    }

    #[must_use]
    pub const fn project_limits(self) -> ProjectPlaybackLimits {
        self.project_limits
    }
}

#[derive(Debug)]
pub struct DesktopIdeApplication {
    config: IdeApplicationConfig,
    outputs: IdeAudioOutputInventory,
    output_selection: IdeOutputSelection,
    inputs: Option<IdeAudioInputInventory>,
    input_selection: IdeInputSelection,
}

#[derive(Debug)]
pub struct IdeApplicationDiscovery {
    application: DesktopIdeApplication,
    input_failure: Option<IdeInputDiscoveryError>,
}

impl IdeApplicationDiscovery {
    #[must_use]
    pub const fn application(&self) -> &DesktopIdeApplication {
        &self.application
    }

    #[must_use]
    pub const fn input_failure(&self) -> Option<&IdeInputDiscoveryError> {
        self.input_failure.as_ref()
    }

    #[must_use]
    pub fn into_parts(self) -> (DesktopIdeApplication, Option<IdeInputDiscoveryError>) {
        (self.application, self.input_failure)
    }
}

impl DesktopIdeApplication {
    /// Discovers outputs and publishes one closed application owner.
    ///
    /// # Errors
    ///
    /// Returns native discovery or fallible inventory construction errors.
    pub fn discover(
        config: IdeApplicationConfig,
    ) -> Result<IdeApplicationDiscovery, IdeOutputDiscoveryError> {
        let outputs = IdeAudioOutputInventory::discover()?;
        let (inputs, input_failure) = match IdeAudioInputInventory::discover() {
            Ok(inputs) => (Some(inputs), None),
            Err(error) => (None, Some(error)),
        };
        Ok(IdeApplicationDiscovery {
            application: Self {
                config,
                outputs,
                output_selection: IdeOutputSelection::SystemDefault,
                inputs,
                input_selection: IdeInputSelection::Disabled,
            },
            input_failure,
        })
    }

    #[must_use]
    pub const fn from_inventory(
        config: IdeApplicationConfig,
        outputs: IdeAudioOutputInventory,
    ) -> Self {
        Self {
            config,
            outputs,
            output_selection: IdeOutputSelection::SystemDefault,
            inputs: None,
            input_selection: IdeInputSelection::Disabled,
        }
    }

    #[must_use]
    pub const fn from_inventories(
        config: IdeApplicationConfig,
        outputs: IdeAudioOutputInventory,
        inputs: IdeAudioInputInventory,
    ) -> Self {
        Self {
            config,
            outputs,
            output_selection: IdeOutputSelection::SystemDefault,
            inputs: Some(inputs),
            input_selection: IdeInputSelection::Disabled,
        }
    }

    /// Replaces the device snapshot only after complete rediscovery.
    ///
    /// # Errors
    ///
    /// Returns discovery errors while retaining the previous inventory.
    pub fn refresh_outputs(&mut self) -> Result<(), IdeOutputDiscoveryError> {
        let outputs = IdeAudioOutputInventory::discover()?;
        self.outputs = outputs;
        Ok(())
    }

    /// Replaces the input snapshot only after complete rediscovery.
    ///
    /// # Errors
    ///
    /// Returns discovery errors while retaining the previous inventory and
    /// selection. A previous routed selection can become unavailable after a
    /// successful refresh and is then retained as explicit stale state.
    pub fn refresh_inputs(&mut self) -> Result<(), IdeInputDiscoveryError> {
        let inputs = IdeAudioInputInventory::discover()?;
        self.inputs = Some(inputs);
        Ok(())
    }

    #[must_use]
    pub fn outputs(&self) -> &IdeAudioOutputInventory {
        &self.outputs
    }

    #[must_use]
    pub fn output_selection(&self) -> &IdeOutputSelection {
        &self.output_selection
    }

    #[must_use]
    pub fn selection(&self) -> &IdeOutputSelection {
        self.output_selection()
    }

    pub fn select_system_default(&mut self) {
        self.output_selection = IdeOutputSelection::SystemDefault;
    }

    /// Selects one routing identity present in the latest device snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`IdeOutputSelectionError`] without changing selection.
    pub fn select_output(&mut self, routing_id: &str) -> Result<(), IdeOutputSelectionError> {
        if self.outputs.find(routing_id).is_none() {
            return Err(IdeOutputSelectionError::UnknownOutput);
        }
        self.output_selection = IdeOutputSelection::Routed(
            try_owned(routing_id).map_err(|_| IdeOutputSelectionError::CapacityUnavailable)?,
        );
        Ok(())
    }

    #[must_use]
    pub const fn inputs(&self) -> Option<&IdeAudioInputInventory> {
        self.inputs.as_ref()
    }

    #[must_use]
    pub const fn input_selection(&self) -> &IdeInputSelection {
        &self.input_selection
    }

    pub fn disable_input_monitoring(&mut self) {
        self.input_selection = IdeInputSelection::Disabled;
    }

    /// Selects the current operating-system default input for cue monitoring.
    ///
    /// # Errors
    ///
    /// Returns [`IdeInputSelectionError`] until input discovery has succeeded
    /// and published at least one routable capture target.
    pub fn select_system_default_input(&mut self) -> Result<(), IdeInputSelectionError> {
        let Some(inputs) = self.inputs.as_ref() else {
            return Err(IdeInputSelectionError::DiscoveryUnavailable);
        };
        if inputs.inputs().is_empty() {
            return Err(IdeInputSelectionError::NoInputsAvailable);
        }
        self.input_selection = IdeInputSelection::SystemDefault;
        Ok(())
    }

    /// Selects one discovered input for cue monitoring.
    ///
    /// # Errors
    ///
    /// Returns [`IdeInputSelectionError`] without changing selection.
    pub fn select_input(&mut self, routing_id: &str) -> Result<(), IdeInputSelectionError> {
        let Some(inputs) = self.inputs.as_ref() else {
            return Err(IdeInputSelectionError::DiscoveryUnavailable);
        };
        if inputs.find(routing_id).is_none() {
            return Err(IdeInputSelectionError::UnknownInput);
        }
        self.input_selection = IdeInputSelection::Routed(
            try_input_owned(routing_id).map_err(|_| IdeInputSelectionError::CapacityUnavailable)?,
        );
        Ok(())
    }

    /// Opens one verified project and transfers the closed owner into running state.
    ///
    /// # Errors
    ///
    /// Returns the unchanged closed application together with the failure.
    #[allow(
        clippy::result_large_err,
        reason = "the failure retains the sole closed application owner without allocating after project-open failure"
    )]
    pub fn open_project(
        self,
        root: impl AsRef<Path>,
    ) -> Result<RunningDesktopIdeApplication, IdeProjectOpenFailure> {
        let output = match self.output_config() {
            Ok(output) => output,
            Err(reason) => {
                return Err(IdeProjectOpenFailure {
                    application: self,
                    reason,
                });
            }
        };
        let monitoring = match self.monitoring_config(output.clone()) {
            Ok(monitoring) => monitoring,
            Err(reason) => {
                return Err(IdeProjectOpenFailure {
                    application: self,
                    reason,
                });
            }
        };
        let opened = match monitoring {
            Some(monitoring) => DesktopPlaybackSession::open_project_monitored(
                monitoring,
                root,
                self.config.project_limits,
            ),
            None => DesktopPlaybackSession::open_project(output, root, self.config.project_limits),
        };
        match opened {
            Ok(session) => Ok(RunningDesktopIdeApplication {
                application: self,
                session: IdePlaybackSessionOwner::Native(session),
            }),
            Err(source) => Err(IdeProjectOpenFailure {
                application: self,
                reason: IdeProjectOpenError::Project(source),
            }),
        }
    }

    fn output_config(&self) -> Result<DesktopOutputConfig, IdeProjectOpenError> {
        let output = DesktopOutputConfig::try_new(48_000, self.config.maximum_block_frames)
            .map_err(IdeProjectOpenError::OutputConfig)?;
        match &self.output_selection {
            IdeOutputSelection::SystemDefault => Ok(output),
            IdeOutputSelection::Routed(routing_id) => {
                if self.outputs.find(routing_id).is_none() {
                    return Err(IdeProjectOpenError::SelectedOutputUnavailable);
                }
                output
                    .with_routing_id(routing_id)
                    .map_err(IdeProjectOpenError::OutputConfig)
            }
        }
    }

    fn monitoring_config(
        &self,
        output: DesktopOutputConfig,
    ) -> Result<Option<DesktopMonitoringConfig>, IdeProjectOpenError> {
        let input = match &self.input_selection {
            IdeInputSelection::Disabled => return Ok(None),
            IdeInputSelection::SystemDefault => {
                let inputs = self
                    .inputs
                    .as_ref()
                    .ok_or(IdeProjectOpenError::InputDiscoveryUnavailable)?;
                if inputs.inputs().is_empty() {
                    return Err(IdeProjectOpenError::SelectedInputUnavailable);
                }
                DesktopInputConfig::try_new(48_000, self.config.maximum_block_frames)
                    .map_err(IdeProjectOpenError::InputConfig)?
            }
            IdeInputSelection::Routed(routing_id) => {
                let inputs = self
                    .inputs
                    .as_ref()
                    .ok_or(IdeProjectOpenError::InputDiscoveryUnavailable)?;
                if inputs.find(routing_id).is_none() {
                    return Err(IdeProjectOpenError::SelectedInputUnavailable);
                }
                DesktopInputConfig::try_new(48_000, self.config.maximum_block_frames)
                    .map_err(IdeProjectOpenError::InputConfig)?
                    .with_routing_id(routing_id)
                    .map_err(IdeProjectOpenError::InputConfig)?
            }
        };
        DesktopMonitoringConfig::try_new(
            input,
            output,
            DESKTOP_MONITORING_TRANSPORT_FRAMES,
            DESKTOP_MONITORING_TARGET_FRAMES,
            DESKTOP_MONITORING_CHUNK_FRAMES,
        )
        .map(Some)
        .map_err(IdeProjectOpenError::MonitoringConfig)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeOutputSelectionError {
    UnknownOutput,
    CapacityUnavailable,
}

impl fmt::Display for IdeOutputSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::UnknownOutput => "audio output is absent from the latest device snapshot",
            Self::CapacityUnavailable => "audio output selection capacity is unavailable",
        })
    }
}

impl Error for IdeOutputSelectionError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeInputSelectionError {
    DiscoveryUnavailable,
    NoInputsAvailable,
    UnknownInput,
    CapacityUnavailable,
}

impl fmt::Display for IdeInputSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::DiscoveryUnavailable => "audio input discovery is unavailable",
            Self::NoInputsAvailable => "no audio inputs are available",
            Self::UnknownInput => "audio input is absent from the latest device snapshot",
            Self::CapacityUnavailable => "audio input selection capacity is unavailable",
        })
    }
}

impl Error for IdeInputSelectionError {}

#[derive(Debug)]
pub enum IdeProjectOpenError {
    SelectedOutputUnavailable,
    InputDiscoveryUnavailable,
    SelectedInputUnavailable,
    OutputConfig(DesktopOutputConfigError),
    InputConfig(DesktopInputConfigError),
    MonitoringConfig(DesktopMonitoringConfigError),
    Project(DesktopProjectOpenError),
}

impl fmt::Display for IdeProjectOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectedOutputUnavailable => {
                formatter.write_str("selected audio output is no longer available")
            }
            Self::InputDiscoveryUnavailable => {
                formatter.write_str("audio input discovery is unavailable")
            }
            Self::SelectedInputUnavailable => {
                formatter.write_str("selected audio input is no longer available")
            }
            Self::OutputConfig(source) => source.fmt(formatter),
            Self::InputConfig(source) => source.fmt(formatter),
            Self::MonitoringConfig(source) => source.fmt(formatter),
            Self::Project(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeProjectOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SelectedOutputUnavailable
            | Self::InputDiscoveryUnavailable
            | Self::SelectedInputUnavailable => None,
            Self::OutputConfig(source) => Some(source),
            Self::InputConfig(source) => Some(source),
            Self::MonitoringConfig(source) => Some(source),
            Self::Project(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub struct IdeProjectOpenFailure {
    application: DesktopIdeApplication,
    reason: IdeProjectOpenError,
}

impl IdeProjectOpenFailure {
    #[must_use]
    pub const fn application(&self) -> &DesktopIdeApplication {
        &self.application
    }

    #[must_use]
    pub const fn reason(&self) -> &IdeProjectOpenError {
        &self.reason
    }

    #[must_use]
    pub fn into_parts(self) -> (DesktopIdeApplication, IdeProjectOpenError) {
        (self.application, self.reason)
    }
}

impl fmt::Display for IdeProjectOpenFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.reason.fmt(formatter)
    }
}

impl Error for IdeProjectOpenFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.reason)
    }
}

/// One base-mixer command addressed within the active compiled project layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeMixerCommand {
    ChannelGain {
        channel_index: usize,
        gain_millionths: u32,
    },
    ChannelPan {
        channel_index: usize,
        pan_millionths: i32,
    },
    ChannelMute {
        channel_index: usize,
        muted: bool,
    },
    ChannelSolo {
        channel_index: usize,
        solo: bool,
    },
    SendGain {
        channel_index: usize,
        send_index: usize,
        gain_millionths: u32,
    },
    SendPan {
        channel_index: usize,
        send_index: usize,
        pan_millionths: i32,
    },
    SendEnabled {
        channel_index: usize,
        send_index: usize,
        enabled: bool,
    },
    MainGain {
        gain_millionths: u32,
    },
    MainMute {
        muted: bool,
    },
}

#[derive(Debug, PartialEq)]
pub enum IdeMixerCommandError {
    Parameter(ParameterError),
    Resolve(MixerParameterResolveError),
    Send(DesktopDirectControlError),
}

impl fmt::Display for IdeMixerCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parameter(source) => source.fmt(formatter),
            Self::Resolve(source) => source.fmt(formatter),
            Self::Send(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeMixerCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parameter(source) => Some(source),
            Self::Resolve(source) => Some(source),
            Self::Send(source) => Some(source),
        }
    }
}

/// One live control for the optional global input-cue path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeMonitoringCommand {
    Gain { gain_millionths: u32 },
    Mute { muted: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeMonitoringCommandError {
    Unavailable,
    ExcessiveGain,
    Backend(DesktopMonitoringControlError),
}

impl fmt::Display for IdeMonitoringCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("input cue monitoring is not active"),
            Self::ExcessiveGain => formatter.write_str("input cue gain must not exceed unity"),
            Self::Backend(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeMonitoringCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Backend(source) => Some(source),
            Self::Unavailable | Self::ExcessiveGain => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeProjectSaveError {
    ProjectSnapshotUnavailable,
    MixerCommit(DesktopDirectControlError),
    MixerCommitTimeout,
    MixerFenceSequenceUnavailable,
    MixerStateUnavailable,
    MixerResourceMissing,
    MixerResourceAmbiguous,
    MixerLayoutMismatch,
    CapacityUnavailable,
    Storage(StorageError),
    Serialization(MixerStateSerializationError),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    Publication(Box<ProjectResourcePublicationError>),
    ReopenAfterPreparation {
        preparation: Box<IdeProjectSaveError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<ProjectResourcePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeProjectSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectSnapshotUnavailable => {
                formatter.write_str("running playback exposes no verified project snapshot")
            }
            Self::MixerCommit(source) => {
                write!(
                    formatter,
                    "mixer state could not be prepared for save: {source}"
                )
            }
            Self::MixerCommitTimeout => {
                formatter.write_str("mixer state did not commit before the save deadline")
            }
            Self::MixerFenceSequenceUnavailable => {
                formatter.write_str("mixer save sequence space is exhausted")
            }
            Self::MixerStateUnavailable => {
                formatter.write_str("no committed live mixer state is available to save")
            }
            Self::MixerResourceMissing => {
                formatter.write_str("verified project has no mixer resource")
            }
            Self::MixerResourceAmbiguous => {
                formatter.write_str("verified project has multiple mixer resources")
            }
            Self::MixerLayoutMismatch => {
                formatter.write_str("live mixer state disagrees with the compiled project layout")
            }
            Self::CapacityUnavailable => formatter.write_str("mixer save capacity is unavailable"),
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before save: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before save ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "project save preparation failed ({preparation}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "project revision {} was saved but could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "project save failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeProjectSaveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerCommit(source) => Some(source),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ProjectSnapshotUnavailable
            | Self::MixerCommitTimeout
            | Self::MixerFenceSequenceUnavailable
            | Self::MixerStateUnavailable
            | Self::MixerResourceMissing
            | Self::MixerResourceAmbiguous
            | Self::MixerLayoutMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
struct IdeMixerSavePlan {
    project_path: PathBuf,
    expected_project_sha256: [u8; 32],
    resource_id: String,
    resource_bytes: Vec<u8>,
}

impl IdeMixerSavePlan {
    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, ProjectResourcePublicationError> {
        publish_project_resource(
            &self.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: self.expected_project_sha256,
                resource_id: &self.resource_id,
                expected_role: ResourceRole::Mixer,
                resource_bytes: &self.resource_bytes,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeMixerChannelKind {
    Audio,
    Group,
    Return,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdeMixerChannelState {
    pub gain: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdeMixerMainState {
    pub gain: f32,
    pub muted: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeMixerSendTap {
    PreFader,
    PostFader,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdeMixerSendState {
    pub gain: f32,
    pub pan: f32,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IdeMonitoringState {
    pub gain: f32,
    pub muted: bool,
    pub input_state: StreamState,
    pub input: CaptureStreamSnapshot,
    pub bridge: MonitoringSnapshot,
}

#[derive(Clone, Copy, Debug)]
pub struct RunningIdeSnapshot<'a> {
    pub project_id: &'a str,
    pub project_name: &'a str,
    pub sample_rate_hz: u32,
    pub scene_count: usize,
    pub scene_names: [&'a str; MAX_IDE_SCENES],
    pub scene_availability: [IdeSceneAvailability; MAX_IDE_SCENES],
    pub mixer_channel_count: usize,
    pub mixer_channel_names: [&'a str; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_kinds: [IdeMixerChannelKind; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_states: [Option<IdeMixerChannelState>; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_counts: [usize; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_target_names:
        [[&'a str; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_taps:
        [[IdeMixerSendTap; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_states: [[Option<IdeMixerSendState>;
        hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT];
        MAX_IDE_MIXER_CHANNELS],
    pub mixer_main_state: Option<IdeMixerMainState>,
    pub output_state: OutputStreamState,
    pub output: OutputStreamSnapshot,
    pub monitoring: Option<IdeMonitoringState>,
    pub recovery: PlaybackRecoverySnapshot,
}

#[derive(Debug)]
pub struct RunningDesktopIdeApplication {
    application: DesktopIdeApplication,
    session: IdePlaybackSessionOwner,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "the small alternative exists only in tests; production has one unboxed native owner"
)]
enum IdePlaybackSessionOwner {
    Native(DesktopPlaybackSession),
    #[cfg(test)]
    Test(TestPlaybackSession),
}

#[cfg(test)]
#[derive(Debug)]
struct TestPlaybackSession {
    commands: Vec<TransportCommand>,
    mixer_commands: Vec<IdeMixerCommand>,
    meters: OutputMeterReceiver,
    close_failure: Option<IdeNativeCloseFailure>,
    save_eligible: bool,
    commit_mixer_fences: bool,
    committed_mixer_fence: u64,
    next_mixer_fence: u64,
}

#[cfg(test)]
impl Default for TestPlaybackSession {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            mixer_commands: Vec::new(),
            meters: output_meter_channel().1,
            close_failure: None,
            save_eligible: false,
            commit_mixer_fences: false,
            committed_mixer_fence: 0,
            next_mixer_fence: 1,
        }
    }
}

impl RunningDesktopIdeApplication {
    fn can_save_mixer(&self) -> bool {
        match &self.session {
            IdePlaybackSessionOwner::Native(session) => session.prepared_project().is_some(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => session.save_eligible,
        }
    }

    fn try_send_mixer_fence(&mut self) -> Result<Option<u64>, DesktopDirectControlError> {
        match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => session.try_send_mixer_fence(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                let sequence = session.next_mixer_fence;
                if sequence == 0 {
                    return Ok(None);
                }
                session.next_mixer_fence = sequence.checked_add(1).unwrap_or(0);
                if session.commit_mixer_fences {
                    session.committed_mixer_fence = session.committed_mixer_fence.max(sequence);
                }
                Ok(Some(sequence))
            }
        }
    }

    #[must_use]
    fn committed_mixer_fence(&self) -> u64 {
        match &self.session {
            IdePlaybackSessionOwner::Native(session) => session.committed_mixer_fence(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => session.committed_mixer_fence,
        }
    }

    fn prepare_mixer_save(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
    ) -> Result<IdeMixerSavePlan, IdeProjectSaveError> {
        let project = recovery
            .prepared_project()
            .ok_or(IdeProjectSaveError::ProjectSnapshotUnavailable)?;
        let package = project.package();
        let mut mixer_descriptors = package
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Mixer);
        let descriptor = mixer_descriptors
            .next()
            .ok_or(IdeProjectSaveError::MixerResourceMissing)?;
        if mixer_descriptors.next().is_some() {
            return Err(IdeProjectSaveError::MixerResourceAmbiguous);
        }
        let source = package
            .read_resource(descriptor.id())
            .map_err(IdeProjectSaveError::Storage)?;
        let live = recovery
            .checkpoint()
            .mixer_parameter_state()
            .ok_or(IdeProjectSaveError::MixerStateUnavailable)?;
        let (_, mixer, _, _) = project.session().parts();
        if live.channel_count() != mixer.slots().len() {
            return Err(IdeProjectSaveError::MixerLayoutMismatch);
        }
        let mut channels = Vec::new();
        channels
            .try_reserve_exact(mixer.slots().len())
            .map_err(|_| IdeProjectSaveError::CapacityUnavailable)?;
        for (channel_index, slot) in mixer.slots().iter().enumerate() {
            let channel = live
                .channel(channel_index)
                .ok_or(IdeProjectSaveError::MixerLayoutMismatch)?;
            if channel.send_count() != slot.sends().len() {
                return Err(IdeProjectSaveError::MixerLayoutMismatch);
            }
            let mut sends = Vec::new();
            sends
                .try_reserve_exact(slot.sends().len())
                .map_err(|_| IdeProjectSaveError::CapacityUnavailable)?;
            for (send_index, compiled_send) in slot.sends().iter().enumerate() {
                let send = channel
                    .send(send_index)
                    .ok_or(IdeProjectSaveError::MixerLayoutMismatch)?;
                let parameters = send.parameters();
                sends.push(MixerSendBaseState {
                    id: try_save_owned(compiled_send.id())?,
                    gain: parameters.gain,
                    pan: parameters.pan,
                    enabled: send.enabled(),
                });
            }
            let parameters = channel.parameters();
            channels.push(MixerChannelBaseState {
                track_id: try_save_owned(slot.track_id())?,
                gain: parameters.gain,
                pan: parameters.pan,
                muted: channel.muted(),
                solo: channel.solo(),
                sends,
            });
        }
        let state = MixerBaseState {
            channels,
            main_gain: live.main_gain(),
            main_muted: live.main_muted(),
        };
        let resource_bytes = serialize_mixer_base_state(
            &source,
            package.manifest().project_id(),
            descriptor.id(),
            &state,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeProjectSaveError::Serialization)?;
        Ok(IdeMixerSavePlan {
            project_path: package.root().to_path_buf(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_id: try_save_owned(descriptor.id())?,
            resource_bytes,
        })
    }

    /// Observes the verified project and the latest non-owning runtime evidence.
    ///
    /// `None` means the underlying playback source exposes no verified project;
    /// it is not evidence that output is ready, failed, or disconnected. Sessions
    /// created by [`DesktopIdeApplication::open_project`] retain that project.
    #[must_use]
    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the second owner variant is compiled only for deterministic controller tests"
    )]
    pub fn snapshot(&self) -> Option<RunningIdeSnapshot<'_>> {
        let session = match &self.session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => return None,
        };
        let project = session.prepared_project()?;
        let (loaded, mixer, _, _) = project.session().parts();
        let scene_count = loaded.scene_count();
        if scene_count > MAX_IDE_SCENES || session.scene_count() != scene_count {
            return None;
        }
        let mut scene_names = [""; MAX_IDE_SCENES];
        let mut scene_availability = [IdeSceneAvailability::NotPrepared; MAX_IDE_SCENES];
        for scene_index in 0..scene_count {
            scene_names[scene_index] = loaded.scene_name(scene_index)?;
            scene_availability[scene_index] = session.scene_availability(scene_index)?;
        }
        let mixer_channel_count = mixer.slots().len();
        if mixer_channel_count > MAX_IDE_MIXER_CHANNELS
            || session.mixer_channel_count() != mixer_channel_count
        {
            return None;
        }
        let recovery = session.recovery_checkpoint().snapshot();
        let mixer_parameters = recovery.mixer_parameter_state();
        if mixer_parameters.is_some_and(|state| state.channel_count() != mixer_channel_count) {
            return None;
        }
        let mut mixer_channel_names = [""; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_channel_kinds = [IdeMixerChannelKind::Audio; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_channel_states = [None; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_send_counts = [0; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_send_target_names =
            [[""; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_send_taps = [[IdeMixerSendTap::PostFader;
            hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT];
            MAX_IDE_MIXER_CHANNELS];
        let mut mixer_send_states =
            [[None; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS];
        for (channel_index, slot) in mixer.slots().iter().enumerate() {
            mixer_channel_names[channel_index] = loaded.track_name(slot.track_id())?;
            mixer_channel_kinds[channel_index] = match slot.kind() {
                CompiledMixerTrackKind::Audio => IdeMixerChannelKind::Audio,
                CompiledMixerTrackKind::Group => IdeMixerChannelKind::Group,
                CompiledMixerTrackKind::Return => IdeMixerChannelKind::Return,
            };
            mixer_channel_states[channel_index] = mixer_parameters
                .and_then(|state| state.channel(channel_index))
                .map(|state| IdeMixerChannelState {
                    gain: state.parameters().gain.value(),
                    pan: state.parameters().pan.value(),
                    muted: state.muted(),
                    solo: state.solo(),
                });
            mixer_send_counts[channel_index] = slot.sends().len();
            let channel_state = mixer_parameters.and_then(|state| state.channel(channel_index));
            if channel_state.is_some_and(|state| state.send_count() != slot.sends().len()) {
                return None;
            }
            for (send_index, send) in slot.sends().iter().enumerate() {
                let target = mixer.slots().get(send.target_slot())?;
                mixer_send_target_names[channel_index][send_index] =
                    loaded.track_name(target.track_id())?;
                mixer_send_taps[channel_index][send_index] = match send.tap() {
                    CompiledMixerTap::PreFader => IdeMixerSendTap::PreFader,
                    CompiledMixerTap::PostFader => IdeMixerSendTap::PostFader,
                };
                mixer_send_states[channel_index][send_index] = channel_state
                    .and_then(|state| state.send(send_index))
                    .map(|state| IdeMixerSendState {
                        gain: state.parameters().gain.value(),
                        pan: state.parameters().pan.value(),
                        enabled: state.enabled(),
                    });
            }
        }
        let mixer_main_state = mixer_parameters.as_ref().map(ide_mixer_main_state);
        let monitoring = session_monitoring_state(session).ok()?;
        Some(RunningIdeSnapshot {
            project_id: project.package().manifest().project_id(),
            project_name: project.package().manifest().name(),
            sample_rate_hz: project.session().sample_rate_hz(),
            scene_count,
            scene_names,
            scene_availability,
            mixer_channel_count,
            mixer_channel_names,
            mixer_channel_kinds,
            mixer_channel_states,
            mixer_send_counts,
            mixer_send_target_names,
            mixer_send_taps,
            mixer_send_states,
            mixer_main_state,
            output_state: session.output_status().state(),
            output: session.counters().snapshot(),
            monitoring,
            recovery,
        })
    }

    /// Sends a bounded transport or scene command through the only live owner.
    ///
    /// # Errors
    ///
    /// Returns queue, target, or disconnection evidence unchanged.
    pub fn try_send_transport(
        &mut self,
        command: TransportCommand,
    ) -> Result<(), DesktopDirectControlError> {
        match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => session.try_send_transport_command(command),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                session.commands.push(command);
                Ok(())
            }
        }
    }

    /// Sends one validated base-mixer command through the only live owner.
    ///
    /// # Errors
    ///
    /// Returns parameter-domain, channel-resolution, queue, or disconnection
    /// evidence without publishing an optimistic state change.
    pub fn try_send_mixer(&mut self, command: IdeMixerCommand) -> Result<(), IdeMixerCommandError> {
        match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => {
                let command = match command {
                    IdeMixerCommand::ChannelGain {
                        channel_index,
                        gain_millionths,
                    } => MixerParameterCommand::ChannelGain {
                        channel: session
                            .resolve_mixer_channel_index(channel_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        gain: LinearGain::from_millionths(gain_millionths)
                            .map_err(IdeMixerCommandError::Parameter)?,
                    },
                    IdeMixerCommand::ChannelPan {
                        channel_index,
                        pan_millionths,
                    } => MixerParameterCommand::ChannelPan {
                        channel: session
                            .resolve_mixer_channel_index(channel_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        pan: Pan::from_millionths(pan_millionths)
                            .map_err(IdeMixerCommandError::Parameter)?,
                    },
                    IdeMixerCommand::ChannelMute {
                        channel_index,
                        muted,
                    } => MixerParameterCommand::ChannelMute {
                        channel: session
                            .resolve_mixer_channel_index(channel_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        muted,
                    },
                    IdeMixerCommand::ChannelSolo {
                        channel_index,
                        solo,
                    } => MixerParameterCommand::ChannelSolo {
                        channel: session
                            .resolve_mixer_channel_index(channel_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        solo,
                    },
                    IdeMixerCommand::SendGain {
                        channel_index,
                        send_index,
                        gain_millionths,
                    } => MixerParameterCommand::SendGain {
                        send: session
                            .resolve_mixer_send_address(channel_index, send_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        gain: LinearGain::from_millionths(gain_millionths)
                            .map_err(IdeMixerCommandError::Parameter)?,
                    },
                    IdeMixerCommand::SendPan {
                        channel_index,
                        send_index,
                        pan_millionths,
                    } => MixerParameterCommand::SendPan {
                        send: session
                            .resolve_mixer_send_address(channel_index, send_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        pan: Pan::from_millionths(pan_millionths)
                            .map_err(IdeMixerCommandError::Parameter)?,
                    },
                    IdeMixerCommand::SendEnabled {
                        channel_index,
                        send_index,
                        enabled,
                    } => MixerParameterCommand::SendEnabled {
                        send: session
                            .resolve_mixer_send_address(channel_index, send_index)
                            .map_err(IdeMixerCommandError::Resolve)?,
                        enabled,
                    },
                    IdeMixerCommand::MainGain { gain_millionths } => {
                        MixerParameterCommand::MainGain {
                            gain: LinearGain::from_millionths(gain_millionths)
                                .map_err(IdeMixerCommandError::Parameter)?,
                        }
                    }
                    IdeMixerCommand::MainMute { muted } => {
                        MixerParameterCommand::MainMute { muted }
                    }
                };
                session
                    .try_send_mixer_parameter_command(command)
                    .map_err(IdeMixerCommandError::Send)
            }
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                validate_test_mixer_command(command)?;
                session.mixer_commands.push(command);
                Ok(())
            }
        }
    }

    /// Applies one lock-free input-cue control to the active monitored session.
    ///
    /// # Errors
    ///
    /// Returns an explicit unavailable result for ordinary playback sessions,
    /// rejects gain above unity, and preserves backend validation failures.
    #[allow(
        clippy::cast_precision_loss,
        clippy::infallible_destructuring_match,
        reason = "the integer cue domain is bounded below exact f32 integer range, and the test-only owner has no monitor"
    )]
    pub fn apply_monitoring(
        &mut self,
        command: IdeMonitoringCommand,
    ) -> Result<(), IdeMonitoringCommandError> {
        let session = match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => {
                return Err(IdeMonitoringCommandError::Unavailable);
            }
        };
        let control = session
            .monitoring_control()
            .ok_or(IdeMonitoringCommandError::Unavailable)?;
        match command {
            IdeMonitoringCommand::Gain { gain_millionths } => {
                if gain_millionths > MONITOR_GAIN_MILLIONTHS_MAXIMUM {
                    return Err(IdeMonitoringCommandError::ExcessiveGain);
                }
                let gain = gain_millionths as f32 / MONITOR_GAIN_MILLIONTHS_MAXIMUM as f32;
                control
                    .set_gain_linear(gain)
                    .map_err(IdeMonitoringCommandError::Backend)
            }
            IdeMonitoringCommand::Mute { muted } => {
                control.set_muted(muted);
                Ok(())
            }
        }
    }

    #[must_use]
    pub fn read_latest_meter(&mut self) -> LatestOutputMeterSnapshot {
        match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => session.meter_receiver().read_latest(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => session.meters.read_latest(),
        }
    }

    #[must_use]
    pub fn application(&self) -> &DesktopIdeApplication {
        &self.application
    }

    /// Closes the project while retaining pending thread ownership when needed.
    ///
    /// A terminal outcome returns the closed application controller and consumes
    /// backend restart recovery. Opening another project performs verification and
    /// loading again.
    #[must_use]
    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the second owner variant is compiled only for deterministic controller tests"
    )]
    pub fn close(self) -> IdeCloseOutcome {
        let Self {
            application,
            session,
        } = self;
        let session = match session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                return IdeCloseOutcome::Closed {
                    application,
                    failure: session.close_failure,
                };
            }
        };
        match session.into_recovery() {
            Ok(_recovery) => IdeCloseOutcome::Closed {
                application,
                failure: None,
            },
            Err(error) => {
                let failure = classify_joined_close_failure(&error);
                match error.into_pending() {
                    Ok(pending) => IdeCloseOutcome::Pending(DesktopIdePendingClose {
                        application,
                        pending,
                    }),
                    Err(error) => {
                        drop(error);
                        IdeCloseOutcome::Closed {
                            application,
                            failure: Some(failure),
                        }
                    }
                }
            }
        }
    }

    #[must_use]
    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the test-only session variant returns a deterministic unavailable outcome"
    )]
    fn close_for_save(self) -> IdeSaveCloseOutcome {
        let Self {
            application,
            session,
        } = self;
        let session = match session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                return IdeSaveCloseOutcome::Unavailable {
                    application,
                    failure: session.close_failure,
                };
            }
        };
        match session.into_recovery() {
            Ok(recovery) => IdeSaveCloseOutcome::Closed {
                application,
                recovery,
                failure: None,
            },
            Err(error) => {
                let failure = classify_joined_close_failure(&error);
                match error.into_pending() {
                    Ok(pending) => IdeSaveCloseOutcome::Pending(DesktopIdePendingSaveClose {
                        application,
                        pending,
                    }),
                    Err(error) => match error.into_recovery() {
                        Ok(recovery) => IdeSaveCloseOutcome::Closed {
                            application,
                            recovery,
                            failure: Some(failure),
                        },
                        Err(_) => IdeSaveCloseOutcome::Unavailable {
                            application,
                            failure: Some(failure),
                        },
                    },
                }
            }
        }
    }
}

fn ide_mixer_main_state(
    state: &hyper_mixer_runtime::MixerParameterStateSnapshot,
) -> IdeMixerMainState {
    IdeMixerMainState {
        gain: state.main_gain().value(),
        muted: state.main_muted(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InconsistentMonitoringState;

fn session_monitoring_state(
    session: &DesktopPlaybackSession,
) -> Result<Option<IdeMonitoringState>, InconsistentMonitoringState> {
    match (
        session.monitoring_control(),
        session.monitoring_input_status(),
        session.monitoring_input_counters(),
        session.monitoring_snapshot(),
    ) {
        (Some(control), Some(input_status), Some(input_counters), Some(bridge)) => {
            Ok(Some(IdeMonitoringState {
                gain: control.gain_linear(),
                muted: control.muted(),
                input_state: input_status.state(),
                input: input_counters.snapshot(),
                bridge,
            }))
        }
        (None, None, None, None) => Ok(None),
        _ => Err(InconsistentMonitoringState),
    }
}

fn try_save_owned(value: &str) -> Result<String, IdeProjectSaveError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| IdeProjectSaveError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
fn validate_test_mixer_command(command: IdeMixerCommand) -> Result<(), IdeMixerCommandError> {
    let (channel_index, send_index) = match command {
        IdeMixerCommand::ChannelGain {
            channel_index,
            gain_millionths,
        } => {
            LinearGain::from_millionths(gain_millionths)
                .map_err(IdeMixerCommandError::Parameter)?;
            (Some(channel_index), None)
        }
        IdeMixerCommand::ChannelPan {
            channel_index,
            pan_millionths,
        } => {
            Pan::from_millionths(pan_millionths).map_err(IdeMixerCommandError::Parameter)?;
            (Some(channel_index), None)
        }
        IdeMixerCommand::ChannelMute { channel_index, .. }
        | IdeMixerCommand::ChannelSolo { channel_index, .. } => (Some(channel_index), None),
        IdeMixerCommand::SendGain {
            channel_index,
            send_index,
            gain_millionths,
        } => {
            LinearGain::from_millionths(gain_millionths)
                .map_err(IdeMixerCommandError::Parameter)?;
            (Some(channel_index), Some(send_index))
        }
        IdeMixerCommand::SendPan {
            channel_index,
            send_index,
            pan_millionths,
        } => {
            Pan::from_millionths(pan_millionths).map_err(IdeMixerCommandError::Parameter)?;
            (Some(channel_index), Some(send_index))
        }
        IdeMixerCommand::SendEnabled {
            channel_index,
            send_index,
            ..
        } => (Some(channel_index), Some(send_index)),
        IdeMixerCommand::MainGain { gain_millionths } => {
            LinearGain::from_millionths(gain_millionths)
                .map_err(IdeMixerCommandError::Parameter)?;
            (None, None)
        }
        IdeMixerCommand::MainMute { .. } => (None, None),
    };
    if channel_index.is_some_and(|index| index >= MAX_IDE_MIXER_CHANNELS) {
        return Err(IdeMixerCommandError::Resolve(
            MixerParameterResolveError::ChannelNotFound,
        ));
    }
    if send_index.is_some_and(|index| index >= hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT) {
        return Err(IdeMixerCommandError::Resolve(
            MixerParameterResolveError::SendNotFound,
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeNativeCloseFailure {
    RuntimeFailed,
    ThreadPanicked,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "pending native ownership must transfer without a new allocation after shutdown has begun"
)]
pub enum IdeCloseOutcome {
    Closed {
        application: DesktopIdeApplication,
        failure: Option<IdeNativeCloseFailure>,
    },
    Pending(DesktopIdePendingClose),
}

#[derive(Debug)]
enum IdeSaveCloseOutcome {
    Closed {
        application: DesktopIdeApplication,
        recovery: DesktopPlaybackRecovery,
        failure: Option<IdeNativeCloseFailure>,
    },
    Pending(DesktopIdePendingSaveClose),
    Unavailable {
        application: DesktopIdeApplication,
        failure: Option<IdeNativeCloseFailure>,
    },
}

#[derive(Debug)]
struct DesktopIdePendingSaveClose {
    application: DesktopIdeApplication,
    pending: DesktopPlaybackRecoveryPendingStop,
}

#[derive(Debug)]
enum IdePendingSaveCloseWait {
    Closed {
        application: DesktopIdeApplication,
        recovery: DesktopPlaybackRecovery,
        failure: Option<IdeNativeCloseFailure>,
    },
    TimedOut(DesktopIdePendingSaveClose),
}

impl DesktopIdePendingSaveClose {
    #[must_use]
    fn wait(self, timeout: Duration) -> IdePendingSaveCloseWait {
        match self.pending.wait(timeout) {
            DesktopPlaybackRecoveryPendingStopWait::Completed(recovery) => {
                IdePendingSaveCloseWait::Closed {
                    application: self.application,
                    recovery,
                    failure: None,
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::Failed(recovery) => {
                IdePendingSaveCloseWait::Closed {
                    application: self.application,
                    recovery,
                    failure: Some(IdeNativeCloseFailure::RuntimeFailed),
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::Panicked(recovery) => {
                IdePendingSaveCloseWait::Closed {
                    application: self.application,
                    recovery,
                    failure: Some(IdeNativeCloseFailure::ThreadPanicked),
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::TimedOut(pending) => {
                IdePendingSaveCloseWait::TimedOut(Self {
                    application: self.application,
                    pending,
                })
            }
        }
    }
}

#[derive(Debug)]
pub struct DesktopIdePendingClose {
    application: DesktopIdeApplication,
    pending: DesktopPlaybackRecoveryPendingStop,
}

#[derive(Debug)]
#[allow(
    clippy::large_enum_variant,
    reason = "a timed-out native owner must be returned without allocating after the bounded wait"
)]
pub enum IdePendingCloseWait {
    Closed {
        application: DesktopIdeApplication,
        failure: Option<IdeNativeCloseFailure>,
    },
    TimedOut(DesktopIdePendingClose),
}

impl DesktopIdePendingClose {
    #[must_use]
    pub fn wait(self, timeout: Duration) -> IdePendingCloseWait {
        match self.pending.wait(timeout) {
            DesktopPlaybackRecoveryPendingStopWait::Completed(_recovery) => {
                IdePendingCloseWait::Closed {
                    application: self.application,
                    failure: None,
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::Failed(_recovery) => {
                IdePendingCloseWait::Closed {
                    application: self.application,
                    failure: Some(IdeNativeCloseFailure::RuntimeFailed),
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::Panicked(_recovery) => {
                IdePendingCloseWait::Closed {
                    application: self.application,
                    failure: Some(IdeNativeCloseFailure::ThreadPanicked),
                }
            }
            DesktopPlaybackRecoveryPendingStopWait::TimedOut(pending) => {
                IdePendingCloseWait::TimedOut(Self {
                    application: self.application,
                    pending,
                })
            }
        }
    }
}

fn classify_joined_close_failure(
    error: &hyper_desktop_audio::DesktopPlaybackRecoveryStopError,
) -> IdeNativeCloseFailure {
    if matches!(error.input, Some(DesktopInputStopError::ThreadPanicked))
        || matches!(error.output, Some(DesktopOutputStopError::ThreadPanicked))
    {
        IdeNativeCloseFailure::ThreadPanicked
    } else {
        IdeNativeCloseFailure::RuntimeFailed
    }
}

fn try_owned(value: &str) -> Result<String, OutputInventoryError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| OutputInventoryError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

fn try_input_owned(value: &str) -> Result<String, InputInventoryError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| InputInventoryError::CapacityUnavailable)?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper_audio_source::DecodeLimits;
    use hyper_media_cache::CacheLimits;
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;

    fn config(
        maximum_block_frames: usize,
    ) -> Result<IdeApplicationConfig, IdeApplicationConfigError> {
        IdeApplicationConfig::try_new(
            maximum_block_frames,
            ProjectPlaybackLimits {
                max_total_resource_bytes: 1_048_576,
                audio: DecodeLimits::new(1_048_576, 48_000, 384_000).unwrap(),
                session: PlaybackSessionAssemblyLimits {
                    documents: LoaderLimits::VERSION_1,
                    media_cache: CacheLimits {
                        max_assets: 8,
                        max_frames_per_asset: 48_000,
                        max_decoded_sample_bytes: 3_072_000,
                    },
                },
            },
        )
    }

    #[test]
    fn output_inventory_is_owned_ordered_and_unique() {
        let inventory =
            IdeAudioOutputInventory::try_new(&[("route-b", "Speakers"), ("route-a", "Headphones")])
                .unwrap();
        assert_eq!(inventory.outputs()[0].routing_id(), "route-b");
        assert_eq!(inventory.outputs()[1].display_name(), "Headphones");
        assert_eq!(
            IdeAudioOutputInventory::try_new(&[("route", "One"), ("route", "Two")]),
            Err(OutputInventoryError::DuplicateRoutingId { output_index: 1 })
        );
        assert_eq!(
            IdeAudioOutput::try_new("", "Output"),
            Err(OutputInventoryError::EmptyRoutingId)
        );
        assert_eq!(
            IdeAudioOutput::try_new("route", ""),
            Err(OutputInventoryError::EmptyDisplayName)
        );
    }

    #[test]
    fn output_selection_is_explicit_and_rejects_stale_inventory() {
        let outputs = IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap();
        let mut application = DesktopIdeApplication::from_inventory(config(256).unwrap(), outputs);
        assert_eq!(application.selection(), &IdeOutputSelection::SystemDefault);
        assert_eq!(
            application.select_output("missing"),
            Err(IdeOutputSelectionError::UnknownOutput)
        );
        application.select_output("route-a").unwrap();
        assert_eq!(
            application.selection(),
            &IdeOutputSelection::Routed("route-a".to_owned())
        );

        application.outputs = IdeAudioOutputInventory::default();
        let failure = application.open_project("project-is-not-read").unwrap_err();
        assert!(matches!(
            failure.reason(),
            IdeProjectOpenError::SelectedOutputUnavailable
        ));
        let (application, _) = failure.into_parts();
        assert_eq!(
            application.selection(),
            &IdeOutputSelection::Routed("route-a".to_owned())
        );
        assert!(application.outputs().find("route-a").is_none());
    }

    #[test]
    fn input_monitoring_is_default_off_and_requires_current_discovery() {
        let outputs = IdeAudioOutputInventory::try_new(&[("output-a", "Output A")]).unwrap();
        let inputs =
            IdeAudioInputInventory::try_new(&[("input-a", "Input A"), ("input-b", "Input B")])
                .unwrap();
        let mut application =
            DesktopIdeApplication::from_inventories(config(256).unwrap(), outputs, inputs);

        assert_eq!(application.input_selection(), &IdeInputSelection::Disabled);
        let output = application.output_config().unwrap();
        assert!(application.monitoring_config(output).unwrap().is_none());

        application.select_system_default_input().unwrap();
        let monitoring = application
            .monitoring_config(application.output_config().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(monitoring.input().routing_id(), None);
        assert_eq!(
            monitoring.transport_capacity_frames(),
            DESKTOP_MONITORING_TRANSPORT_FRAMES
        );
        assert_eq!(
            monitoring.target_latency_frames(),
            DESKTOP_MONITORING_TARGET_FRAMES
        );
        assert_eq!(
            monitoring.processing_chunk_frames(),
            DESKTOP_MONITORING_CHUNK_FRAMES
        );

        application.select_input("input-b").unwrap();
        let monitoring = application
            .monitoring_config(application.output_config().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(monitoring.input().routing_id(), Some("input-b"));

        application.inputs = Some(IdeAudioInputInventory::default());
        assert!(matches!(
            application.monitoring_config(application.output_config().unwrap()),
            Err(IdeProjectOpenError::SelectedInputUnavailable)
        ));
        assert_eq!(
            application.input_selection(),
            &IdeInputSelection::Routed("input-b".to_owned())
        );
        application.disable_input_monitoring();
        assert_eq!(application.input_selection(), &IdeInputSelection::Disabled);
    }

    #[test]
    fn input_inventory_is_owned_ordered_and_unique() {
        let inventory =
            IdeAudioInputInventory::try_new(&[("route-b", "Mic"), ("route-a", "Line")]).unwrap();
        assert_eq!(inventory.inputs()[0].routing_id(), "route-b");
        assert_eq!(inventory.inputs()[1].display_name(), "Line");
        assert_eq!(
            IdeAudioInputInventory::try_new(&[("route", "One"), ("route", "Two")]),
            Err(InputInventoryError::DuplicateRoutingId { input_index: 1 })
        );
        assert_eq!(
            IdeAudioInput::try_new("", "Input"),
            Err(InputInventoryError::EmptyRoutingId)
        );
    }

    #[test]
    fn application_policy_is_fixed_to_validated_callback_capacity() {
        assert!(matches!(
            config(0),
            Err(IdeApplicationConfigError::Output(
                DesktopOutputConfigError::InvalidMaximumBlockFrames { actual: 0, .. }
            ))
        ));
        let config = config(512).unwrap();
        assert_eq!(config.maximum_block_frames(), 512);
        assert_eq!(config.project_limits().max_total_resource_bytes, 1_048_576);
    }

    #[test]
    fn desktop_launch_policy_is_bounded_to_version_one_playback() {
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        assert_eq!(config.maximum_block_frames(), DESKTOP_MAXIMUM_BLOCK_FRAMES);
        assert_eq!(limits.audio.max_frames(), DESKTOP_MAX_AUDIO_FRAMES);
        assert_eq!(
            limits.session.media_cache.max_assets,
            LoaderLimits::VERSION_1.max_clip_documents
        );
        assert!(
            limits.session.media_cache.max_decoded_sample_bytes
                >= limits.audio.max_decoded_sample_bytes()
        );
    }

    #[test]
    fn project_load_failure_returns_the_unchanged_closed_application() {
        let outputs = IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap();
        let mut application = DesktopIdeApplication::from_inventory(config(256).unwrap(), outputs);
        application.select_output("route-a").unwrap();
        let missing = std::env::temp_dir().join(format!(
            "hyper-desktop-ide-missing-project-{}",
            std::process::id()
        ));
        assert!(!missing.exists(), "test path must remain absent");

        let failure = application.open_project(&missing).unwrap_err();
        assert!(matches!(
            failure.reason(),
            IdeProjectOpenError::Project(
                DesktopProjectOpenError::Recovery(_) | DesktopProjectOpenError::Load { .. }
            )
        ));
        let (application, _) = failure.into_parts();
        assert_eq!(
            application.selection(),
            &IdeOutputSelection::Routed("route-a".to_owned())
        );
        assert_eq!(application.outputs().outputs().len(), 1);
        assert_eq!(
            application.outputs().outputs()[0].display_name(),
            "Output A"
        );
    }

    #[test]
    fn running_owner_serializes_transport_and_returns_to_closed_state() {
        let application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::default(),
        );
        let mut running = RunningDesktopIdeApplication {
            application,
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        for command in [
            TransportCommand::Start,
            TransportCommand::Locate {
                frame: 0,
                state: LocateState::Preserve,
            },
            TransportCommand::LaunchScene { scene_index: 2 },
            TransportCommand::Stop,
        ] {
            running.try_send_transport(command).unwrap();
        }
        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(
            session.commands,
            [
                TransportCommand::Start,
                TransportCommand::Locate {
                    frame: 0,
                    state: LocateState::Preserve,
                },
                TransportCommand::LaunchScene { scene_index: 2 },
                TransportCommand::Stop,
            ]
        );
        assert!(running.snapshot().is_none());
        assert!(running.read_latest_meter().snapshot().is_none());
        assert!(matches!(
            running.close(),
            IdeCloseOutcome::Closed { failure: None, .. }
        ));
    }

    #[test]
    fn running_owner_validates_and_serializes_base_mixer_commands() {
        let application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::default(),
        );
        let mut running = RunningDesktopIdeApplication {
            application,
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        let commands = [
            IdeMixerCommand::ChannelGain {
                channel_index: 0,
                gain_millionths: 1_250_000,
            },
            IdeMixerCommand::ChannelPan {
                channel_index: 0,
                pan_millionths: -250_000,
            },
            IdeMixerCommand::ChannelMute {
                channel_index: 0,
                muted: true,
            },
            IdeMixerCommand::ChannelSolo {
                channel_index: 0,
                solo: true,
            },
            IdeMixerCommand::SendGain {
                channel_index: 0,
                send_index: 0,
                gain_millionths: 500_000,
            },
            IdeMixerCommand::SendPan {
                channel_index: 0,
                send_index: 0,
                pan_millionths: 250_000,
            },
            IdeMixerCommand::SendEnabled {
                channel_index: 0,
                send_index: 0,
                enabled: false,
            },
            IdeMixerCommand::MainGain {
                gain_millionths: 750_000,
            },
            IdeMixerCommand::MainMute { muted: true },
        ];
        for command in commands {
            running.try_send_mixer(command).unwrap();
        }
        assert_eq!(
            running.try_send_mixer(IdeMixerCommand::ChannelGain {
                channel_index: MAX_IDE_MIXER_CHANNELS,
                gain_millionths: 1_000_000,
            }),
            Err(IdeMixerCommandError::Resolve(
                MixerParameterResolveError::ChannelNotFound
            ))
        );
        assert_eq!(
            running.try_send_mixer(IdeMixerCommand::MainGain {
                gain_millionths: 2_000_001,
            }),
            Err(IdeMixerCommandError::Parameter(
                ParameterError::LinearGainOutOfRange
            ))
        );
        assert_eq!(
            running.try_send_mixer(IdeMixerCommand::ChannelPan {
                channel_index: 0,
                pan_millionths: -1_000_001,
            }),
            Err(IdeMixerCommandError::Parameter(
                ParameterError::PanOutOfRange
            ))
        );
        assert_eq!(
            running.try_send_mixer(IdeMixerCommand::SendEnabled {
                channel_index: 0,
                send_index: hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT,
                enabled: true,
            }),
            Err(IdeMixerCommandError::Resolve(
                MixerParameterResolveError::SendNotFound
            ))
        );

        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(session.mixer_commands, commands);
    }
}
