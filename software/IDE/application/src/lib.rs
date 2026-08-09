#![doc = "Single-owner desktop IDE application state above native audio playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use hyper_audio_engine::LoudnessMeasurementState as IdeLoudnessMeasurementState;
use hyper_audio_engine::{
    LinearGain, LoudnessMeasurementCommand, Pan, ParameterError, StereoSineTestToneConfigError,
};
use hyper_audio_source::{DecodeError, DecodeLimits};
use hyper_control_mappings::{ControlBinding, ControlScope, ControlTarget};
use hyper_desktop_audio::{
    CaptureStreamSnapshot, DesktopAudioPreviewCommandError, DesktopAudioPreviewControlError,
    DesktopAudioPreviewSnapshot, DesktopDirectControlError, DesktopInputConfig,
    DesktopInputConfigError, DesktopInputDiscoveryError, DesktopInputStopError,
    DesktopMonitoringConfig, DesktopMonitoringConfigError, DesktopMonitoringControlError,
    DesktopOutputConfig, DesktopOutputConfigError, DesktopOutputDiscoveryError,
    DesktopOutputStartError, DesktopOutputStopError, DesktopPlaybackRecovery,
    DesktopPlaybackRecoveryPendingStop, DesktopPlaybackRecoveryPendingStopWait,
    DesktopPlaybackSession, DesktopProjectOpenError, DesktopTestToneConfig, DesktopTestToneSession,
    MonitoringSnapshot, OutputStreamSnapshot, OutputStreamState, PendingNativeShutdown,
    PendingNativeShutdownWait, StreamState, discover_desktop_input_targets,
    discover_desktop_output_targets,
};
pub use hyper_desktop_audio::{
    DesktopAudioPreviewState, DesktopSceneAvailability as IdeSceneAvailability,
};
use hyper_media_cache::CacheLimits;
pub use hyper_mixer_runtime::{
    EbuLoudnessPresentationConfig as IdeLoudnessPresentationConfig,
    EbuLoudnessScale as IdeLoudnessScale, LocateState,
    LoudnessDisplayUnit as IdeLoudnessDisplayUnit,
    LoudnessPresentationSnapshot as IdeLoudnessPresentationSnapshot,
    LoudnessScaleRegion as IdeLoudnessScaleRegion,
    MaximumTruePeakDisplay as IdeMaximumTruePeakDisplay, PlaybackRecoveryTransportState,
    ScaledLoudnessValue as IdeScaledLoudnessValue, TransportCommand,
};
use hyper_mixer_runtime::{
    LatestOutputMeterSnapshot, LoudnessCommandSendError, MixerParameterCommand,
    MixerParameterResolveError, PlaybackRecoverySnapshot,
};
#[cfg(test)]
use hyper_mixer_runtime::{OutputMeterReceiver, output_meter_channel};
pub use hyper_playback_json::is_native_audio_clip_gain_decibels as ide_audio_clip_gain_is_supported;
use hyper_playback_json::{
    ArrangementPlacementDocumentSerializationError, AudioClipAdditionSerializationError,
    AudioClipDocumentSerializationError, AudioClipDuplicationSerializationError,
    AudioClipGainSerializationError, AudioClipRenameSerializationError,
    AudioTrackAdditionSerializationError, AudioTrackDocumentSerializationError,
    AudioTrackRemovalBlockers, AudioTrackRemovalSerializationError,
    AudioTrackRenameSerializationError, CompiledMixerTap, CompiledMixerTrackKind, LoaderLimits,
    MixerBaseState, MixerChannelBaseState, MixerSendBaseState, MixerStateSerializationError,
    serialize_arrangement_placement_activation, serialize_arrangement_placement_addition,
    serialize_arrangement_placement_duration, serialize_arrangement_placement_front_trim,
    serialize_arrangement_placement_move, serialize_arrangement_placement_removal,
    serialize_audio_clip_activation, serialize_audio_clip_duplication, serialize_audio_clip_gain,
    serialize_audio_clip_rename, serialize_audio_track_activation, serialize_audio_track_addition,
    serialize_audio_track_removal, serialize_audio_track_rename, serialize_mixer_base_state,
    serialize_recording_audio_clip_addition,
};
use hyper_playback_session::{AudioClipGainValidationError, PlaybackSessionAssemblyLimits};
pub use hyper_playback_session::{
    PlaybackArrangement as IdePlaybackArrangement, PlaybackExtent as IdePlaybackExtent,
};
use hyper_project_loader::{PreparedProjectPlayback, ProjectPlaybackLimits};
use hyper_project_model::ResourceRole;
use hyper_storage_engine::{
    ArrangementPublicationRequest, AudioClipPublicationOutcome, AudioClipPublicationRequest,
    AudioTrackPublicationOutcome, AudioTrackPublicationRequest, AudioTrackRemovalOutcome,
    AudioTrackRemovalRequest, ProjectResourcePublicationError, ProjectResourcePublicationOutcome,
    ProjectResourcePublicationRequest, StorageError, publish_arrangement, publish_audio_clip,
    publish_audio_track, publish_project_resource, remove_audio_track,
};

mod recording;
mod worker;

pub use recording::{
    IdeRecordingAbortError, IdeRecordingAbortOutcome, IdeRecordingError, IdeRecordingFinalizeError,
    IdeRecordingImportOutcome, IdeRecordingObservation, IdeRecordingPreparationError,
    IdeRecordingStartError,
};

pub use worker::{
    DesktopIdeWorker, DesktopIdeWorkerPendingShutdown, IdeAudioClip, IdeAudioPreviewObservation,
    IdeClosedSnapshot, IdeMixerChannel, IdeMixerSend, IdeProjectCreationError,
    IdeRecordingCaptureKind, IdeRecordingState, IdeRecordingTake, IdeRunningObservation, IdeScene,
    IdeWorkerCommand, IdeWorkerCommandFailure, IdeWorkerCommandKind, IdeWorkerEvent,
    IdeWorkerEventReceiveError, IdeWorkerJoinError, IdeWorkerPendingShutdownWait,
    IdeWorkerSendError, IdeWorkerShutdownOutcome, IdeWorkerShutdownReport, IdeWorkerStartError,
    MAX_IDE_WORKER_COMMANDS, MAX_IDE_WORKER_EVENTS, desktop_ide_worker_supervisor_snapshot,
};

const DESKTOP_MAXIMUM_BLOCK_FRAMES: usize = 4_096;
const DESKTOP_MONITORING_TRANSPORT_FRAMES: usize = DESKTOP_MAXIMUM_BLOCK_FRAMES * 2;
const DESKTOP_MONITORING_TARGET_FRAMES: usize = 256;
const DESKTOP_MONITORING_CHUNK_FRAMES: usize = 64;
const MONITOR_GAIN_MILLIONTHS_MAXIMUM: u32 = 1_000_000;
const PREVIEW_GAIN_MILLIONTHS_MAXIMUM: u32 = 1_000_000;
pub const IDE_OUTPUT_TEST_FREQUENCY_HZ: f64 = 1_000.0;
pub const IDE_OUTPUT_TEST_LEVEL_DBFS: f32 = -18.0;
const DESKTOP_RECORDING_TRANSPORT_FRAMES: usize = DESKTOP_MAXIMUM_BLOCK_FRAMES * 4;
const DESKTOP_RECORDING_DRAIN_FRAMES: usize = DESKTOP_MAXIMUM_BLOCK_FRAMES;
const DESKTOP_RECORDING_COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);
const DESKTOP_RECORDING_MAXIMUM_FRAMES: u64 = DESKTOP_MAX_AUDIO_FRAMES;
const DESKTOP_RECORDING_DIRECTORY: &str = "recording-captures";
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

    /// Starts the fixed stereo output diagnostic through the selected route.
    ///
    /// The diagnostic owns the native output exclusively and uses the EBU
    /// alignment-check signal parameters of 1 kHz at -18 dBFS. It is a digital
    /// routing check, not evidence of physical calibration or acoustic level.
    #[must_use]
    pub fn start_output_test(self) -> IdeOutputTestStartOutcome {
        let output = match self.output_test_config() {
            Ok(output) => output,
            Err(reason) => {
                return IdeOutputTestStartOutcome::Failed {
                    application: self,
                    reason,
                };
            }
        };
        let config = match DesktopTestToneConfig::try_new(
            output,
            IDE_OUTPUT_TEST_FREQUENCY_HZ,
            IDE_OUTPUT_TEST_LEVEL_DBFS,
        ) {
            Ok(config) => config,
            Err(source) => {
                return IdeOutputTestStartOutcome::Failed {
                    application: self,
                    reason: IdeOutputTestError::ToneConfig(source),
                };
            }
        };
        match DesktopTestToneSession::start(config) {
            Ok(session) => IdeOutputTestStartOutcome::Started(OutputTestingDesktopIdeApplication {
                application: self,
                session: IdeOutputTestSessionOwner::Native(session),
            }),
            Err(DesktopOutputStartError::CancellationPending { shutdown }) => {
                IdeOutputTestStartOutcome::Pending(IdePendingOutputTestShutdown {
                    application: self,
                    shutdown: *shutdown,
                    origin: IdeOutputTestPendingOrigin::Start,
                })
            }
            Err(source) => IdeOutputTestStartOutcome::Failed {
                application: self,
                reason: IdeOutputTestError::OutputStart(source),
            },
        }
    }

    fn output_test_config(&self) -> Result<DesktopOutputConfig, IdeOutputTestError> {
        let output = DesktopOutputConfig::try_new(48_000, self.config.maximum_block_frames)
            .map_err(IdeOutputTestError::OutputConfig)?;
        match &self.output_selection {
            IdeOutputSelection::SystemDefault => Ok(output),
            IdeOutputSelection::Routed(routing_id) => {
                if self.outputs.find(routing_id).is_none() {
                    return Err(IdeOutputTestError::SelectedOutputUnavailable);
                }
                output
                    .with_routing_id(routing_id)
                    .map_err(IdeOutputTestError::OutputConfig)
            }
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
        let Some(input) = self.input_config()? else {
            return Ok(None);
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

    fn input_config(&self) -> Result<Option<DesktopInputConfig>, IdeProjectOpenError> {
        match &self.input_selection {
            IdeInputSelection::Disabled => Ok(None),
            IdeInputSelection::SystemDefault => {
                let inputs = self
                    .inputs
                    .as_ref()
                    .ok_or(IdeProjectOpenError::InputDiscoveryUnavailable)?;
                if inputs.inputs().is_empty() {
                    return Err(IdeProjectOpenError::SelectedInputUnavailable);
                }
                DesktopInputConfig::try_new(48_000, self.config.maximum_block_frames)
                    .map(Some)
                    .map_err(IdeProjectOpenError::InputConfig)
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
                    .map(Some)
                    .map_err(IdeProjectOpenError::InputConfig)
            }
        }
    }
}

#[derive(Debug)]
pub enum IdeOutputTestError {
    SelectedOutputUnavailable,
    OutputConfig(DesktopOutputConfigError),
    ToneConfig(StereoSineTestToneConfigError),
    OutputStart(DesktopOutputStartError),
    OutputStop(DesktopOutputStopError),
    StartupCancellationCompleted,
    PendingRuntimeFailed,
    PendingThreadPanicked,
}

impl fmt::Display for IdeOutputTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectedOutputUnavailable => {
                formatter.write_str("selected audio output is no longer available")
            }
            Self::OutputConfig(source) => source.fmt(formatter),
            Self::ToneConfig(source) => source.fmt(formatter),
            Self::OutputStart(source) => source.fmt(formatter),
            Self::OutputStop(source) => source.fmt(formatter),
            Self::StartupCancellationCompleted => formatter.write_str(
                "output diagnostic startup failed and its native cancellation completed",
            ),
            Self::PendingRuntimeFailed => {
                formatter.write_str("output diagnostic failed while native shutdown was pending")
            }
            Self::PendingThreadPanicked => formatter
                .write_str("output diagnostic thread panicked while native shutdown was pending"),
        }
    }
}

