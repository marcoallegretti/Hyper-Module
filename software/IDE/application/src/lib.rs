#![doc = "Single-owner desktop IDE application state above native audio playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;
use std::path::Path;
use std::time::Duration;

use hyper_audio_engine::{LinearGain, Pan, ParameterError};
use hyper_audio_source::{DecodeError, DecodeLimits};
pub use hyper_desktop_audio::DesktopSceneAvailability as IdeSceneAvailability;
use hyper_desktop_audio::{
    DesktopDirectControlError, DesktopInputStopError, DesktopOutputConfig,
    DesktopOutputConfigError, DesktopOutputDiscoveryError, DesktopOutputStopError,
    DesktopPlaybackRecoveryPendingStop, DesktopPlaybackRecoveryPendingStopWait,
    DesktopPlaybackSession, DesktopProjectOpenError, OutputStreamSnapshot, OutputStreamState,
    discover_desktop_output_targets,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{
    LatestOutputMeterSnapshot, MixerParameterCommand, MixerParameterResolveError,
    PlaybackRecoverySnapshot,
};
#[cfg(test)]
use hyper_mixer_runtime::{OutputMeterReceiver, output_meter_channel};
pub use hyper_mixer_runtime::{PlaybackRecoveryTransportState, TransportCommand};
use hyper_playback_json::{CompiledMixerTrackKind, LoaderLimits};
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;

mod worker;

pub use worker::{
    DesktopIdeWorker, DesktopIdeWorkerPendingShutdown, IdeClosedSnapshot, IdeMixerChannel,
    IdeRunningObservation, IdeScene, IdeWorkerCommand, IdeWorkerCommandFailure,
    IdeWorkerCommandKind, IdeWorkerEvent, IdeWorkerEventReceiveError, IdeWorkerJoinError,
    IdeWorkerPendingShutdownWait, IdeWorkerSendError, IdeWorkerShutdownOutcome,
    IdeWorkerShutdownReport, IdeWorkerStartError, MAX_IDE_WORKER_COMMANDS, MAX_IDE_WORKER_EVENTS,
    desktop_ide_worker_supervisor_snapshot,
};

const DESKTOP_MAXIMUM_BLOCK_FRAMES: usize = 4_096;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeApplicationConfigError {
    Output(DesktopOutputConfigError),
}

impl fmt::Display for IdeApplicationConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Output(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeApplicationConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Output(source) => Some(source),
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
        DesktopOutputConfig::try_new(48_000, maximum_block_frames)
            .map_err(IdeApplicationConfigError::Output)?;
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
    selection: IdeOutputSelection,
}

impl DesktopIdeApplication {
    /// Discovers outputs and publishes one closed application owner.
    ///
    /// # Errors
    ///
    /// Returns native discovery or fallible inventory construction errors.
    pub fn discover(config: IdeApplicationConfig) -> Result<Self, IdeOutputDiscoveryError> {
        Ok(Self {
            config,
            outputs: IdeAudioOutputInventory::discover()?,
            selection: IdeOutputSelection::SystemDefault,
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
            selection: IdeOutputSelection::SystemDefault,
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

    #[must_use]
    pub fn outputs(&self) -> &IdeAudioOutputInventory {
        &self.outputs
    }

    #[must_use]
    pub fn selection(&self) -> &IdeOutputSelection {
        &self.selection
    }

    pub fn select_system_default(&mut self) {
        self.selection = IdeOutputSelection::SystemDefault;
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
        self.selection = IdeOutputSelection::Routed(
            try_owned(routing_id).map_err(|_| IdeOutputSelectionError::CapacityUnavailable)?,
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
        match DesktopPlaybackSession::open_project(output, root, self.config.project_limits) {
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
        match &self.selection {
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

#[derive(Debug)]
pub enum IdeProjectOpenError {
    SelectedOutputUnavailable,
    OutputConfig(DesktopOutputConfigError),
    Project(DesktopProjectOpenError),
}

impl fmt::Display for IdeProjectOpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelectedOutputUnavailable => {
                formatter.write_str("selected audio output is no longer available")
            }
            Self::OutputConfig(source) => source.fmt(formatter),
            Self::Project(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeProjectOpenError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SelectedOutputUnavailable => None,
            Self::OutputConfig(source) => Some(source),
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
    pub mixer_main_state: Option<IdeMixerMainState>,
    pub output_state: OutputStreamState,
    pub output: OutputStreamSnapshot,
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
}

#[cfg(test)]
impl Default for TestPlaybackSession {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            mixer_commands: Vec::new(),
            meters: output_meter_channel().1,
            close_failure: None,
        }
    }
}

impl RunningDesktopIdeApplication {
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
        }
        let mixer_main_state = mixer_parameters.map(|state| IdeMixerMainState {
            gain: state.main_gain().value(),
            muted: state.main_muted(),
        });
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
            mixer_main_state,
            output_state: session.output_status().state(),
            output: session.counters().snapshot(),
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
}

#[cfg(test)]
fn validate_test_mixer_command(command: IdeMixerCommand) -> Result<(), IdeMixerCommandError> {
    let channel_index = match command {
        IdeMixerCommand::ChannelGain {
            channel_index,
            gain_millionths,
        } => {
            LinearGain::from_millionths(gain_millionths)
                .map_err(IdeMixerCommandError::Parameter)?;
            Some(channel_index)
        }
        IdeMixerCommand::ChannelPan {
            channel_index,
            pan_millionths,
        } => {
            Pan::from_millionths(pan_millionths).map_err(IdeMixerCommandError::Parameter)?;
            Some(channel_index)
        }
        IdeMixerCommand::ChannelMute { channel_index, .. }
        | IdeMixerCommand::ChannelSolo { channel_index, .. } => Some(channel_index),
        IdeMixerCommand::MainGain { gain_millionths } => {
            LinearGain::from_millionths(gain_millionths)
                .map_err(IdeMixerCommandError::Parameter)?;
            None
        }
        IdeMixerCommand::MainMute { .. } => None,
    };
    if channel_index.is_some_and(|index| index >= MAX_IDE_MIXER_CHANNELS) {
        return Err(IdeMixerCommandError::Resolve(
            MixerParameterResolveError::ChannelNotFound,
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

        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(session.mixer_commands, commands);
    }
}