impl Error for IdeOutputTestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OutputConfig(source) => Some(source),
            Self::ToneConfig(source) => Some(source),
            Self::OutputStart(source) => Some(source),
            Self::OutputStop(source) => Some(source),
            Self::SelectedOutputUnavailable
            | Self::StartupCancellationCompleted
            | Self::PendingRuntimeFailed
            | Self::PendingThreadPanicked => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeOutputTestStartOutcome {
    Started(OutputTestingDesktopIdeApplication),
    Failed {
        application: DesktopIdeApplication,
        reason: IdeOutputTestError,
    },
    Pending(IdePendingOutputTestShutdown),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeOutputTestState {
    Starting,
    Running,
    StopRequested,
    Stopped,
    Failed,
}

impl From<StreamState> for IdeOutputTestState {
    fn from(state: StreamState) -> Self {
        match state {
            StreamState::Starting => Self::Starting,
            StreamState::Running => Self::Running,
            StreamState::StopRequested => Self::StopRequested,
            StreamState::Stopped => Self::Stopped,
            StreamState::Failed => Self::Failed,
        }
    }
}

#[derive(Debug)]
pub struct IdeOutputTestSnapshot {
    frequency_hz: f64,
    level_dbfs: f32,
    state: IdeOutputTestState,
    output: OutputStreamSnapshot,
}

impl IdeOutputTestSnapshot {
    #[must_use]
    pub const fn frequency_hz(&self) -> f64 {
        self.frequency_hz
    }

    #[must_use]
    pub const fn level_dbfs(&self) -> f32 {
        self.level_dbfs
    }

    #[must_use]
    pub const fn state(&self) -> IdeOutputTestState {
        self.state
    }

    #[must_use]
    pub const fn output(&self) -> OutputStreamSnapshot {
        self.output
    }
}

#[derive(Debug)]
pub struct OutputTestingDesktopIdeApplication {
    application: DesktopIdeApplication,
    session: IdeOutputTestSessionOwner,
}

#[derive(Debug)]
enum IdeOutputTestSessionOwner {
    Native(DesktopTestToneSession),
    #[cfg(test)]
    Test {
        snapshot: IdeOutputTestSnapshot,
        stop_failure: Option<IdeNativeCloseFailure>,
    },
}

impl OutputTestingDesktopIdeApplication {
    #[must_use]
    pub fn snapshot(&self) -> IdeOutputTestSnapshot {
        match &self.session {
            IdeOutputTestSessionOwner::Native(session) => {
                let tone = session.config().tone();
                IdeOutputTestSnapshot {
                    frequency_hz: tone.frequency_hz(),
                    level_dbfs: tone.level_dbfs(),
                    state: session.output_status().state().into(),
                    output: session.counters().snapshot(),
                }
            }
            #[cfg(test)]
            IdeOutputTestSessionOwner::Test { snapshot, .. } => IdeOutputTestSnapshot {
                frequency_hz: snapshot.frequency_hz,
                level_dbfs: snapshot.level_dbfs,
                state: snapshot.state,
                output: snapshot.output,
            },
        }
    }

    #[must_use]
    pub fn stop(self) -> IdeOutputTestStopOutcome {
        let Self {
            application,
            session,
        } = self;
        match session {
            IdeOutputTestSessionOwner::Native(session) => match session.stop() {
                Ok(()) => IdeOutputTestStopOutcome::Closed {
                    application,
                    failure: None,
                },
                Err(DesktopOutputStopError::ShutdownPending { shutdown }) => {
                    IdeOutputTestStopOutcome::Pending(IdePendingOutputTestShutdown {
                        application,
                        shutdown: *shutdown,
                        origin: IdeOutputTestPendingOrigin::Stop,
                    })
                }
                Err(source) => IdeOutputTestStopOutcome::Closed {
                    application,
                    failure: Some(IdeOutputTestError::OutputStop(source)),
                },
            },
            #[cfg(test)]
            IdeOutputTestSessionOwner::Test { stop_failure, .. } => {
                IdeOutputTestStopOutcome::Closed {
                    application,
                    failure: stop_failure.map(|failure| {
                        IdeOutputTestError::OutputStop(match failure {
                            IdeNativeCloseFailure::RuntimeFailed => {
                                DesktopOutputStopError::RuntimeFailed
                            }
                            IdeNativeCloseFailure::ThreadPanicked => {
                                DesktopOutputStopError::ThreadPanicked
                            }
                        })
                    }),
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum IdeOutputTestStopOutcome {
    Closed {
        application: DesktopIdeApplication,
        failure: Option<IdeOutputTestError>,
    },
    Pending(IdePendingOutputTestShutdown),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdeOutputTestPendingOrigin {
    Start,
    Stop,
}

#[derive(Debug)]
pub struct IdePendingOutputTestShutdown {
    application: DesktopIdeApplication,
    shutdown: PendingNativeShutdown,
    origin: IdeOutputTestPendingOrigin,
}

#[derive(Debug)]
pub enum IdePendingOutputTestShutdownWait {
    Closed {
        application: DesktopIdeApplication,
        failure: Option<IdeOutputTestError>,
    },
    TimedOut(IdePendingOutputTestShutdown),
}

impl IdePendingOutputTestShutdown {
    #[must_use]
    pub fn wait(mut self, timeout: Duration) -> IdePendingOutputTestShutdownWait {
        match self.shutdown.wait(timeout) {
            PendingNativeShutdownWait::TimedOut => IdePendingOutputTestShutdownWait::TimedOut(self),
            PendingNativeShutdownWait::Completed => IdePendingOutputTestShutdownWait::Closed {
                application: self.application,
                failure: (self.origin == IdeOutputTestPendingOrigin::Start)
                    .then_some(IdeOutputTestError::StartupCancellationCompleted),
            },
            PendingNativeShutdownWait::Failed => IdePendingOutputTestShutdownWait::Closed {
                application: self.application,
                failure: Some(IdeOutputTestError::PendingRuntimeFailed),
            },
            PendingNativeShutdownWait::Panicked => IdePendingOutputTestShutdownWait::Closed {
                application: self.application,
                failure: Some(IdeOutputTestError::PendingThreadPanicked),
            },
        }
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

/// One lock-free control for dry recorded-take audition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeAudioPreviewControlCommand {
    Gain { gain_millionths: u32 },
    Mute { muted: bool },
}

#[derive(Debug)]
pub enum IdeAudioPreviewError {
    Unavailable,
    ExcessiveGain,
    MixerCommit(DesktopDirectControlError),
    MixerCommitTimeout,
    MixerFenceSequenceUnavailable,
    Command(DesktopAudioPreviewCommandError),
    Control(DesktopAudioPreviewControlError),
}

#[derive(Debug)]
pub enum IdeTransportSeekError {
    Unavailable,
    Unbounded,
    OutOfRange { requested: usize, end_frame: usize },
    Send(DesktopDirectControlError),
}

#[derive(Debug)]
pub enum IdeArrangementLocateError {
    Unavailable,
    StaleProject,
    StalePlacement { requested_index: usize },
    Send(DesktopDirectControlError),
}

/// Programme-loudness controls applied at the next audio process boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeLoudnessCommand {
    Run,
    Standby,
    Reset,
}

impl IdeLoudnessCommand {
    const fn runtime(self) -> LoudnessMeasurementCommand {
        match self {
            Self::Run => LoudnessMeasurementCommand::SetState(IdeLoudnessMeasurementState::Running),
            Self::Standby => {
                LoudnessMeasurementCommand::SetState(IdeLoudnessMeasurementState::Standby)
            }
            Self::Reset => LoudnessMeasurementCommand::Reset,
        }
    }
}

#[derive(Debug)]
pub enum IdeLoudnessCommandError {
    Send(LoudnessCommandSendError),
}

impl fmt::Display for IdeLoudnessCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeLoudnessCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Send(source) => Some(source),
        }
    }
}

impl fmt::Display for IdeArrangementLocateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => {
                formatter.write_str("verified playback arrangement is unavailable")
            }
            Self::StaleProject => {
                formatter.write_str("arrangement project authority is no longer current")
            }
            Self::StalePlacement { requested_index } => write!(
                formatter,
                "arrangement placement {requested_index} is no longer current"
            ),
            Self::Send(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeArrangementLocateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Send(source) => Some(source),
            Self::Unavailable | Self::StaleProject | Self::StalePlacement { .. } => None,
        }
    }
}

impl fmt::Display for IdeTransportSeekError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("verified playback extent is unavailable"),
            Self::Unbounded => {
                formatter.write_str("recorded scene authority has no finite playback endpoint")
            }
            Self::OutOfRange {
                requested,
                end_frame,
            } => write!(
                formatter,
                "requested frame {requested} exceeds playback endpoint {end_frame}"
            ),
            Self::Send(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeTransportSeekError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Send(source) => Some(source),
            Self::Unavailable | Self::Unbounded | Self::OutOfRange { .. } => None,
        }
    }
}

impl fmt::Display for IdeAudioPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("audio preview is unavailable"),
            Self::ExcessiveGain => formatter.write_str("audio preview gain must not exceed unity"),
            Self::MixerCommit(source) => {
                write!(
                    formatter,
                    "audio preview stop fence could not be queued: {source}"
                )
            }
            Self::MixerCommitTimeout => {
                formatter.write_str("transport did not stop before audio preview")
            }
            Self::MixerFenceSequenceUnavailable => {
                formatter.write_str("audio preview stop-fence sequence is exhausted")
            }
            Self::Command(source) => source.fmt(formatter),
            Self::Control(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeAudioPreviewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::Control(source) => Some(source),
            Self::MixerCommit(source) => Some(source),
            Self::Unavailable
            | Self::ExcessiveGain
            | Self::MixerCommitTimeout
            | Self::MixerFenceSequenceUnavailable => None,
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
pub enum IdeAudioTrackAdditionError {
    InvalidName,
    MixerSave(Box<IdeProjectSaveError>),
    Serialization(AudioTrackAdditionSerializationError),
    CapacityUnavailable,
    Publication(Box<ProjectResourcePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioTrackAdditionError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: AudioTrackPublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<ProjectResourcePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioTrackAdditionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                formatter.write_str("audio-track name must contain 1 through 120 characters")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-track edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before adding the audio track: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before adding the audio track ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-track preparation failed ({preparation}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio track {} was added in project revision {} but the project could not be reopened: {source}",
                outcome.track_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-track publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioTrackAdditionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidName | Self::CapacityUnavailable | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IdeAudioTrackRemovalBlockers {
    pub playback: AudioTrackRemovalBlockers,
    pub control_mappings: usize,
    pub recording_metadata: usize,
    pub has_automation: bool,
}

impl IdeAudioTrackRemovalBlockers {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.playback.is_empty()
            && self.control_mappings == 0
            && self.recording_metadata == 0
            && !self.has_automation
    }
}

#[derive(Debug)]
pub enum IdeAudioTrackRemovalError {
    InvalidTrackIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    Blocked(IdeAudioTrackRemovalBlockers),
    Serialization(AudioTrackRemovalSerializationError),
    CapacityUnavailable,
    Publication(Box<ProjectResourcePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioTrackRemovalError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: AudioTrackRemovalOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<ProjectResourcePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioTrackRemovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTrackIdentity => {
                formatter.write_str("audio-track removal requires a track identity")
            }
            Self::MixerSave(source) => write!(
                formatter,
                "live mixer state could not be preserved: {source}"
            ),
            Self::Blocked(blockers) => write!(
                formatter,
                "audio track cannot be removed while referenced (clips: {}, child tracks: {}, scene slots: {}, placements: {}, mappings: {}, recordings: {}, automation: {})",
                blockers.playback.clip_references,
                blockers.playback.child_tracks,
                blockers.playback.scene_slots,
                blockers.playback.arrangement_placements,
                blockers.control_mappings,
                blockers.recording_metadata,
                blockers.has_automation
            ),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-track removal capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before removing the audio track: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before removing the audio track ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-track removal preparation failed ({preparation}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio track {} was removed in project revision {} but the project could not be reopened: {source}",
                outcome.track_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-track removal failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioTrackRemovalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidTrackIdentity
            | Self::Blocked(_)
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioTrackEditPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Track {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl fmt::Display for IdeAudioTrackEditPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => {
                write!(
                    formatter,
                    "live mixer could not be preserved before the audio-track edit: {source}"
                )
            }
            Self::Track { source, .. } if source.published() => write!(
                formatter,
                "audio-track edit was published, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Track { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but the audio-track edit failed: {source}",
                mixer.project_revision()
            ),
            Self::Track { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeAudioTrackEditPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Track { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioTrackEditError {
    InvalidName,
    MixerSave(Box<IdeProjectSaveError>),
    TrackResourceMissing,
    TrackResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioTrackDocumentSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioTrackEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioTrackEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioTrackEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioTrackEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                formatter.write_str("audio-track name must contain 1 through 120 characters")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::TrackResourceMissing => {
                formatter.write_str("the selected audio-track resource is no longer present")
            }
            Self::TrackResourceRoleMismatch => {
                formatter.write_str("the selected project resource is not a structural track")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-track edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the audio track: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the audio track ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-track edit preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio track was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-track edit publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioTrackEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidName
            | Self::TrackResourceMissing
            | Self::TrackResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipCreationPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Clip {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl fmt::Display for IdeAudioClipCreationPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before creating the audio clip: {source}"
            ),
            Self::Clip { source, .. } if source.published() => write!(
                formatter,
                "audio clip was created, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Clip { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but audio-clip creation failed: {source}",
                mixer.project_revision()
            ),
            Self::Clip { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeAudioClipCreationPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Clip { source, .. } => Some(source),
        }
    }
}

pub type IdeRecordingClipCreationPublicationError = IdeAudioClipCreationPublicationError;

#[derive(Debug)]
pub enum IdeRecordingClipCreationError {
    InvalidRecordingIdentity,
    InvalidClipName,
    MixerSave(Box<IdeProjectSaveError>),
    RecordingMetadataMissing,
    RecordingMetadataAmbiguous,
    TrackResourceMissing,
    TrackResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipAdditionSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeRecordingClipCreationPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeRecordingClipCreationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: AudioClipPublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeRecordingClipCreationPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeRecordingClipCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRecordingIdentity => {
                formatter.write_str("audio-clip creation requires a recorded-take identity")
            }
            Self::InvalidClipName => {
                formatter.write_str("audio-clip name must contain 1 through 120 characters")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::RecordingMetadataMissing => {
                formatter.write_str("the selected recorded take is no longer present")
            }
            Self::RecordingMetadataAmbiguous => {
                formatter.write_str("the selected recorded-take identity is ambiguous")
            }
            Self::TrackResourceMissing => {
                formatter.write_str("the recorded take's owning track is no longer present")
            }
            Self::TrackResourceRoleMismatch => {
                formatter.write_str("the recorded take's owner is not a structural track")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("recorded-take audio-clip creation capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before creating the audio clip: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before creating the audio clip ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip creation preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio clip {} was created in project revision {} but the project could not be reopened: {source}",
                outcome.clip_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip creation failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeRecordingClipCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidRecordingIdentity
            | Self::InvalidClipName
            | Self::RecordingMetadataMissing
            | Self::RecordingMetadataAmbiguous
            | Self::TrackResourceMissing
            | Self::TrackResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipDuplicationError {
    InvalidSourceClipIdentity,
    InvalidClipName,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    TrackResourceMissing,
    TrackResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipDuplicationSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipCreationPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipDuplicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: AudioClipPublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipCreationPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioClipDuplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSourceClipIdentity => {
                formatter.write_str("audio-clip duplication requires a source clip identity")
            }
            Self::InvalidClipName => formatter
                .write_str("duplicated audio-clip name must contain 1 through 120 characters"),
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ClipResourceMissing => {
                formatter.write_str("the source audio clip is no longer present")
            }
            Self::ClipResourceRoleMismatch => {
                formatter.write_str("the source identity is not an audio-clip resource")
            }
            Self::TrackResourceMissing => {
                formatter.write_str("the source audio clip's owning track is no longer present")
            }
            Self::TrackResourceRoleMismatch => {
                formatter.write_str("the source audio clip's owner is not a structural track")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-clip duplication capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before duplicating the audio clip: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before duplicating the audio clip ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip duplication preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio clip {} was duplicated in project revision {} but the project could not be reopened: {source}",
                outcome.clip_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip duplication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipDuplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidSourceClipIdentity
            | Self::InvalidClipName
            | Self::ClipResourceMissing
            | Self::ClipResourceRoleMismatch
            | Self::TrackResourceMissing
            | Self::TrackResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeArrangementPlacementEditPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Arrangement {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl fmt::Display for IdeArrangementPlacementEditPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before the arrangement edit: {source}"
            ),
            Self::Arrangement { source, .. } if source.published() => write!(
                formatter,
                "arrangement edit was published, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Arrangement { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but the arrangement edit failed: {source}",
                mixer.project_revision()
            ),
            Self::Arrangement { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeArrangementPlacementEditPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Arrangement { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeArrangementPlacementEditError {
    InvalidPlacementIdentity,
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ArrangementResourceMissing,
    ArrangementResourceAmbiguous,
    Storage(StorageError),
    Serialization(ArrangementPlacementDocumentSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeArrangementPlacementEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeArrangementPlacementEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeArrangementPlacementEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeArrangementPlacementEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlacementIdentity => {
                formatter.write_str("arrangement edit requires a placement identity")
            }
            Self::InvalidClipIdentity => formatter
                .write_str("arrangement placement addition requires an audio-clip identity"),
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ArrangementResourceMissing => {
                formatter.write_str("the verified project has no arrangement resource")
            }
            Self::ArrangementResourceAmbiguous => {
                formatter.write_str("the verified project has multiple arrangement resources")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("arrangement edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the arrangement: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the arrangement ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "arrangement edit preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "arrangement was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "arrangement edit publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeArrangementPlacementEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidPlacementIdentity
            | Self::InvalidClipIdentity
            | Self::ArrangementResourceMissing
            | Self::ArrangementResourceAmbiguous
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipEditPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Clip {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl fmt::Display for IdeAudioClipEditPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before the audio-clip edit: {source}"
            ),
            Self::Clip { source, .. } if source.published() => write!(
                formatter,
                "audio-clip edit was published, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Clip { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but the audio-clip edit failed: {source}",
                mixer.project_revision()
            ),
            Self::Clip { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeAudioClipEditPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Clip { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipRenameEditError {
    InvalidClipIdentity,
    InvalidName,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipRenameSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipRenameEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioClipRenameEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip rename requires a clip identity")
            }
            Self::InvalidName => {
                formatter.write_str("audio-clip rename requires a name of 1 to 120 characters")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ClipResourceMissing => {
                formatter.write_str("the selected audio-clip resource is missing")
            }
            Self::ClipResourceRoleMismatch => {
                formatter.write_str("the selected identity is not an audio-clip resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-clip rename capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before renaming the audio clip: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before renaming the audio clip ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip rename preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio clip was renamed in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip rename publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipRenameEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidClipIdentity
            | Self::InvalidName
            | Self::ClipResourceMissing
            | Self::ClipResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipGainEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipGainSerializationError),
    NativeValidation(AudioClipGainValidationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipGainEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioClipGainEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip gain edit requires a clip identity")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ClipResourceMissing => {
                formatter.write_str("the selected audio-clip resource is missing")
            }
            Self::ClipResourceRoleMismatch => {
                formatter.write_str("the selected identity is not an audio-clip resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::NativeValidation(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-clip gain edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip gain: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip gain ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip gain preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip gain was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip gain publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipGainEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::NativeValidation(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidClipIdentity
            | Self::ClipResourceMissing
            | Self::ClipResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAudioClipActivationEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipDocumentSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipActivationEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: ProjectResourcePublicationOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl fmt::Display for IdeAudioClipActivationEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip activation edit requires a clip identity")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ClipResourceMissing => {
                formatter.write_str("the selected audio-clip resource is missing")
            }
            Self::ClipResourceRoleMismatch => {
                formatter.write_str("the selected identity is not an audio-clip resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-clip activation edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip activation: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip activation ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip activation preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip activation was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip activation publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipActivationEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidClipIdentity
            | Self::ClipResourceMissing
            | Self::ClipResourceRoleMismatch
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

#[derive(Debug)]
struct IdeAudioClipCreationPlan {
    mixer: IdeMixerSavePlan,
    clip_id: String,
    clip_bytes: Vec<u8>,
    track_id: String,
    track_bytes: Vec<u8>,
    asset_id: String,
}

#[derive(Debug)]
struct IdeAudioTrackAdditionPlan {
    project_path: PathBuf,
    expected_project_sha256: [u8; 32],
    track_id: String,
    track_bytes: Vec<u8>,
    mixer_id: String,
    mixer_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeAudioTrackRemovalPlan {
    project_path: PathBuf,
    expected_project_sha256: [u8; 32],
    track_id: String,
    mixer_id: String,
    mixer_bytes: Vec<u8>,
}

impl IdeAudioTrackRemovalPlan {
    fn publish(&self) -> Result<AudioTrackRemovalOutcome, ProjectResourcePublicationError> {
        remove_audio_track(
            &self.project_path,
            AudioTrackRemovalRequest {
                expected_project_sha256: self.expected_project_sha256,
                track_id: &self.track_id,
                mixer_id: &self.mixer_id,
                mixer_bytes: &self.mixer_bytes,
            },
        )
    }
}

#[derive(Debug)]
struct IdeAudioTrackEditPlan {
    mixer: IdeMixerSavePlan,
    track_id: String,
    track_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeArrangementPlacementEditPlan {
    mixer: IdeMixerSavePlan,
    arrangement_id: String,
    arrangement_bytes: Vec<u8>,
    addition: bool,
}

#[derive(Debug)]
struct IdeAudioClipEditPlan {
    mixer: IdeMixerSavePlan,
    clip_id: String,
    clip_bytes: Vec<u8>,
}

fn prepare_audio_clip_rename_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    clip_name: &str,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipRenameEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipRenameEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipRenameEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipRenameEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_rename(project, &source, clip_id, clip_name, limits)
        .map_err(IdeAudioClipRenameEditError::Serialization)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipRenameEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

fn prepare_audio_clip_activation_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    active: bool,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipActivationEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipActivationEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipActivationEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipActivationEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_activation(project, &source, clip_id, active, limits)
        .map_err(IdeAudioClipActivationEditError::Serialization)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipActivationEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

fn prepare_audio_clip_gain_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    gain_decibels: f64,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipGainEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipGainEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipGainEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipGainEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_gain(project, &source, clip_id, gain_decibels, limits)
        .map_err(IdeAudioClipGainEditError::Serialization)?;
    prepared
        .session()
        .validate_audio_clip_gain(clip_id, gain_decibels)
        .map_err(IdeAudioClipGainEditError::NativeValidation)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipGainEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

impl IdeAudioClipEditPlan {
    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAudioClipEditPublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAudioClipEditPublicationError::Mixer(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.clip_id,
                expected_role: ResourceRole::Clip,
                resource_bytes: &self.clip_bytes,
            },
        )
        .map_err(|source| IdeAudioClipEditPublicationError::Clip {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeAudioClipCreationPlan {
    fn publish(&self) -> Result<AudioClipPublicationOutcome, IdeAudioClipCreationPublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAudioClipCreationPublicationError::Mixer(Box::new(source)))?;
        publish_audio_clip(
            &self.mixer.project_path,
            AudioClipPublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                clip_id: &self.clip_id,
                clip_bytes: &self.clip_bytes,
                track_id: &self.track_id,
                track_bytes: &self.track_bytes,
                asset_id: &self.asset_id,
            },
        )
        .map_err(|source| IdeAudioClipCreationPublicationError::Clip {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeArrangementPlacementEditPlan {
    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeArrangementPlacementEditPublicationError>
    {
        let mixer = self.mixer.publish().map_err(|source| {
            IdeArrangementPlacementEditPublicationError::Mixer(Box::new(source))
        })?;
        let publication = if self.addition {
            publish_arrangement(
                &self.mixer.project_path,
                ArrangementPublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    arrangement_id: &self.arrangement_id,
                    arrangement_bytes: &self.arrangement_bytes,
                },
            )
        } else {
            publish_project_resource(
                &self.mixer.project_path,
                ProjectResourcePublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    resource_id: &self.arrangement_id,
                    expected_role: ResourceRole::Arrangement,
                    resource_bytes: &self.arrangement_bytes,
                },
            )
        };
        publication.map_err(
            |source| IdeArrangementPlacementEditPublicationError::Arrangement {
                mixer: Box::new(mixer),
                source: Box::new(source),
            },
        )
    }
}

impl IdeAudioTrackEditPlan {
    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAudioTrackEditPublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAudioTrackEditPublicationError::Mixer(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.track_id,
                expected_role: ResourceRole::Track,
                resource_bytes: &self.track_bytes,
            },
        )
        .map_err(|source| IdeAudioTrackEditPublicationError::Track {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeAudioTrackAdditionPlan {
    fn publish(&self) -> Result<AudioTrackPublicationOutcome, ProjectResourcePublicationError> {
        publish_audio_track(
            &self.project_path,
            AudioTrackPublicationRequest {
                expected_project_sha256: self.expected_project_sha256,
                track_id: &self.track_id,
                track_bytes: &self.track_bytes,
                mixer_id: &self.mixer_id,
                mixer_bytes: &self.mixer_bytes,
            },
        )
    }
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

#[derive(Clone, Debug)]
pub struct RunningIdeSnapshot<'a> {
    pub(crate) loaded: &'a hyper_playback_json::LoadedPlaybackProject,
    pub project_path: &'a Path,
    pub project_id: &'a str,
    pub project_name: &'a str,
    pub sample_rate_hz: u32,
    pub playback_extent: IdePlaybackExtent,
    pub arrangement: Arc<IdePlaybackArrangement>,
    pub recording_metadata: &'a [hyper_recording_metadata::RecordingMetadataDocument],
    pub scene_count: usize,
    pub track_count: usize,
    pub scene_names: [&'a str; MAX_IDE_SCENES],
    pub scene_availability: [IdeSceneAvailability; MAX_IDE_SCENES],
    pub mixer_channel_count: usize,
    pub mixer_channel_ids: [&'a str; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_names: [&'a str; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_kinds: [IdeMixerChannelKind; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_active: [bool; MAX_IDE_MIXER_CHANNELS],
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
    pub preview: DesktopAudioPreviewSnapshot,
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
    loudness_commands: Vec<IdeLoudnessCommand>,
    mixer_commands: Vec<IdeMixerCommand>,
    meters: OutputMeterReceiver,
    close_failure: Option<IdeNativeCloseFailure>,
    save_eligible: bool,
    commit_mixer_fences: bool,
    committed_mixer_fence: u64,
    next_mixer_fence: u64,
    extent: IdePlaybackExtent,
    arrangement_project_id: String,
    arrangement_placements: Vec<(String, usize)>,
}

#[cfg(test)]
impl Default for TestPlaybackSession {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            loudness_commands: Vec::new(),
            mixer_commands: Vec::new(),
            meters: output_meter_channel().1,
            close_failure: None,
            save_eligible: false,
            commit_mixer_fences: false,
            committed_mixer_fence: 0,
            next_mixer_fence: 1,
            extent: IdePlaybackExtent::Bounded { end_frame: 0 },
            arrangement_project_id: String::new(),
            arrangement_placements: Vec::new(),
        }
    }
}

impl RunningDesktopIdeApplication {
    pub(crate) fn audio_preview_is_active(&self) -> bool {
        match &self.session {
            IdePlaybackSessionOwner::Native(session) => {
                session.audio_preview_snapshot().state().is_active()
            }
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => false,
        }
    }

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

    fn prepare_audio_track_addition(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        track_name: &str,
    ) -> Result<IdeAudioTrackAdditionPlan, IdeAudioTrackAdditionError> {
        let name_length = track_name.chars().count();
        if track_name.trim() != track_name || !(1..=120).contains(&name_length) {
            return Err(IdeAudioTrackAdditionError::InvalidName);
        }
        let mixer_plan = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioTrackAdditionError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioTrackAdditionError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeAudioTrackAdditionError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let track_id = uuid::Uuid::new_v4().to_string();
        let documents = serialize_audio_track_addition(
            project,
            &assets,
            &mixer_plan.resource_bytes,
            &mixer_plan.resource_id,
            &track_id,
            track_name,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeAudioTrackAdditionError::Serialization)?;
        let (track_bytes, mixer_bytes) = documents.into_parts();
        Ok(IdeAudioTrackAdditionPlan {
            project_path: mixer_plan.project_path,
            expected_project_sha256: mixer_plan.expected_project_sha256,
            track_id,
            track_bytes,
            mixer_id: mixer_plan.resource_id,
            mixer_bytes,
        })
    }

    fn prepare_audio_track_removal(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        track_id: &str,
    ) -> Result<IdeAudioTrackRemovalPlan, IdeAudioTrackRemovalError> {
        let mixer_plan = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioTrackRemovalError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioTrackRemovalError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (project, mixer, _, media) = prepared.session().parts();
        let playback = project.audio_track_removal_blockers(track_id);
        let track_slot = mixer
            .slots()
            .iter()
            .find(|slot| slot.track_id() == track_id);
        let control_mappings = prepared.control_mapping().map_or(0, |mapping| {
            mapping
                .bindings()
                .iter()
                .filter(|binding| {
                    control_binding_references_audio_track(binding, track_id, |send_id| {
                        track_slot.is_some_and(|slot| {
                            slot.sends().iter().any(|send| send.id() == send_id)
                        })
                    })
                })
                .count()
        });
        let recording_metadata = prepared
            .recording_metadata()
            .iter()
            .filter(|metadata| metadata.track_id() == track_id)
            .count();
        let blockers = IdeAudioTrackRemovalBlockers {
            playback,
            control_mappings,
            recording_metadata,
            has_automation: prepared.session().has_automation(),
        };
        if !blockers.is_empty() {
            return Err(IdeAudioTrackRemovalError::Blocked(blockers));
        }
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeAudioTrackRemovalError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let mixer_bytes = serialize_audio_track_removal(
            project,
            &assets,
            &mixer_plan.resource_bytes,
            &mixer_plan.resource_id,
            track_id,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeAudioTrackRemovalError::Serialization)?;
        let mut owned_track_id = String::new();
        owned_track_id
            .try_reserve_exact(track_id.len())
            .map_err(|_| IdeAudioTrackRemovalError::CapacityUnavailable)?;
        owned_track_id.push_str(track_id);
        Ok(IdeAudioTrackRemovalPlan {
            project_path: mixer_plan.project_path,
            expected_project_sha256: mixer_plan.expected_project_sha256,
            track_id: owned_track_id,
            mixer_id: mixer_plan.resource_id,
            mixer_bytes,
        })
    }

    fn prepare_audio_track_rename(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        track_id: &str,
        track_name: &str,
    ) -> Result<IdeAudioTrackEditPlan, IdeAudioTrackEditError> {
        let name_length = track_name.chars().count();
        if track_name.trim() != track_name || !(1..=120).contains(&name_length) {
            return Err(IdeAudioTrackEditError::InvalidName);
        }
        Self::prepare_audio_track_edit(
            application,
            recovery,
            track_id,
            |project, source, limits| {
                serialize_audio_track_rename(project, source, track_id, track_name, limits).map_err(
                    |source| match source {
                        AudioTrackRenameSerializationError::InvalidTrackName => {
                            IdeAudioTrackEditError::InvalidName
                        }
                        AudioTrackRenameSerializationError::Document(source) => {
                            IdeAudioTrackEditError::Serialization(source)
                        }
                    },
                )
            },
        )
    }

    fn prepare_audio_track_activation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        track_id: &str,
        active: bool,
    ) -> Result<IdeAudioTrackEditPlan, IdeAudioTrackEditError> {
        Self::prepare_audio_track_edit(
            application,
            recovery,
            track_id,
            |project, source, limits| {
                serialize_audio_track_activation(project, source, track_id, active, limits)
                    .map_err(IdeAudioTrackEditError::Serialization)
            },
        )
    }

    fn prepare_audio_track_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        track_id: &str,
        serialize: impl FnOnce(
            &hyper_playback_json::LoadedPlaybackProject,
            &[u8],
            LoaderLimits,
        ) -> Result<Vec<u8>, IdeAudioTrackEditError>,
    ) -> Result<IdeAudioTrackEditPlan, IdeAudioTrackEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioTrackEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioTrackEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == track_id)
            .ok_or(IdeAudioTrackEditError::TrackResourceMissing)?;
        if descriptor.role() != ResourceRole::Track {
            return Err(IdeAudioTrackEditError::TrackResourceRoleMismatch);
        }
        let source = package
            .read_resource(track_id)
            .map_err(IdeAudioTrackEditError::Storage)?;
        let (project, _, _, _) = prepared.session().parts();
        let track_bytes = serialize(
            project,
            &source,
            application.config.project_limits.session.documents,
        )?;
        let mut owned_track_id = String::new();
        owned_track_id
            .try_reserve_exact(track_id.len())
            .map_err(|_| IdeAudioTrackEditError::CapacityUnavailable)?;
        owned_track_id.push_str(track_id);
        Ok(IdeAudioTrackEditPlan {
            mixer,
            track_id: owned_track_id,
            track_bytes,
        })
    }

    fn prepare_arrangement_placement_activation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        placement_id: &str,
        active: bool,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_placement_edit(
            application,
            recovery,
            |prepared, source, limits| {
                let (project, _, _, _) = prepared.session().parts();
                serialize_arrangement_placement_activation(
                    project,
                    source,
                    placement_id,
                    active,
                    limits,
                )
            },
        )
    }

    fn prepare_recording_clip_creation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        recording_id: &str,
        clip_name: &str,
    ) -> Result<IdeAudioClipCreationPlan, IdeRecordingClipCreationError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeRecordingClipCreationError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeRecordingClipCreationError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_recording_clip_creation_from_project(
            application,
            mixer,
            prepared,
            recording_id,
            clip_name,
        )
    }

    fn prepare_recording_clip_creation_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        recording_id: &str,
        clip_name: &str,
    ) -> Result<IdeAudioClipCreationPlan, IdeRecordingClipCreationError> {
        let mut recordings = prepared
            .recording_metadata()
            .iter()
            .filter(|metadata| metadata.id() == recording_id);
        let metadata = recordings
            .next()
            .ok_or(IdeRecordingClipCreationError::RecordingMetadataMissing)?;
        if recordings.next().is_some() {
            return Err(IdeRecordingClipCreationError::RecordingMetadataAmbiguous);
        }
        let package = prepared.package();
        let track_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == metadata.track_id())
            .ok_or(IdeRecordingClipCreationError::TrackResourceMissing)?;
        if track_descriptor.role() != ResourceRole::Track {
            return Err(IdeRecordingClipCreationError::TrackResourceRoleMismatch);
        }
        let source_track = package
            .read_resource(metadata.track_id())
            .map_err(IdeRecordingClipCreationError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeRecordingClipCreationError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let clip_id = uuid::Uuid::new_v4().to_string();
        let documents = serialize_recording_audio_clip_addition(
            project,
            &assets,
            &source_track,
            metadata.track_id(),
            metadata.asset_id(),
            metadata.timeline_start_tick(),
            metadata.frame_count(),
            &clip_id,
            clip_name,
            application.config.project_limits.session.documents,
        )
        .map_err(|source| match source {
            AudioClipAdditionSerializationError::InvalidClipName => {
                IdeRecordingClipCreationError::InvalidClipName
            }
            source => IdeRecordingClipCreationError::Serialization(source),
        })?;
        let (track_bytes, clip_bytes, _duration_ticks) = documents.into_parts();
        let mut track_id = String::new();
        let mut asset_id = String::new();
        track_id
            .try_reserve_exact(metadata.track_id().len())
            .map_err(|_| IdeRecordingClipCreationError::CapacityUnavailable)?;
        asset_id
            .try_reserve_exact(metadata.asset_id().len())
            .map_err(|_| IdeRecordingClipCreationError::CapacityUnavailable)?;
        track_id.push_str(metadata.track_id());
        asset_id.push_str(metadata.asset_id());
        Ok(IdeAudioClipCreationPlan {
            mixer,
            clip_id,
            clip_bytes,
            track_id,
            track_bytes,
            asset_id,
        })
    }

    fn prepare_audio_clip_duplication(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        source_clip_id: &str,
        clip_name: &str,
    ) -> Result<IdeAudioClipCreationPlan, IdeAudioClipDuplicationError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipDuplicationError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipDuplicationError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_audio_clip_duplication_from_project(
            application,
            mixer,
            prepared,
            source_clip_id,
            clip_name,
        )
    }

    fn prepare_audio_clip_duplication_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        source_clip_id: &str,
        clip_name: &str,
    ) -> Result<IdeAudioClipCreationPlan, IdeAudioClipDuplicationError> {
        let package = prepared.package();
        let clip_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == source_clip_id)
            .ok_or(IdeAudioClipDuplicationError::ClipResourceMissing)?;
        if clip_descriptor.role() != ResourceRole::Clip {
            return Err(IdeAudioClipDuplicationError::ClipResourceRoleMismatch);
        }
        let source_clip = package
            .read_resource(source_clip_id)
            .map_err(IdeAudioClipDuplicationError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let source_clip_index = (0..project.clip_count())
            .find(|index| project.clip_id_at(*index) == Some(source_clip_id))
            .ok_or(IdeAudioClipDuplicationError::ClipResourceMissing)?;
        let track_id = project
            .clip_track_id_at(source_clip_index)
            .ok_or(IdeAudioClipDuplicationError::TrackResourceMissing)?;
        let track_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == track_id)
            .ok_or(IdeAudioClipDuplicationError::TrackResourceMissing)?;
        if track_descriptor.role() != ResourceRole::Track {
            return Err(IdeAudioClipDuplicationError::TrackResourceRoleMismatch);
        }
        let source_track = package
            .read_resource(track_id)
            .map_err(IdeAudioClipDuplicationError::Storage)?;
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeAudioClipDuplicationError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let clip_id = uuid::Uuid::new_v4().to_string();
        let documents = serialize_audio_clip_duplication(
            project,
            &assets,
            &source_clip,
            &source_track,
            source_clip_id,
            &clip_id,
            clip_name,
            application.config.project_limits.session.documents,
        )
        .map_err(|source| match source {
            AudioClipDuplicationSerializationError::InvalidClipName => {
                IdeAudioClipDuplicationError::InvalidClipName
            }
            source => IdeAudioClipDuplicationError::Serialization(source),
        })?;
        let (track_bytes, clip_bytes, asset_id) = documents.into_parts();
        let mut owned_track_id = String::new();
        owned_track_id
            .try_reserve_exact(track_id.len())
            .map_err(|_| IdeAudioClipDuplicationError::CapacityUnavailable)?;
        owned_track_id.push_str(track_id);
        Ok(IdeAudioClipCreationPlan {
            mixer,
            clip_id,
            clip_bytes,
            track_id: owned_track_id,
            track_bytes,
            asset_id,
        })
    }

    fn prepare_arrangement_placement_addition(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        start_tick: u64,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeArrangementPlacementEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeArrangementPlacementEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let package = prepared.package();
        let mut descriptors = package
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Arrangement);
        let descriptor = descriptors.next();
        if descriptors.next().is_some() {
            return Err(IdeArrangementPlacementEditError::ArrangementResourceAmbiguous);
        }
        let addition = descriptor.is_none();
        let arrangement_id = descriptor.map_or_else(
            || uuid::Uuid::new_v4().to_string(),
            |descriptor| descriptor.id().to_owned(),
        );
        let source = descriptor
            .map(|descriptor| package.read_resource(descriptor.id()))
            .transpose()
            .map_err(IdeArrangementPlacementEditError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let placement_id = uuid::Uuid::new_v4().to_string();
        let arrangement_bytes = serialize_arrangement_placement_addition(
            project,
            &assets,
            source.as_deref(),
            &arrangement_id,
            &placement_id,
            clip_id,
            start_tick,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeArrangementPlacementEditError::Serialization)?;
        Ok(IdeArrangementPlacementEditPlan {
            mixer,
            arrangement_id,
            arrangement_bytes,
            addition,
        })
    }

    fn prepare_arrangement_placement_move(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        placement_id: &str,
        start_tick: u64,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_placement_edit(
            application,
            recovery,
            |prepared, source, limits| {
                let (project, _, _, _) = prepared.session().parts();
                serialize_arrangement_placement_move(
                    project,
                    source,
                    placement_id,
                    start_tick,
                    limits,
                )
            },
        )
    }

    fn prepare_arrangement_placement_duration(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        placement_id: &str,
        duration_ticks: u64,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_placement_edit(
            application,
            recovery,
            |prepared, source, limits| {
                let (project, _, _, _) = prepared.session().parts();
                serialize_arrangement_placement_duration(
                    project,
                    source,
                    placement_id,
                    duration_ticks,
                    limits,
                )
            },
        )
    }

    fn prepare_arrangement_placement_front_trim(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        placement_id: &str,
        start_tick: u64,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_placement_edit(
            application,
            recovery,
            |prepared, source, limits| {
                let (project, _, _, _) = prepared.session().parts();
                serialize_arrangement_placement_front_trim(
                    project,
                    source,
                    placement_id,
                    start_tick,
                    limits,
                )
            },
        )
    }

    fn prepare_arrangement_placement_removal(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        placement_id: &str,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_placement_edit(
            application,
            recovery,
            |prepared, source, limits| {
                let (project, _, _, _) = prepared.session().parts();
                serialize_arrangement_placement_removal(project, source, placement_id, limits)
            },
        )
    }

    fn prepare_arrangement_placement_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        serialize: impl FnOnce(
            &PreparedProjectPlayback,
            &[u8],
            LoaderLimits,
        )
            -> Result<Vec<u8>, ArrangementPlacementDocumentSerializationError>,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeArrangementPlacementEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeArrangementPlacementEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let package = prepared.package();
        let mut descriptors = package
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Arrangement);
        let descriptor = descriptors
            .next()
            .ok_or(IdeArrangementPlacementEditError::ArrangementResourceMissing)?;
        if descriptors.next().is_some() {
            return Err(IdeArrangementPlacementEditError::ArrangementResourceAmbiguous);
        }
        let source = package
            .read_resource(descriptor.id())
            .map_err(IdeArrangementPlacementEditError::Storage)?;
        let arrangement_bytes = serialize(
            prepared,
            &source,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeArrangementPlacementEditError::Serialization)?;
        let mut arrangement_id = String::new();
        arrangement_id
            .try_reserve_exact(descriptor.id().len())
            .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
        arrangement_id.push_str(descriptor.id());
        Ok(IdeArrangementPlacementEditPlan {
            mixer,
            arrangement_id,
            arrangement_bytes,
            addition: false,
        })
    }

    fn prepare_audio_clip_rename(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        clip_name: &str,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipRenameEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipRenameEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipRenameEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_rename_resource(
            prepared,
            clip_id,
            clip_name,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
        })
    }

    fn prepare_audio_clip_gain(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        gain_decibels: f64,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipGainEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipGainEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipGainEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_gain_resource(
            prepared,
            clip_id,
            gain_decibels,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
        })
    }

    fn prepare_audio_clip_activation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        active: bool,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipActivationEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipActivationEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipActivationEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_activation_resource(
            prepared,
            clip_id,
            active,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
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
    #[allow(
        clippy::too_many_lines,
        reason = "the snapshot performs one linear bounded assembly of the verified project and runtime state"
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
        let mut mixer_channel_ids = [""; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_channel_names = [""; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_channel_kinds = [IdeMixerChannelKind::Audio; MAX_IDE_MIXER_CHANNELS];
        let mut mixer_channel_active = [false; MAX_IDE_MIXER_CHANNELS];
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
            mixer_channel_ids[channel_index] = slot.track_id();
            mixer_channel_names[channel_index] = loaded.track_name(slot.track_id())?;
            mixer_channel_active[channel_index] = loaded.track_active(slot.track_id())?;
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
            loaded,
            project_path: project.package().root(),
            project_id: project.package().manifest().project_id(),
            project_name: project.package().manifest().name(),
            sample_rate_hz: project.session().sample_rate_hz(),
            playback_extent: project.session().extent(),
            arrangement: project.session().shared_arrangement(),
            recording_metadata: project.recording_metadata(),
            scene_count,
            track_count: loaded.track_count(),
            scene_names,
            scene_availability,
            mixer_channel_count,
            mixer_channel_ids,
            mixer_channel_names,
            mixer_channel_kinds,
            mixer_channel_active,
            mixer_channel_states,
            mixer_send_counts,
            mixer_send_target_names,
            mixer_send_taps,
            mixer_send_states,
            mixer_main_state,
            output_state: session.output_status().state(),
            output: session.counters().snapshot(),
            monitoring,
            preview: session.audio_preview_snapshot(),
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

    /// Sends one bounded programme-loudness command through the live output
    /// owner. Momentary and short-term measurement remain live in standby;
    /// reset clears programme and maximum measurements together.
    ///
    /// # Errors
    ///
    /// Returns queue saturation or disconnection without changing UI state.
    pub fn try_send_loudness(
        &mut self,
        command: IdeLoudnessCommand,
    ) -> Result<(), IdeLoudnessCommandError> {
        match &mut self.session {
            IdePlaybackSessionOwner::Native(session) => session
                .loudness_command_sender()
                .try_send(command.runtime())
                .map_err(IdeLoudnessCommandError::Send),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                session.loudness_commands.push(command);
                Ok(())
            }
        }
    }

    /// Locates within the exact finite endpoint compiled for this project.
    ///
    /// # Errors
    ///
    /// Rejects non-project sessions, open-ended recorded scene authority,
    /// out-of-range frames, and bounded command delivery failures.
    pub fn try_seek(&mut self, frame: usize) -> Result<(), IdeTransportSeekError> {
        let extent = match &self.session {
            IdePlaybackSessionOwner::Native(session) => session
                .prepared_project()
                .ok_or(IdeTransportSeekError::Unavailable)?
                .session()
                .extent(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => session.extent,
        };
        let IdePlaybackExtent::Bounded { end_frame } = extent else {
            return Err(IdeTransportSeekError::Unbounded);
        };
        if frame > end_frame {
            return Err(IdeTransportSeekError::OutOfRange {
                requested: frame,
                end_frame,
            });
        }
        self.try_send_transport(TransportCommand::Locate {
            frame,
            state: LocateState::Preserve,
        })
        .map_err(IdeTransportSeekError::Send)
    }

    /// Locates to one exact compiled placement start after validating the
    /// immutable placement identity published to the UI.
    ///
    /// # Errors
    ///
    /// Rejects unavailable project authority, stale project or placement
    /// identities, and command delivery failures without queueing a locate.
    pub fn try_locate_arrangement_placement(
        &mut self,
        project_id: &str,
        index: usize,
        placement_id: &str,
    ) -> Result<usize, IdeArrangementLocateError> {
        let frame = match &self.session {
            IdePlaybackSessionOwner::Native(session) => {
                let prepared = session
                    .prepared_project()
                    .ok_or(IdeArrangementLocateError::Unavailable)?;
                if prepared.session().project_id() != project_id {
                    return Err(IdeArrangementLocateError::StaleProject);
                }
                let arrangement = prepared.session().arrangement();
                let placement = arrangement.placements().get(index).ok_or(
                    IdeArrangementLocateError::StalePlacement {
                        requested_index: index,
                    },
                )?;
                if placement.id() != placement_id {
                    return Err(IdeArrangementLocateError::StalePlacement {
                        requested_index: index,
                    });
                }
                placement.start_frame()
            }
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                if session.arrangement_project_id != project_id {
                    return Err(IdeArrangementLocateError::StaleProject);
                }
                let (current_id, frame) = session.arrangement_placements.get(index).ok_or(
                    IdeArrangementLocateError::StalePlacement {
                        requested_index: index,
                    },
                )?;
                if current_id != placement_id {
                    return Err(IdeArrangementLocateError::StalePlacement {
                        requested_index: index,
                    });
                }
                *frame
            }
        };
        self.try_send_transport(TransportCommand::Locate {
            frame,
            state: LocateState::Preserve,
        })
        .map_err(IdeArrangementLocateError::Send)?;
        Ok(frame)
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

    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the test-only session variant makes this match fallible in test builds"
    )]
    pub(crate) fn validate_audio_preview_asset(
        &self,
        asset_id: &str,
    ) -> Result<(), IdeAudioPreviewError> {
        let session = match &self.session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => {
                return Err(IdeAudioPreviewError::Unavailable);
            }
        };
        if session.audio_preview_snapshot().state().is_active() {
            return Err(IdeAudioPreviewError::Command(
                DesktopAudioPreviewCommandError::PreviewBusy,
            ));
        }
        let project = session
            .prepared_project()
            .ok_or(IdeAudioPreviewError::Command(
                DesktopAudioPreviewCommandError::ProjectUnavailable,
            ))?;
        project
            .recording_metadata()
            .iter()
            .any(|recording| recording.asset_id() == asset_id)
            .then_some(())
            .ok_or(IdeAudioPreviewError::Command(
                DesktopAudioPreviewCommandError::RecordingUnavailable,
            ))
    }

    /// Starts one verified recorded take on the session's dry audition path.
    ///
    /// # Errors
    ///
    /// Rejects test-only/non-project sessions, unknown assets, and overlapping
    /// previews without changing the active callback state.
    pub fn try_start_audio_preview(&self, asset_id: &str) -> Result<(), IdeAudioPreviewError> {
        match &self.session {
            IdePlaybackSessionOwner::Native(session) => session
                .try_start_audio_preview(asset_id)
                .map_err(IdeAudioPreviewError::Command),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => Err(IdeAudioPreviewError::Unavailable),
        }
    }

    /// Requests a bounded ramp to silence for the active recorded-take preview.
    ///
    /// # Errors
    ///
    /// Rejects an inactive or unavailable preview path.
    pub fn try_stop_audio_preview(&self) -> Result<(), IdeAudioPreviewError> {
        match &self.session {
            IdePlaybackSessionOwner::Native(session) => session
                .try_stop_audio_preview()
                .map_err(IdeAudioPreviewError::Command),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => Err(IdeAudioPreviewError::Unavailable),
        }
    }

    /// Applies one lock-free gain or mute control to recorded-take audition.
    ///
    /// # Errors
    ///
    /// Rejects test-only sessions, gain above unity, and backend validation
    /// failures without mutation.
    #[allow(
        clippy::cast_precision_loss,
        reason = "the integer preview domain is bounded below exact f32 integer range"
    )]
    #[allow(
        clippy::infallible_destructuring_match,
        reason = "the test-only session variant makes this match fallible in test builds"
    )]
    pub fn apply_audio_preview_control(
        &self,
        command: IdeAudioPreviewControlCommand,
    ) -> Result<(), IdeAudioPreviewError> {
        let session = match &self.session {
            IdePlaybackSessionOwner::Native(session) => session,
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => {
                return Err(IdeAudioPreviewError::Unavailable);
            }
        };
        let control = session.audio_preview_control();
        match command {
            IdeAudioPreviewControlCommand::Gain { gain_millionths } => {
                if gain_millionths > PREVIEW_GAIN_MILLIONTHS_MAXIMUM {
                    return Err(IdeAudioPreviewError::ExcessiveGain);
                }
                let gain = gain_millionths as f32 / PREVIEW_GAIN_MILLIONTHS_MAXIMUM as f32;
                control
                    .set_gain_linear(gain)
                    .map_err(IdeAudioPreviewError::Control)
            }
            IdeAudioPreviewControlCommand::Mute { muted } => {
                control.set_muted(muted);
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

fn control_binding_references_audio_track(
    binding: &ControlBinding,
    track_id: &str,
    send_owned_by_track: impl Fn(&str) -> bool,
) -> bool {
    matches!(binding.scope(), ControlScope::Track { track_id: id } if id == track_id)
        || match binding.target() {
            ControlTarget::MixerChannel { track_id: id, .. } => id == track_id,
            ControlTarget::MixerSend { send_id, .. } => send_owned_by_track(send_id),
            ControlTarget::MixerMain { .. }
            | ControlTarget::MixerCue { .. }
            | ControlTarget::Scene { .. }
            | ControlTarget::Transport { .. } => false,
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
    use hyper_control_mappings::{
        ChannelParameter, ControlSource, ControlTransform, SendParameter,
    };
    use hyper_media_cache::CacheLimits;
    use hyper_playback_json::LoaderLimits;
    use hyper_playback_session::PlaybackSessionAssemblyLimits;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hyper-desktop-ide-track-rename-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    const ARRANGEMENT_TEST_PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338500";
    const ARRANGEMENT_TEST_TIMELINE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338501";
    const ARRANGEMENT_TEST_TRACK_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338502";
    const ARRANGEMENT_TEST_CLIP_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338503";
    const ARRANGEMENT_TEST_ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338504";
    const ARRANGEMENT_TEST_RESOURCE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338505";
    const ARRANGEMENT_TEST_PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338506";
    const ARRANGEMENT_TEST_MIXER_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338507";

    struct ArrangementProjectFixture {
        directory: TestDirectory,
    }

    impl ArrangementProjectFixture {
        fn new() -> Self {
            Self::with_audio(&arrangement_test_wav(&[0; 100]))
        }

        fn without_arrangement() -> Self {
            let fixture = Self::new();
            let manifest_path = fixture.directory.0.join("project.json");
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            let resources = manifest["resources"].as_array_mut().unwrap();
            let index = resources
                .iter()
                .position(|descriptor| descriptor["role"] == "arrangement")
                .unwrap();
            let path = resources[index]["path"].as_str().unwrap().to_owned();
            resources.remove(index);
            std::fs::write(
                manifest_path,
                serde_json_canonicalizer::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            std::fs::remove_file(fixture.directory.0.join(path)).unwrap();
            std::fs::remove_dir(fixture.directory.0.join("arrangement")).unwrap();
            hyper_storage_engine::verify_project_package(&fixture.directory.0).unwrap();
            fixture
        }

        #[allow(
            clippy::too_many_lines,
            reason = "one fixture keeps the complete verified arrangement package graph visible"
        )]
        fn with_audio(audio: &[u8]) -> Self {
            let directory = TestDirectory::new();
            let resources = [
                (
                    ARRANGEMENT_TEST_TIMELINE_ID,
                    "timeline",
                    "timeline/timeline.json",
                    json!({
                        "format":"hyper-timeline", "format_version":1,
                        "id":ARRANGEMENT_TEST_TIMELINE_ID,
                        "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                        "ticks_per_quarter":1920,
                        "tempo_events":[{"tick":0,"microseconds_per_quarter":1_000_000}],
                        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
                        "extensions":{}
                    }),
                ),
                (
                    ARRANGEMENT_TEST_TRACK_ID,
                    "track",
                    "tracks/audio.json",
                    json!({
                        "format":"hyper-track", "format_version":1,
                        "id":ARRANGEMENT_TEST_TRACK_ID,
                        "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                        "name":"Audio", "kind":"audio", "order":0, "active":true,
                        "clip_ids":[ARRANGEMENT_TEST_CLIP_ID], "extensions":{}
                    }),
                ),
                (
                    ARRANGEMENT_TEST_CLIP_ID,
                    "clip",
                    "clips/clip.json",
                    json!({
                        "format":"hyper-audio-clip", "format_version":1,
                        "id":ARRANGEMENT_TEST_CLIP_ID,
                        "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                        "track_id":ARRANGEMENT_TEST_TRACK_ID,
                        "name":"Disabled Clip", "enabled":false, "duration_ticks":4,
                        "source":{"asset_id":ARRANGEMENT_TEST_ASSET_ID,"start_frame":0,"frame_count":100},
                        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
                        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
                        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
                        "warp_markers":[
                            {"tick":0,"source_frame_offset":0},
                            {"tick":4,"source_frame_offset":100}
                        ],
                        "extensions":{}
                    }),
                ),
                (
                    ARRANGEMENT_TEST_RESOURCE_ID,
                    "arrangement",
                    "arrangement/main.json",
                    json!({
                        "format":"hyper-arrangement", "format_version":1,
                        "id":ARRANGEMENT_TEST_RESOURCE_ID,
                        "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                        "name":"Main", "enabled":true,
                        "placements":[{
                            "id":ARRANGEMENT_TEST_PLACEMENT_ID,
                            "track_id":ARRANGEMENT_TEST_TRACK_ID,
                            "clip_id":ARRANGEMENT_TEST_CLIP_ID,
                            "start_tick":0, "clip_start_tick":0,
                            "duration_ticks":4, "enabled":true
                        }],
                        "scene_launches":[], "extensions":{}
                    }),
                ),
                (
                    ARRANGEMENT_TEST_MIXER_ID,
                    "mixer",
                    "mixer/main.json",
                    json!({
                        "format":"hyper-mixer", "format_version":1,
                        "id":ARRANGEMENT_TEST_MIXER_ID,
                        "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                        "channels":[{
                            "track_id":ARRANGEMENT_TEST_TRACK_ID,
                            "gain_millionths":1_000_000, "pan_millionths":0,
                            "mute":false, "solo":false,
                            "destination":{"kind":"main"}, "sends":[]
                        }],
                        "return_buses":[],
                        "main":{"gain_millionths":1_000_000,"mute":false},
                        "cue":{"source":"main","gain_millionths":1_000_000,"mute":false},
                        "extensions":{}
                    }),
                ),
            ];
            let mut descriptors = Vec::new();
            for (id, role, path, value) in resources {
                let bytes = arrangement_test_canonical(&value);
                arrangement_test_write(&directory.0, path, &bytes);
                descriptors.push(arrangement_test_descriptor(id, role, path, &bytes));
            }
            arrangement_test_write(&directory.0, "assets/samples/sample.wav", audio);
            let manifest = json!({
                "format":"hyper-project", "format_version":1,
                "project_id":ARRANGEMENT_TEST_PROJECT_ID, "revision":1,
                "name":"Arrangement Edit Test", "sample_rate_hz":48_000,
                "required_capabilities":{
                    "format":"hyper-hardware-capabilities", "format_version":1,
                    "audio_inputs":2, "audio_outputs":2,
                    "cv_inputs":0, "cv_outputs":0, "gate_outputs":0,
                    "has_clock_input":false, "has_reset_input":false,
                    "has_clock_output":false, "usb_mode":"none",
                    "has_ethernet":true, "extensions":{}
                },
                "resources":descriptors,
                "assets":[{
                    "id":ARRANGEMENT_TEST_ASSET_ID, "role":"sample",
                    "path":"assets/samples/sample.wav", "media_type":"audio/wav",
                    "byte_length":audio.len(), "sha256":arrangement_test_sha256(audio),
                    "extensions":{}
                }],
                "extensions":{}
            });
            std::fs::write(
                directory.0.join("project.json"),
                arrangement_test_canonical(&manifest),
            )
            .unwrap();
            Self { directory }
        }

        fn root(&self) -> &Path {
            &self.directory.0
        }
    }

    fn arrangement_test_descriptor(id: &str, role: &str, path: &str, bytes: &[u8]) -> Value {
        json!({
            "id":id, "role":role, "path":path,
            "media_type":"application/json", "byte_length":bytes.len(),
            "sha256":arrangement_test_sha256(bytes), "extensions":{}
        })
    }

    fn arrangement_test_write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn arrangement_test_canonical(value: &Value) -> Vec<u8> {
        serde_json_canonicalizer::to_vec(value).unwrap()
    }

    fn assert_audio_clip_rename_success_event(
        outcome: &ProjectResourcePublicationOutcome,
        expected_changed: bool,
    ) {
        assert!(matches!(
            worker::audio_clip_rename_success_event("Lead vocal".to_owned(), outcome),
            IdeWorkerEvent::AudioClipRenamed {
                revision: 2,
                changed,
                name,
            } if changed == expected_changed && name == "Lead vocal"
        ));
    }

    fn assert_audio_clip_rename_reopen_authority(outcome: ProjectResourcePublicationOutcome) {
        assert!(matches!(
            worker::audio_clip_rename_edit_reopen_failure(
                Ok(outcome),
                IdeProjectOpenError::SelectedOutputUnavailable,
            ),
            IdeWorkerCommandFailure::AudioClipRename(reason)
                if matches!(reason.as_ref(),
                    IdeAudioClipRenameEditError::ReopenAfterPublication { outcome, source }
                        if outcome.changed()
                            && outcome.project_revision() == 2
                            && matches!(source.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable))
        ));
    }

    fn arrangement_test_sha256(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            use core::fmt::Write;
            write!(&mut output, "{byte:02x}").unwrap();
        }
        output
    }

    fn arrangement_test_wav(samples: &[i16]) -> Vec<u8> {
        let data_bytes = u32::try_from(samples.len() * 2).unwrap();
        let mut bytes = Vec::with_capacity(44 + samples.len() * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&96_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn arrangement_test_float_wav(samples: &[f32]) -> Vec<u8> {
        let data_bytes = u32::try_from(samples.len() * 4).unwrap();
        let mut bytes = Vec::with_capacity(44 + samples.len() * 4);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&192_000_u32.to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

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
        assert_eq!(DESKTOP_RECORDING_MAXIMUM_FRAMES, DESKTOP_MAX_AUDIO_FRAMES);
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
    fn audio_track_mapping_references_include_owned_mixer_sends() {
        let transform = ControlTransform::Linear {
            input_min: -1_000_000,
            input_max: 1_000_000,
            output_min: 0,
            output_max: 1_000_000,
        };
        let track_id = "018f47a2-6b2d-7c11-8a44-001122334456";
        let send_id = "018f47a2-6b2d-7c11-8a44-001122334457";
        let send = ControlBinding::try_new(
            "018f47a2-6b2d-7c11-8a44-001122334458".to_owned(),
            true,
            ControlSource::CvInput { input_index: 0 },
            ControlScope::MixerPage,
            transform,
            ControlTarget::MixerSend {
                send_id: send_id.to_owned(),
                parameter: SendParameter::Gain,
            },
        )
        .unwrap();
        let channel = ControlBinding::try_new(
            "018f47a2-6b2d-7c11-8a44-001122334459".to_owned(),
            true,
            ControlSource::CvInput { input_index: 1 },
            ControlScope::Global,
            transform,
            ControlTarget::MixerChannel {
                track_id: track_id.to_owned(),
                parameter: ChannelParameter::Gain,
            },
        )
        .unwrap();

        assert!(control_binding_references_audio_track(
            &send,
            track_id,
            |candidate| candidate == send_id
        ));
        assert!(!control_binding_references_audio_track(
            &send,
            track_id,
            |_| false
        ));
        assert!(control_binding_references_audio_track(
            &channel,
            track_id,
            |_| false
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the end-to-end test keeps durable edit publication, retry, and no-op evidence visible"
    )]
    fn audio_track_edits_preserve_live_mixer_state_and_publish_only_real_changes() {
        let parent = TestDirectory::new();
        let created = hyper_storage_engine::create_project(
            &parent.0,
            hyper_storage_engine::ProjectCreationRequest {
                directory_name: "Project",
                project_name: "Track Edit Test",
            },
        )
        .unwrap();
        let project_path = created.project().root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let track = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Track)
            .unwrap();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let source_track = package.read_resource(track.id()).unwrap();
        let source_mixer = package.read_resource(mixer.id()).unwrap();
        let changed_mixer = serialize_mixer_base_state(
            &source_mixer,
            package.manifest().project_id(),
            mixer.id(),
            &MixerBaseState {
                channels: vec![MixerChannelBaseState {
                    track_id: track.id().to_owned(),
                    gain: LinearGain::UNITY,
                    pan: Pan::CENTER,
                    muted: false,
                    solo: false,
                    sends: Vec::new(),
                }],
                main_gain: LinearGain::from_millionths(750_000).unwrap(),
                main_muted: false,
            },
            limits.session.documents,
        )
        .unwrap();
        let (loaded, _, _, _) = prepared.session().parts();
        let renamed_track = serialize_audio_track_rename(
            loaded,
            &source_track,
            track.id(),
            "Lead Vocal",
            limits.session.documents,
        )
        .unwrap();
        let track_id = track.id().to_owned();
        let mixer_id = mixer.id().to_owned();
        let initial_identity = *package.manifest_sha256();
        drop(prepared);

        let failed = IdeAudioTrackEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: initial_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            track_id: track_id.clone(),
            track_bytes: b"not-json".to_vec(),
        };
        let error = failed.publish().unwrap_err();
        let IdeAudioTrackEditPublicationError::Track { mixer, source } = error else {
            panic!("rename must fail after the mixer publication");
        };
        assert!(mixer.changed());
        assert_eq!(mixer.project_revision(), 2);
        assert_eq!(
            source.kind(),
            hyper_storage_engine::ProjectResourcePublicationErrorKind::InvalidResource
        );

        let verified = hyper_storage_engine::verify_project_package(&project_path).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(verified.read_resource(&mixer_id).unwrap(), changed_mixer);
        assert_eq!(verified.read_resource(&track_id).unwrap(), source_track);
        let retry_identity = *verified.manifest_sha256();
        drop(verified);

        let retry = IdeAudioTrackEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: retry_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            track_id: track_id.clone(),
            track_bytes: renamed_track.clone(),
        };

        let outcome = retry.publish().unwrap();

        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 3);
        let verified = hyper_storage_engine::verify_project_package(&project_path).unwrap();
        assert_eq!(verified.read_resource(&mixer_id).unwrap(), changed_mixer);
        let renamed_identity = *verified.manifest_sha256();
        drop(verified);
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened.session().parts().0.track_name(&track_id),
            Some("Lead Vocal")
        );
        let renamed_source_track = reopened.package().read_resource(&track_id).unwrap();
        let disabled_track = serialize_audio_track_activation(
            reopened.session().parts().0,
            &renamed_source_track,
            &track_id,
            false,
            limits.session.documents,
        )
        .unwrap();
        drop(reopened);

        let unchanged = IdeAudioTrackEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: renamed_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            track_id: track_id.clone(),
            track_bytes: renamed_track,
        }
        .publish()
        .unwrap();

        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 3);

        let disabled = IdeAudioTrackEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: renamed_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            track_id: track_id.clone(),
            track_bytes: disabled_track.clone(),
        }
        .publish()
        .unwrap();
        assert!(disabled.changed());
        assert_eq!(disabled.project_revision(), 4);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened.session().parts().0.track_active(&track_id),
            Some(false)
        );
        let disabled_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let unchanged = IdeAudioTrackEditPlan {
            mixer: IdeMixerSavePlan {
                project_path,
                expected_project_sha256: disabled_identity,
                resource_id: mixer_id,
                resource_bytes: changed_mixer,
            },
            track_id,
            track_bytes: disabled_track,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 4);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the integration test keeps partial publication, recovery, reopen, and no-op evidence together"
    )]
    fn arrangement_placement_edits_publish_reopen_and_preserve_partial_success() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let placement = &prepared.session().arrangement().placements()[0];
        assert!(placement.enabled());
        assert!(!placement.active());
        let package = prepared.package();
        let arrangement = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .unwrap();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let source_arrangement = package.read_resource(arrangement.id()).unwrap();
        let source_mixer = package.read_resource(mixer.id()).unwrap();
        let changed_mixer = serialize_mixer_base_state(
            &source_mixer,
            package.manifest().project_id(),
            mixer.id(),
            &MixerBaseState {
                channels: vec![MixerChannelBaseState {
                    track_id: ARRANGEMENT_TEST_TRACK_ID.to_owned(),
                    gain: LinearGain::UNITY,
                    pan: Pan::CENTER,
                    muted: false,
                    solo: false,
                    sends: Vec::new(),
                }],
                main_gain: LinearGain::from_millionths(750_000).unwrap(),
                main_muted: false,
            },
            limits.session.documents,
        )
        .unwrap();
        let (loaded, _, _, _) = prepared.session().parts();
        let disabled_arrangement = serialize_arrangement_placement_activation(
            loaded,
            &source_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            false,
            limits.session.documents,
        )
        .unwrap();
        let arrangement_id = arrangement.id().to_owned();
        let mixer_id = mixer.id().to_owned();
        let initial_identity = *package.manifest_sha256();
        drop(prepared);

        let failed = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: initial_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: b"not-json".to_vec(),
            addition: false,
        };
        let error = failed.publish().unwrap_err();
        let IdeArrangementPlacementEditPublicationError::Arrangement { mixer, source } = error
        else {
            panic!("arrangement edit must fail after the mixer publication");
        };
        assert!(mixer.changed());
        assert_eq!(mixer.project_revision(), 2);
        assert_eq!(
            source.kind(),
            hyper_storage_engine::ProjectResourcePublicationErrorKind::InvalidResource
        );

        let verified = hyper_storage_engine::verify_project_package(&project_path).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(verified.read_resource(&mixer_id).unwrap(), changed_mixer);
        assert_eq!(
            verified.read_resource(&arrangement_id).unwrap(),
            source_arrangement
        );
        let retry_identity = *verified.manifest_sha256();
        drop(verified);

        let retry = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: retry_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: disabled_arrangement.clone(),
            addition: false,
        };
        let outcome = retry.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 3);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let placement = &reopened.session().arrangement().placements()[0];
        assert!(!placement.enabled());
        assert!(!placement.active());
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            disabled_arrangement
        );
        let (loaded, _, _, _) = reopened.session().parts();
        let moved_arrangement = serialize_arrangement_placement_move(
            loaded,
            &disabled_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            8,
            limits.session.documents,
        )
        .unwrap();
        let disabled_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let moved = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: disabled_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: moved_arrangement.clone(),
            addition: false,
        }
        .publish()
        .unwrap();
        assert!(moved.changed());
        assert_eq!(moved.project_revision(), 4);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened.session().arrangement().placements()[0].start_tick(),
            8
        );
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            moved_arrangement
        );
        let arrangement: serde_json::Value = serde_json::from_slice(&moved_arrangement).unwrap();
        assert_eq!(arrangement["placements"][0]["start_tick"], 8);
        let (loaded, _, _, _) = reopened.session().parts();
        let resized_arrangement = serialize_arrangement_placement_duration(
            loaded,
            &moved_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            2,
            limits.session.documents,
        )
        .unwrap();
        let moved_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let resized = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: moved_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: resized_arrangement.clone(),
            addition: false,
        }
        .publish()
        .unwrap();
        assert!(resized.changed());
        assert_eq!(resized.project_revision(), 5);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let placement = &reopened.session().arrangement().placements()[0];
        assert_eq!(placement.start_tick(), 8);
        assert_eq!(placement.duration_ticks(), 2);
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            resized_arrangement
        );
        let arrangement: serde_json::Value = serde_json::from_slice(&resized_arrangement).unwrap();
        assert_eq!(arrangement["placements"][0]["start_tick"], 8);
        assert_eq!(arrangement["placements"][0]["duration_ticks"], 2);
        let (loaded, _, _, _) = reopened.session().parts();
        let front_trimmed_arrangement = serialize_arrangement_placement_front_trim(
            loaded,
            &resized_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            9,
            limits.session.documents,
        )
        .unwrap();
        let resized_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let front_trimmed = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: resized_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: front_trimmed_arrangement.clone(),
            addition: false,
        }
        .publish()
        .unwrap();
        assert!(front_trimmed.changed());
        assert_eq!(front_trimmed.project_revision(), 6);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let placement = &reopened.session().arrangement().placements()[0];
        assert_eq!(placement.start_tick(), 9);
        assert_eq!(placement.duration_ticks(), 1);
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            front_trimmed_arrangement
        );
        let arrangement: serde_json::Value =
            serde_json::from_slice(&front_trimmed_arrangement).unwrap();
        assert_eq!(arrangement["placements"][0]["start_tick"], 9);
        assert_eq!(arrangement["placements"][0]["clip_start_tick"], 1);
        assert_eq!(arrangement["placements"][0]["duration_ticks"], 1);
        let (loaded, _, _, _) = reopened.session().parts();
        let removed_arrangement = serialize_arrangement_placement_removal(
            loaded,
            &front_trimmed_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            limits.session.documents,
        )
        .unwrap();
        let front_trimmed_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let unchanged = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: front_trimmed_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: front_trimmed_arrangement.clone(),
            addition: false,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 6);

        let removed = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: front_trimmed_identity,
                resource_id: mixer_id,
                resource_bytes: changed_mixer,
            },
            arrangement_id: arrangement_id.clone(),
            arrangement_bytes: removed_arrangement.clone(),
            addition: false,
        }
        .publish()
        .unwrap();
        assert!(removed.changed());
        assert_eq!(removed.project_revision(), 7);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(reopened.session().arrangement().placements().is_empty());
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            removed_arrangement
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the end-to-end test keeps recording import, clip publication, reopen, and non-placement evidence together"
    )]
    fn recorded_take_clip_plan_publishes_and_reopens_without_arrangement_placement() {
        const METADATA_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338520";
        const ASSET_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338521";
        let parent = TestDirectory::new();
        let created = hyper_storage_engine::create_project(
            &parent.0,
            hyper_storage_engine::ProjectCreationRequest {
                directory_name: "Project",
                project_name: "Recorded Take Clip Test",
            },
        )
        .unwrap();
        let project_path = created.project().root().to_path_buf();
        let project_id = created.project().manifest().project_id().to_owned();
        let track_id = created
            .project()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Track)
            .unwrap()
            .id()
            .to_owned();
        drop(created);
        let recording = arrangement_test_wav(&[0; 63]);
        let recording_path = parent.0.join("capture.wav");
        std::fs::write(&recording_path, &recording).unwrap();
        let metadata = arrangement_test_canonical(&json!({
            "format":"hyper-recording-metadata", "format_version":1,
            "id":METADATA_ID, "project_id":project_id, "asset_id":ASSET_ID,
            "track_id":track_id, "capture_kind":"new",
            "captured_at_utc":"2026-08-09T12:00:00.000Z",
            "timeline_start_tick":0, "source_input_indices":[0],
            "sample_rate_hz":48_000, "channels":1, "frame_count":63,
            "extensions":{}
        }));
        hyper_storage_engine::import_recording(
            &project_path,
            hyper_storage_engine::RecordingImportRequest {
                recording_path: &recording_path,
                recording_byte_length: recording.len() as u64,
                recording_sha256: Sha256::digest(&recording).into(),
                metadata_bytes: &metadata,
            },
        )
        .unwrap();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let source_mixer = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let application = DesktopIdeApplication::from_inventories(
            desktop_ide_application_config().unwrap(),
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let wrong_identity =
            RunningDesktopIdeApplication::prepare_recording_clip_creation_from_project(
                &application,
                IdeMixerSavePlan {
                    project_path: project_path.clone(),
                    expected_project_sha256,
                    resource_id: mixer_id.clone(),
                    resource_bytes: source_mixer.clone(),
                },
                &prepared,
                ASSET_ID,
                "Vocal take",
            )
            .unwrap_err();
        assert!(matches!(
            wrong_identity,
            IdeRecordingClipCreationError::RecordingMetadataMissing
        ));
        let plan = RunningDesktopIdeApplication::prepare_recording_clip_creation_from_project(
            &application,
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: source_mixer.clone(),
            },
            &prepared,
            METADATA_ID,
            "Vocal take",
        )
        .unwrap();
        let clip_id = plan.clip_id.clone();
        let serialized_clip: Value = serde_json::from_slice(&plan.clip_bytes).unwrap();
        let duration_ticks = serialized_clip["duration_ticks"].as_u64().unwrap();
        drop(prepared);

        let outcome = plan.publish().unwrap();

        assert_eq!(outcome.project_revision(), 3);
        assert_eq!(outcome.clip_id(), clip_id);
        assert_eq!(duration_ticks, 6);
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, _) = reopened.session().parts();
        assert_eq!(project.clip_count(), 1);
        assert_eq!(project.placement_count(), 0);
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            source_mixer
        );
        let track: Value =
            serde_json::from_slice(&reopened.package().read_resource(&track_id).unwrap()).unwrap();
        assert_eq!(track["clip_ids"], json!([clip_id]));
        let clip: Value =
            serde_json::from_slice(&reopened.package().read_resource(&clip_id).unwrap()).unwrap();
        assert_eq!(clip["source"]["asset_id"], ASSET_ID);
        assert_eq!(clip["duration_ticks"], duration_ticks);
    }

    #[test]
    fn audio_clip_duplication_publishes_independent_identity_without_duplicating_media_or_placement()
     {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_clip_bytes = package.read_resource(ARRANGEMENT_TEST_CLIP_ID).unwrap();
        let arrangement_bytes = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let source_audio = std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let application = DesktopIdeApplication::from_inventories(
            desktop_ide_application_config().unwrap(),
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let plan = RunningDesktopIdeApplication::prepare_audio_clip_duplication_from_project(
            &application,
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: mixer_bytes.clone(),
            },
            &prepared,
            ARRANGEMENT_TEST_CLIP_ID,
            "Disabled Clip copy",
        )
        .unwrap();
        let duplicated_clip_id = plan.clip_id.clone();
        assert_ne!(duplicated_clip_id, ARRANGEMENT_TEST_CLIP_ID);
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.clip_id(), duplicated_clip_id);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = reopened.package();
        let (project, _, _, _) = reopened.session().parts();
        assert_eq!(project.clip_count(), 2);
        assert_eq!(project.placement_count(), 1);
        assert_eq!(project.clip_id_at(0), Some(ARRANGEMENT_TEST_CLIP_ID));
        assert_eq!(project.clip_id_at(1), Some(duplicated_clip_id.as_str()));
        assert_eq!(project.clip_track_id_at(1), Some(ARRANGEMENT_TEST_TRACK_ID));
        assert_eq!(project.clip_name_at(1), Some("Disabled Clip copy"));
        assert_eq!(
            reopened.session().arrangement().placements()[0].clip_index(),
            0
        );
        assert_eq!(
            package.read_resource(ARRANGEMENT_TEST_CLIP_ID).unwrap(),
            source_clip_bytes
        );
        assert_eq!(
            package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap(),
            arrangement_bytes
        );
        assert_eq!(package.read_resource(&mixer_id).unwrap(), mixer_bytes);
        assert_eq!(
            std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap(),
            source_audio
        );

        let source_clip: Value = serde_json::from_slice(&source_clip_bytes).unwrap();
        let mut expected_duplicate = source_clip;
        expected_duplicate["id"] = json!(duplicated_clip_id);
        expected_duplicate["name"] = json!("Disabled Clip copy");
        let duplicate: Value =
            serde_json::from_slice(&package.read_resource(outcome.clip_id()).unwrap()).unwrap();
        assert_eq!(duplicate, expected_duplicate);
        let track: Value =
            serde_json::from_slice(&package.read_resource(ARRANGEMENT_TEST_TRACK_ID).unwrap())
                .unwrap();
        assert_eq!(
            track["clip_ids"],
            json!([ARRANGEMENT_TEST_CLIP_ID, outcome.clip_id()])
        );
    }

    #[test]
    fn first_arrangement_placement_publishes_and_reopens() {
        let fixture = ArrangementProjectFixture::without_arrangement();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(prepared.session().arrangement().placements().is_empty());
        let package = prepared.package();
        assert!(
            !package
                .manifest()
                .resources()
                .iter()
                .any(|descriptor| descriptor.role() == ResourceRole::Arrangement)
        );
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let arrangement_id = "018f47a2-6b2d-7c11-8a44-001122338508";
        let placement_id = "018f47a2-6b2d-7c11-8a44-001122338509";
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let arrangement_bytes = serialize_arrangement_placement_addition(
            project,
            &assets,
            None,
            arrangement_id,
            placement_id,
            ARRANGEMENT_TEST_CLIP_ID,
            7,
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id: arrangement_id.to_owned(),
            arrangement_bytes: arrangement_bytes.clone(),
            addition: true,
        }
        .publish()
        .unwrap();

        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(
            outcome.resource_path(),
            format!("arrangement/{arrangement_id}.json")
        );
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let arrangement = reopened.session().arrangement();
        let placements = arrangement.placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].id(), placement_id);
        assert_eq!(
            arrangement.clip_name(placements[0].clip_index()),
            Some("Disabled Clip")
        );
        assert_eq!(
            arrangement.track_name(placements[0].track_index()),
            Some("Audio")
        );
        assert_eq!(placements[0].start_tick(), 7);
        assert_eq!(placements[0].clip_start_tick(), 0);
        assert_eq!(placements[0].duration_ticks(), 4);
        assert_eq!(
            reopened.package().read_resource(arrangement_id).unwrap(),
            arrangement_bytes
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the integration test keeps partial publication, recovery, reopen, and no-op evidence together"
    )]
    fn audio_clip_gain_edits_publish_reopen_and_preserve_partial_success() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let clip = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Clip)
            .unwrap();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let source_clip = package.read_resource(clip.id()).unwrap();
        let source_mixer = package.read_resource(mixer.id()).unwrap();
        let changed_mixer = serialize_mixer_base_state(
            &source_mixer,
            package.manifest().project_id(),
            mixer.id(),
            &MixerBaseState {
                channels: vec![MixerChannelBaseState {
                    track_id: ARRANGEMENT_TEST_TRACK_ID.to_owned(),
                    gain: LinearGain::UNITY,
                    pan: Pan::CENTER,
                    muted: false,
                    solo: false,
                    sends: Vec::new(),
                }],
                main_gain: LinearGain::from_millionths(750_000).unwrap(),
                main_muted: false,
            },
            limits.session.documents,
        )
        .unwrap();
        let changed_clip = serialize_audio_clip_gain(
            prepared.session().parts().0,
            &source_clip,
            clip.id(),
            -12.5,
            limits.session.documents,
        )
        .unwrap();
        let clip_id = clip.id().to_owned();
        let mixer_id = mixer.id().to_owned();
        let initial_identity = *package.manifest_sha256();
        drop(prepared);

        let failed = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: initial_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            clip_id: clip_id.clone(),
            clip_bytes: b"not-json".to_vec(),
        };
        let error = failed.publish().unwrap_err();
        let IdeAudioClipEditPublicationError::Clip { mixer, source } = error else {
            panic!("audio-clip edit must fail after the mixer publication");
        };
        assert!(mixer.changed());
        assert_eq!(mixer.project_revision(), 2);
        assert_eq!(
            source.kind(),
            hyper_storage_engine::ProjectResourcePublicationErrorKind::InvalidResource
        );

        let verified = hyper_storage_engine::verify_project_package(&project_path).unwrap();
        assert_eq!(verified.manifest().revision(), 2);
        assert_eq!(verified.read_resource(&mixer_id).unwrap(), changed_mixer);
        assert_eq!(verified.read_resource(&clip_id).unwrap(), source_clip);
        let retry_identity = *verified.manifest_sha256();
        drop(verified);

        let outcome = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: retry_identity,
                resource_id: mixer_id.clone(),
                resource_bytes: changed_mixer.clone(),
            },
            clip_id: clip_id.clone(),
            clip_bytes: changed_clip.clone(),
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 3);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened.session().parts().0.clip_gain_decibels_at(0),
            Some(-12.5)
        );
        assert_eq!(
            reopened.package().read_resource(&clip_id).unwrap(),
            changed_clip
        );
        let changed_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let unchanged = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path,
                expected_project_sha256: changed_identity,
                resource_id: mixer_id,
                resource_bytes: changed_mixer,
            },
            clip_id,
            clip_bytes: changed_clip,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 3);
    }

    #[test]
    fn audio_clip_rename_publishes_and_reopens_without_changing_audio_or_placement_identity() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(mixer.id()).unwrap();
        let arrangement_bytes = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let source_audio = std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap();
        let identity = *package.manifest_sha256();
        let (clip_id, clip_bytes) = prepare_audio_clip_rename_resource(
            &prepared,
            ARRANGEMENT_TEST_CLIP_ID,
            "Lead vocal",
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: identity,
                resource_id: mixer_id.clone(),
                resource_bytes: mixer_bytes.clone(),
            },
            clip_id: clip_id.clone(),
            clip_bytes: clip_bytes.clone(),
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_audio_clip_rename_success_event(&outcome, true);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, _) = reopened.session().parts();
        assert_eq!(project.clip_name_at(0), Some("Lead vocal"));
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_RESOURCE_ID)
                .unwrap(),
            arrangement_bytes
        );
        assert_eq!(
            reopened.session().arrangement().placements()[0].clip_index(),
            0
        );
        assert_eq!(
            std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap(),
            source_audio
        );
        let reopened_identity = *reopened.package().manifest_sha256();
        drop(reopened);

        let unchanged = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path,
                expected_project_sha256: reopened_identity,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            clip_id,
            clip_bytes,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 2);
        assert_audio_clip_rename_success_event(&unchanged, false);
        assert_audio_clip_rename_reopen_authority(outcome);
    }

    #[test]
    fn audio_clip_activation_publishes_and_reopens_the_compiled_project() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(mixer.id()).unwrap();
        let identity = *package.manifest_sha256();
        let (clip_id, clip_bytes) = prepare_audio_clip_activation_resource(
            &prepared,
            ARRANGEMENT_TEST_CLIP_ID,
            true,
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: identity,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            clip_id,
            clip_bytes,
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, _) = reopened.session().parts();
        assert_eq!(project.clip_active_at(0), Some(true));
        assert!(reopened.session().arrangement().placements()[0].enabled());
    }

    #[test]
    fn audio_clip_gain_rejects_native_sample_overflow_before_publication() {
        let fixture =
            ArrangementProjectFixture::with_audio(&arrangement_test_float_wav(&[f32::MAX; 100]));
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let original_identity = *prepared.package().manifest_sha256();
        let original_clip = prepared
            .package()
            .read_resource(ARRANGEMENT_TEST_CLIP_ID)
            .unwrap();

        assert!(matches!(
            prepare_audio_clip_gain_resource(
                &prepared,
                ARRANGEMENT_TEST_CLIP_ID,
                20.0 * 2.0_f64.log10(),
                limits.session.documents,
            ),
            Err(IdeAudioClipGainEditError::NativeValidation(
                AudioClipGainValidationError::Native(
                    hyper_clip_engine::ClipPlanError::SampleGainOverflow
                )
            ))
        ));
        drop(prepared);

        let verified = hyper_storage_engine::verify_project_package(&project_path).unwrap();
        assert_eq!(verified.manifest().revision(), 1);
        assert_eq!(verified.manifest_sha256(), &original_identity);
        assert_eq!(
            verified.read_resource(ARRANGEMENT_TEST_CLIP_ID).unwrap(),
            original_clip
        );
        drop(verified);
        assert!(hyper_project_loader::prepare_project_playback(&project_path, limits).is_ok());
    }

    #[test]
    fn output_diagnostic_uses_the_selected_route_and_fixed_ebu_alignment_signal() {
        let mut application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::try_new(&[("route-a", "Output A")]).unwrap(),
        );
        application.select_output("route-a").unwrap();
        let output = application.output_test_config().unwrap();
        let tone = DesktopTestToneConfig::try_new(
            output,
            IDE_OUTPUT_TEST_FREQUENCY_HZ,
            IDE_OUTPUT_TEST_LEVEL_DBFS,
        )
        .unwrap();

        assert_eq!(tone.output().routing_id(), Some("route-a"));
        assert_eq!(tone.output().sample_rate_hz(), 48_000);
        assert_eq!(tone.tone().frequency_hz().to_bits(), 1_000.0_f64.to_bits());
        assert_eq!(tone.tone().level_dbfs().to_bits(), (-18.0_f32).to_bits());

        application.output_selection = IdeOutputSelection::Routed("stale-route".to_owned());
        assert!(matches!(
            application.output_test_config(),
            Err(IdeOutputTestError::SelectedOutputUnavailable)
        ));
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
    fn bounded_seek_rejects_unbounded_and_out_of_range_positions_without_queueing() {
        let application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::default(),
        );
        let mut running = RunningDesktopIdeApplication {
            application,
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                extent: IdePlaybackExtent::Bounded { end_frame: 1_000 },
                ..TestPlaybackSession::default()
            }),
        };
        assert!(matches!(
            running.try_seek(1_001),
            Err(IdeTransportSeekError::OutOfRange {
                requested: 1_001,
                end_frame: 1_000
            })
        ));
        running.try_seek(750).unwrap();
        let IdePlaybackSessionOwner::Test(session) = &mut running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(
            session.commands,
            [TransportCommand::Locate {
                frame: 750,
                state: LocateState::Preserve,
            }]
        );
        session.extent = IdePlaybackExtent::Unbounded;
        assert!(matches!(
            running.try_seek(0),
            Err(IdeTransportSeekError::Unbounded)
        ));
        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(session.commands.len(), 1);
    }

    #[test]
    fn arrangement_locate_validates_identity_and_works_with_unbounded_scene_authority() {
        let application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::default(),
        );
        let mut running = RunningDesktopIdeApplication {
            application,
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession {
                extent: IdePlaybackExtent::Unbounded,
                arrangement_project_id: "project-a".to_owned(),
                arrangement_placements: vec![("placement-a".to_owned(), 480)],
                ..TestPlaybackSession::default()
            }),
        };
        assert!(matches!(
            running.try_locate_arrangement_placement("project-b", 0, "placement-a"),
            Err(IdeArrangementLocateError::StaleProject)
        ));
        assert!(matches!(
            running.try_locate_arrangement_placement("project-a", 0, "stale-placement"),
            Err(IdeArrangementLocateError::StalePlacement { requested_index: 0 })
        ));
        assert_eq!(
            running
                .try_locate_arrangement_placement("project-a", 0, "placement-a")
                .unwrap(),
            480
        );
        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(
            session.commands,
            [TransportCommand::Locate {
                frame: 480,
                state: LocateState::Preserve,
            }]
        );
    }

    #[test]
    fn loudness_controls_preserve_typed_run_standby_and_reset_order() {
        assert_eq!(
            IdeLoudnessCommand::Run.runtime(),
            LoudnessMeasurementCommand::SetState(IdeLoudnessMeasurementState::Running)
        );
        assert_eq!(
            IdeLoudnessCommand::Standby.runtime(),
            LoudnessMeasurementCommand::SetState(IdeLoudnessMeasurementState::Standby)
        );
        assert_eq!(
            IdeLoudnessCommand::Reset.runtime(),
            LoudnessMeasurementCommand::Reset
        );

        let application = DesktopIdeApplication::from_inventory(
            config(256).unwrap(),
            IdeAudioOutputInventory::default(),
        );
        let mut running = RunningDesktopIdeApplication {
            application,
            session: IdePlaybackSessionOwner::Test(TestPlaybackSession::default()),
        };
        for command in [
            IdeLoudnessCommand::Standby,
            IdeLoudnessCommand::Reset,
            IdeLoudnessCommand::Run,
        ] {
            running.try_send_loudness(command).unwrap();
        }
        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(
            session.loudness_commands,
            [
                IdeLoudnessCommand::Standby,
                IdeLoudnessCommand::Reset,
                IdeLoudnessCommand::Run,
            ]
        );
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
