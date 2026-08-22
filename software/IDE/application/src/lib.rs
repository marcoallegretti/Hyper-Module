#![doc = "Single-owner desktop IDE application state above native audio playback."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;
use core::mem::size_of;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use hyper_audio_engine::LoudnessMeasurementState as IdeLoudnessMeasurementState;
use hyper_audio_engine::{
    LinearGain, LoudnessMeasurementCommand, Pan, ParameterError, StereoSineTestToneConfigError,
};
use hyper_audio_source::{
    AudioContainer, DecodeError, DecodeLimits, decode_bytes_at_source_rate, encode_float_wav,
    float_wav_encoded_capacity, plan_resampling_memory, resample_audio,
};
pub use hyper_control_mappings::{
    ChannelParameter as IdeControlChannelParameter, ControlBinding, ControlMappingDocument,
    ControlMappingParseError, ControlMappingProject, ControlScope, ControlSource, ControlTarget,
    ControlTransform, CueParameter as IdeControlCueParameter, Edge,
    MainParameter as IdeControlMainParameter, SceneAction,
    SendParameter as IdeControlSendParameter, TransportAction, TriggerBehavior,
};
pub use hyper_desktop_audio::OfflineExportReport;
use hyper_desktop_audio::{
    CaptureStreamSnapshot, DesktopAudioPreviewCommandError, DesktopAudioPreviewControlError,
    DesktopAudioPreviewSnapshot, DesktopDirectControlError, DesktopInputConfig,
    DesktopInputConfigError, DesktopInputDiscoveryError, DesktopInputStopError,
    DesktopMonitoringConfig, DesktopMonitoringConfigError, DesktopMonitoringControlError,
    DesktopOutputConfig, DesktopOutputConfigError, DesktopOutputDiscoveryError,
    DesktopOutputStartError, DesktopOutputStopError, DesktopPlaybackRecovery,
    DesktopPlaybackRecoveryPendingStop, DesktopPlaybackRecoveryPendingStopWait,
    DesktopPlaybackSession, DesktopProjectOpenError, DesktopTestToneConfig, DesktopTestToneSession,
    MonitoringSnapshot, OfflineExportError, OfflineExportSpec, OutputStreamSnapshot,
    OutputStreamState, PendingNativeShutdown, PendingNativeShutdownWait, StreamState,
    discover_desktop_input_targets, discover_desktop_output_targets,
    export_prepared_project_wav_with_mixer,
};
pub use hyper_desktop_audio::{
    DesktopAudioPreviewState, DesktopSceneAvailability as IdeSceneAvailability,
};
use hyper_mapping_compiler::{ControlMappingResolver, MixerSendAddress};
use hyper_fs_authority::{
    StableFileEvidence, confirm_stable_absolute_file, read_stable_absolute_file,
};
use hyper_mapping_editor::{
    MappingEditor, MappingEditorMutationError, MappingEditorPreviewError,
    MappingEditorPublicationError,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::MixerParameterStateSnapshot;
pub use hyper_mixer_runtime::{
    EbuLoudnessPresentationConfig as IdeLoudnessPresentationConfig,
    EbuLoudnessScale as IdeLoudnessScale, LocateState,
    LoudnessDisplayUnit as IdeLoudnessDisplayUnit,
    LoudnessPresentationSnapshot as IdeLoudnessPresentationSnapshot,
    LoudnessScaleRegion as IdeLoudnessScaleRegion,
    MaximumTruePeakDisplay as IdeMaximumTruePeakDisplay, PlaybackRecoveryTransportState,
    ScaledLoudnessValue as IdeScaledLoudnessValue, SceneLaunchState as IdeSceneLaunchState,
    TransportCommand, TransportLoopRange, TransportLoopRangeError,
};
use hyper_mixer_runtime::{
    LatestOutputMeterSnapshot, LoudnessCommandSendError, MixerParameterCommand,
    MixerParameterResolveError, PlaybackRecoverySnapshot,
};
#[cfg(test)]
use hyper_mixer_runtime::{OutputMeterReceiver, output_meter_channel};
use hyper_playback_compiler::scene::LaunchQuantization;
pub use hyper_playback_compiler::{
    MeterEvent, MusicalPosition, TempoEvent, musical_position_at_frame,
};
pub use hyper_playback_json::is_native_audio_clip_gain_decibels as ide_audio_clip_gain_is_supported;
use hyper_playback_json::{
    ArrangementPlacementDocumentSerializationError, AudioClipAdditionSerializationError,
    AudioClipDocumentSerializationError, AudioClipDuplicationSerializationError,
    AudioClipFadeSerializationError, AudioClipGainSerializationError,
    AudioClipLoopSerializationError, AudioClipRemovalSerializationError,
    AudioClipRenameSerializationError,
    AudioClipSourceStartSerializationError, AudioClipSourceViewSerializationError,
    AudioTrackAdditionSerializationError,
    AudioTrackDocumentSerializationError, AudioTrackRemovalBlockers,
    AudioTrackRemovalSerializationError, AudioTrackRenameSerializationError,
    AutomationTakeAddition, AutomationTakeAdditionSerializationError,
    AutomationTakeEditSerializationError, AutomationTargetSelection, CompiledMixerGraph,
    CompiledMixerTap, CompiledMixerTrackKind, LoadError as PlaybackDocumentLoadError, LoaderLimits,
    MediaAssetFact, MixerBaseState, MixerChannelBaseState, MixerSendBaseState,
    MixerStateSerializationError, SceneAdditionSerializationError, SceneDocumentSerializationError,
    SceneDuplicationSerializationError, SceneRenameSerializationError, SceneSlotEdit,
    SceneSlotEditSerializationError, TempoSerializationError, serialize_arrangement_loop_range,
    serialize_arrangement_placement_activation, serialize_arrangement_placement_addition,
    serialize_arrangement_placement_duplication, serialize_arrangement_placement_duration,
    serialize_arrangement_placement_front_trim, serialize_arrangement_placement_move,
    serialize_arrangement_placement_removal, serialize_arrangement_placement_split,
    serialize_arrangement_scene_launch_addition, serialize_arrangement_scene_launch_move,
    serialize_arrangement_scene_launch_removal, serialize_arrangement_scene_launch_scene_edit,
    serialize_audio_clip_activation, serialize_audio_clip_duplication, serialize_audio_clip_fades,
    serialize_audio_clip_gain, serialize_audio_clip_loop, serialize_audio_clip_removal,
    serialize_audio_clip_rename, serialize_audio_clip_reverse, serialize_audio_clip_source_start,
    serialize_audio_clip_source_view,
    serialize_audio_track_activation, serialize_audio_track_addition,
    serialize_audio_track_removal, serialize_audio_track_rename,
    serialize_automation_take_activation, serialize_automation_take_addition,
    serialize_mixer_base_state, serialize_recording_audio_clip_addition,
    serialize_scene_activation, serialize_scene_addition, serialize_scene_duplication,
    serialize_scene_rename, serialize_scene_slot_edit, serialize_timeline_meter,
    serialize_timeline_tempo,
};
pub use hyper_playback_json::{
    AutomationChannelParameter, AutomationInterpolation, AutomationMainParameter, AutomationPoint,
    AutomationSendParameter, AutomationTarget, AutomationValue,
};
pub use hyper_playback_json::{
    bpm_for_microseconds_per_quarter, meter_is_in_version_one_domain,
    microseconds_per_quarter_for_bpm,
};
use hyper_playback_session::{
    AudioClipGainValidationError, AudioClipSourceSlipValidationError, PlaybackSessionAssemblyLimits,
};
pub use hyper_playback_session::{
    PlaybackArrangement as IdePlaybackArrangement, PlaybackAutomation as IdePlaybackAutomation,
    PlaybackAutomationLane as IdePlaybackAutomationLane, PlaybackExtent as IdePlaybackExtent,
};
pub use hyper_project_history::{
    MAX_RECENT_PROJECTS, ProjectHistory, ProjectHistoryError, ProjectHistoryErrorKind,
    RecentProject,
};
use hyper_project_loader::{PreparedProjectPlayback, ProjectPlaybackError, ProjectPlaybackLimits};
use hyper_project_model::ResourceRole;
pub use hyper_native_device::{
    MAX_COMPRESSOR_ATTACK_MICROSECONDS, MAX_COMPRESSOR_MAKEUP_MILLIDECIBELS,
    MAX_COMPRESSOR_RATIO_MILLIONTHS, MAX_COMPRESSOR_RELEASE_MICROSECONDS,
    MAX_COMPRESSOR_THRESHOLD_MILLIDECIBELS,
    MAX_FILTER_CUTOFF_MILLIHERTZ, MAX_FILTER_RESONANCE_MILLIONTHS,
    MIN_COMPRESSOR_ATTACK_MICROSECONDS, MIN_COMPRESSOR_MAKEUP_MILLIDECIBELS,
    MIN_COMPRESSOR_RATIO_MILLIONTHS, MIN_COMPRESSOR_RELEASE_MICROSECONDS,
    MIN_COMPRESSOR_THRESHOLD_MILLIDECIBELS,
    MIN_FILTER_CUTOFF_MILLIHERTZ, MIN_FILTER_RESONANCE_MILLIONTHS,
    MAX_DEVICES_PER_CHAIN, NativeFilterMode,
};
use hyper_native_device::{
    DeviceChain, DeviceChainError, NativeCompressorState, NativeDeviceInstance, NativeFilterState,
};
use hyper_storage_engine::{
    ArrangementPublicationRequest, AudioClipPublicationOutcome, AudioClipPublicationRequest,
    AudioClipRemovalOutcome, AudioClipRemovalRequest,
    AudioTrackPublicationOutcome, AudioTrackPublicationRequest, AudioTrackRemovalOutcome,
    AudioTrackRemovalRequest, AutomationTakePublicationRequest, AutomationTakeRemovalRequest,
    ControlMappingPublicationRequest,
    DeviceChainPublicationRequest, DeviceChainRemovalRequest,
    ImportedAudioClipPublicationRequest, ProjectResourcePublicationError,
    ProjectResourcePublicationOutcome, ProjectResourcePublicationRequest,
    RequiredControlCapabilities, SampleCleanupOutcome, SampleCleanupRequest,
    ScenePublicationRequest, StorageError, clean_unused_samples, publish_arrangement,
    publish_audio_clip, publish_audio_track, publish_automation_take, publish_control_mapping,
    publish_device_chain, publish_imported_audio_clip, publish_project_resource, publish_scene,
    remove_audio_clip, remove_audio_track, remove_automation_take, remove_device_chain,
};

mod recording;
mod worker;

pub use recording::{
    IdeRecordingAbortError, IdeRecordingAbortOutcome, IdeRecordingError, IdeRecordingFinalizeError,
    IdeRecordingImportOutcome, IdeRecordingObservation, IdeRecordingPreparationError,
    IdeRecordingStartError,
};

pub use worker::{
    DesktopIdeWorker, DesktopIdeWorkerPendingShutdown, IdeAudioClip,
    IdeAudioClipEditCommittedWarning, IdeAudioPreviewObservation, IdeClosedSnapshot,
    IdeMixerChannel, IdeMixerSend, IdeNativeCompressor, IdeNativeDevice, IdeNativeFilter, IdeProjectCreationError, IdeRecordingCaptureKind,
    IdeRecordingState, IdeRecordingTake, IdeRunningObservation, IdeRuntimePreparationError,
    IdeScene, IdeSceneTrackAction, IdeSceneTrackSlot, IdeWorkerCommand, IdeWorkerCommandFailure,
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
const DESKTOP_MAX_AUDIO_IMPORT_WORKING_BYTES: usize = 7 * 256 * 1_024 * 1_024;
/// Maximum scene metadata rows published by the version-1 desktop IDE.
pub const MAX_IDE_SCENES: usize = LoaderLimits::VERSION_1.max_scene_documents;
/// Maximum channel rows published by the version-1 desktop IDE mixer.
pub const MAX_IDE_MIXER_CHANNELS: usize = hyper_mixer_runtime::MAX_MAIN_MIXER_SLOTS;

fn audio_import_container(path: &Path) -> Result<AudioContainer, IdeAudioImportError> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or(IdeAudioImportError::UnsupportedContainer)?;
    if extension.eq_ignore_ascii_case("wav") {
        Ok(AudioContainer::Wav)
    } else if extension.eq_ignore_ascii_case("flac") {
        Ok(AudioContainer::Flac)
    } else {
        Err(IdeAudioImportError::UnsupportedContainer)
    }
}

fn audio_import_clip_name(path: &Path) -> Result<String, IdeAudioImportError> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.chars().count() <= 120)
        .ok_or(IdeAudioImportError::InvalidClipName)?;
    let mut owned = String::new();
    owned
        .try_reserve_exact(name.len())
        .map_err(|_| IdeAudioImportError::CapacityUnavailable)?;
    owned.push_str(name);
    Ok(owned)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioImportMemoryPlan {
    decode_peak: usize,
    resampling_peak: usize,
    encoding_peak: usize,
    resampling_working: usize,
    peak: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AudioImportMemoryInput {
    retained_project_bytes: usize,
    encoded_source_bytes: usize,
    input_frames: usize,
    input_sample_rate_hz: u32,
    target_sample_rate_hz: u32,
    channels: usize,
    maximum_output_frames: u64,
}

fn plan_audio_import_memory(
    input: AudioImportMemoryInput,
    maximum_working_bytes: usize,
) -> Result<AudioImportMemoryPlan, IdeAudioImportError> {
    if maximum_working_bytes == 0 {
        return Err(IdeAudioImportError::CapacityUnavailable);
    }
    let resampling = plan_resampling_memory(
        input.input_frames,
        input.input_sample_rate_hz,
        input.target_sample_rate_hz,
        input.channels,
        input.maximum_output_frames,
    )
    .map_err(IdeAudioImportError::Source)?;
    let normalized_bytes = resampling
        .expected_output_frames()
        .checked_mul(input.channels)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or(IdeAudioImportError::CapacityUnavailable)?;
    let encoded_output_bytes = float_wav_encoded_capacity(
        resampling.expected_output_frames(),
        input.channels,
    )
    .map_err(IdeAudioImportError::Source)?;
    let decode_peak = input
        .retained_project_bytes
        .checked_add(input.encoded_source_bytes)
        .and_then(|bytes| bytes.checked_add(resampling.input_bytes()))
        .ok_or(IdeAudioImportError::CapacityUnavailable)?;
    let resampling_peak = input.retained_project_bytes
        .checked_add(resampling.working_bytes())
        .ok_or(IdeAudioImportError::CapacityUnavailable)?;
    let encoding_peak = input
        .retained_project_bytes
        .checked_add(normalized_bytes)
        .and_then(|bytes| bytes.checked_add(encoded_output_bytes))
        .ok_or(IdeAudioImportError::CapacityUnavailable)?;
    let peak = decode_peak.max(resampling_peak).max(encoding_peak);
    if peak > maximum_working_bytes {
        return Err(IdeAudioImportError::WorkingMemoryLimitExceeded {
            required: peak,
            maximum: maximum_working_bytes,
        });
    }
    Ok(AudioImportMemoryPlan {
        decode_peak,
        resampling_peak,
        encoding_peak,
        resampling_working: resampling.working_bytes(),
        peak,
    })
}

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
    playback_session_generation: u64,
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
                playback_session_generation: 0,
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
            playback_session_generation: 0,
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
            playback_session_generation: 0,
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
        mut self,
        root: impl AsRef<Path>,
    ) -> Result<RunningDesktopIdeApplication, IdeProjectOpenFailure> {
        let Some(next_playback_session_generation) =
            self.playback_session_generation.checked_add(1)
        else {
            return Err(IdeProjectOpenFailure {
                application: self,
                reason: IdeProjectOpenError::PlaybackSessionGenerationExhausted,
            });
        };
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
            Ok(session) => {
                self.playback_session_generation = next_playback_session_generation;
                let running = RunningDesktopIdeApplication {
                    application: self,
                    session: IdePlaybackSessionOwner::Native(session),
                };
                Ok(running)
            }
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
    PlaybackSessionGenerationExhausted,
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
            Self::PlaybackSessionGenerationExhausted => {
                formatter.write_str("playback session generation is exhausted")
            }
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
            Self::PlaybackSessionGenerationExhausted
            | Self::SelectedOutputUnavailable
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
    StaleSceneLaunch { requested_index: usize },
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
            Self::StaleSceneLaunch { requested_index } => write!(
                formatter,
                "arrangement scene marker {requested_index} is no longer current"
            ),
            Self::Send(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeArrangementLocateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Send(source) => Some(source),
            Self::Unavailable
            | Self::StaleProject
            | Self::StalePlacement { .. }
            | Self::StaleSceneLaunch { .. } => None,
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
        outcome: Box<ProjectResourcePublicationOutcome>,
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
                write!(formatter, "live mixer state could not be preserved: {source}")
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
        outcome: Box<ProjectResourcePublicationOutcome>,
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

impl IdeAudioClipCreationPublicationError {
    #[must_use]
    pub const fn clip_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Clip { source, .. } => source.published(),
        }
    }

    const fn committed_clip_publication(&self) -> Option<&ProjectResourcePublicationError> {
        match self {
            Self::Clip { source, .. } if source.published() => Some(source),
            _ => None,
        }
    }
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
pub enum IdeAudioImportError {
    EmptySelection,
    InvalidDestinationTrack,
    UnsupportedContainer,
    InvalidClipName,
    Source(DecodeError),
    SourceChanged(io::Error),
    MixerSave(Box<IdeProjectSaveError>),
    ProjectReload(ProjectPlaybackError),
    TrackResourceMissing,
    TrackResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipAdditionSerializationError),
    CapacityUnavailable,
    WorkingMemoryLimitExceeded { required: usize, maximum: usize },
    Publication(Box<ProjectResourcePublicationError>),
    NativeClose(IdeNativeCloseFailure),
}

impl fmt::Display for IdeAudioImportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySelection => formatter.write_str("audio import requires at least one file"),
            Self::InvalidDestinationTrack => {
                formatter.write_str("audio import requires an existing audio track")
            }
            Self::UnsupportedContainer => {
                formatter.write_str("audio import accepts WAV and native FLAC files")
            }
            Self::InvalidClipName => formatter.write_str(
                "the imported file name must contain 1 through 120 characters after normalization",
            ),
            Self::Source(source) => source.fmt(formatter),
            Self::SourceChanged(source) => write!(
                formatter,
                "selected audio source changed or could not be reverified before publication: {source}"
            ),
            Self::MixerSave(source) => write!(
                formatter,
                "live mixer state could not be preserved: {source}"
            ),
            Self::ProjectReload(source) => write!(
                formatter,
                "project could not be reloaded between imports: {source}"
            ),
            Self::TrackResourceMissing => {
                formatter.write_str("the destination track is no longer present")
            }
            Self::TrackResourceRoleMismatch => {
                formatter.write_str("the import destination is not a structural track")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio import capacity is unavailable")
            }
            Self::WorkingMemoryLimitExceeded { required, maximum } => write!(
                formatter,
                "audio import needs {required} bytes of peak working memory, limit is {maximum}"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before audio import: {source:?}"
            ),
        }
    }
}

impl Error for IdeAudioImportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source(source) => Some(source),
            Self::SourceChanged(source) => Some(source),
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::ProjectReload(source) => Some(source),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::EmptySelection
            | Self::InvalidDestinationTrack
            | Self::UnsupportedContainer
            | Self::InvalidClipName
            | Self::TrackResourceMissing
            | Self::TrackResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::WorkingMemoryLimitExceeded { .. }
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct IdeAudioImportBatchFailure {
    completed: usize,
    total: usize,
    path: PathBuf,
    source: Box<IdeAudioImportError>,
}

impl IdeAudioImportBatchFailure {
    #[cfg(any(test, feature = "ui-test-support"))]
    #[must_use]
    pub fn for_test(
        completed: usize,
        total: usize,
        path: PathBuf,
        source: IdeAudioImportError,
    ) -> Self {
        Self {
            completed,
            total,
            path,
            source: Box::new(source),
        }
    }

    #[must_use]
    pub const fn completed(&self) -> usize {
        self.completed
    }

    #[must_use]
    pub const fn total(&self) -> usize {
        self.total
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn reason(&self) -> &IdeAudioImportError {
        self.source.as_ref()
    }
}

impl fmt::Display for IdeAudioImportBatchFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "imported {} of {} files; {} failed: {}",
            self.completed,
            self.total,
            self.path.display(),
            self.source
        )
    }
}

impl Error for IdeAudioImportBatchFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeAudioImportBatchOutcome {
    imported: usize,
    final_revision: u64,
    clip_ids: Vec<String>,
}

impl IdeAudioImportBatchOutcome {
    #[cfg(any(test, feature = "ui-test-support"))]
    #[must_use]
    pub fn for_test(imported: usize, final_revision: u64, clip_ids: Vec<String>) -> Self {
        Self {
            imported,
            final_revision,
            clip_ids,
        }
    }

    #[must_use]
    pub const fn imported(&self) -> usize {
        self.imported
    }

    #[must_use]
    pub const fn final_revision(&self) -> u64 {
        self.final_revision
    }

    #[must_use]
    pub fn clip_ids(&self) -> &[String] {
        &self.clip_ids
    }
}

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
pub struct IdeAudioClipRemovalCommittedWarning {
    source: Box<dyn Error + Send + Sync>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdeUnusedSampleCleanupSummary {
    project_identity: [u8; 32],
    project_revision: u64,
    assets: usize,
    bytes: u64,
}

impl IdeUnusedSampleCleanupSummary {
    #[must_use]
    pub const fn new(
        project_identity: [u8; 32],
        project_revision: u64,
        assets: usize,
        bytes: u64,
    ) -> Self {
        Self {
            project_identity,
            project_revision,
            assets,
            bytes,
        }
    }
    #[must_use]
    pub const fn project_identity(&self) -> &[u8; 32] {
        &self.project_identity
    }
    #[must_use]
    pub const fn project_revision(&self) -> u64 {
        self.project_revision
    }
    #[must_use]
    pub const fn assets(&self) -> usize {
        self.assets
    }
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.assets == 0
    }
}

#[derive(Debug)]
pub struct IdeUnusedSampleCleanupCommittedWarning {
    source: Box<dyn Error + Send + Sync>,
}

impl IdeUnusedSampleCleanupCommittedWarning {
    fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for IdeUnusedSampleCleanupCommittedWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unused sample media was removed, but verification or transaction recovery is incomplete: {}",
            self.source
        )
    }
}

impl Error for IdeUnusedSampleCleanupCommittedWarning {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub enum IdeUnusedSampleCleanupError {
    Empty,
    StaleConfirmation,
    CapacityUnavailable,
    MixerSave(Box<IdeProjectSaveError>),
    Publication(Box<ProjectResourcePublicationError>),
    PublicationCommittedWithWarning(IdeUnusedSampleCleanupCommittedWarning),
    NativeClose(IdeNativeCloseFailure),
    ReopenAfterPublication {
        outcome: SampleCleanupOutcome,
        source: Box<IdeProjectOpenError>,
    },
    ReopenAfterFailure {
        cleanup: Box<IdeUnusedSampleCleanupError>,
        source: Box<IdeProjectOpenError>,
    },
}

impl IdeUnusedSampleCleanupError {
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::PublicationCommittedWithWarning(_) | Self::ReopenAfterPublication { .. } => true,
            Self::ReopenAfterFailure { cleanup, .. } => cleanup.publication_committed(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeUnusedSampleCleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("the project has no unused sample media"),
            Self::StaleConfirmation => {
                formatter.write_str("the unused-media cleanup confirmation is no longer current")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("unused-media cleanup capacity is unavailable")
            }
            Self::MixerSave(source) => write!(
                formatter,
                "live mixer state could not be preserved: {source}"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::PublicationCommittedWithWarning(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before unused-media cleanup: {source:?}"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "{} unused sample assets were removed in project revision {}, but the project could not be reopened: {source}",
                outcome.removed_assets(),
                outcome.project_revision()
            ),
            Self::ReopenAfterFailure { cleanup, source } if cleanup.publication_committed() => {
                write!(
                    formatter,
                    "unused sample media was removed with incomplete recovery ({cleanup}), and the project could not be reopened ({source})"
                )
            }
            Self::ReopenAfterFailure { cleanup, source } => write!(
                formatter,
                "unused-media cleanup failed ({cleanup}), and the unchanged project could not be reopened ({source})"
            ),
        }
    }
}

impl Error for IdeUnusedSampleCleanupError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Publication(source) => Some(source.as_ref()),
            Self::PublicationCommittedWithWarning(source) => Some(source),
            Self::ReopenAfterPublication { source, .. }
            | Self::ReopenAfterFailure { source, .. } => Some(source.as_ref()),
            Self::Empty
            | Self::StaleConfirmation
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

impl IdeAudioClipRemovalCommittedWarning {
    fn new(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            source: Box::new(source),
        }
    }
}

impl fmt::Display for IdeAudioClipRemovalCommittedWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(formatter)
    }
}

impl Error for IdeAudioClipRemovalCommittedWarning {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug)]
pub enum IdeAudioClipRemovalError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    TrackResourceMissing,
    TrackResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipRemovalSerializationError),
    CapacityUnavailable,
    Publication(Box<ProjectResourcePublicationError>),
    PublicationCommittedWithWarning(IdeAudioClipRemovalCommittedWarning),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipRemovalError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: AudioClipRemovalOutcome,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<ProjectResourcePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    PublicationCommittedWithWarningAndReopen {
        warning: IdeAudioClipRemovalCommittedWarning,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipRemovalError {
    /// Returns whether the structural clip removal crossed its durable manifest
    /// boundary, even when cleanup, verification, or the final reopen failed.
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::PublicationCommittedWithWarning(_)
            | Self::PublicationCommittedWithWarningAndReopen { .. }
            | Self::ReopenAfterPublication { .. } => true,
            Self::Publication(source) => source.published(),
            Self::PublicationAndReopen { publication, .. } => publication.published(),
            Self::ReopenAfterPreparation { preparation, .. } => {
                preparation.publication_committed()
            }
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipRemovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip removal requires a clip identity")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::ClipResourceMissing => {
                formatter.write_str("the audio clip is no longer present")
            }
            Self::ClipResourceRoleMismatch => {
                formatter.write_str("the selected identity is not an audio-clip resource")
            }
            Self::TrackResourceMissing => {
                formatter.write_str("the audio clip's owning track is no longer present")
            }
            Self::TrackResourceRoleMismatch => {
                formatter.write_str("the audio clip's owner is not a structural track")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("audio-clip removal capacity is unavailable")
            }
            Self::PublicationCommittedWithWarning(source) => write!(
                formatter,
                "the audio clip was removed, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Publication(source) if source.published() => write!(
                formatter,
                "the audio clip was removed, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before removing the audio clip: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before removing the audio clip ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip removal preparation failed ({preparation}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio clip {} was removed in project revision {} but the project could not be reopened: {source}",
                outcome.clip_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.published() => write!(
                formatter,
                "the audio clip was removed, but project verification or transaction recovery is incomplete ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip removal failed ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationCommittedWithWarningAndReopen { warning, reopen } => write!(
                formatter,
                "the audio clip was removed, but project verification or transaction recovery is incomplete ({warning}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipRemovalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::PublicationCommittedWithWarning(source) => Some(source),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::PublicationCommittedWithWarningAndReopen { warning, .. } => Some(warning),
            Self::InvalidClipIdentity
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
    Publication {
        clip_id: String,
        source: Box<IdeAudioClipCreationPublicationError>,
    },
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
        clip_id: String,
        publication: Box<IdeAudioClipCreationPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipDuplicationError {
    #[must_use]
    pub fn committed_clip_id(&self) -> Option<&str> {
        match self {
            Self::Publication { clip_id, source } if source.clip_published() => Some(clip_id),
            Self::PublicationAndReopen {
                clip_id,
                publication,
                ..
            } if publication.clip_published() => Some(clip_id),
            _ => None,
        }
    }
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
            Self::Publication { clip_id, source } => {
                if let Some(publication) = source.committed_clip_publication() {
                    write!(
                        formatter,
                        "audio clip {clip_id} was duplicated, but project verification or transaction recovery is incomplete: {publication}"
                    )
                } else {
                    source.fmt(formatter)
                }
            }
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
                clip_id,
                publication,
                reopen,
            } => {
                if let Some(source) = publication.committed_clip_publication() {
                    write!(
                        formatter,
                        "audio clip {clip_id} was duplicated, but project verification or transaction recovery is incomplete ({source}) and the project could not be reopened ({reopen})"
                    )
                } else {
                    write!(
                        formatter,
                        "audio-clip duplication for {clip_id} failed ({publication}) and the project could not be reopened ({reopen})"
                    )
                }
            }
        }
    }
}

impl Error for IdeAudioClipDuplicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication { source, .. } => Some(source.as_ref()),
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

impl IdeArrangementPlacementEditPublicationError {
    #[must_use]
    pub const fn arrangement_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Arrangement { source, .. } => source.published(),
        }
    }
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
    InvalidSceneIdentity,
    InvalidSceneLaunchIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ArrangementResourceMissing,
    ArrangementResourceAmbiguous,
    /// A marked loop could not be placed on the timeline. Each reason is
    /// distinct and diagnosable; they used to share one message that told the
    /// user their project had no arrangement.
    LoopMark(LoopMarkFailure),
    Storage(StorageError),
    Serialization(ArrangementPlacementDocumentSerializationError),
    CapacityUnavailable,
    Publication {
        created_placement_id: Option<Box<str>>,
        source: Box<IdeArrangementPlacementEditPublicationError>,
    },
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
        outcome: Box<ProjectResourcePublicationOutcome>,
        created_placement_id: Option<Box<str>>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        created_placement_id: Option<Box<str>>,
        publication: Box<IdeArrangementPlacementEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeArrangementPlacementEditError {
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::Publication { source, .. } => source.arrangement_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.arrangement_published(),
            _ => false,
        }
    }

    #[must_use]
    pub fn committed_created_placement_id(&self) -> Option<&str> {
        match self {
            Self::Publication {
                created_placement_id: Some(placement_id),
                source,
            } if source.arrangement_published() => Some(placement_id),
            Self::PublicationAndReopen {
                created_placement_id: Some(placement_id),
                publication,
                ..
            } if publication.arrangement_published() => Some(placement_id),
            Self::ReopenAfterPublication {
                created_placement_id: Some(placement_id),
                ..
            } => Some(placement_id),
            _ => None,
        }
    }
}

impl fmt::Display for IdeArrangementPlacementEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPlacementIdentity => {
                formatter.write_str("arrangement edit requires a placement identity")
            }
            Self::InvalidClipIdentity => formatter
                .write_str("arrangement placement addition requires an audio-clip identity"),
            Self::InvalidSceneIdentity => {
                formatter.write_str("arrangement scene launch requires a scene identity")
            }
            Self::InvalidSceneLaunchIdentity => {
                formatter.write_str("arrangement scene marker edit requires a marker identity")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::LoopMark(reason) => reason.fmt(formatter),
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
            Self::Publication {
                created_placement_id: Some(placement_id),
                source,
            } if source.arrangement_published() => write!(
                formatter,
                "arrangement placement {placement_id} was committed, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Publication { source, .. } => source.fmt(formatter),
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
            Self::ReopenAfterPublication {
                outcome,
                created_placement_id,
                source,
            } => {
                if let Some(placement_id) = created_placement_id {
                    write!(
                        formatter,
                        "arrangement placement {placement_id} was committed in project revision {} but the project could not be reopened: {source}",
                        outcome.project_revision()
                    )
                } else {
                    write!(
                        formatter,
                        "arrangement was edited in project revision {} but the project could not be reopened: {source}",
                        outcome.project_revision()
                    )
                }
            }
            Self::PublicationAndReopen {
                created_placement_id,
                publication,
                reopen,
            } => {
                if publication.arrangement_published() {
                    if let Some(placement_id) = created_placement_id {
                        write!(
                            formatter,
                            "arrangement placement {placement_id} was committed, but project verification or transaction recovery is incomplete ({publication}) and the project could not be reopened ({reopen})"
                        )
                    } else {
                        write!(
                            formatter,
                            "arrangement edit was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
                        )
                    }
                } else {
                    write!(
                        formatter,
                        "arrangement edit publication failed ({publication}) and the project could not be reopened ({reopen})"
                    )
                }
            }
        }
    }
}

/// Why a marked loop could not be placed on the timeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopMarkFailure {
    /// The project could not be compiled, so the timeline that converts a
    /// marked frame to a tick is unavailable.
    TimelineUnavailable,
    /// A marked frame lies outside the timeline's tick domain.
    MarkOutOfRange,
    /// Both marks resolve to the same tick, so the range would be empty.
    MarksCollapse,
}

impl fmt::Display for LoopMarkFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TimelineUnavailable => {
                "the project timeline could not be compiled to place the loop"
            }
            Self::MarkOutOfRange => "a loop mark lies outside the project timeline",
            Self::MarksCollapse => {
                "the loop marks are closer together than one tick, so the range would be empty"
            }
        })
    }
}

impl Error for IdeArrangementPlacementEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication { source, .. } => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidPlacementIdentity
            | Self::InvalidClipIdentity
            | Self::InvalidSceneIdentity
            | Self::InvalidSceneLaunchIdentity
            | Self::ArrangementResourceMissing
            | Self::ArrangementResourceAmbiguous
            | Self::LoopMark(_)
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

#[derive(Debug)]
pub enum IdeSceneEditPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Scene {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdeSceneLaunchQuantization {
    Immediate,
    Grid { quantum_ticks: u64 },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeSceneSlotEdit {
    Continue,
    Stop,
    Play { clip_id: String },
}

impl IdeSceneSlotEdit {
    fn as_serialization_edit(&self) -> SceneSlotEdit<'_> {
        match self {
            Self::Continue => SceneSlotEdit::Continue,
            Self::Stop => SceneSlotEdit::Stop,
            Self::Play { clip_id } => SceneSlotEdit::Play { clip_id },
        }
    }
}

impl From<IdeSceneLaunchQuantization> for LaunchQuantization {
    fn from(value: IdeSceneLaunchQuantization) -> Self {
        match value {
            IdeSceneLaunchQuantization::Immediate => Self::Immediate,
            IdeSceneLaunchQuantization::Grid { quantum_ticks } => Self::Grid { quantum_ticks },
        }
    }
}

#[derive(Debug)]
pub enum IdeScenePublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Scene {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl IdeScenePublicationError {
    #[must_use]
    pub const fn scene_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Scene { source, .. } => source.published(),
        }
    }
}

impl fmt::Display for IdeScenePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before publishing the new scene: {source}"
            ),
            Self::Scene { source, .. } if source.published() => write!(
                formatter,
                "new scene was published, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Scene { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but new scene publication failed: {source}",
                mixer.project_revision()
            ),
            Self::Scene { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeScenePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Scene { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeSceneCreationError {
    InvalidName,
    InvalidQuantization,
    MixerSave(Box<IdeProjectSaveError>),
    Serialization(SceneAdditionSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeScenePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeSceneCreationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeScenePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeSceneCreationError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.scene_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.scene_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeSceneCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                formatter.write_str("scene name must contain 1 through 120 characters")
            }
            Self::InvalidQuantization => {
                formatter.write_str("scene grid quantization must contain at least one tick")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("scene creation capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before creating the scene: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before creating the scene ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "scene creation preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "scene {} was created in project revision {} but the project could not be reopened: {source}",
                outcome.resource_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.scene_published() => write!(
                formatter,
                "scene creation committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "scene creation failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeSceneCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidName
            | Self::InvalidQuantization
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

/// One mixer parameter a desktop-authored automation lane may drive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeAutomationTarget {
    Channel {
        track_id: String,
        parameter: AutomationChannelParameter,
    },
    Send {
        send_id: String,
        parameter: AutomationSendParameter,
    },
    Main {
        parameter: AutomationMainParameter,
    },
}

/// One desktop-authored automation take holding exactly one lane.
#[derive(Clone, Debug)]
pub struct IdeAutomationTakeRequest {
    pub name: String,
    pub start_tick: u64,
    pub end_tick: u64,
    pub target: IdeAutomationTarget,
    pub interpolation: AutomationInterpolation,
    pub points: Vec<AutomationPoint>,
}

/// One complete native-device mutation submitted through the project's
/// close-publish-reopen lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeNativeFilterEdit {
    Add {
        track_id: String,
        mode: NativeFilterMode,
        cutoff_millihertz: u32,
        resonance_millionths: u32,
    },
    Update {
        chain_id: String,
        device_instance_id: String,
        enabled: bool,
        mode: NativeFilterMode,
        cutoff_millihertz: u32,
        resonance_millionths: u32,
    },
    AddCompressor {
        track_id: String,
        threshold_millidecibels: i32,
        ratio_millionths: u32,
        attack_microseconds: u32,
        release_microseconds: u32,
        makeup_millidecibels: i32,
    },
    UpdateCompressor {
        chain_id: String,
        device_instance_id: String,
        enabled: bool,
        threshold_millidecibels: i32,
        ratio_millionths: u32,
        attack_microseconds: u32,
        release_microseconds: u32,
        makeup_millidecibels: i32,
    },
    Move {
        chain_id: String,
        device_instance_id: String,
        destination_index: usize,
    },
    Duplicate {
        chain_id: String,
        device_instance_id: String,
    },
    CopyTo {
        source_chain_id: String,
        source_device_instance_id: String,
        target_chain_id: String,
        target_device_instance_id: String,
    },
    Remove {
        chain_id: String,
        device_instance_id: String,
    },
}

#[derive(Debug)]
pub enum IdeNativeFilterEditError {
    InvalidIdentity,
    CopyRequiresDistinctChains,
    TrackUnavailable,
    ChainUnavailable,
    MixerSave(Box<IdeProjectSaveError>),
    Device(DeviceChainError),
    Serialization(DeviceChainError),
    MixerPublication(Box<ProjectResourcePublicationError>),
    Publication {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<Self>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<Self>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeNativeFilterEditError {
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::Publication { source, .. } => source.published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. }
            | Self::ReopenAfterPreparation {
                preparation: publication,
                ..
            } => publication.publication_committed(),
            Self::InvalidIdentity
            | Self::CopyRequiresDistinctChains
            | Self::TrackUnavailable
            | Self::ChainUnavailable
            | Self::MixerSave(_)
            | Self::Device(_)
            | Self::Serialization(_)
            | Self::MixerPublication(_)
            | Self::NativeClose(_)
            | Self::NativeCloseAndReopen { .. } => false,
        }
    }
}

impl fmt::Display for IdeNativeFilterEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity => formatter.write_str("native device identity is invalid"),
            Self::CopyRequiresDistinctChains => {
                formatter.write_str("a native device copy target must be in another chain")
            }
            Self::TrackUnavailable => formatter.write_str("native device track is unavailable"),
            Self::ChainUnavailable => formatter.write_str("native device chain is unavailable"),
            Self::MixerSave(source) => write!(formatter, "live mixer state could not be preserved: {source}"),
            Self::Device(source) | Self::Serialization(source) => source.fmt(formatter),
            Self::MixerPublication(source) => write!(formatter, "live mixer state could not be published: {source}"),
            Self::Publication { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but native device publication failed: {source}",
                mixer.project_revision()
            ),
            Self::Publication { source, .. } => source.fmt(formatter),
            Self::NativeClose(source) => write!(formatter, "native playback did not close cleanly before editing native devices: {source:?}"),
            Self::NativeCloseAndReopen { close, reopen } => write!(formatter, "native playback did not close cleanly before editing native devices ({close:?}) and authoritative playback could not be reopened: {reopen}"),
            Self::ReopenAfterPreparation { preparation, reopen } => write!(formatter, "native device edit was refused ({preparation}) and authoritative playback could not be reopened: {reopen}"),
            Self::ReopenAfterPublication { outcome, source } => write!(formatter, "native device edit committed as project revision {}, but playback could not be reopened: {source}", outcome.project_revision()),
            Self::PublicationAndReopen { publication, reopen } => write!(formatter, "native device publication failed ({publication}) and authoritative playback could not be reopened: {reopen}"),
        }
    }
}

impl Error for IdeNativeFilterEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source),
            Self::Device(source) | Self::Serialization(source) => Some(source),
            Self::MixerPublication(source) | Self::Publication { source, .. } => Some(source),
            Self::ReopenAfterPreparation { preparation, .. }
            | Self::PublicationAndReopen {
                publication: preparation,
                ..
            } => Some(preparation),
            Self::ReopenAfterPublication { source, .. } => Some(source),
            Self::InvalidIdentity
            | Self::CopyRequiresDistinctChains
            | Self::TrackUnavailable
            | Self::ChainUnavailable
            | Self::NativeClose(_)
            | Self::NativeCloseAndReopen { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAutomationTakePublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Take {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<ProjectResourcePublicationError>,
    },
}

impl IdeAutomationTakePublicationError {
    #[must_use]
    pub const fn take_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Take { source, .. } => source.published(),
        }
    }
}

impl fmt::Display for IdeAutomationTakePublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before changing automation takes: {source}"
            ),
            Self::Take { source, .. } if source.published() => write!(
                formatter,
                "automation take change committed, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Take { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but the automation take change failed: {source}",
                mixer.project_revision()
            ),
            Self::Take { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeAutomationTakePublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Take { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeAutomationTakeCreationError {
    InvalidName,
    InvalidTargetIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    Storage(StorageError),
    MixerDocument(PlaybackDocumentLoadError),
    Serialization(AutomationTakeAdditionSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAutomationTakePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAutomationTakeCreationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAutomationTakePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAutomationTakeCreationError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.take_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.take_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAutomationTakeCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                formatter.write_str("automation take name must contain 1 through 120 characters")
            }
            Self::InvalidTargetIdentity => {
                formatter.write_str("the automation target identity is empty")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::MixerDocument(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("automation take capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before adding the automation take: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before adding the automation take ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "automation take preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "automation take {} was added in project revision {} but the project could not be reopened: {source}",
                outcome.resource_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.take_published() => write!(
                formatter,
                "automation take committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "automation take publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAutomationTakeCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::MixerDocument(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidName
            | Self::InvalidTargetIdentity
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeAutomationTakeEditError {
    InvalidTakeIdentity,
    TakeUnavailable,
    MixerSave(Box<IdeProjectSaveError>),
    Storage(StorageError),
    MixerDocument(PlaybackDocumentLoadError),
    Serialization(AutomationTakeEditSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAutomationTakePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAutomationTakeEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAutomationTakePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAutomationTakeEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.take_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.take_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAutomationTakeEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTakeIdentity => {
                formatter.write_str("the automation take identity is empty")
            }
            Self::TakeUnavailable => formatter.write_str("the automation take is unavailable"),
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::MixerDocument(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("automation take edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the automation take: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the automation take ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "automation take edit preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "automation take {} was edited in project revision {} but the project could not be reopened: {source}",
                outcome.resource_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.take_published() => write!(
                formatter,
                "automation take edit committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "automation take edit failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAutomationTakeEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::MixerDocument(source) => Some(source),
            Self::Serialization(source) => Some(source),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidTakeIdentity
            | Self::TakeUnavailable
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

/// One control binding the desktop IDE asks the mapping editor to insert.
///
/// Identity and enabled state are owned by the transaction; every other field
/// is validated by the version 1 binding domain before the edit is previewed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdeControlBindingRequest {
    pub source: ControlSource,
    pub scope: ControlScope,
    pub transform: ControlTransform,
    pub target: ControlTarget,
}

/// One control-mapping mutation the desktop IDE submits as a single durable
/// transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdeControlMappingEdit {
    Insert(IdeControlBindingRequest),
    SetEnabled { binding_id: String, enabled: bool },
    Remove { binding_id: String },
}

/// A loop the user marked with the playhead, in frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdeLoopRangeFrames {
    pub start_frame: usize,
    pub end_frame: usize,
    pub enabled: bool,
}

#[derive(Debug)]
pub enum IdeTempoEditError {
    TimelineResourceMissing,
    TimelineResourceAmbiguous,
    MixerSave(Box<IdeProjectSaveError>),
    Storage(StorageError),
    CapacityUnavailable,
    Serialization(Box<TempoSerializationError>),
    MixerPublication(Box<ProjectResourcePublicationError>),
    Publication(Box<ProjectResourcePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeTempoEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeTempoEditError {
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::ReopenAfterPublication { .. } => true,
            Self::Publication(source) => source.published(),
            Self::ReopenAfterPreparation { preparation, .. } => preparation.publication_committed(),
            Self::TimelineResourceMissing
            | Self::TimelineResourceAmbiguous
            | Self::MixerSave(_)
            | Self::Storage(_)
            | Self::CapacityUnavailable
            | Self::Serialization(_)
            | Self::MixerPublication(_)
            | Self::NativeClose(_) => false,
        }
    }
}

impl fmt::Display for IdeTempoEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimelineResourceMissing => {
                formatter.write_str("the project has no timeline resource")
            }
            Self::TimelineResourceAmbiguous => {
                formatter.write_str("the project has more than one timeline resource")
            }
            Self::MixerSave(source) => write!(
                formatter,
                "live mixer state could not be preserved: {source}"
            ),
            Self::Storage(source) => source.fmt(formatter),
            Self::CapacityUnavailable => formatter.write_str("tempo edit capacity is unavailable"),
            Self::Serialization(source) => source.fmt(formatter),
            Self::MixerPublication(source) => write!(
                formatter,
                "live mixer state could not be published before the tempo change: {source}"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before the tempo change: {source:?}"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "the tempo was saved in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "the tempo change failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeTempoEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::Serialization(source) => Some(source.as_ref()),
            Self::MixerPublication(source) | Self::Publication(source) => Some(source.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::TimelineResourceMissing
            | Self::TimelineResourceAmbiguous
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeResourceRestoreError {
    ResourceMissing,
    MixerSave(Box<IdeProjectSaveError>),
    Storage(StorageError),
    CapacityUnavailable,
    CandidateInvalid(Box<dyn Error + Send + Sync>),
    MixerPublication(Box<ProjectResourcePublicationError>),
    Publication(Box<ProjectResourcePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeResourceRestoreError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
}

impl IdeResourceRestoreError {
    /// Whether the restored bytes reached the project despite the failure.
    /// A committed mixer publication does not count: the entry's own resource
    /// was never written.
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::ReopenAfterPublication { .. } => true,
            Self::Publication(source) => source.published(),
            Self::ReopenAfterPreparation { preparation, .. } => preparation.publication_committed(),
            Self::MixerPublication(_)
            | Self::ResourceMissing
            | Self::MixerSave(_)
            | Self::Storage(_)
            | Self::CapacityUnavailable
            | Self::CandidateInvalid(_)
            | Self::NativeClose(_)
            | Self::NativeCloseAndReopen { .. } => false,
        }
    }

    /// A refusal that cannot succeed on a later attempt, so retaining the
    /// entry would wedge every older entry behind it.
    #[must_use]
    pub const fn is_permanent_refusal(&self) -> bool {
        matches!(self, Self::ResourceMissing | Self::CandidateInvalid(_))
    }
}

impl fmt::Display for IdeResourceRestoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResourceMissing => {
                formatter.write_str("the edited resource no longer exists in this project")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::MixerPublication(source) => write!(
                formatter,
                "live mixer state could not be published before the undo: {source}"
            ),
            Self::Storage(source) => source.fmt(formatter),
            Self::CapacityUnavailable => formatter.write_str("undo capacity is unavailable"),
            Self::CandidateInvalid(source) => write!(
                formatter,
                "undoing this edit would leave a project that cannot open: {source}"
            ),
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before the undo: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before the undo ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "undo preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "the previous state of {} was restored in project revision {} but the project could not be reopened: {source}",
                outcome.resource_id(),
                outcome.project_revision()
            ),
        }
    }
}

impl Error for IdeResourceRestoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Storage(source) => Some(source),
            Self::CandidateInvalid(source) => Some(source.as_ref()),
            Self::MixerPublication(source) | Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::ResourceMissing | Self::CapacityUnavailable | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeProjectExportError {
    ProjectSnapshotUnavailable,
    DestinationUnavailable,
    LiveMixerUnavailable,
    Export(Box<OfflineExportError>),
}

/// Places the staging journal beside the destination without ever colliding
/// with it, including when the destination is itself named `.journal`.
#[must_use]
pub fn export_journal_path(wav_path: &Path) -> Option<std::path::PathBuf> {
    let file_name = wav_path.file_name()?.to_str()?;
    let journal = wav_path.with_file_name(format!("{file_name}.export-journal"));
    (journal != wav_path).then_some(journal)
}

impl fmt::Display for IdeProjectExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectSnapshotUnavailable => {
                formatter.write_str("no open project is available to export")
            }
            Self::DestinationUnavailable => {
                formatter.write_str("the export destination is not a usable file path")
            }
            Self::LiveMixerUnavailable => {
                formatter.write_str("the audible mixer state could not be compiled for export")
            }
            Self::Export(source) => source.fmt(formatter),
        }
    }
}

impl Error for IdeProjectExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Export(source) => Some(source.as_ref()),
            Self::ProjectSnapshotUnavailable
            | Self::DestinationUnavailable
            | Self::LiveMixerUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeControlMappingEditError {
    InvalidBindingIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    MappingResourceMissing,
    CapacityUnavailable,
    Binding(ControlMappingParseError),
    Mutation(MappingEditorMutationError),
    Preview(Box<MappingEditorPreviewError>),
    StalePreview,
    Publication(Box<IdeControlMappingPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeControlMappingEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        revision: u64,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeControlMappingPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeControlMappingEditError {
    #[must_use]
    pub fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.mapping_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.mapping_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeControlMappingEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBindingIdentity => {
                formatter.write_str("the control binding identity is empty")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::MappingResourceMissing => {
                formatter.write_str("this project declares no control mapping to edit")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("control mapping edit capacity is unavailable")
            }
            Self::Binding(source) => source.fmt(formatter),
            Self::Mutation(source) => source.fmt(formatter),
            Self::Preview(source) => source.fmt(formatter),
            Self::StalePreview => {
                formatter.write_str("the control mapping changed while it was being confirmed")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing control mappings: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing control mappings ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "control mapping edit preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { revision, source } => write!(
                formatter,
                "control mappings were published in project revision {revision} but the project could not be reopened: {source}"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.mapping_published() => write!(
                formatter,
                "control mapping edit committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "control mapping edit failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeControlMappingEditError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MixerSave(source) => Some(source.as_ref()),
            Self::Binding(source) => Some(source),
            Self::Mutation(source) => Some(source),
            Self::Preview(source) => Some(source.as_ref()),
            Self::Publication(source) => Some(source.as_ref()),
            Self::NativeCloseAndReopen { reopen, .. } => Some(reopen.as_ref()),
            Self::ReopenAfterPreparation { preparation, .. } => Some(preparation.as_ref()),
            Self::ReopenAfterPublication { source, .. } => Some(source.as_ref()),
            Self::PublicationAndReopen { publication, .. } => Some(publication.as_ref()),
            Self::InvalidBindingIdentity
            | Self::MappingResourceMissing
            | Self::CapacityUnavailable
            | Self::StalePreview
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeControlMappingPublicationError {
    Mixer(Box<ProjectResourcePublicationError>),
    Mapping {
        mixer: Box<ProjectResourcePublicationOutcome>,
        source: Box<MappingEditorPublicationError>,
    },
}

impl IdeControlMappingPublicationError {
    #[must_use]
    pub fn mapping_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Mapping { source, .. } => match source.as_ref() {
                MappingEditorPublicationError::Storage(storage) => storage.published(),
                MappingEditorPublicationError::UnboundPreview => false,
            },
        }
    }
}

impl fmt::Display for IdeControlMappingPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before publishing control mappings: {source}"
            ),
            Self::Mapping { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but control mapping publication failed: {source}",
                mixer.project_revision()
            ),
            Self::Mapping { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeControlMappingPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) => Some(source.as_ref()),
            Self::Mapping { source, .. } => Some(source.as_ref()),
        }
    }
}

#[derive(Debug)]
pub enum IdeSceneDuplicationError {
    InvalidSourceSceneIdentity,
    InvalidName,
    MixerSave(Box<IdeProjectSaveError>),
    SceneResourceMissing,
    SceneResourceRoleMismatch,
    Storage(StorageError),
    Serialization(SceneDuplicationSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeScenePublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeSceneDuplicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeScenePublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeSceneDuplicationError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.scene_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.scene_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeSceneDuplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSourceSceneIdentity => {
                formatter.write_str("scene duplication requires a source scene identity")
            }
            Self::InvalidName => {
                formatter.write_str("duplicated scene name must contain 1 through 120 characters")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::SceneResourceMissing => {
                formatter.write_str("the source scene is no longer present")
            }
            Self::SceneResourceRoleMismatch => {
                formatter.write_str("the source identity is not a scene resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("scene duplication capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before duplicating the scene: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before duplicating the scene ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "scene duplication preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "scene {} was duplicated in project revision {} but the project could not be reopened: {source}",
                outcome.resource_id(),
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.scene_published() => {
                write!(
                    formatter,
                    "scene duplication committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
                )
            }
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "scene duplication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeSceneDuplicationError {
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
            Self::InvalidSourceSceneIdentity
            | Self::InvalidName
            | Self::SceneResourceMissing
            | Self::SceneResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

#[derive(Debug)]
pub enum IdeSceneSlotEditError {
    InvalidSceneIdentity,
    InvalidTrackIdentity,
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    SceneResourceMissing,
    SceneResourceRoleMismatch,
    Storage(StorageError),
    Serialization(SceneSlotEditSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeSceneEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeSceneSlotEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeSceneEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeSceneSlotEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.scene_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.scene_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeSceneSlotEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSceneIdentity => {
                formatter.write_str("scene slot edit requires a scene identity")
            }
            Self::InvalidTrackIdentity => {
                formatter.write_str("scene slot edit requires a track identity")
            }
            Self::InvalidClipIdentity => {
                formatter.write_str("scene play action requires an audio-clip identity")
            }
            Self::MixerSave(source) => {
                write!(
                    formatter,
                    "live mixer state could not be preserved: {source}"
                )
            }
            Self::SceneResourceMissing => {
                formatter.write_str("the selected scene resource is missing")
            }
            Self::SceneResourceRoleMismatch => {
                formatter.write_str("the selected identity is not a scene resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("scene slot edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the scene slot: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the scene slot ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "scene slot preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "scene slot was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.scene_published() => write!(
                formatter,
                "scene slot edit committed with incomplete verification or recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "scene slot edit failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeSceneSlotEditError {
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
            Self::InvalidSceneIdentity
            | Self::InvalidTrackIdentity
            | Self::InvalidClipIdentity
            | Self::SceneResourceMissing
            | Self::SceneResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

impl IdeSceneEditPublicationError {
    #[must_use]
    pub const fn scene_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Scene { source, .. } => source.published(),
        }
    }
}

impl fmt::Display for IdeSceneEditPublicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mixer(source) if source.published() => write!(
                formatter,
                "live mixer was preserved, but its project transaction cleanup is incomplete: {source}"
            ),
            Self::Mixer(source) => write!(
                formatter,
                "live mixer could not be preserved before the scene edit: {source}"
            ),
            Self::Scene { source, .. } if source.published() => write!(
                formatter,
                "scene edit was published, but project verification or transaction recovery is incomplete: {source}"
            ),
            Self::Scene { mixer, source } if mixer.changed() => write!(
                formatter,
                "live mixer was preserved in project revision {} but the scene edit failed: {source}",
                mixer.project_revision()
            ),
            Self::Scene { source, .. } => source.fmt(formatter),
        }
    }
}

impl Error for IdeSceneEditPublicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Mixer(source) | Self::Scene { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub enum IdeSceneEditError {
    InvalidSceneIdentity,
    InvalidName,
    MixerSave(Box<IdeProjectSaveError>),
    SceneResourceMissing,
    SceneResourceRoleMismatch,
    Storage(StorageError),
    Serialization(SceneDocumentSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeSceneEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeSceneEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeSceneEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeSceneEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.scene_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.scene_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeSceneEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSceneIdentity => formatter.write_str("scene edit requires an identity"),
            Self::InvalidName => {
                formatter.write_str("scene rename requires a name of 1 to 120 characters")
            }
            Self::MixerSave(source) => write!(
                formatter,
                "live mixer state could not be preserved: {source}"
            ),
            Self::SceneResourceMissing => {
                formatter.write_str("the selected scene resource is missing")
            }
            Self::SceneResourceRoleMismatch => {
                formatter.write_str("the selected identity is not a scene resource")
            }
            Self::Storage(source) => source.fmt(formatter),
            Self::Serialization(source) => source.fmt(formatter),
            Self::CapacityUnavailable => formatter.write_str("scene edit capacity is unavailable"),
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the scene: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the scene ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "scene edit preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "scene edit was committed in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.scene_published() => write!(
                formatter,
                "scene edit was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "scene edit publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeSceneEditError {
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
            Self::InvalidSceneIdentity
            | Self::InvalidName
            | Self::SceneResourceMissing
            | Self::SceneResourceRoleMismatch
            | Self::CapacityUnavailable
            | Self::NativeClose(_) => None,
        }
    }
}

pub type IdeSceneRenameError = IdeSceneEditError;

impl IdeAudioClipEditPublicationError {
    #[must_use]
    pub const fn clip_published(&self) -> bool {
        match self {
            Self::Mixer(_) => false,
            Self::Clip { source, .. } => source.published(),
        }
    }
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
                "audio-clip edit was published, but project verification or transaction recovery is incomplete: {source}"
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
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipRenameEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
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
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip rename was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
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
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipGainEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
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
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip gain was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
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
pub enum IdeAudioClipFadeEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipFadeSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipFadeEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipFadeEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipFadeEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip fade edit requires a clip identity")
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
                formatter.write_str("audio-clip fade edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip fades: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip fades ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip fade preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip fades were edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip fades were committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip fade publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipFadeEditError {
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
pub enum IdeAudioClipLoopEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipLoopSerializationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipLoopEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipLoopEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipLoopEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip loop edit requires a clip identity")
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
                formatter.write_str("audio-clip loop edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing the audio-clip loop: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing the audio-clip loop ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip loop preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip loop was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip loop was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip loop publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipLoopEditError {
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
pub enum IdeAudioClipReverseEditError {
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
        preparation: Box<IdeAudioClipReverseEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipReverseEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipReverseEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip reverse edit requires a clip identity")
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
                formatter.write_str("audio-clip reverse edit capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip reverse playback: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before editing audio-clip reverse playback ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip reverse preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip reverse playback was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip reverse playback was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip reverse publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipReverseEditError {
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
pub enum IdeAudioClipSourceSlipEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipSourceStartSerializationError),
    NativeValidation(AudioClipSourceSlipValidationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipSourceSlipEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipSourceSlipEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipSourceSlipEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip source slip requires a clip identity")
            }
            Self::MixerSave(source) => {
                write!(formatter, "live mixer state could not be preserved: {source}")
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
                formatter.write_str("audio-clip source-slip capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before slipping the audio-clip source: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before slipping the audio-clip source ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip source-slip preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip source start was edited in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip source start was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip source-slip publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipSourceSlipEditError {
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
pub enum IdeAudioClipSourceTrimEditError {
    InvalidClipIdentity,
    MixerSave(Box<IdeProjectSaveError>),
    ClipResourceMissing,
    ClipResourceRoleMismatch,
    Storage(StorageError),
    Serialization(AudioClipSourceViewSerializationError),
    NativeValidation(AudioClipSourceSlipValidationError),
    CapacityUnavailable,
    Publication(Box<IdeAudioClipEditPublicationError>),
    NativeClose(IdeNativeCloseFailure),
    NativeCloseAndReopen {
        close: IdeNativeCloseFailure,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPreparation {
        preparation: Box<IdeAudioClipSourceTrimEditError>,
        reopen: Box<IdeProjectOpenError>,
    },
    ReopenAfterPublication {
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipSourceTrimEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
}

impl fmt::Display for IdeAudioClipSourceTrimEditError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidClipIdentity => {
                formatter.write_str("audio-clip source-view trim requires a clip identity")
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
                formatter.write_str("audio-clip source-view trim capacity is unavailable")
            }
            Self::Publication(source) => source.fmt(formatter),
            Self::NativeClose(source) => write!(
                formatter,
                "native playback did not close cleanly before trimming the audio-clip source view: {source:?}"
            ),
            Self::NativeCloseAndReopen { close, reopen } => write!(
                formatter,
                "native playback did not close cleanly before trimming the audio-clip source view ({close:?}) and the unchanged project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPreparation {
                preparation,
                reopen,
            } => write!(
                formatter,
                "audio-clip source-view trim preparation failed ({preparation}) and the project could not be reopened ({reopen})"
            ),
            Self::ReopenAfterPublication { outcome, source } => write!(
                formatter,
                "audio-clip source view was trimmed in project revision {} but the project could not be reopened: {source}",
                outcome.project_revision()
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip source view was trimmed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
            ),
            Self::PublicationAndReopen {
                publication,
                reopen,
            } => write!(
                formatter,
                "audio-clip source-view trim publication failed ({publication}) and the project could not be reopened ({reopen})"
            ),
        }
    }
}

impl Error for IdeAudioClipSourceTrimEditError {
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
        outcome: Box<ProjectResourcePublicationOutcome>,
        source: Box<IdeProjectOpenError>,
    },
    PublicationAndReopen {
        publication: Box<IdeAudioClipEditPublicationError>,
        reopen: Box<IdeProjectOpenError>,
    },
}

impl IdeAudioClipActivationEditError {
    #[must_use]
    pub const fn publication_committed(&self) -> bool {
        match self {
            Self::Publication(source) => source.clip_published(),
            Self::ReopenAfterPublication { .. } => true,
            Self::PublicationAndReopen { publication, .. } => publication.clip_published(),
            _ => false,
        }
    }
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
            } if publication.clip_published() => write!(
                formatter,
                "audio-clip activation was committed with incomplete project verification or transaction recovery ({publication}) and the project could not be reopened ({reopen})"
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

#[derive(Clone, Debug)]
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
struct IdeImportedAudioClipPlan {
    source_path: PathBuf,
    source_maximum_bytes: u64,
    source_evidence: StableFileEvidence,
    project_path: PathBuf,
    expected_project_sha256: [u8; 32],
    clip_id: String,
    clip_bytes: Vec<u8>,
    track_id: String,
    track_bytes: Vec<u8>,
    asset_id: String,
    sample_bytes: Vec<u8>,
}

impl IdeImportedAudioClipPlan {
    fn publish(&self) -> Result<AudioClipPublicationOutcome, IdeAudioImportError> {
        confirm_stable_absolute_file(
            &self.source_path,
            self.source_maximum_bytes,
            &self.source_evidence,
        )
        .map_err(IdeAudioImportError::SourceChanged)?;
        publish_imported_audio_clip(
            &self.project_path,
            ImportedAudioClipPublicationRequest {
                clip: AudioClipPublicationRequest {
                    expected_project_sha256: self.expected_project_sha256,
                    clip_id: &self.clip_id,
                    clip_bytes: &self.clip_bytes,
                    track_id: &self.track_id,
                    track_bytes: &self.track_bytes,
                    asset_id: &self.asset_id,
                },
                sample_bytes: &self.sample_bytes,
            },
        )
        .map_err(|source| IdeAudioImportError::Publication(Box::new(source)))
    }
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

#[derive(Debug)]
struct IdeAudioClipRemovalPlan {
    mixer: IdeMixerSavePlan,
    clip_id: String,
    track_id: String,
    track_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeUnusedSampleCleanupPlan {
    mixer: IdeMixerSavePlan,
    asset_ids: Vec<String>,
}

impl IdeUnusedSampleCleanupPlan {
    fn publish(&self) -> Result<SampleCleanupOutcome, IdeUnusedSampleCleanupError> {
        let mixer = self.mixer.publish().map_err(|source| {
            IdeUnusedSampleCleanupError::MixerSave(Box::new(IdeProjectSaveError::Publication(
                Box::new(source),
            )))
        })?;
        clean_unused_samples(
            &self.mixer.project_path,
            SampleCleanupRequest {
                expected_project_sha256: *mixer.project_identity(),
                asset_ids: &self.asset_ids,
            },
        )
        .map_err(|source| {
            if source.published() {
                IdeUnusedSampleCleanupError::PublicationCommittedWithWarning(
                    IdeUnusedSampleCleanupCommittedWarning::new(source),
                )
            } else {
                IdeUnusedSampleCleanupError::Publication(Box::new(source))
            }
        })
    }
}

impl IdeAudioClipRemovalPlan {
    fn publish(&self) -> Result<AudioClipRemovalOutcome, IdeAudioClipRemovalError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| {
                IdeAudioClipRemovalError::MixerSave(Box::new(IdeProjectSaveError::Publication(
                    Box::new(source),
                )))
            })?;
        remove_audio_clip(
            &self.mixer.project_path,
            AudioClipRemovalRequest {
                expected_project_sha256: *mixer.project_identity(),
                clip_id: &self.clip_id,
                track_id: &self.track_id,
                track_bytes: &self.track_bytes,
            },
        )
        .map_err(|source| {
            if source.published() {
                IdeAudioClipRemovalError::PublicationCommittedWithWarning(
                    IdeAudioClipRemovalCommittedWarning::new(source),
                )
            } else {
                IdeAudioClipRemovalError::Publication(Box::new(source))
            }
        })
    }
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
    created_placement_id: Option<String>,
}

#[derive(Debug)]
struct IdeTimelineEditPlan {
    mixer: IdeMixerSavePlan,
    timeline_id: String,
    timeline_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeAudioClipEditPlan {
    mixer: IdeMixerSavePlan,
    clip_id: String,
    clip_bytes: Vec<u8>,
}

/// A source-view trim retains the prior canonical clip document before its
/// publication begins. A committed storage warning therefore still has the
/// exact inverse needed to keep undo truthful.
#[derive(Debug)]
struct IdeAudioClipSourceTrimPlan {
    edit: IdeAudioClipEditPlan,
    inverse: IdeHistoryOperation,
}

#[derive(Debug)]
struct IdeAudioClipSourceTrimResource {
    clip_id: String,
    clip_bytes: Vec<u8>,
    prior_clip_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeSceneEditPlan {
    mixer: IdeMixerSavePlan,
    scene_id: String,
    scene_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeScenePublicationPlan {
    mixer: IdeMixerSavePlan,
    scene_id: String,
    scene_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeAutomationTakePublicationPlan {
    mixer: IdeMixerSavePlan,
    take_id: String,
    take_bytes: Vec<u8>,
}

/// Resolves an edited mapping against the capability record its publication
/// will commit.
///
/// The capability raise and the mapping land in one transaction, so the
/// preview must compile against the raised record. Compiling against the
/// current record would reject a binding the same transaction is about to make
/// valid.
struct RaisedCapabilityProject<'project> {
    project: hyper_project_loader::LoadedMappingProject<'project>,
    cv_inputs: u8,
    has_clock_input: bool,
    has_reset_input: bool,
}

impl<'project> RaisedCapabilityProject<'project> {
    fn new(
        project: hyper_project_loader::LoadedMappingProject<'project>,
        bindings: &[ControlBinding],
    ) -> Self {
        let required = RequiredControlCapabilities::of_bindings(bindings);
        Self {
            cv_inputs: project.cv_inputs().max(required.cv_inputs()),
            has_clock_input: project.has_clock_input() || required.has_clock_input(),
            has_reset_input: project.has_reset_input() || required.has_reset_input(),
            project,
        }
    }
}

impl ControlMappingProject for RaisedCapabilityProject<'_> {
    fn project_id(&self) -> &str {
        self.project.project_id()
    }

    fn cv_inputs(&self) -> u8 {
        self.cv_inputs
    }

    fn has_clock_input(&self) -> bool {
        self.has_clock_input
    }

    fn has_reset_input(&self) -> bool {
        self.has_reset_input
    }

    fn has_mixer(&self) -> bool {
        self.project.has_mixer()
    }

    fn contains_nested_entity(&self, id: &str) -> bool {
        self.project.contains_nested_entity(id)
    }

    fn contains_entity(&self, id: &str) -> bool {
        self.project.contains_entity(id)
    }

    fn contains_track(&self, id: &str) -> bool {
        self.project.contains_track(id)
    }

    fn contains_scene(&self, id: &str) -> bool {
        self.project.contains_scene(id)
    }

    fn contains_mixer_channel(&self, track_id: &str) -> bool {
        self.project.contains_mixer_channel(track_id)
    }

    fn contains_mixer_send(&self, send_id: &str) -> bool {
        self.project.contains_mixer_send(send_id)
    }

    fn is_solo_source_track(&self, track_id: &str) -> bool {
        self.project.is_solo_source_track(track_id)
    }
}

impl ControlMappingResolver for RaisedCapabilityProject<'_> {
    fn track_index(&self, track_id: &str) -> Option<usize> {
        self.project.track_index(track_id)
    }

    fn scene_index(&self, scene_id: &str) -> Option<usize> {
        self.project.scene_index(scene_id)
    }

    fn mixer_channel_index(&self, track_id: &str) -> Option<usize> {
        self.project.mixer_channel_index(track_id)
    }

    fn mixer_send_address(&self, send_id: &str) -> Option<MixerSendAddress> {
        self.project.mixer_send_address(send_id)
    }
}

#[derive(Debug)]
struct IdeResourceRestorePlan {
    mixer: IdeMixerSavePlan,
    resource_id: String,
    role: ResourceRole,
    resource_bytes: Vec<u8>,
    displaced_bytes: Option<Vec<u8>>,
}

#[derive(Debug)]
struct IdeDeviceChainHistoryRestorePlan {
    mixer: IdeMixerSavePlan,
    chain_id: String,
    publication: IdeNativeFilterPublication,
    displaced: IdeHistoryOperation,
}

#[derive(Debug)]
enum IdeAutomationTakeHistoryPublication {
    Addition(Vec<u8>),
    Removal,
}

#[derive(Debug)]
struct IdeAutomationTakeHistoryRestorePlan {
    mixer: IdeMixerSavePlan,
    take_id: String,
    publication: IdeAutomationTakeHistoryPublication,
    displaced: IdeHistoryOperation,
}

impl IdeAutomationTakeHistoryRestorePlan {
    fn displaced(&self) -> IdeHistoryOperation {
        self.displaced.clone()
    }

    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeResourceRestoreError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeResourceRestoreError::MixerPublication(Box::new(source)))?;
        let result = match &self.publication {
            IdeAutomationTakeHistoryPublication::Addition(bytes) => publish_automation_take(
                &self.mixer.project_path,
                AutomationTakePublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    take_id: &self.take_id,
                    take_bytes: bytes,
                },
            ),
            IdeAutomationTakeHistoryPublication::Removal => remove_automation_take(
                &self.mixer.project_path,
                AutomationTakeRemovalRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    take_id: &self.take_id,
                },
            ),
        };
        result.map_err(|source| IdeResourceRestoreError::Publication(Box::new(source)))
    }
}

impl IdeDeviceChainHistoryRestorePlan {
    fn displaced(&self) -> IdeHistoryOperation {
        self.displaced.clone()
    }

    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeResourceRestoreError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeResourceRestoreError::MixerPublication(Box::new(source)))?;
        let result = match &self.publication {
            IdeNativeFilterPublication::Addition(bytes) => publish_device_chain(
                &self.mixer.project_path,
                DeviceChainPublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    chain_id: &self.chain_id,
                    chain_bytes: bytes,
                },
            ),
            IdeNativeFilterPublication::Removal => remove_device_chain(
                &self.mixer.project_path,
                DeviceChainRemovalRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    chain_id: &self.chain_id,
                },
            ),
            IdeNativeFilterPublication::Replacement(_) => {
                unreachable!("device-chain structural history cannot replace a resource")
            }
        };
        result.map_err(|source| IdeResourceRestoreError::Publication(Box::new(source)))
    }
}

impl IdeResourceRestorePlan {
    fn displaced(&self) -> Option<IdeHistoryOperation> {
        self.displaced_bytes.as_ref().map(|bytes| {
            IdeHistoryOperation::ResourceReplacement {
                resource_id: self.resource_id.clone(),
                bytes: bytes.clone(),
            }
        })
    }

    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeResourceRestoreError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeResourceRestoreError::MixerPublication(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.resource_id,
                expected_role: self.role,
                resource_bytes: &self.resource_bytes,
            },
        )
        .map_err(|source| IdeResourceRestoreError::Publication(Box::new(source)))
    }
}

#[derive(Debug)]
struct IdeControlMappingEditPlan {
    mixer: IdeMixerSavePlan,
    mapping_bytes: Vec<u8>,
}

impl IdeControlMappingEditPlan {
    fn publish(&self) -> Result<u64, IdeControlMappingPublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeControlMappingPublicationError::Mixer(Box::new(source)))?;
        publish_control_mapping(
            &self.mixer.project_path,
            ControlMappingPublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                mapping_bytes: &self.mapping_bytes,
            },
        )
        .map(|outcome| outcome.project_revision())
        .map_err(|source| IdeControlMappingPublicationError::Mapping {
            mixer: Box::new(mixer),
            source: Box::new(MappingEditorPublicationError::Storage(source)),
        })
    }
}

#[derive(Debug)]
struct IdeAutomationTakeEditPlan {
    mixer: IdeMixerSavePlan,
    take_id: String,
    take_bytes: Vec<u8>,
}

#[derive(Debug)]
struct IdeAutomationTakeRemovalPlan {
    mixer: IdeMixerSavePlan,
    take_id: String,
    take_bytes: Vec<u8>,
}

#[derive(Debug)]
enum IdeNativeFilterPublication {
    Addition(Vec<u8>),
    Replacement(Vec<u8>),
    Removal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IdeHistoryOperation {
    ResourceReplacement {
        resource_id: String,
        bytes: Vec<u8>,
    },
    DeviceChainAddition {
        chain_id: String,
        bytes: Vec<u8>,
    },
    DeviceChainRemoval {
        chain_id: String,
        expected_bytes: Vec<u8>,
    },
    AutomationTakeAddition {
        take_id: String,
        bytes: Vec<u8>,
    },
    AutomationTakeRemoval {
        take_id: String,
        expected_bytes: Vec<u8>,
    },
}

#[derive(Debug)]
struct IdeNativeFilterEditPlan {
    mixer: IdeMixerSavePlan,
    chain_id: String,
    publication: IdeNativeFilterPublication,
    inverse: IdeHistoryOperation,
}

fn find_native_filter_chain<'a>(
    prepared: &'a PreparedProjectPlayback,
    chain_id: &str,
    device_instance_id: &str,
) -> Result<&'a DeviceChain, IdeNativeFilterEditError> {
    if chain_id.is_empty() || device_instance_id.is_empty() {
        return Err(IdeNativeFilterEditError::InvalidIdentity);
    }
    let chain = prepared
        .device_chains()
        .iter()
        .find(|chain| chain.id() == chain_id)
        .ok_or(IdeNativeFilterEditError::ChainUnavailable)?;
    if !chain
        .devices()
        .iter()
        .any(|device| device.id() == device_instance_id)
    {
        return Err(IdeNativeFilterEditError::InvalidIdentity);
    }
    Ok(chain)
}

fn native_filter_state(
    mode: NativeFilterMode,
    cutoff_millihertz: u32,
    resonance_millionths: u32,
) -> Result<NativeFilterState, IdeNativeFilterEditError> {
    NativeFilterState::try_new(mode, cutoff_millihertz, resonance_millionths)
        .map_err(IdeNativeFilterEditError::Device)
}

fn native_compressor_state(
    threshold_millidecibels: i32,
    ratio_millionths: u32,
    attack_microseconds: u32,
    release_microseconds: u32,
    makeup_millidecibels: i32,
) -> Result<NativeCompressorState, IdeNativeFilterEditError> {
    NativeCompressorState::try_new(
        threshold_millidecibels,
        ratio_millionths,
        attack_microseconds,
        release_microseconds,
        makeup_millidecibels,
    )
    .map_err(IdeNativeFilterEditError::Device)
}

fn serialize_native_filter_chain(
    chain: &DeviceChain,
) -> Result<Vec<u8>, IdeNativeFilterEditError> {
    chain
        .to_canonical_vec()
        .map_err(IdeNativeFilterEditError::Serialization)
}

impl IdeNativeFilterEditPlan {
    fn inverse(&self) -> IdeHistoryOperation {
        self.inverse.clone()
    }

    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeNativeFilterEditError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeNativeFilterEditError::MixerPublication(Box::new(source)))?;
        let result = match &self.publication {
            IdeNativeFilterPublication::Addition(bytes) => publish_device_chain(
                &self.mixer.project_path,
                DeviceChainPublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    chain_id: &self.chain_id,
                    chain_bytes: bytes,
                },
            ),
            IdeNativeFilterPublication::Replacement(bytes) => publish_project_resource(
                &self.mixer.project_path,
                ProjectResourcePublicationRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    resource_id: &self.chain_id,
                    expected_role: ResourceRole::DeviceChain,
                    resource_bytes: bytes,
                },
            ),
            IdeNativeFilterPublication::Removal => remove_device_chain(
                &self.mixer.project_path,
                DeviceChainRemovalRequest {
                    expected_project_sha256: *mixer.project_identity(),
                    chain_id: &self.chain_id,
                },
            ),
        };
        result.map_err(|source| IdeNativeFilterEditError::Publication {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeAutomationTakeEditPlan {
    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAutomationTakePublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAutomationTakePublicationError::Mixer(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.take_id,
                expected_role: ResourceRole::AutomationTake,
                resource_bytes: &self.take_bytes,
            },
        )
        .map_err(|source| IdeAutomationTakePublicationError::Take {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeAutomationTakeRemovalPlan {
    fn inverse(&self) -> IdeHistoryOperation {
        IdeHistoryOperation::AutomationTakeAddition {
            take_id: self.take_id.clone(),
            bytes: self.take_bytes.clone(),
        }
    }

    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAutomationTakePublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAutomationTakePublicationError::Mixer(Box::new(source)))?;
        remove_automation_take(
            &self.mixer.project_path,
            AutomationTakeRemovalRequest {
                expected_project_sha256: *mixer.project_identity(),
                take_id: &self.take_id,
            },
        )
        .map_err(|source| IdeAutomationTakePublicationError::Take {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeAutomationTakePublicationPlan {
    fn inverse(&self) -> IdeHistoryOperation {
        IdeHistoryOperation::AutomationTakeRemoval {
            take_id: self.take_id.clone(),
            expected_bytes: self.take_bytes.clone(),
        }
    }

    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAutomationTakePublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeAutomationTakePublicationError::Mixer(Box::new(source)))?;
        publish_automation_take(
            &self.mixer.project_path,
            AutomationTakePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                take_id: &self.take_id,
                take_bytes: &self.take_bytes,
            },
        )
        .map_err(|source| IdeAutomationTakePublicationError::Take {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

fn prepare_scene_edit_resource(
    prepared: &PreparedProjectPlayback,
    scene_id: &str,
    limits: LoaderLimits,
    serialize: impl FnOnce(
        &hyper_playback_json::LoadedPlaybackProject,
        &[u8],
        LoaderLimits,
    ) -> Result<Vec<u8>, IdeSceneEditError>,
) -> Result<(String, Vec<u8>), IdeSceneEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == scene_id)
        .ok_or(IdeSceneEditError::SceneResourceMissing)?;
    if descriptor.role() != ResourceRole::Scene {
        return Err(IdeSceneEditError::SceneResourceRoleMismatch);
    }
    let source = package
        .read_resource(scene_id)
        .map_err(IdeSceneEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let scene_bytes = serialize(project, &source, limits)?;
    let mut owned_scene_id = String::new();
    owned_scene_id
        .try_reserve_exact(scene_id.len())
        .map_err(|_| IdeSceneEditError::CapacityUnavailable)?;
    owned_scene_id.push_str(scene_id);
    Ok((owned_scene_id, scene_bytes))
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

fn prepare_audio_clip_reverse_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    reverse: bool,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipReverseEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipReverseEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipReverseEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipReverseEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_reverse(project, &source, clip_id, reverse, limits)
        .map_err(IdeAudioClipReverseEditError::Serialization)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipReverseEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

fn prepare_audio_clip_source_slip_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    source_start_frame: u64,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipSourceSlipEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipSourceSlipEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipSourceSlipEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipSourceSlipEditError::Storage)?;
    let (project, _, _, media) = prepared.session().parts();
    let mut assets = Vec::new();
    assets
        .try_reserve_exact(media.asset_count())
        .map_err(|_| IdeAudioClipSourceSlipEditError::CapacityUnavailable)?;
    assets.extend(media.media_asset_facts());
    let clip_bytes = serialize_audio_clip_source_start(
        project,
        &assets,
        &source,
        clip_id,
        source_start_frame,
        limits,
    )
    .map_err(IdeAudioClipSourceSlipEditError::Serialization)?;
    let native_start = usize::try_from(source_start_frame).map_err(|_| {
        IdeAudioClipSourceSlipEditError::NativeValidation(
            AudioClipSourceSlipValidationError::SourceRangeOverflow,
        )
    })?;
    prepared
        .session()
        .validate_audio_clip_source_start(clip_id, native_start)
        .map_err(IdeAudioClipSourceSlipEditError::NativeValidation)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipSourceSlipEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

fn prepare_audio_clip_source_trim_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    source_start_frame: u64,
    source_frame_count: u64,
    limits: LoaderLimits,
) -> Result<IdeAudioClipSourceTrimResource, IdeAudioClipSourceTrimEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipSourceTrimEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipSourceTrimEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipSourceTrimEditError::Storage)?;
    let (project, _, _, media) = prepared.session().parts();
    let mut assets = Vec::new();
    assets
        .try_reserve_exact(media.asset_count())
        .map_err(|_| IdeAudioClipSourceTrimEditError::CapacityUnavailable)?;
    assets.extend(media.media_asset_facts());
    let clip_bytes = serialize_audio_clip_source_view(
        project,
        &assets,
        &source,
        clip_id,
        source_start_frame,
        source_frame_count,
        limits,
    )
    .map_err(IdeAudioClipSourceTrimEditError::Serialization)?;
    let native_start = usize::try_from(source_start_frame).map_err(|_| {
        IdeAudioClipSourceTrimEditError::NativeValidation(
            AudioClipSourceSlipValidationError::SourceRangeOverflow,
        )
    })?;
    let native_count = usize::try_from(source_frame_count).map_err(|_| {
        IdeAudioClipSourceTrimEditError::NativeValidation(
            AudioClipSourceSlipValidationError::SourceRangeOverflow,
        )
    })?;
    prepared
        .session()
        .validate_audio_clip_source_view(clip_id, native_start, native_count)
        .map_err(IdeAudioClipSourceTrimEditError::NativeValidation)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipSourceTrimEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok(IdeAudioClipSourceTrimResource {
        clip_id: owned_clip_id,
        clip_bytes,
        prior_clip_bytes: source,
    })
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

fn prepare_audio_clip_fade_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    fade_in_ticks: u64,
    fade_out_ticks: u64,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipFadeEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipFadeEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipFadeEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipFadeEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_fades(
        project,
        &source,
        clip_id,
        fade_in_ticks,
        fade_out_ticks,
        limits,
    )
    .map_err(IdeAudioClipFadeEditError::Serialization)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipFadeEditError::CapacityUnavailable)?;
    owned_clip_id.push_str(clip_id);
    Ok((owned_clip_id, clip_bytes))
}

fn prepare_audio_clip_loop_resource(
    prepared: &PreparedProjectPlayback,
    clip_id: &str,
    enabled: bool,
    start_tick: u64,
    end_tick: u64,
    limits: LoaderLimits,
) -> Result<(String, Vec<u8>), IdeAudioClipLoopEditError> {
    let package = prepared.package();
    let descriptor = package
        .manifest()
        .resources()
        .iter()
        .find(|descriptor| descriptor.id() == clip_id)
        .ok_or(IdeAudioClipLoopEditError::ClipResourceMissing)?;
    if descriptor.role() != ResourceRole::Clip {
        return Err(IdeAudioClipLoopEditError::ClipResourceRoleMismatch);
    }
    let source = package
        .read_resource(clip_id)
        .map_err(IdeAudioClipLoopEditError::Storage)?;
    let (project, _, _, _) = prepared.session().parts();
    let clip_bytes = serialize_audio_clip_loop(
        project,
        source.as_slice(),
        clip_id,
        enabled,
        start_tick,
        end_tick,
        limits,
    )
    .map_err(IdeAudioClipLoopEditError::Serialization)?;
    let mut owned_clip_id = String::new();
    owned_clip_id
        .try_reserve_exact(clip_id.len())
        .map_err(|_| IdeAudioClipLoopEditError::CapacityUnavailable)?;
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

impl IdeAudioClipSourceTrimPlan {
    fn inverse(&self) -> IdeHistoryOperation {
        self.inverse.clone()
    }

    fn publish(
        &self,
    ) -> Result<ProjectResourcePublicationOutcome, IdeAudioClipEditPublicationError> {
        self.edit.publish()
    }
}

impl IdeSceneEditPlan {
    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeSceneEditPublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeSceneEditPublicationError::Mixer(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.scene_id,
                expected_role: ResourceRole::Scene,
                resource_bytes: &self.scene_bytes,
            },
        )
        .map_err(|source| IdeSceneEditPublicationError::Scene {
            mixer: Box::new(mixer),
            source: Box::new(source),
        })
    }
}

impl IdeScenePublicationPlan {
    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeScenePublicationError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeScenePublicationError::Mixer(Box::new(source)))?;
        publish_scene(
            &self.mixer.project_path,
            ScenePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                scene_id: &self.scene_id,
                scene_bytes: &self.scene_bytes,
            },
        )
        .map_err(|source| IdeScenePublicationError::Scene {
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

impl IdeTimelineEditPlan {
    fn publish(&self) -> Result<ProjectResourcePublicationOutcome, IdeTempoEditError> {
        let mixer = self
            .mixer
            .publish()
            .map_err(|source| IdeTempoEditError::MixerPublication(Box::new(source)))?;
        publish_project_resource(
            &self.mixer.project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: *mixer.project_identity(),
                resource_id: &self.timeline_id,
                expected_role: ResourceRole::Timeline,
                resource_bytes: &self.timeline_bytes,
            },
        )
        .map_err(|source| IdeTempoEditError::Publication(Box::new(source)))
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
    pub playback_session_generation: u64,
    pub project_path: &'a Path,
    pub project_id: &'a str,
    pub project_name: &'a str,
    pub sample_rate_hz: u32,
    pub playback_extent: IdePlaybackExtent,
    pub arrangement: Arc<IdePlaybackArrangement>,
    pub automation: Arc<IdePlaybackAutomation>,
    pub recording_metadata: &'a [hyper_recording_metadata::RecordingMetadataDocument],
    pub device_chains: &'a [DeviceChain],
    pub unused_sample_cleanup: IdeUnusedSampleCleanupSummary,
    pub scene_count: usize,
    pub track_count: usize,
    pub scene_ids: [&'a str; MAX_IDE_SCENES],
    pub scene_names: [&'a str; MAX_IDE_SCENES],
    pub scene_active: [bool; MAX_IDE_SCENES],
    pub scene_availability: [IdeSceneAvailability; MAX_IDE_SCENES],
    pub mixer_channel_count: usize,
    pub mixer_channel_ids: [&'a str; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_names: [&'a str; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_kinds: [IdeMixerChannelKind; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_active: [bool; MAX_IDE_MIXER_CHANNELS],
    pub mixer_channel_states: [Option<IdeMixerChannelState>; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_counts: [usize; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_ids:
        [[&'a str; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_target_names:
        [[&'a str; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_taps:
        [[IdeMixerSendTap; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS],
    pub mixer_send_states: [[Option<IdeMixerSendState>;
        hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT];
        MAX_IDE_MIXER_CHANNELS],
    pub mixer_main_state: Option<IdeMixerMainState>,
    pub control_bindings: &'a [ControlBinding],
    pub output_state: OutputStreamState,
    pub output: OutputStreamSnapshot,
    pub monitoring: Option<IdeMonitoringState>,
    pub preview: DesktopAudioPreviewSnapshot,
    pub recovery: PlaybackRecoverySnapshot,
}

fn unused_sample_cleanup_summary(
    prepared: &PreparedProjectPlayback,
) -> Result<IdeUnusedSampleCleanupSummary, IdeUnusedSampleCleanupError> {
    let mut assets = 0_usize;
    let mut bytes = 0_u64;
    for asset in prepared.unused_sample_assets() {
        assets = assets
            .checked_add(1)
            .ok_or(IdeUnusedSampleCleanupError::CapacityUnavailable)?;
        bytes = bytes
            .checked_add(asset.byte_length())
            .ok_or(IdeUnusedSampleCleanupError::CapacityUnavailable)?;
    }
    Ok(IdeUnusedSampleCleanupSummary {
        project_identity: *prepared.package().manifest_sha256(),
        project_revision: prepared.package().manifest().revision(),
        assets,
        bytes,
    })
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
    arrangement_scene_launches: Vec<(String, usize)>,
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
            arrangement_scene_launches: Vec::new(),
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

    /// Folds the audio thread's live parameter values onto the compiled graph.
    ///
    /// Live gain, pan, mute, solo, send, and main values are applied to the
    /// runtime and reach the project only through this fold, so anything that
    /// must reflect what is audible has to use it.
    fn live_mixer_base_state(
        mixer: &CompiledMixerGraph,
        live: &MixerParameterStateSnapshot,
    ) -> Result<MixerBaseState, IdeProjectSaveError> {
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
        Ok(MixerBaseState {
            channels,
            main_gain: live.main_gain(),
            main_muted: live.main_muted(),
        })
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
        let state = Self::live_mixer_base_state(mixer, &live)?;
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

    fn import_audio_files(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        paths: &[PathBuf],
        track_id: &str,
    ) -> Result<IdeAudioImportBatchOutcome, IdeAudioImportBatchFailure> {
        let total = paths.len();
        let failure = |completed: usize, path: PathBuf, source| IdeAudioImportBatchFailure {
            completed,
            total,
            path,
            source: Box::new(source),
        };
        let Some(first_path) = paths.first() else {
            return Err(failure(
                0,
                PathBuf::new(),
                IdeAudioImportError::EmptySelection,
            ));
        };
        if track_id.is_empty() {
            return Err(failure(
                0,
                first_path.clone(),
                IdeAudioImportError::InvalidDestinationTrack,
            ));
        }
        let mixer = Self::prepare_mixer_save(application, recovery).map_err(|source| {
            failure(
                0,
                first_path.clone(),
                IdeAudioImportError::MixerSave(Box::new(source)),
            )
        })?;
        let project_path = mixer.project_path.clone();
        mixer.publish().map_err(|source| {
            failure(
                0,
                first_path.clone(),
                IdeAudioImportError::MixerSave(Box::new(IdeProjectSaveError::Publication(
                    Box::new(source),
                ))),
            )
        })?;
        let limits = application.config.project_limits();
        let mut prepared = hyper_project_loader::prepare_project_playback(&project_path, limits)
            .map_err(|source| {
                failure(
                    0,
                    first_path.clone(),
                    IdeAudioImportError::ProjectReload(source),
                )
            })?;
        let mut clip_ids = Vec::new();
        clip_ids.try_reserve_exact(total).map_err(|_| {
            failure(
                0,
                first_path.clone(),
                IdeAudioImportError::CapacityUnavailable,
            )
        })?;
        let mut final_revision = prepared.package().manifest().revision();
        for (index, path) in paths.iter().enumerate() {
            let plan = Self::prepare_imported_audio_clip(application, &prepared, path, track_id)
                .map_err(|source| failure(index, path.clone(), source))?;
            let outcome = plan
                .publish()
                .map_err(|source| failure(index, path.clone(), source))?;
            final_revision = outcome.project_revision();
            clip_ids.push(outcome.clip_id().to_owned());
            if index + 1 < total {
                prepared = hyper_project_loader::prepare_project_playback(&project_path, limits)
                    .map_err(|source| {
                        failure(
                            index + 1,
                            paths[index + 1].clone(),
                            IdeAudioImportError::ProjectReload(source),
                        )
                    })?;
            }
        }
        Ok(IdeAudioImportBatchOutcome {
            imported: total,
            final_revision,
            clip_ids,
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the import plan keeps bounded decode, destination authority, exact clip construction, and total package capacity in one auditable prepublication boundary"
    )]
    fn prepare_imported_audio_clip(
        application: &DesktopIdeApplication,
        prepared: &PreparedProjectPlayback,
        path: &Path,
        track_id: &str,
    ) -> Result<IdeImportedAudioClipPlan, IdeAudioImportError> {
        let container = audio_import_container(path)?;
        let limits = application.config.project_limits();
        let source = read_stable_absolute_file(path, limits.audio.max_file_bytes())
            .map_err(|error| IdeAudioImportError::Source(error.into()))?;
        let (source_bytes, source_evidence) = source.into_parts();
        let encoded_source_bytes = source_bytes.len();
        let decoded = decode_bytes_at_source_rate(source_bytes, container, limits.audio)
            .map_err(IdeAudioImportError::Source)?;
        let media = prepared.session().parts().3;
        let maximum_output_frames = u64::try_from(
            limits.session.media_cache.max_frames_per_asset,
        )
        .map_err(|_| IdeAudioImportError::CapacityUnavailable)?;
        let memory = plan_audio_import_memory(
            AudioImportMemoryInput {
                retained_project_bytes: media.decoded_sample_bytes(),
                encoded_source_bytes,
                input_frames: decoded.frame_count(),
                input_sample_rate_hz: decoded.sample_rate_hz(),
                target_sample_rate_hz: prepared.package().manifest().sample_rate_hz(),
                channels: decoded.channels(),
                maximum_output_frames,
            },
            DESKTOP_MAX_AUDIO_IMPORT_WORKING_BYTES,
        )?;
        let converted = resample_audio(
            decoded,
            prepared.package().manifest().sample_rate_hz(),
            maximum_output_frames,
            memory.resampling_working,
        )
        .map_err(IdeAudioImportError::Source)?;
        let decoded_bytes = converted
            .frame_count()
            .checked_mul(converted.channels())
            .and_then(|samples| samples.checked_mul(size_of::<f32>()))
            .ok_or(IdeAudioImportError::CapacityUnavailable)?;
        if media.asset_count() >= limits.session.media_cache.max_assets
            || decoded_bytes > limits.audio.max_decoded_sample_bytes()
            || media
                .decoded_sample_bytes()
                .checked_add(decoded_bytes)
                .is_none_or(|bytes| bytes > limits.session.media_cache.max_decoded_sample_bytes)
        {
            return Err(IdeAudioImportError::CapacityUnavailable);
        }
        let sample_bytes = encode_float_wav(&converted).map_err(IdeAudioImportError::Source)?;
        if u64::try_from(sample_bytes.len())
            .map_or(true, |bytes| bytes > limits.audio.max_file_bytes())
        {
            return Err(IdeAudioImportError::Source(DecodeError::FileTooLarge {
                actual: u64::try_from(sample_bytes.len()).unwrap_or(u64::MAX),
                maximum: limits.audio.max_file_bytes(),
            }));
        }
        let clip_name = audio_import_clip_name(path)?;
        let package = prepared.package();
        let track_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == track_id)
            .ok_or(IdeAudioImportError::TrackResourceMissing)?;
        if track_descriptor.role() != ResourceRole::Track {
            return Err(IdeAudioImportError::TrackResourceRoleMismatch);
        }
        let source_track = package
            .read_resource(track_id)
            .map_err(IdeAudioImportError::Storage)?;
        let asset_id = uuid::Uuid::new_v4().to_string();
        let clip_id = uuid::Uuid::new_v4().to_string();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count() + 1)
            .map_err(|_| IdeAudioImportError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        assets.push(MediaAssetFact {
            id: &asset_id,
            sample_rate_hz: converted.sample_rate_hz(),
            frame_count: converted.frame_count(),
        });
        let documents = serialize_recording_audio_clip_addition(
            prepared.session().parts().0,
            &assets,
            &source_track,
            track_id,
            &asset_id,
            0,
            u64::try_from(converted.frame_count())
                .map_err(|_| IdeAudioImportError::CapacityUnavailable)?,
            &clip_id,
            &clip_name,
            limits.session.documents,
        )
        .map_err(IdeAudioImportError::Serialization)?;
        let (track_bytes, clip_bytes, _) = documents.into_parts();
        let current_resource_bytes = package
            .manifest()
            .resources()
            .iter()
            .map(hyper_project_model::ResourceDescriptor::byte_length)
            .chain(
                package
                    .manifest()
                    .assets()
                    .iter()
                    .map(hyper_project_model::AssetDescriptor::byte_length),
            )
            .try_fold(0_u64, u64::checked_add)
            .ok_or(IdeAudioImportError::CapacityUnavailable)?;
        let candidate_bytes = current_resource_bytes
            .checked_sub(track_descriptor.byte_length())
            .and_then(|bytes| bytes.checked_add(track_bytes.len() as u64))
            .and_then(|bytes| bytes.checked_add(clip_bytes.len() as u64))
            .and_then(|bytes| bytes.checked_add(sample_bytes.len() as u64))
            .ok_or(IdeAudioImportError::CapacityUnavailable)?;
        if candidate_bytes > limits.max_total_resource_bytes as u64 {
            return Err(IdeAudioImportError::CapacityUnavailable);
        }
        Ok(IdeImportedAudioClipPlan {
            source_path: path.to_path_buf(),
            source_maximum_bytes: limits.audio.max_file_bytes(),
            source_evidence,
            project_path: package.root().to_path_buf(),
            expected_project_sha256: *package.manifest_sha256(),
            clip_id,
            clip_bytes,
            track_id: track_id.to_owned(),
            track_bytes,
            asset_id,
            sample_bytes,
        })
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

    fn prepare_audio_clip_removal(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
    ) -> Result<IdeAudioClipRemovalPlan, IdeAudioClipRemovalError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipRemovalError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipRemovalError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_audio_clip_removal_from_project(
            application,
            mixer,
            prepared,
            clip_id,
        )
    }

    fn prepare_unused_sample_cleanup(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        confirmation: IdeUnusedSampleCleanupSummary,
    ) -> Result<IdeUnusedSampleCleanupPlan, IdeUnusedSampleCleanupError> {
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeUnusedSampleCleanupError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let current = unused_sample_cleanup_summary(prepared)?;
        if current.is_empty() {
            return Err(IdeUnusedSampleCleanupError::Empty);
        }
        if current != confirmation {
            return Err(IdeUnusedSampleCleanupError::StaleConfirmation);
        }
        let mut asset_ids = Vec::new();
        asset_ids
            .try_reserve_exact(current.assets())
            .map_err(|_| IdeUnusedSampleCleanupError::CapacityUnavailable)?;
        for asset in prepared.unused_sample_assets() {
            asset_ids.push(asset.id().to_owned());
        }
        asset_ids.sort_unstable();
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeUnusedSampleCleanupError::MixerSave(Box::new(source)))?;
        Ok(IdeUnusedSampleCleanupPlan { mixer, asset_ids })
    }

    fn prepare_audio_clip_removal_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        clip_id: &str,
    ) -> Result<IdeAudioClipRemovalPlan, IdeAudioClipRemovalError> {
        let package = prepared.package();
        let clip_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == clip_id)
            .ok_or(IdeAudioClipRemovalError::ClipResourceMissing)?;
        if clip_descriptor.role() != ResourceRole::Clip {
            return Err(IdeAudioClipRemovalError::ClipResourceRoleMismatch);
        }
        let (project, _, _, _) = prepared.session().parts();
        let clip_index = (0..project.clip_count())
            .find(|index| project.clip_id_at(*index) == Some(clip_id))
            .ok_or(IdeAudioClipRemovalError::ClipResourceMissing)?;
        let track_id = project
            .clip_track_id_at(clip_index)
            .ok_or(IdeAudioClipRemovalError::TrackResourceMissing)?;
        let track_descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == track_id)
            .ok_or(IdeAudioClipRemovalError::TrackResourceMissing)?;
        if track_descriptor.role() != ResourceRole::Track {
            return Err(IdeAudioClipRemovalError::TrackResourceRoleMismatch);
        }
        let source_track = package
            .read_resource(track_id)
            .map_err(IdeAudioClipRemovalError::Storage)?;
        let documents = serialize_audio_clip_removal(
            project,
            &source_track,
            clip_id,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeAudioClipRemovalError::Serialization)?;
        let (track_bytes, owned_track_id, _) = documents.into_parts();
        let mut owned_clip_id = String::new();
        owned_clip_id
            .try_reserve_exact(clip_id.len())
            .map_err(|_| IdeAudioClipRemovalError::CapacityUnavailable)?;
        owned_clip_id.push_str(clip_id);
        Ok(IdeAudioClipRemovalPlan {
            mixer,
            clip_id: owned_clip_id,
            track_id: owned_track_id,
            track_bytes,
        })
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
            created_placement_id: Some(placement_id),
        })
    }

    fn prepare_arrangement_scene_launch_addition(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_id: &str,
        tick: u64,
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
        let launch_id = uuid::Uuid::new_v4().to_string();
        let arrangement_bytes = serialize_arrangement_scene_launch_addition(
            project,
            &assets,
            source.as_deref(),
            &arrangement_id,
            &launch_id,
            scene_id,
            tick,
            application.config.project_limits.session.documents,
        )
        .map_err(IdeArrangementPlacementEditError::Serialization)?;
        Ok(IdeArrangementPlacementEditPlan {
            mixer,
            arrangement_id,
            arrangement_bytes,
            addition,
            created_placement_id: None,
        })
    }

    fn prepare_arrangement_scene_launch_move(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        launch_id: &str,
        tick: u64,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_scene_launch_edit(
            application,
            recovery,
            |project, assets, source, limits| {
                serialize_arrangement_scene_launch_move(
                    project, assets, source, launch_id, tick, limits,
                )
            },
        )
    }

    /// Converts the marks the user set in frames into the ticks the arrangement
    /// stores, using the timeline the project was compiled with rather than an
    /// approximation made in the UI.
    fn prepare_arrangement_loop_range(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        range: Option<IdeLoopRangeFrames>,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        let prepared = recovery
            .prepared_project()
            .ok_or(IdeArrangementPlacementEditError::ArrangementResourceMissing)?;
        let ticks = Self::loop_range_ticks(prepared, range)?;
        Self::prepare_arrangement_scene_launch_edit(
            application,
            recovery,
            |project, assets, source, limits| {
                serialize_arrangement_loop_range(project, assets, source, ticks, limits)
            },
        )
    }

    /// Converts the frames a user marked into the ticks the arrangement stores,
    /// through the project's own timeline. Marks closer together than one tick
    /// collapse, and a collapsed range is refused rather than written.
    fn loop_range_ticks(
        prepared: &PreparedProjectPlayback,
        range: Option<IdeLoopRangeFrames>,
    ) -> Result<Option<(u64, u64, bool)>, IdeArrangementPlacementEditError> {
        let Some(frames) = range else {
            return Ok(None);
        };
        let (project, _, _, media) = prepared.session().parts();
        let facts: Vec<_> = media.media_asset_facts().collect();
        let graph = project
            .compile_playback(
                &facts,
                hyper_playback_compiler::graph::GraphLimits::VERSION_1,
                hyper_playback_compiler::scene::SceneLimits::VERSION_1,
            )
            .map_err(|_| {
                IdeArrangementPlacementEditError::LoopMark(LoopMarkFailure::TimelineUnavailable)
            })?;
        let authority = graph.scene_authority();
        let start_tick = authority.frame_to_tick(frames.start_frame).map_err(|_| {
            IdeArrangementPlacementEditError::LoopMark(LoopMarkFailure::MarkOutOfRange)
        })?;
        let end_tick = authority.frame_to_tick(frames.end_frame).map_err(|_| {
            IdeArrangementPlacementEditError::LoopMark(LoopMarkFailure::MarkOutOfRange)
        })?;
        if end_tick <= start_tick {
            return Err(IdeArrangementPlacementEditError::LoopMark(
                LoopMarkFailure::MarksCollapse,
            ));
        }
        Ok(Some((start_tick, end_tick, frames.enabled)))
    }

    fn prepare_arrangement_scene_launch_scene_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        launch_id: &str,
        scene_id: &str,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_scene_launch_edit(
            application,
            recovery,
            |project, assets, source, limits| {
                serialize_arrangement_scene_launch_scene_edit(
                    project, assets, source, launch_id, scene_id, limits,
                )
            },
        )
    }

    fn prepare_arrangement_scene_launch_removal(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        launch_id: &str,
    ) -> Result<IdeArrangementPlacementEditPlan, IdeArrangementPlacementEditError> {
        Self::prepare_arrangement_scene_launch_edit(
            application,
            recovery,
            |project, assets, source, limits| {
                serialize_arrangement_scene_launch_removal(
                    project, assets, source, launch_id, limits,
                )
            },
        )
    }

    /// Rewrites the tempo at the origin. The edited timeline is validated by
    /// reloading and recompiling inside the serializer before anything durable
    /// happens, exactly as every other resource edit is.
    fn prepare_timeline_tempo(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        microseconds_per_quarter: u32,
    ) -> Result<IdeTimelineEditPlan, IdeTempoEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeTempoEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeTempoEditError::MixerSave(Box::new(IdeProjectSaveError::ProjectSnapshotUnavailable))
        })?;
        Self::prepare_timeline_tempo_from_project(
            application,
            mixer,
            prepared,
            microseconds_per_quarter,
        )
    }

    fn prepare_timeline_meter(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        numerator: u16,
        denominator: u16,
    ) -> Result<IdeTimelineEditPlan, IdeTempoEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeTempoEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeTempoEditError::MixerSave(Box::new(IdeProjectSaveError::ProjectSnapshotUnavailable))
        })?;
        Self::prepare_timeline_edit_from_project(
            application,
            mixer,
            prepared,
            |project, facts, source, limits| {
                serialize_timeline_meter(project, facts, source, numerator, denominator, limits)
            },
        )
    }

    fn prepare_timeline_tempo_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        microseconds_per_quarter: u32,
    ) -> Result<IdeTimelineEditPlan, IdeTempoEditError> {
        Self::prepare_timeline_edit_from_project(
            application,
            mixer,
            prepared,
            |project, facts, source, limits| {
                serialize_timeline_tempo(project, facts, source, microseconds_per_quarter, limits)
            },
        )
    }

    fn prepare_timeline_edit_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        serialize: impl FnOnce(
            &hyper_playback_json::LoadedPlaybackProject,
            &[hyper_playback_json::MediaAssetFact<'_>],
            &[u8],
            LoaderLimits,
        ) -> Result<Vec<u8>, TempoSerializationError>,
    ) -> Result<IdeTimelineEditPlan, IdeTempoEditError> {
        let package = prepared.package();
        let mut descriptors = package
            .manifest()
            .resources()
            .iter()
            .filter(|descriptor| descriptor.role() == ResourceRole::Timeline);
        let descriptor = descriptors
            .next()
            .ok_or(IdeTempoEditError::TimelineResourceMissing)?;
        if descriptors.next().is_some() {
            return Err(IdeTempoEditError::TimelineResourceAmbiguous);
        }
        let source = package
            .read_resource(descriptor.id())
            .map_err(IdeTempoEditError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let facts: Vec<_> = media.media_asset_facts().collect();
        let timeline_bytes = serialize(
            project,
            &facts,
            &source,
            application.config.project_limits.session.documents,
        )
        .map_err(|source| IdeTempoEditError::Serialization(Box::new(source)))?;
        let mut timeline_id = String::new();
        timeline_id
            .try_reserve_exact(descriptor.id().len())
            .map_err(|_| IdeTempoEditError::CapacityUnavailable)?;
        timeline_id.push_str(descriptor.id());
        Ok(IdeTimelineEditPlan {
            mixer,
            timeline_id,
            timeline_bytes,
        })
    }

    fn prepare_arrangement_scene_launch_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        serialize: impl FnOnce(
            &hyper_playback_json::LoadedPlaybackProject,
            &[hyper_playback_json::MediaAssetFact<'_>],
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
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let arrangement_bytes = serialize(
            project,
            &assets,
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
            created_placement_id: None,
        })
    }

    fn prepare_arrangement_placement_duplication(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        source_placement_id: &str,
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
        let descriptor = descriptors
            .next()
            .ok_or(IdeArrangementPlacementEditError::ArrangementResourceMissing)?;
        if descriptors.next().is_some() {
            return Err(IdeArrangementPlacementEditError::ArrangementResourceAmbiguous);
        }
        let source = package
            .read_resource(descriptor.id())
            .map_err(IdeArrangementPlacementEditError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let placement_id = uuid::Uuid::new_v4().to_string();
        let arrangement_bytes = serialize_arrangement_placement_duplication(
            project,
            &assets,
            &source,
            source_placement_id,
            &placement_id,
            start_tick,
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
            created_placement_id: Some(placement_id),
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

    fn prepare_arrangement_placement_split(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        source_placement_id: &str,
        split_tick: u64,
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
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeArrangementPlacementEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let right_placement_id = uuid::Uuid::new_v4().to_string();
        let arrangement_bytes = serialize_arrangement_placement_split(
            project,
            &assets,
            &source,
            source_placement_id,
            &right_placement_id,
            split_tick,
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
            created_placement_id: Some(right_placement_id),
        })
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
            created_placement_id: None,
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

    fn prepare_scene_creation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_name: &str,
        quantization: IdeSceneLaunchQuantization,
    ) -> Result<IdeScenePublicationPlan, IdeSceneCreationError> {
        let name_length = scene_name.chars().count();
        if scene_name.trim() != scene_name || !(1..=120).contains(&name_length) {
            return Err(IdeSceneCreationError::InvalidName);
        }
        if matches!(
            quantization,
            IdeSceneLaunchQuantization::Grid { quantum_ticks: 0 }
        ) {
            return Err(IdeSceneCreationError::InvalidQuantization);
        }
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeSceneCreationError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeSceneCreationError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeSceneCreationError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let scene_id = uuid::Uuid::new_v4().to_string();
        let scene_bytes = serialize_scene_addition(
            project,
            &assets,
            &scene_id,
            scene_name,
            quantization.into(),
            application.config.project_limits.session.documents,
        )
        .map_err(IdeSceneCreationError::Serialization)?;
        Ok(IdeScenePublicationPlan {
            mixer,
            scene_id,
            scene_bytes,
        })
    }

    /// Renders the open project to a WAV beside the requested destination.
    ///
    /// This reads the prepared project and writes outside it, so it takes no
    /// mixer fence and does not close or reopen playback.
    ///
    /// # Errors
    ///
    /// Returns a classified rejection when no project is open, the destination
    /// has no parent directory, or the render, publication, or measurement
    /// fails.
    pub fn export_project(
        &self,
        wav_path: &Path,
        block_frames: usize,
    ) -> Result<OfflineExportReport, IdeProjectExportError> {
        let prepared = match &self.session {
            IdePlaybackSessionOwner::Native(session) => session.prepared_project(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => None,
        }
        .ok_or(IdeProjectExportError::ProjectSnapshotUnavailable)?;
        let journal_path =
            export_journal_path(wav_path).ok_or(IdeProjectExportError::DestinationUnavailable)?;
        let live_mixer = self.live_compiled_mixer(prepared)?;
        let report = export_prepared_project_wav_with_mixer(
            prepared,
            OfflineExportSpec { block_frames },
            live_mixer.as_ref(),
            &journal_path,
            wav_path,
        )
        .map_err(|source| IdeProjectExportError::Export(Box::new(source)))?;
        Ok(report)
    }

    /// Compiles the graph the audio thread is actually rendering with.
    ///
    /// Returns `None` only when no live parameter state exists yet, in which
    /// case the prepared graph already is the audible one.
    fn live_compiled_mixer(
        &self,
        prepared: &PreparedProjectPlayback,
    ) -> Result<Option<CompiledMixerGraph>, IdeProjectExportError> {
        let Some(live) = (match &self.session {
            IdePlaybackSessionOwner::Native(session) => session
                .recovery_checkpoint()
                .snapshot()
                .mixer_parameter_state(),
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(_) => None,
        }) else {
            return Ok(None);
        };
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .ok_or(IdeProjectExportError::LiveMixerUnavailable)?;
        let source = package
            .read_resource(descriptor.id())
            .map_err(|_| IdeProjectExportError::LiveMixerUnavailable)?;
        let (project, mixer, _, media) = prepared.session().parts();
        let state = Self::live_mixer_base_state(mixer, &live)
            .map_err(|_| IdeProjectExportError::LiveMixerUnavailable)?;
        let bytes = serialize_mixer_base_state(
            &source,
            package.manifest().project_id(),
            descriptor.id(),
            &state,
            self.application.config.project_limits.session.documents,
        )
        .map_err(|_| IdeProjectExportError::LiveMixerUnavailable)?;
        let assets: Vec<_> = media.media_asset_facts().collect();
        project
            .load_mixer(
                &bytes,
                &assets,
                self.application.config.project_limits.session.documents,
            )
            .map_err(|_| IdeProjectExportError::LiveMixerUnavailable)?
            .compile_main_graph()
            .map(Some)
            .map_err(|_| IdeProjectExportError::LiveMixerUnavailable)
    }

    /// Prepares republication of one resource's earlier bytes.
    ///
    /// Other resources may have changed since the bytes were captured, so the
    /// complete candidate project is reloaded and recompiled before anything
    /// is written. A restore that would produce an unopenable project is
    /// refused with the entry intact.
    fn prepare_resource_restore(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        resource_id: &str,
        resource_bytes: &[u8],
    ) -> Result<IdeResourceRestorePlan, IdeResourceRestoreError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeResourceRestoreError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeResourceRestoreError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_resource_restore_from_project(
            application,
            mixer,
            prepared,
            resource_id,
            resource_bytes,
        )
    }

    fn prepare_device_chain_history_restore(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        operation: &IdeHistoryOperation,
    ) -> Result<IdeDeviceChainHistoryRestorePlan, IdeResourceRestoreError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeResourceRestoreError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeResourceRestoreError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let invalid = |message: &'static str| {
            IdeResourceRestoreError::CandidateInvalid(Box::new(std::io::Error::other(message)))
        };
        match operation {
            IdeHistoryOperation::DeviceChainAddition { chain_id, bytes } => {
                let chain = DeviceChain::parse(bytes)
                    .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
                let canonical = chain
                    .to_canonical_vec()
                    .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
                if canonical != *bytes
                    || chain.id() != chain_id
                    || chain.project_id() != prepared.package().manifest().project_id()
                {
                    return Err(invalid("device-chain undo bytes do not match their authority"));
                }
                let (_, compiled_mixer, _, _) = prepared.session().parts();
                if !compiled_mixer
                    .slots()
                    .iter()
                    .any(|slot| slot.track_id() == chain.track_id())
                    || prepared
                        .device_chains()
                        .iter()
                        .any(|active| active.id() == chain_id || active.track_id() == chain.track_id())
                {
                    return Err(invalid(
                        "device-chain undo addition conflicts with the active project",
                    ));
                }
                Ok(IdeDeviceChainHistoryRestorePlan {
                    mixer,
                    chain_id: chain_id.clone(),
                    publication: IdeNativeFilterPublication::Addition(bytes.clone()),
                    displaced: IdeHistoryOperation::DeviceChainRemoval {
                        chain_id: chain_id.clone(),
                        expected_bytes: bytes.clone(),
                    },
                })
            }
            IdeHistoryOperation::DeviceChainRemoval {
                chain_id,
                expected_bytes,
            } => {
                let active = prepared
                    .device_chains()
                    .iter()
                    .find(|chain| chain.id() == chain_id)
                    .ok_or(IdeResourceRestoreError::ResourceMissing)?;
                let active_bytes = active
                    .to_canonical_vec()
                    .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
                if active_bytes != *expected_bytes {
                    return Err(invalid(
                        "device-chain undo removal no longer matches the active resource",
                    ));
                }
                Ok(IdeDeviceChainHistoryRestorePlan {
                    mixer,
                    chain_id: chain_id.clone(),
                    publication: IdeNativeFilterPublication::Removal,
                    displaced: IdeHistoryOperation::DeviceChainAddition {
                        chain_id: chain_id.clone(),
                        bytes: active_bytes,
                    },
                })
            }
            IdeHistoryOperation::ResourceReplacement { .. }
            | IdeHistoryOperation::AutomationTakeAddition { .. }
            | IdeHistoryOperation::AutomationTakeRemoval { .. } => Err(invalid(
                "non-device-chain operation was sent to structural device-chain undo",
            )),
        }
    }

    fn prepare_automation_take_history_restore(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        operation: &IdeHistoryOperation,
    ) -> Result<IdeAutomationTakeHistoryRestorePlan, IdeResourceRestoreError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeResourceRestoreError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeResourceRestoreError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_automation_take_history_restore_from_project(
            application,
            mixer,
            prepared,
            operation,
        )
    }

    fn prepare_automation_take_history_restore_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        operation: &IdeHistoryOperation,
    ) -> Result<IdeAutomationTakeHistoryRestorePlan, IdeResourceRestoreError> {
        let invalid = |message: &'static str| {
            IdeResourceRestoreError::CandidateInvalid(Box::new(std::io::Error::other(message)))
        };
        match operation {
            IdeHistoryOperation::AutomationTakeAddition { take_id, bytes } => {
                if take_id.is_empty()
                    || prepared
                        .package()
                        .manifest()
                        .resources()
                        .iter()
                        .any(|descriptor| descriptor.id() == take_id)
                {
                    return Err(invalid(
                        "automation-take undo addition conflicts with the active project",
                    ));
                }
                Self::validate_automation_take_addition_candidate(
                    application,
                    &mixer,
                    prepared,
                    take_id,
                    bytes,
                )?;
                Ok(IdeAutomationTakeHistoryRestorePlan {
                    mixer,
                    take_id: take_id.clone(),
                    publication: IdeAutomationTakeHistoryPublication::Addition(bytes.clone()),
                    displaced: IdeHistoryOperation::AutomationTakeRemoval {
                        take_id: take_id.clone(),
                        expected_bytes: bytes.clone(),
                    },
                })
            }
            IdeHistoryOperation::AutomationTakeRemoval {
                take_id,
                expected_bytes,
            } => {
                let package = prepared.package();
                let descriptor = package
                    .manifest()
                    .resources()
                    .iter()
                    .find(|descriptor| {
                        descriptor.id() == take_id
                            && descriptor.role() == ResourceRole::AutomationTake
                    })
                    .ok_or(IdeResourceRestoreError::ResourceMissing)?;
                let active_bytes = package
                    .read_resource(descriptor.id())
                    .map_err(IdeResourceRestoreError::Storage)?;
                if active_bytes != *expected_bytes {
                    return Err(invalid(
                        "automation-take undo removal no longer matches the active resource",
                    ));
                }
                Ok(IdeAutomationTakeHistoryRestorePlan {
                    mixer,
                    take_id: take_id.clone(),
                    publication: IdeAutomationTakeHistoryPublication::Removal,
                    displaced: IdeHistoryOperation::AutomationTakeAddition {
                        take_id: take_id.clone(),
                        bytes: active_bytes,
                    },
                })
            }
            IdeHistoryOperation::ResourceReplacement { .. }
            | IdeHistoryOperation::DeviceChainAddition { .. }
            | IdeHistoryOperation::DeviceChainRemoval { .. } => Err(invalid(
                "non-automation operation was sent to structural automation undo",
            )),
        }
    }

    fn validate_automation_take_addition_candidate(
        application: &DesktopIdeApplication,
        mixer: &IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        take_id: &str,
        candidate: &[u8],
    ) -> Result<(), IdeResourceRestoreError> {
        let package = prepared.package();
        let (project, _, _, media) = prepared.session().parts();
        let limits = application.config.project_limits.session.documents;
        let facts: Vec<_> = media.media_asset_facts().collect();
        let loaded_mixer = project
            .load_mixer(&mixer.resource_bytes, &facts, limits)
            .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
        let mut takes = Vec::new();
        for descriptor in package.manifest().resources() {
            if descriptor.role() == ResourceRole::AutomationTake {
                takes
                    .try_reserve(1)
                    .map_err(|_| IdeResourceRestoreError::CapacityUnavailable)?;
                takes.push(
                    package
                        .read_resource(descriptor.id())
                        .map_err(IdeResourceRestoreError::Storage)?,
                );
            }
        }
        takes
            .try_reserve(1)
            .map_err(|_| IdeResourceRestoreError::CapacityUnavailable)?;
        takes.push(candidate.to_vec());
        let references: Vec<_> = takes.iter().map(Vec::as_slice).collect();
        let automation = project
            .load_automation_takes(&loaded_mixer, &facts, &references, limits)
            .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
        if !automation.takes().iter().any(|take| take.id() == take_id) {
            return Err(IdeResourceRestoreError::CandidateInvalid(Box::new(
                std::io::Error::other("automation-take undo bytes do not match their authority"),
            )));
        }
        let compiled_mixer = loaded_mixer
            .compile_main_graph()
            .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
        project
            .compile_main_automation(&compiled_mixer, &automation)
            .map_err(|source| IdeResourceRestoreError::CandidateInvalid(Box::new(source)))?;
        Ok(())
    }

    fn prepare_resource_restore_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        resource_id: &str,
        resource_bytes: &[u8],
    ) -> Result<IdeResourceRestorePlan, IdeResourceRestoreError> {
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == resource_id)
            .ok_or(IdeResourceRestoreError::ResourceMissing)?;
        let role = descriptor.role();
        let active_bytes = package
            .read_resource(resource_id)
            .map_err(IdeResourceRestoreError::Storage)?;
        Self::validate_restore_candidate(application, prepared, resource_id, resource_bytes)?;
        let mut owned_id = String::new();
        owned_id
            .try_reserve_exact(resource_id.len())
            .map_err(|_| IdeResourceRestoreError::CapacityUnavailable)?;
        owned_id.push_str(resource_id);
        let mut owned_bytes = Vec::new();
        owned_bytes
            .try_reserve_exact(resource_bytes.len())
            .map_err(|_| IdeResourceRestoreError::CapacityUnavailable)?;
        owned_bytes.extend_from_slice(resource_bytes);
        let displaced_bytes = (active_bytes != resource_bytes).then_some(active_bytes);
        Ok(IdeResourceRestorePlan {
            mixer,
            resource_id: owned_id,
            role,
            resource_bytes: owned_bytes,
            displaced_bytes,
        })
    }

    /// Reloads and recompiles the whole project with `resource_bytes`
    /// substituted, exactly as the serializers validate their own candidates.
    fn validate_restore_candidate(
        application: &DesktopIdeApplication,
        prepared: &PreparedProjectPlayback,
        resource_id: &str,
        resource_bytes: &[u8],
    ) -> Result<(), IdeResourceRestoreError> {
        let package = prepared.package();
        let limits = application.config.project_limits.session.documents;
        let mut timeline = Vec::new();
        let mut tracks = Vec::new();
        let mut clips = Vec::new();
        let mut scenes = Vec::new();
        let mut arrangement = None;
        let mut mixer_bytes = Vec::new();
        let mut takes = Vec::new();
        for descriptor in package.manifest().resources() {
            let bytes = if descriptor.id() == resource_id {
                resource_bytes.to_vec()
            } else {
                package
                    .read_resource(descriptor.id())
                    .map_err(IdeResourceRestoreError::Storage)?
            };
            match descriptor.role() {
                ResourceRole::Timeline => timeline = bytes,
                ResourceRole::Track => tracks.push(bytes),
                ResourceRole::Clip => clips.push(bytes),
                ResourceRole::Scene => scenes.push(bytes),
                ResourceRole::Arrangement => arrangement = Some(bytes),
                ResourceRole::Mixer => mixer_bytes = bytes,
                ResourceRole::AutomationTake => takes.push(bytes),
                ResourceRole::Mapping
                | ResourceRole::RecordingMetadata
                | ResourceRole::DeviceChain => {}
            }
        }
        let invalid = |source: Box<dyn Error + Send + Sync>| {
            IdeResourceRestoreError::CandidateInvalid(source)
        };
        let track_refs: Vec<&[u8]> = tracks.iter().map(Vec::as_slice).collect();
        let clip_refs: Vec<&[u8]> = clips.iter().map(Vec::as_slice).collect();
        let scene_refs: Vec<&[u8]> = scenes.iter().map(Vec::as_slice).collect();
        let take_refs: Vec<&[u8]> = takes.iter().map(Vec::as_slice).collect();
        let loaded = hyper_playback_json::load(
            hyper_playback_json::PlaybackJsonInput {
                project_id: package.manifest().project_id(),
                sample_rate_hz: package.manifest().sample_rate_hz(),
                timeline: &timeline,
                tracks: &track_refs,
                clips: &clip_refs,
                scenes: &scene_refs,
                arrangement: arrangement.as_deref(),
            },
            limits,
        )
        .map_err(|source| invalid(Box::new(source)))?;
        let (_, _, _, media) = prepared.session().parts();
        let facts: Vec<_> = media.media_asset_facts().collect();
        let loaded_mixer = loaded
            .load_mixer(&mixer_bytes, &facts, limits)
            .map_err(|source| invalid(Box::new(source)))?;
        let compiled = loaded_mixer
            .compile_main_graph()
            .map_err(|source| invalid(Box::new(source)))?;
        if !take_refs.is_empty() {
            let automation = loaded
                .load_automation_takes(&loaded_mixer, &facts, &take_refs, limits)
                .map_err(|source| invalid(Box::new(source)))?;
            loaded
                .compile_main_automation(&compiled, &automation)
                .map_err(|source| invalid(Box::new(source)))?;
        }
        loaded
            .compile_playback(
                &facts,
                hyper_playback_compiler::graph::GraphLimits::VERSION_1,
                hyper_playback_compiler::scene::SceneLimits::VERSION_1,
            )
            .map_err(|source| invalid(Box::new(source)))?;
        Ok(())
    }

    fn prepare_control_mapping_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        edit: &IdeControlMappingEdit,
    ) -> Result<IdeControlMappingEditPlan, IdeControlMappingEditError> {
        let empty_identity = match edit {
            IdeControlMappingEdit::Insert(_) => false,
            IdeControlMappingEdit::SetEnabled { binding_id, .. }
            | IdeControlMappingEdit::Remove { binding_id } => binding_id.is_empty(),
        };
        if empty_identity {
            return Err(IdeControlMappingEditError::InvalidBindingIdentity);
        }
        let (mixer, prepared) = Self::prepare_control_mapping_fence(application, recovery)?;
        Self::prepare_control_mapping_edit_from_project(mixer, prepared, edit)
    }

    fn prepare_control_mapping_fence<'recovery>(
        application: &DesktopIdeApplication,
        recovery: &'recovery DesktopPlaybackRecovery,
    ) -> Result<(IdeMixerSavePlan, &'recovery PreparedProjectPlayback), IdeControlMappingEditError>
    {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeControlMappingEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeControlMappingEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Ok((mixer, prepared))
    }

    /// Applies one mutation through the mapping editor and returns the exact
    /// canonical document the editor confirmed.
    ///
    /// Publication chains the identity produced by the mixer save rather than
    /// the one the preview resolved against. That is sound because the mixer
    /// save only rewrites base-state values over an unchanged topology, so
    /// every identity the mapping resolves stays valid across it.
    fn prepare_control_mapping_edit_from_project(
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        edit: &IdeControlMappingEdit,
    ) -> Result<IdeControlMappingEditPlan, IdeControlMappingEditError> {
        let create_missing = matches!(edit, IdeControlMappingEdit::Insert(_));
        let document = match prepared.control_mapping() {
            Some(document) => document.clone(),
            None if create_missing => ControlMappingDocument::try_new(
                uuid::Uuid::new_v4().to_string(),
                prepared.package().manifest().project_id().to_owned(),
                Vec::new(),
                std::collections::BTreeMap::new(),
            )
            .map_err(IdeControlMappingEditError::Binding)?,
            None => return Err(IdeControlMappingEditError::MappingResourceMissing),
        };
        let mut editor = MappingEditor::new(document);
        match edit {
            IdeControlMappingEdit::Insert(request) => {
                let binding = ControlBinding::try_new(
                    uuid::Uuid::new_v4().to_string(),
                    true,
                    request.source.clone(),
                    request.scope.clone(),
                    request.transform,
                    request.target.clone(),
                )
                .map_err(IdeControlMappingEditError::Binding)?;
                editor
                    .insert_binding(binding)
                    .map_err(IdeControlMappingEditError::Mutation)?;
            }
            IdeControlMappingEdit::SetEnabled {
                binding_id,
                enabled,
            } => editor
                .set_binding_enabled(binding_id, *enabled)
                .map_err(IdeControlMappingEditError::Mutation)?,
            IdeControlMappingEdit::Remove { binding_id } => {
                editor
                    .remove_binding(binding_id)
                    .map(|_| ())
                    .map_err(IdeControlMappingEditError::Mutation)?;
            }
        }
        let resolver = RaisedCapabilityProject::new(prepared.mapping_project(), editor.bindings());
        let preview = editor
            .preview_for_project(&resolver, *prepared.package().manifest_sha256())
            .map_err(|source| IdeControlMappingEditError::Preview(Box::new(source)))?;
        let confirmed = editor
            .confirm(preview)
            .map_err(|_| IdeControlMappingEditError::StalePreview)?;
        let mut mapping_bytes = Vec::new();
        mapping_bytes
            .try_reserve_exact(confirmed.canonical_json().len())
            .map_err(|_| IdeControlMappingEditError::CapacityUnavailable)?;
        mapping_bytes.extend_from_slice(confirmed.canonical_json());
        Ok(IdeControlMappingEditPlan {
            mixer,
            mapping_bytes,
        })
    }

    fn prepare_automation_take_creation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        request: &IdeAutomationTakeRequest,
    ) -> Result<IdeAutomationTakePublicationPlan, IdeAutomationTakeCreationError> {
        let name_length = request.name.chars().count();
        if request.name.trim() != request.name || !(1..=120).contains(&name_length) {
            return Err(IdeAutomationTakeCreationError::InvalidName);
        }
        let empty_target = match &request.target {
            IdeAutomationTarget::Channel { track_id, .. } => track_id.is_empty(),
            IdeAutomationTarget::Send { send_id, .. } => send_id.is_empty(),
            IdeAutomationTarget::Main { .. } => false,
        };
        if empty_target {
            return Err(IdeAutomationTakeCreationError::InvalidTargetIdentity);
        }
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAutomationTakeCreationError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAutomationTakeCreationError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_automation_take_creation_from_project(application, mixer, prepared, request)
    }

    fn prepare_native_filter_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        edit: &IdeNativeFilterEdit,
    ) -> Result<IdeNativeFilterEditPlan, IdeNativeFilterEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeNativeFilterEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeNativeFilterEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_native_filter_edit_from_project(mixer, prepared, edit)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive edit match keeps each publication and its exact history inverse adjacent"
    )]
    fn prepare_native_filter_edit_from_project(
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        edit: &IdeNativeFilterEdit,
    ) -> Result<IdeNativeFilterEditPlan, IdeNativeFilterEditError> {
        let (chain_id, publication, inverse) = match edit {
            IdeNativeFilterEdit::Add {
                track_id,
                mode,
                cutoff_millihertz,
                resonance_millionths,
            } => {
                if track_id.is_empty() {
                    return Err(IdeNativeFilterEditError::InvalidIdentity);
                }
                let (_, compiled_mixer, _, _) = prepared.session().parts();
                if !compiled_mixer
                    .slots()
                    .iter()
                    .any(|slot| slot.track_id() == track_id)
                {
                    return Err(IdeNativeFilterEditError::TrackUnavailable);
                }
                let filter_state =
                    native_filter_state(*mode, *cutoff_millihertz, *resonance_millionths)?;
                let device_id = uuid::Uuid::new_v4().to_string();
                if let Some(chain) = prepared
                    .device_chains()
                    .iter()
                    .find(|chain| chain.track_id() == track_id)
                {
                    let changed = chain
                        .try_with_appended_filter(device_id, true, filter_state)
                        .map_err(IdeNativeFilterEditError::Device)?;
                    let previous = serialize_native_filter_chain(chain)?;
                    (
                        chain.id().to_owned(),
                        IdeNativeFilterPublication::Replacement(
                            serialize_native_filter_chain(&changed)?,
                        ),
                        IdeHistoryOperation::ResourceReplacement {
                            resource_id: chain.id().to_owned(),
                            bytes: previous,
                        },
                    )
                } else {
                    let chain_id = uuid::Uuid::new_v4().to_string();
                    let chain = DeviceChain::try_new(
                        chain_id.clone(),
                        prepared.package().manifest().project_id().to_owned(),
                        track_id.clone(),
                        vec![
                            NativeDeviceInstance::try_new_filter(device_id, true, filter_state)
                                .map_err(IdeNativeFilterEditError::Device)?,
                        ],
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                    let bytes = serialize_native_filter_chain(&chain)?;
                    (
                        chain_id,
                        IdeNativeFilterPublication::Addition(bytes.clone()),
                        IdeHistoryOperation::DeviceChainRemoval {
                            chain_id: chain.id().to_owned(),
                            expected_bytes: bytes,
                        },
                    )
                }
            }
            IdeNativeFilterEdit::AddCompressor {
                track_id,
                threshold_millidecibels,
                ratio_millionths,
                attack_microseconds,
                release_microseconds,
                makeup_millidecibels,
            } => {
                if track_id.is_empty() {
                    return Err(IdeNativeFilterEditError::InvalidIdentity);
                }
                let (_, compiled_mixer, _, _) = prepared.session().parts();
                if !compiled_mixer
                    .slots()
                    .iter()
                    .any(|slot| slot.track_id() == track_id)
                {
                    return Err(IdeNativeFilterEditError::TrackUnavailable);
                }
                let compressor_state = native_compressor_state(
                    *threshold_millidecibels,
                    *ratio_millionths,
                    *attack_microseconds,
                    *release_microseconds,
                    *makeup_millidecibels,
                )?;
                let device_id = uuid::Uuid::new_v4().to_string();
                if let Some(chain) = prepared
                    .device_chains()
                    .iter()
                    .find(|chain| chain.track_id() == track_id)
                {
                    let changed = chain
                        .try_with_appended_compressor(device_id, true, compressor_state)
                        .map_err(IdeNativeFilterEditError::Device)?;
                    let previous = serialize_native_filter_chain(chain)?;
                    (
                        chain.id().to_owned(),
                        IdeNativeFilterPublication::Replacement(
                            serialize_native_filter_chain(&changed)?,
                        ),
                        IdeHistoryOperation::ResourceReplacement {
                            resource_id: chain.id().to_owned(),
                            bytes: previous,
                        },
                    )
                } else {
                    let chain_id = uuid::Uuid::new_v4().to_string();
                    let chain = DeviceChain::try_new(
                        chain_id.clone(),
                        prepared.package().manifest().project_id().to_owned(),
                        track_id.clone(),
                        vec![
                            NativeDeviceInstance::try_new_compressor(
                                device_id,
                                true,
                                compressor_state,
                            )
                            .map_err(IdeNativeFilterEditError::Device)?,
                        ],
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                    let bytes = serialize_native_filter_chain(&chain)?;
                    (
                        chain_id,
                        IdeNativeFilterPublication::Addition(bytes.clone()),
                        IdeHistoryOperation::DeviceChainRemoval {
                            chain_id: chain.id().to_owned(),
                            expected_bytes: bytes,
                        },
                    )
                }
            }
            IdeNativeFilterEdit::Update {
                chain_id,
                device_instance_id,
                enabled,
                mode,
                cutoff_millihertz,
                resonance_millionths,
            } => {
                let chain = find_native_filter_chain(prepared, chain_id, device_instance_id)?;
                let changed = chain
                    .try_with_filter_state(
                        device_instance_id,
                        *enabled,
                        native_filter_state(*mode, *cutoff_millihertz, *resonance_millionths)?,
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                (
                    chain_id.clone(),
                    IdeNativeFilterPublication::Replacement(
                        serialize_native_filter_chain(&changed)?,
                    ),
                    IdeHistoryOperation::ResourceReplacement {
                        resource_id: chain.id().to_owned(),
                        bytes: serialize_native_filter_chain(chain)?,
                    },
                )
            }
            IdeNativeFilterEdit::UpdateCompressor {
                chain_id,
                device_instance_id,
                enabled,
                threshold_millidecibels,
                ratio_millionths,
                attack_microseconds,
                release_microseconds,
                makeup_millidecibels,
            } => {
                let chain = find_native_filter_chain(prepared, chain_id, device_instance_id)?;
                let changed = chain
                    .try_with_compressor_state(
                        device_instance_id,
                        *enabled,
                        native_compressor_state(
                            *threshold_millidecibels,
                            *ratio_millionths,
                            *attack_microseconds,
                            *release_microseconds,
                            *makeup_millidecibels,
                        )?,
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                (
                    chain_id.clone(),
                    IdeNativeFilterPublication::Replacement(
                        serialize_native_filter_chain(&changed)?,
                    ),
                    IdeHistoryOperation::ResourceReplacement {
                        resource_id: chain.id().to_owned(),
                        bytes: serialize_native_filter_chain(chain)?,
                    },
                )
            }
            IdeNativeFilterEdit::Move {
                chain_id,
                device_instance_id,
                destination_index,
            } => {
                let chain = find_native_filter_chain(prepared, chain_id, device_instance_id)?;
                let changed = chain
                    .try_with_moved_device(device_instance_id, *destination_index)
                    .map_err(IdeNativeFilterEditError::Device)?;
                (
                    chain_id.clone(),
                    IdeNativeFilterPublication::Replacement(
                        serialize_native_filter_chain(&changed)?,
                    ),
                    IdeHistoryOperation::ResourceReplacement {
                        resource_id: chain.id().to_owned(),
                        bytes: serialize_native_filter_chain(chain)?,
                    },
                )
            }
            IdeNativeFilterEdit::Duplicate {
                chain_id,
                device_instance_id,
            } => {
                let chain = find_native_filter_chain(prepared, chain_id, device_instance_id)?;
                let changed = chain
                    .try_with_duplicated_device(
                        device_instance_id,
                        uuid::Uuid::new_v4().to_string(),
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                (
                    chain_id.clone(),
                    IdeNativeFilterPublication::Replacement(
                        serialize_native_filter_chain(&changed)?,
                    ),
                    IdeHistoryOperation::ResourceReplacement {
                        resource_id: chain.id().to_owned(),
                        bytes: serialize_native_filter_chain(chain)?,
                    },
                )
            }
            IdeNativeFilterEdit::CopyTo {
                source_chain_id,
                source_device_instance_id,
                target_chain_id,
                target_device_instance_id,
            } => {
                if source_chain_id == target_chain_id {
                    return Err(IdeNativeFilterEditError::CopyRequiresDistinctChains);
                }
                let source_chain = find_native_filter_chain(
                    prepared,
                    source_chain_id,
                    source_device_instance_id,
                )?;
                let source = source_chain
                    .devices()
                    .iter()
                    .find(|device| device.id() == source_device_instance_id)
                    .ok_or(IdeNativeFilterEditError::InvalidIdentity)?;
                let target_chain = find_native_filter_chain(
                    prepared,
                    target_chain_id,
                    target_device_instance_id,
                )?;
                let changed = target_chain
                    .try_with_copied_device_after(
                        source,
                        target_device_instance_id,
                        uuid::Uuid::new_v4().to_string(),
                    )
                    .map_err(IdeNativeFilterEditError::Device)?;
                (
                    target_chain_id.clone(),
                    IdeNativeFilterPublication::Replacement(
                        serialize_native_filter_chain(&changed)?,
                    ),
                    IdeHistoryOperation::ResourceReplacement {
                        resource_id: target_chain.id().to_owned(),
                        bytes: serialize_native_filter_chain(target_chain)?,
                    },
                )
            }
            IdeNativeFilterEdit::Remove {
                chain_id,
                device_instance_id,
            } => {
                let chain = find_native_filter_chain(prepared, chain_id, device_instance_id)?;
                let previous = serialize_native_filter_chain(chain)?;
                let (publication, inverse) = match chain
                    .try_without_device(device_instance_id)
                    .map_err(IdeNativeFilterEditError::Device)?
                {
                    Some(changed) => (
                        IdeNativeFilterPublication::Replacement(
                            serialize_native_filter_chain(&changed)?,
                        ),
                        IdeHistoryOperation::ResourceReplacement {
                            resource_id: chain.id().to_owned(),
                            bytes: previous.clone(),
                        },
                    ),
                    None => (
                        IdeNativeFilterPublication::Removal,
                        IdeHistoryOperation::DeviceChainAddition {
                            chain_id: chain.id().to_owned(),
                            bytes: previous,
                        },
                    ),
                };
                (chain_id.clone(), publication, inverse)
            }
        };
        Ok(IdeNativeFilterEditPlan {
            mixer,
            chain_id,
            publication,
            inverse,
        })
    }

    fn prepare_automation_take_creation_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        request: &IdeAutomationTakeRequest,
    ) -> Result<IdeAutomationTakePublicationPlan, IdeAutomationTakeCreationError> {
        let package = prepared.package();
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeAutomationTakeCreationError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let limits = application.config.project_limits.session.documents;
        let loaded_mixer = project
            .load_mixer(&mixer.resource_bytes, &assets, limits)
            .map_err(IdeAutomationTakeCreationError::MixerDocument)?;
        let mut existing = Vec::new();
        for descriptor in package.manifest().resources() {
            if descriptor.role() != ResourceRole::AutomationTake {
                continue;
            }
            existing
                .try_reserve(1)
                .map_err(|_| IdeAutomationTakeCreationError::CapacityUnavailable)?;
            existing.push(
                package
                    .read_resource(descriptor.id())
                    .map_err(IdeAutomationTakeCreationError::Storage)?,
            );
        }
        let mut references = Vec::new();
        references
            .try_reserve_exact(existing.len())
            .map_err(|_| IdeAutomationTakeCreationError::CapacityUnavailable)?;
        references.extend(existing.iter().map(Vec::as_slice));
        let take_id = uuid::Uuid::new_v4().to_string();
        let lane_id = uuid::Uuid::new_v4().to_string();
        let captured_at_utc =
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let target = match &request.target {
            IdeAutomationTarget::Channel {
                track_id,
                parameter,
            } => AutomationTargetSelection::Channel {
                track_id,
                parameter: *parameter,
            },
            IdeAutomationTarget::Send { send_id, parameter } => AutomationTargetSelection::Send {
                send_id,
                parameter: *parameter,
            },
            IdeAutomationTarget::Main { parameter } => AutomationTargetSelection::Main {
                parameter: *parameter,
            },
        };
        let take_bytes = serialize_automation_take_addition(
            project,
            &loaded_mixer,
            &assets,
            &references,
            AutomationTakeAddition {
                take_id: &take_id,
                lane_id: &lane_id,
                name: &request.name,
                captured_at_utc: &captured_at_utc,
                start_tick: request.start_tick,
                end_tick: request.end_tick,
                target,
                interpolation: request.interpolation,
                points: &request.points,
            },
            limits,
        )
        .map_err(IdeAutomationTakeCreationError::Serialization)?;
        Ok(IdeAutomationTakePublicationPlan {
            mixer,
            take_id,
            take_bytes,
        })
    }

    fn prepare_automation_take_activation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        take_id: &str,
        enabled: bool,
    ) -> Result<IdeAutomationTakeEditPlan, IdeAutomationTakeEditError> {
        if take_id.is_empty() {
            return Err(IdeAutomationTakeEditError::InvalidTakeIdentity);
        }
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAutomationTakeEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAutomationTakeEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_automation_take_activation_from_project(
            application,
            mixer,
            prepared,
            take_id,
            enabled,
        )
    }

    fn prepare_automation_take_activation_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        take_id: &str,
        enabled: bool,
    ) -> Result<IdeAutomationTakeEditPlan, IdeAutomationTakeEditError> {
        let package = prepared.package();
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeAutomationTakeEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let limits = application.config.project_limits.session.documents;
        let loaded_mixer = project
            .load_mixer(&mixer.resource_bytes, &assets, limits)
            .map_err(IdeAutomationTakeEditError::MixerDocument)?;
        let mut existing = Vec::new();
        for descriptor in package.manifest().resources() {
            if descriptor.role() != ResourceRole::AutomationTake {
                continue;
            }
            existing
                .try_reserve(1)
                .map_err(|_| IdeAutomationTakeEditError::CapacityUnavailable)?;
            existing.push(
                package
                    .read_resource(descriptor.id())
                    .map_err(IdeAutomationTakeEditError::Storage)?,
            );
        }
        let mut references = Vec::new();
        references
            .try_reserve_exact(existing.len())
            .map_err(|_| IdeAutomationTakeEditError::CapacityUnavailable)?;
        references.extend(existing.iter().map(Vec::as_slice));
        let take_bytes = serialize_automation_take_activation(
            project,
            &loaded_mixer,
            &assets,
            &references,
            take_id,
            enabled,
            limits,
        )
        .map_err(IdeAutomationTakeEditError::Serialization)?;
        let mut owned_take_id = String::new();
        owned_take_id
            .try_reserve_exact(take_id.len())
            .map_err(|_| IdeAutomationTakeEditError::CapacityUnavailable)?;
        owned_take_id.push_str(take_id);
        Ok(IdeAutomationTakeEditPlan {
            mixer,
            take_id: owned_take_id,
            take_bytes,
        })
    }

    fn prepare_automation_take_removal(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        take_id: &str,
    ) -> Result<IdeAutomationTakeRemovalPlan, IdeAutomationTakeEditError> {
        if take_id.is_empty() {
            return Err(IdeAutomationTakeEditError::InvalidTakeIdentity);
        }
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAutomationTakeEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAutomationTakeEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_automation_take_removal_from_project(mixer, prepared, take_id)
    }

    fn prepare_automation_take_removal_from_project(
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        take_id: &str,
    ) -> Result<IdeAutomationTakeRemovalPlan, IdeAutomationTakeEditError> {
        if take_id.is_empty() {
            return Err(IdeAutomationTakeEditError::InvalidTakeIdentity);
        }
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| {
                descriptor.id() == take_id && descriptor.role() == ResourceRole::AutomationTake
            })
            .ok_or(IdeAutomationTakeEditError::TakeUnavailable)?;
        let take_bytes = package
            .read_resource(descriptor.id())
            .map_err(IdeAutomationTakeEditError::Storage)?;
        let mut owned_take_id = String::new();
        owned_take_id
            .try_reserve_exact(take_id.len())
            .map_err(|_| IdeAutomationTakeEditError::CapacityUnavailable)?;
        owned_take_id.push_str(take_id);
        Ok(IdeAutomationTakeRemovalPlan {
            mixer,
            take_id: owned_take_id,
            take_bytes,
        })
    }

    fn prepare_scene_duplication(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        source_scene_id: &str,
        scene_name: &str,
    ) -> Result<IdeScenePublicationPlan, IdeSceneDuplicationError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeSceneDuplicationError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeSceneDuplicationError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        Self::prepare_scene_duplication_from_project(
            application,
            mixer,
            prepared,
            source_scene_id,
            scene_name,
        )
    }

    fn prepare_scene_duplication_from_project(
        application: &DesktopIdeApplication,
        mixer: IdeMixerSavePlan,
        prepared: &PreparedProjectPlayback,
        source_scene_id: &str,
        scene_name: &str,
    ) -> Result<IdeScenePublicationPlan, IdeSceneDuplicationError> {
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == source_scene_id)
            .ok_or(IdeSceneDuplicationError::SceneResourceMissing)?;
        if descriptor.role() != ResourceRole::Scene {
            return Err(IdeSceneDuplicationError::SceneResourceRoleMismatch);
        }
        let source_scene = package
            .read_resource(source_scene_id)
            .map_err(IdeSceneDuplicationError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeSceneDuplicationError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let scene_id = uuid::Uuid::new_v4().to_string();
        let scene_bytes = serialize_scene_duplication(
            project,
            &assets,
            &source_scene,
            source_scene_id,
            &scene_id,
            scene_name,
            application.config.project_limits.session.documents,
        )
        .map_err(|source| match source {
            SceneDuplicationSerializationError::InvalidSceneName => {
                IdeSceneDuplicationError::InvalidName
            }
            source => IdeSceneDuplicationError::Serialization(source),
        })?;
        Ok(IdeScenePublicationPlan {
            mixer,
            scene_id,
            scene_bytes,
        })
    }

    fn prepare_scene_rename(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_id: &str,
        scene_name: &str,
    ) -> Result<IdeSceneEditPlan, IdeSceneEditError> {
        Self::prepare_scene_edit(
            application,
            recovery,
            scene_id,
            |project, source, limits| {
                serialize_scene_rename(project, source, scene_id, scene_name, limits).map_err(
                    |source| match source {
                        SceneRenameSerializationError::InvalidSceneName => {
                            IdeSceneEditError::InvalidName
                        }
                        SceneRenameSerializationError::Document(source) => {
                            IdeSceneEditError::Serialization(source)
                        }
                    },
                )
            },
        )
    }

    fn prepare_scene_activation(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_id: &str,
        active: bool,
    ) -> Result<IdeSceneEditPlan, IdeSceneEditError> {
        Self::prepare_scene_edit(
            application,
            recovery,
            scene_id,
            |project, source, limits| {
                serialize_scene_activation(project, source, scene_id, active, limits)
                    .map_err(IdeSceneEditError::Serialization)
            },
        )
    }

    fn prepare_scene_slot_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_id: &str,
        track_id: &str,
        edit: &IdeSceneSlotEdit,
    ) -> Result<IdeSceneEditPlan, IdeSceneSlotEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeSceneSlotEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeSceneSlotEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let package = prepared.package();
        let descriptor = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.id() == scene_id)
            .ok_or(IdeSceneSlotEditError::SceneResourceMissing)?;
        if descriptor.role() != ResourceRole::Scene {
            return Err(IdeSceneSlotEditError::SceneResourceRoleMismatch);
        }
        let source = package
            .read_resource(scene_id)
            .map_err(IdeSceneSlotEditError::Storage)?;
        let (project, _, _, media) = prepared.session().parts();
        let mut assets = Vec::new();
        assets
            .try_reserve_exact(media.asset_count())
            .map_err(|_| IdeSceneSlotEditError::CapacityUnavailable)?;
        assets.extend(media.media_asset_facts());
        let scene_bytes = serialize_scene_slot_edit(
            project,
            &assets,
            &source,
            scene_id,
            track_id,
            edit.as_serialization_edit(),
            application.config.project_limits.session.documents,
        )
        .map_err(IdeSceneSlotEditError::Serialization)?;
        let mut owned_scene_id = String::new();
        owned_scene_id
            .try_reserve_exact(scene_id.len())
            .map_err(|_| IdeSceneSlotEditError::CapacityUnavailable)?;
        owned_scene_id.push_str(scene_id);
        Ok(IdeSceneEditPlan {
            mixer,
            scene_id: owned_scene_id,
            scene_bytes,
        })
    }

    fn prepare_scene_edit(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        scene_id: &str,
        serialize: impl FnOnce(
            &hyper_playback_json::LoadedPlaybackProject,
            &[u8],
            LoaderLimits,
        ) -> Result<Vec<u8>, IdeSceneEditError>,
    ) -> Result<IdeSceneEditPlan, IdeSceneEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeSceneEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeSceneEditError::MixerSave(Box::new(IdeProjectSaveError::ProjectSnapshotUnavailable))
        })?;
        let (scene_id, scene_bytes) = prepare_scene_edit_resource(
            prepared,
            scene_id,
            application.config.project_limits.session.documents,
            serialize,
        )?;
        Ok(IdeSceneEditPlan {
            mixer,
            scene_id,
            scene_bytes,
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

    fn prepare_audio_clip_fades(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        fade_in_ticks: u64,
        fade_out_ticks: u64,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipFadeEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipFadeEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipFadeEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_fade_resource(
            prepared,
            clip_id,
            fade_in_ticks,
            fade_out_ticks,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
        })
    }

    fn prepare_audio_clip_loop(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        enabled: bool,
        start_tick: u64,
        end_tick: u64,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipLoopEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipLoopEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipLoopEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_loop_resource(
            prepared,
            clip_id,
            enabled,
            start_tick,
            end_tick,
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

    fn prepare_audio_clip_reverse(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        reverse: bool,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipReverseEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipReverseEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipReverseEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_reverse_resource(
            prepared,
            clip_id,
            reverse,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
        })
    }

    fn prepare_audio_clip_source_slip(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        source_start_frame: u64,
    ) -> Result<IdeAudioClipEditPlan, IdeAudioClipSourceSlipEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipSourceSlipEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipSourceSlipEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let (clip_id, clip_bytes) = prepare_audio_clip_source_slip_resource(
            prepared,
            clip_id,
            source_start_frame,
            application.config.project_limits.session.documents,
        )?;
        Ok(IdeAudioClipEditPlan {
            mixer,
            clip_id,
            clip_bytes,
        })
    }

    fn prepare_audio_clip_source_trim(
        application: &DesktopIdeApplication,
        recovery: &DesktopPlaybackRecovery,
        clip_id: &str,
        source_start_frame: u64,
        source_frame_count: u64,
    ) -> Result<IdeAudioClipSourceTrimPlan, IdeAudioClipSourceTrimEditError> {
        let mixer = Self::prepare_mixer_save(application, recovery)
            .map_err(|source| IdeAudioClipSourceTrimEditError::MixerSave(Box::new(source)))?;
        let prepared = recovery.prepared_project().ok_or_else(|| {
            IdeAudioClipSourceTrimEditError::MixerSave(Box::new(
                IdeProjectSaveError::ProjectSnapshotUnavailable,
            ))
        })?;
        let resource = prepare_audio_clip_source_trim_resource(
            prepared,
            clip_id,
            source_start_frame,
            source_frame_count,
            application.config.project_limits.session.documents,
        )?;
        let inverse = IdeHistoryOperation::ResourceReplacement {
            resource_id: resource.clip_id.clone(),
            bytes: resource.prior_clip_bytes,
        };
        Ok(IdeAudioClipSourceTrimPlan {
            edit: IdeAudioClipEditPlan {
                mixer,
                clip_id: resource.clip_id,
                clip_bytes: resource.clip_bytes,
            },
            inverse,
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
        let mut scene_ids = [""; MAX_IDE_SCENES];
        let mut scene_names = [""; MAX_IDE_SCENES];
        let mut scene_active = [false; MAX_IDE_SCENES];
        let mut scene_availability = [IdeSceneAvailability::NotPrepared; MAX_IDE_SCENES];
        for scene_index in 0..scene_count {
            scene_ids[scene_index] = loaded.scene_id(scene_index)?;
            scene_names[scene_index] = loaded.scene_name(scene_index)?;
            scene_active[scene_index] = loaded.scene_enabled(scene_index)?;
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
        let mut mixer_send_ids =
            [[""; hyper_mixer_runtime::MAX_MIXER_SENDS_PER_SLOT]; MAX_IDE_MIXER_CHANNELS];
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
                mixer_send_ids[channel_index][send_index] = send.id();
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
            playback_session_generation: self.application.playback_session_generation,
            project_path: project.package().root(),
            project_id: project.package().manifest().project_id(),
            project_name: project.package().manifest().name(),
            sample_rate_hz: project.session().sample_rate_hz(),
            playback_extent: project.session().extent(),
            arrangement: project.session().shared_arrangement(),
            automation: project.session().shared_automation_takes(),
            recording_metadata: project.recording_metadata(),
            device_chains: project.device_chains(),
            unused_sample_cleanup: unused_sample_cleanup_summary(project).ok()?,
            scene_count,
            track_count: loaded.track_count(),
            scene_ids,
            scene_names,
            scene_active,
            scene_availability,
            mixer_channel_count,
            mixer_channel_ids,
            mixer_channel_names,
            mixer_channel_kinds,
            mixer_channel_active,
            mixer_channel_states,
            mixer_send_counts,
            mixer_send_ids,
            mixer_send_target_names,
            mixer_send_taps,
            mixer_send_states,
            mixer_main_state,
            control_bindings: project
                .control_mapping()
                .map_or(&[] as &[ControlBinding], ControlMappingDocument::bindings),
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

    /// Locates to one exact compiled scene-marker frame after validating the
    /// immutable project and marker identities published to the UI.
    ///
    /// The requested locate state can preserve, stop, or start transport in
    /// the same bounded command, so play-from-marker has no intermediate seek
    /// state visible to the audio thread.
    ///
    /// # Errors
    ///
    /// Rejects unavailable project authority, stale project or marker
    /// identities, and command delivery failures without queueing a locate.
    pub fn try_locate_arrangement_scene_launch(
        &mut self,
        project_id: &str,
        index: usize,
        launch_id: &str,
        state: LocateState,
    ) -> Result<usize, IdeArrangementLocateError> {
        let frame = match &self.session {
            IdePlaybackSessionOwner::Native(session) => {
                let prepared = session
                    .prepared_project()
                    .ok_or(IdeArrangementLocateError::Unavailable)?;
                if prepared.session().project_id() != project_id {
                    return Err(IdeArrangementLocateError::StaleProject);
                }
                let launch = prepared
                    .session()
                    .arrangement()
                    .scene_launches()
                    .get(index)
                    .ok_or(IdeArrangementLocateError::StaleSceneLaunch {
                        requested_index: index,
                    })?;
                if launch.id() != launch_id {
                    return Err(IdeArrangementLocateError::StaleSceneLaunch {
                        requested_index: index,
                    });
                }
                launch.start_frame()
            }
            #[cfg(test)]
            IdePlaybackSessionOwner::Test(session) => {
                if session.arrangement_project_id != project_id {
                    return Err(IdeArrangementLocateError::StaleProject);
                }
                let (current_id, frame) = session.arrangement_scene_launches.get(index).ok_or(
                    IdeArrangementLocateError::StaleSceneLaunch {
                        requested_index: index,
                    },
                )?;
                if current_id != launch_id {
                    return Err(IdeArrangementLocateError::StaleSceneLaunch {
                        requested_index: index,
                    });
                }
                *frame
            }
        };
        self.try_send_transport(TransportCommand::Locate { frame, state })
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
            // A panic here would unwind through Drop and abort the whole test
            // binary, taking unrelated tests with it. Teardown reports nothing.
            let _ = std::fs::remove_dir_all(&self.0);
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
    const ARRANGEMENT_TEST_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338508";
    const ARRANGEMENT_TEST_SECOND_SCENE_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338510";

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

        fn with_trim_compatible_source(self) -> Self {
            let fixture = self;
            let manifest_path = fixture.directory.0.join("project.json");
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            let descriptor = manifest["resources"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|descriptor| descriptor["id"] == ARRANGEMENT_TEST_CLIP_ID)
                .unwrap();
            let path = descriptor["path"].as_str().unwrap().to_owned();
            let clip_path = fixture.directory.0.join(&path);
            let mut clip: Value =
                serde_json::from_slice(&std::fs::read(&clip_path).unwrap()).unwrap();
            clip["loop"]["end_tick"] = json!(2);
            let clip_bytes = arrangement_test_canonical(&clip);
            std::fs::write(&clip_path, &clip_bytes).unwrap();
            descriptor["byte_length"] = json!(clip_bytes.len());
            descriptor["sha256"] = json!(arrangement_test_sha256(&clip_bytes));
            std::fs::write(manifest_path, arrangement_test_canonical(&manifest)).unwrap();
            hyper_storage_engine::verify_project_package(&fixture.directory.0).unwrap();
            fixture
        }

        fn with_scene() -> Self {
            Self::new().add_scene()
        }

        fn with_two_scenes() -> Self {
            Self::with_scene().add_second_scene()
        }

        fn with_scene_without_arrangement() -> Self {
            Self::without_arrangement().add_scene()
        }

        fn add_scene(self) -> Self {
            let fixture = self;
            let scene = json!({
                "format":"hyper-scene", "format_version":1,
                "id":ARRANGEMENT_TEST_SCENE_ID,
                "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                "name":"Intro", "order":0, "enabled":true,
                "launch_quantization":{"mode":"immediate"},
                "slots":[], "extensions":{"com.hyper.fixture":"preserve"}
            });
            let scene_bytes = arrangement_test_canonical(&scene);
            arrangement_test_write(&fixture.directory.0, "scenes/intro.json", &scene_bytes);
            let manifest_path = fixture.directory.0.join("project.json");
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            manifest["resources"]
                .as_array_mut()
                .unwrap()
                .push(arrangement_test_descriptor(
                    ARRANGEMENT_TEST_SCENE_ID,
                    "scene",
                    "scenes/intro.json",
                    &scene_bytes,
                ));
            std::fs::write(manifest_path, arrangement_test_canonical(&manifest)).unwrap();
            hyper_storage_engine::verify_project_package(&fixture.directory.0).unwrap();
            fixture
        }

        fn add_second_scene(self) -> Self {
            let fixture = self;
            let scene = json!({
                "format":"hyper-scene", "format_version":1,
                "id":ARRANGEMENT_TEST_SECOND_SCENE_ID,
                "project_id":ARRANGEMENT_TEST_PROJECT_ID,
                "name":"Chorus", "order":1, "enabled":true,
                "launch_quantization":{"mode":"immediate"},
                "slots":[], "extensions":{"com.hyper.fixture":"preserve-second"}
            });
            let scene_bytes = arrangement_test_canonical(&scene);
            arrangement_test_write(&fixture.directory.0, "scenes/chorus.json", &scene_bytes);
            let manifest_path = fixture.directory.0.join("project.json");
            let mut manifest: Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            manifest["resources"]
                .as_array_mut()
                .unwrap()
                .push(arrangement_test_descriptor(
                    ARRANGEMENT_TEST_SECOND_SCENE_ID,
                    "scene",
                    "scenes/chorus.json",
                    &scene_bytes,
                ));
            std::fs::write(manifest_path, arrangement_test_canonical(&manifest)).unwrap();
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

    fn fixture_mixer_save_plan(prepared: &PreparedProjectPlayback) -> IdeMixerSavePlan {
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        IdeMixerSavePlan {
            project_path: package.root().to_path_buf(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_bytes: package.read_resource(&mixer_id).unwrap(),
            resource_id: mixer_id,
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

    fn assert_scene_edit_recovery_authority(outcome: &ProjectResourcePublicationOutcome) {
        assert!(matches!(
            worker::scene_rename_reopen_failure(
                Ok(outcome.clone()),
                IdeProjectOpenError::SelectedOutputUnavailable,
            ),
            IdeWorkerCommandFailure::SceneRename(reason)
                if matches!(reason.as_ref(), IdeSceneRenameError::ReopenAfterPublication {
                    outcome,
                    source,
                } if outcome.changed()
                    && outcome.project_revision() == 2
                    && matches!(source.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable))
        ));
        let committed = IdeSceneRenameError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(matches!(
            worker::scene_rename_failure_event("Verse A".to_owned(), committed),
            IdeWorkerEvent::SceneRenameCommittedWithWarning { name, warning }
                if name == "Verse A" && warning.publication_committed()
        ));
        assert!(matches!(
            worker::scene_rename_failure_event(
                "Verse A".to_owned(),
                IdeSceneRenameError::InvalidName,
            ),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::RenameScene,
                reason: IdeWorkerCommandFailure::SceneRename(reason),
            } if matches!(reason.as_ref(), IdeSceneRenameError::InvalidName)
        ));
        assert!(matches!(
            worker::scene_activation_reopen_failure(
                Ok(outcome.clone()),
                IdeProjectOpenError::SelectedOutputUnavailable,
            ),
            IdeWorkerCommandFailure::SceneActivation(reason)
                if matches!(reason.as_ref(), IdeSceneEditError::ReopenAfterPublication {
                    outcome,
                    source,
                } if outcome.changed()
                    && outcome.project_revision() == 2
                    && matches!(source.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable))
        ));
        let committed = IdeSceneEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed.to_string().contains("scene edit was committed"));
        assert!(!committed.to_string().contains("renamed"));
        assert!(matches!(
            worker::scene_activation_failure_event(false, committed),
            IdeWorkerEvent::SceneActivationCommittedWithWarning { active: false, warning }
                if warning.publication_committed()
        ));
        assert!(matches!(
            worker::scene_activation_failure_event(
                false,
                IdeSceneEditError::InvalidSceneIdentity,
            ),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetSceneActive,
                reason: IdeWorkerCommandFailure::SceneActivation(reason),
            } if matches!(reason.as_ref(), IdeSceneEditError::InvalidSceneIdentity)
        ));
    }

    fn prepare_test_scene_rename(
        prepared: &PreparedProjectPlayback,
        scene_id: &str,
        name: &str,
        limits: LoaderLimits,
    ) -> Result<(String, Vec<u8>), IdeSceneEditError> {
        prepare_scene_edit_resource(prepared, scene_id, limits, |project, source, limits| {
            serialize_scene_rename(project, source, scene_id, name, limits).map_err(|source| {
                match source {
                    SceneRenameSerializationError::InvalidSceneName => {
                        IdeSceneEditError::InvalidName
                    }
                    SceneRenameSerializationError::Document(source) => {
                        IdeSceneEditError::Serialization(source)
                    }
                }
            })
        })
    }

    fn prepare_test_scene_activation(
        prepared: &PreparedProjectPlayback,
        scene_id: &str,
        active: bool,
        limits: LoaderLimits,
    ) -> Result<(String, Vec<u8>), IdeSceneEditError> {
        prepare_scene_edit_resource(prepared, scene_id, limits, |project, source, limits| {
            serialize_scene_activation(project, source, scene_id, active, limits)
                .map_err(IdeSceneEditError::Serialization)
        })
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

    fn arrangement_test_import_plan(
        source: &Path,
    ) -> (ArrangementProjectFixture, IdeImportedAudioClipPlan, [u8; 32]) {
        let fixture = ArrangementProjectFixture::new();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared = hyper_project_loader::prepare_project_playback(fixture.root(), limits).unwrap();
        let original_identity = *prepared.package().manifest_sha256();
        let application = DesktopIdeApplication::from_inventory(
            desktop_ide_application_config().unwrap(),
            IdeAudioOutputInventory::try_new(&[("output", "Output")]).unwrap(),
        );
        let plan = RunningDesktopIdeApplication::prepare_imported_audio_clip(
            &application,
            &prepared,
            source,
            ARRANGEMENT_TEST_TRACK_ID,
        )
        .unwrap();
        (fixture, plan, original_identity)
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

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn audio_import_memory_plan_has_independent_phase_peaks_and_exact_refusal() {
        let upsample = plan_audio_import_memory(
            AudioImportMemoryInput {
                retained_project_bytes: 1_000,
                encoded_source_bytes: 200,
                input_frames: 441,
                input_sample_rate_hz: 44_100,
                target_sample_rate_hz: 48_000,
                channels: 2,
                maximum_output_frames: 500,
            },
            695_068,
        )
        .unwrap();
        assert_eq!(upsample.decode_peak, 4_728);
        assert_eq!(upsample.resampling_peak, 695_068);
        assert_eq!(upsample.encoding_peak, 8_748);
        assert_eq!(upsample.resampling_working, 694_068);
        assert_eq!(upsample.peak, 695_068);
        assert!(matches!(
            plan_audio_import_memory(
                AudioImportMemoryInput {
                    retained_project_bytes: 1_000,
                    encoded_source_bytes: 200,
                    input_frames: 441,
                    input_sample_rate_hz: 44_100,
                    target_sample_rate_hz: 48_000,
                    channels: 2,
                    maximum_output_frames: 500,
                },
                695_067,
            ),
            Err(IdeAudioImportError::WorkingMemoryLimitExceeded {
                required: 695_068,
                maximum: 695_067,
            })
        ));

        let downsample = plan_audio_import_memory(
            AudioImportMemoryInput {
                retained_project_bytes: 1_000,
                encoded_source_bytes: 200,
                input_frames: 480,
                input_sample_rate_hz: 48_000,
                target_sample_rate_hz: 24_000,
                channels: 1,
                maximum_output_frames: 500,
            },
            675_548,
        )
        .unwrap();
        assert_eq!(downsample.decode_peak, 3_120);
        assert_eq!(downsample.resampling_peak, 675_548);
        assert_eq!(downsample.encoding_peak, 2_988);
        assert_eq!(downsample.peak, 675_548);
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
    fn scene_rename_publishes_durably_and_preserves_other_resources() {
        let fixture = ArrangementProjectFixture::with_scene();
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
        let source_mixer = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let source_scene = package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap();
        let (scene_id, scene_bytes) = prepare_test_scene_rename(
            &prepared,
            ARRANGEMENT_TEST_SCENE_ID,
            "Verse A",
            limits.session.documents,
        )
        .unwrap();
        assert_ne!(scene_bytes, source_scene);
        let expected_project_sha256 = *package.manifest_sha256();
        drop(prepared);

        let outcome = IdeSceneEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: source_mixer.clone(),
            },
            scene_id,
            scene_bytes: scene_bytes.clone(),
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_scene_edit_recovery_authority(&outcome);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (loaded, _, _, _) = reopened.session().parts();
        assert_eq!(loaded.scene_name(0), Some("Verse A"));
        assert_eq!(loaded.scene_id(0), Some(ARRANGEMENT_TEST_SCENE_ID));
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            source_mixer
        );
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_RESOURCE_ID)
                .unwrap(),
            source_arrangement
        );
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_SCENE_ID)
                .unwrap(),
            scene_bytes
        );
        let unchanged_identity = *reopened.package().manifest_sha256();
        let (scene_id, unchanged_scene) = prepare_test_scene_rename(
            &reopened,
            ARRANGEMENT_TEST_SCENE_ID,
            "Verse A",
            limits.session.documents,
        )
        .unwrap();
        assert!(matches!(
            prepare_test_scene_rename(
                &reopened,
                ARRANGEMENT_TEST_TRACK_ID,
                "Invalid",
                limits.session.documents,
            ),
            Err(IdeSceneRenameError::SceneResourceRoleMismatch)
        ));
        drop(reopened);

        let unchanged = IdeSceneEditPlan {
            mixer: IdeMixerSavePlan {
                project_path,
                expected_project_sha256: unchanged_identity,
                resource_id: mixer_id,
                resource_bytes: source_mixer,
            },
            scene_id,
            scene_bytes: unchanged_scene,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 2);
    }

    #[test]
    fn scene_creation_publishes_durably_and_reopens_compiled_state() {
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
        let source_mixer = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let scene_bytes = serialize_scene_addition(
            project,
            &assets,
            ARRANGEMENT_TEST_SCENE_ID,
            "Intro",
            LaunchQuantization::Grid {
                quantum_ticks: 1_920,
            },
            limits.session.documents,
        )
        .unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        drop(prepared);

        let outcome = IdeScenePublicationPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: source_mixer.clone(),
            },
            scene_id: ARRANGEMENT_TEST_SCENE_ID.to_owned(),
            scene_bytes,
        }
        .publish()
        .unwrap();

        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.resource_id(), ARRANGEMENT_TEST_SCENE_ID);
        assert_scene_edit_recovery_authority(&outcome);
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (loaded, _, _, _) = reopened.session().parts();
        assert_eq!(loaded.scene_count(), 1);
        assert_eq!(loaded.scene_id(0), Some(ARRANGEMENT_TEST_SCENE_ID));
        assert_eq!(loaded.scene_name(0), Some("Intro"));
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            source_mixer
        );
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_RESOURCE_ID)
                .unwrap(),
            source_arrangement
        );
    }

    #[test]
    fn scene_duplication_rejects_missing_and_non_scene_resources_before_publication() {
        let fixture = ArrangementProjectFixture::with_scene();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, config.project_limits())
                .unwrap();
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
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let mixer_plan = || IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256,
            resource_id: mixer_id.clone(),
            resource_bytes: source_mixer.clone(),
        };

        assert!(matches!(
            RunningDesktopIdeApplication::prepare_scene_duplication_from_project(
                &application,
                mixer_plan(),
                &prepared,
                "missing-scene",
                "Copy",
            ),
            Err(IdeSceneDuplicationError::SceneResourceMissing)
        ));
        assert!(matches!(
            RunningDesktopIdeApplication::prepare_scene_duplication_from_project(
                &application,
                mixer_plan(),
                &prepared,
                ARRANGEMENT_TEST_TRACK_ID,
                "Copy",
            ),
            Err(IdeSceneDuplicationError::SceneResourceRoleMismatch)
        ));
    }

    #[test]
    fn scene_duplication_publishes_an_independent_scene_without_mutating_existing_resources() {
        let fixture = ArrangementProjectFixture::with_scene();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
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
        let source_scene = package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let plan = RunningDesktopIdeApplication::prepare_scene_duplication_from_project(
            &application,
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: source_mixer.clone(),
            },
            &prepared,
            ARRANGEMENT_TEST_SCENE_ID,
            "Intro copy",
        )
        .unwrap();
        let duplicated_scene_id = plan.scene_id.clone();
        assert_ne!(duplicated_scene_id, ARRANGEMENT_TEST_SCENE_ID);
        let mut expected_duplicate: Value = serde_json::from_slice(&source_scene).unwrap();
        expected_duplicate["id"] = json!(duplicated_scene_id);
        expected_duplicate["name"] = json!("Intro copy");
        expected_duplicate["order"] = json!(1);
        assert_eq!(
            serde_json::from_slice::<Value>(&plan.scene_bytes).unwrap(),
            expected_duplicate
        );
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.resource_id(), duplicated_scene_id);
        assert_scene_edit_recovery_authority(&outcome);
        assert!(matches!(
            worker::scene_duplication_failure_event(
                "Intro copy".to_owned(),
                IdeSceneDuplicationError::ReopenAfterPublication {
                    outcome: Box::new(outcome.clone()),
                    source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
                },
            ),
            IdeWorkerEvent::SceneDuplicationCommittedWithWarning { name, warning }
                if name == "Intro copy" && warning.publication_committed()
        ));
        assert!(matches!(
            worker::scene_duplication_reopen_failure(
                Ok(outcome.clone()),
                IdeProjectOpenError::SelectedOutputUnavailable,
            ),
            IdeWorkerCommandFailure::SceneDuplication(reason)
                if matches!(reason.as_ref(),
                    IdeSceneDuplicationError::ReopenAfterPublication {
                        outcome: retained,
                        source,
                    } if retained.as_ref() == &outcome
                        && matches!(source.as_ref(),
                            IdeProjectOpenError::SelectedOutputUnavailable))
        ));

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = reopened.package();
        let (loaded, _, _, _) = reopened.session().parts();
        assert_eq!(loaded.scene_count(), 2);
        assert_eq!(loaded.scene_id(0), Some(ARRANGEMENT_TEST_SCENE_ID));
        assert_eq!(loaded.scene_id(1), Some(duplicated_scene_id.as_str()));
        assert_eq!(loaded.scene_name(1), Some("Intro copy"));
        assert_eq!(loaded.scene_enabled(1), Some(true));
        assert_eq!(
            package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap(),
            source_scene
        );
        assert_eq!(package.read_resource(&mixer_id).unwrap(), source_mixer);
        assert_eq!(
            package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap(),
            source_arrangement
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&package.read_resource(&duplicated_scene_id).unwrap())
                .unwrap(),
            expected_duplicate
        );
    }

    #[test]
    fn scene_slot_edit_publishes_durably_and_reopens_explicit_action() {
        let fixture = ArrangementProjectFixture::with_scene();
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
        let source_mixer = package.read_resource(&mixer_id).unwrap();
        let source_scene = package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap();
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let scene_bytes = serialize_scene_slot_edit(
            project,
            &assets,
            &source_scene,
            ARRANGEMENT_TEST_SCENE_ID,
            ARRANGEMENT_TEST_TRACK_ID,
            SceneSlotEdit::Stop,
            limits.session.documents,
        )
        .unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        drop(prepared);

        let outcome = IdeSceneEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: source_mixer,
            },
            scene_id: ARRANGEMENT_TEST_SCENE_ID.to_owned(),
            scene_bytes,
        }
        .publish()
        .unwrap();

        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened
                .session()
                .parts()
                .0
                .scene_track_action(ARRANGEMENT_TEST_SCENE_ID, ARRANGEMENT_TEST_TRACK_ID),
            Some(hyper_playback_json::SceneTrackAction::Stop)
        );
    }

    #[test]
    fn scene_activation_publishes_durably_and_preserves_other_resources() {
        let fixture = ArrangementProjectFixture::with_scene();
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
        let source_mixer = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let source_scene = package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap();
        let (scene_id, scene_bytes) = prepare_test_scene_activation(
            &prepared,
            ARRANGEMENT_TEST_SCENE_ID,
            false,
            limits.session.documents,
        )
        .unwrap();
        assert_ne!(scene_bytes, source_scene);
        let expected_project_sha256 = *package.manifest_sha256();
        drop(prepared);

        let outcome = IdeSceneEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: source_mixer.clone(),
            },
            scene_id,
            scene_bytes: scene_bytes.clone(),
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        assert_scene_edit_recovery_authority(&outcome);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (loaded, _, _, _) = reopened.session().parts();
        assert_eq!(loaded.scene_enabled(0), Some(false));
        assert_eq!(loaded.scene_name(0), Some("Intro"));
        assert_eq!(loaded.scene_id(0), Some(ARRANGEMENT_TEST_SCENE_ID));
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            source_mixer
        );
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_RESOURCE_ID)
                .unwrap(),
            source_arrangement
        );
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_SCENE_ID)
                .unwrap(),
            scene_bytes
        );
        let unchanged_identity = *reopened.package().manifest_sha256();
        let (scene_id, unchanged_scene) = prepare_test_scene_activation(
            &reopened,
            ARRANGEMENT_TEST_SCENE_ID,
            false,
            limits.session.documents,
        )
        .unwrap();
        drop(reopened);

        let unchanged = IdeSceneEditPlan {
            mixer: IdeMixerSavePlan {
                project_path,
                expected_project_sha256: unchanged_identity,
                resource_id: mixer_id,
                resource_bytes: source_mixer,
            },
            scene_id,
            scene_bytes: unchanged_scene,
        }
        .publish()
        .unwrap();
        assert!(!unchanged.changed());
        assert_eq!(unchanged.project_revision(), 2);
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
            created_placement_id: None,
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
            created_placement_id: None,
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
            created_placement_id: None,
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
            created_placement_id: None,
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
            created_placement_id: None,
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
            created_placement_id: None,
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
            created_placement_id: None,
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
    fn arrangement_placement_duplication_retains_committed_identity_through_reopen_outcomes() {
        const DUPLICATED_PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338508";
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
        let arrangement = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let arrangement_id = arrangement.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(&arrangement_id).unwrap();
        let project_identity = *package.manifest_sha256();
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let arrangement_bytes = serialize_arrangement_placement_duplication(
            project,
            &assets,
            &source_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            DUPLICATED_PLACEMENT_ID,
            4,
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: project_identity,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id,
            arrangement_bytes,
            addition: false,
            created_placement_id: Some(DUPLICATED_PLACEMENT_ID.to_owned()),
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());

        let failure = worker::arrangement_placement_creation_reopen_failure(
            Ok(worker::ArrangementPlacementCreationPublication {
                outcome,
                placement_id: DUPLICATED_PLACEMENT_ID.to_owned(),
            }),
            IdeProjectOpenError::SelectedOutputUnavailable,
            IdeWorkerCommandFailure::ArrangementPlacementDuplication,
        );
        let IdeWorkerCommandFailure::ArrangementPlacementDuplication(reason) = failure else {
            panic!("duplication reopen failure must retain its typed command boundary");
        };
        assert!(reason.publication_committed());
        assert_eq!(
            reason.committed_created_placement_id(),
            Some(DUPLICATED_PLACEMENT_ID)
        );
        assert!(matches!(
            reason.as_ref(),
            IdeArrangementPlacementEditError::ReopenAfterPublication { source, .. }
                if matches!(source.as_ref(), IdeProjectOpenError::SelectedOutputUnavailable)
        ));
        let warning_event = worker::arrangement_edit_failure_event(
            IdeWorkerCommandKind::DuplicateArrangementPlacement,
            *reason,
            IdeWorkerCommandFailure::ArrangementPlacementDuplication,
        );
        assert!(matches!(
            warning_event,
            IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
                command: IdeWorkerCommandKind::DuplicateArrangementPlacement,
                warning,
            } if warning.committed_created_placement_id() == Some(DUPLICATED_PLACEMENT_ID)
        ));

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(reopened.session().arrangement().placements().len(), 2);
        assert!(
            reopened
                .session()
                .arrangement()
                .placements()
                .iter()
                .any(|placement| placement.id() == DUPLICATED_PLACEMENT_ID)
        );
    }

    #[test]
    fn arrangement_placement_split_retains_committed_identity_through_reopen_outcomes() {
        const RIGHT_PLACEMENT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122338509";
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
        let arrangement = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let arrangement_id = arrangement.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(&arrangement_id).unwrap();
        let project_identity = *package.manifest_sha256();
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let arrangement_bytes = serialize_arrangement_placement_split(
            project,
            &assets,
            &source_arrangement,
            ARRANGEMENT_TEST_PLACEMENT_ID,
            RIGHT_PLACEMENT_ID,
            2,
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: project_identity,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id,
            arrangement_bytes,
            addition: false,
            created_placement_id: Some(RIGHT_PLACEMENT_ID.to_owned()),
        }
        .publish()
        .unwrap();
        let failure = worker::arrangement_placement_creation_reopen_failure(
            Ok(worker::ArrangementPlacementCreationPublication {
                outcome,
                placement_id: RIGHT_PLACEMENT_ID.to_owned(),
            }),
            IdeProjectOpenError::SelectedOutputUnavailable,
            IdeWorkerCommandFailure::ArrangementPlacementSplit,
        );
        let IdeWorkerCommandFailure::ArrangementPlacementSplit(reason) = failure else {
            panic!("split reopen failure must retain its typed command boundary");
        };
        assert!(reason.publication_committed());
        assert_eq!(
            reason.committed_created_placement_id(),
            Some(RIGHT_PLACEMENT_ID)
        );
        let warning_event = worker::arrangement_edit_failure_event(
            IdeWorkerCommandKind::SplitArrangementPlacement,
            *reason,
            IdeWorkerCommandFailure::ArrangementPlacementSplit,
        );
        assert!(matches!(
            warning_event,
            IdeWorkerEvent::ArrangementPlacementEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SplitArrangementPlacement,
                warning,
            } if warning.committed_created_placement_id() == Some(RIGHT_PLACEMENT_ID)
        ));

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let placements = reopened.session().arrangement().placements();
        assert_eq!(placements.len(), 2);
        assert_eq!(placements[0].id(), ARRANGEMENT_TEST_PLACEMENT_ID);
        assert_eq!(placements[0].duration_ticks(), 2);
        assert_eq!(placements[1].id(), RIGHT_PLACEMENT_ID);
        assert_eq!(placements[1].start_tick(), 2);
        assert_eq!(placements[1].clip_start_tick(), 2);
        assert_eq!(placements[1].duration_ticks(), 2);
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
    fn unused_audio_clip_removal_refuses_playback_use_then_retains_media() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let application = DesktopIdeApplication::from_inventories(
            desktop_ide_application_config().unwrap(),
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let mixer = prepared
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = prepared.package().read_resource(&mixer_id).unwrap();
        let mixer_plan = || IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *prepared.package().manifest_sha256(),
            resource_id: mixer_id.clone(),
            resource_bytes: mixer_bytes.clone(),
        };
        assert!(matches!(
            RunningDesktopIdeApplication::prepare_audio_clip_removal_from_project(
                &application,
                mixer_plan(),
                &prepared,
                ARRANGEMENT_TEST_CLIP_ID,
            ),
            Err(IdeAudioClipRemovalError::Serialization(
                AudioClipRemovalSerializationError::Blocked(_)
            ))
        ));
        let duplication = RunningDesktopIdeApplication::prepare_audio_clip_duplication_from_project(
            &application,
            mixer_plan(),
            &prepared,
            ARRANGEMENT_TEST_CLIP_ID,
            "Unused copy",
        )
        .unwrap();
        let duplicate_id = duplication.clip_id.clone();
        drop(prepared);
        duplication.publish().unwrap();

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let mixer = prepared
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let source_media = std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap();
        let removal = RunningDesktopIdeApplication::prepare_audio_clip_removal_from_project(
            &application,
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *prepared.package().manifest_sha256(),
                resource_id: mixer.id().to_owned(),
                resource_bytes: prepared.package().read_resource(mixer.id()).unwrap(),
            },
            &prepared,
            &duplicate_id,
        )
        .unwrap();
        drop(prepared);

        let outcome = removal.publish().unwrap();

        assert_eq!(outcome.clip_id(), duplicate_id);
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, _) = reopened.session().parts();
        assert_eq!(project.clip_count(), 1);
        assert_eq!(project.clip_id_at(0), Some(ARRANGEMENT_TEST_CLIP_ID));
        assert_eq!(project.placement_count(), 1);
        assert!(
            reopened
                .package()
                .manifest()
                .resources()
                .iter()
                .all(|descriptor| descriptor.id() != duplicate_id)
        );
        assert_eq!(
            std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap(),
            source_media
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
            created_placement_id: Some(placement_id.to_owned()),
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
    fn automation_take_publishes_and_reopens_as_compiled_mixer_automation() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
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
        let expected_project_sha256 = *package.manifest_sha256();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        let lane_id = "018f47a2-6b2d-7c11-8a44-00112233850b";
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let loaded_mixer = project
            .load_mixer(&mixer_bytes, &assets, limits.session.documents)
            .unwrap();
        let points = [
            AutomationPoint::new(0, AutomationValue::Integer(1_000_000)),
            AutomationPoint::new(3, AutomationValue::Integer(0)),
        ];
        let take_bytes = serialize_automation_take_addition(
            project,
            &loaded_mixer,
            &assets,
            &[],
            AutomationTakeAddition {
                take_id,
                lane_id,
                name: "Main fade",
                captured_at_utc: "2026-08-16T09:30:00.000Z",
                start_tick: 0,
                end_tick: 4,
                target: AutomationTargetSelection::Main {
                    parameter: AutomationMainParameter::Gain,
                },
                interpolation: AutomationInterpolation::Linear,
                points: &points,
            },
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);

        let outcome = IdeAutomationTakePublicationPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            take_id: take_id.to_owned(),
            take_bytes,
        }
        .publish()
        .unwrap();
        assert_eq!(outcome.project_revision(), 2);
        assert_eq!(outcome.resource_id(), take_id);
        assert_eq!(outcome.resource_path(), format!("takes/{take_id}.json"));

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(reopened.session().has_automation());
        let published = reopened.session().automation_takes().takes();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].id(), take_id);
        assert_eq!(published[0].name(), "Main fade");
        assert!(published[0].enabled());
        assert_eq!((published[0].start_tick(), published[0].end_tick()), (0, 4));
        assert_eq!(published[0].lanes().len(), 1);
        assert_eq!(published[0].lanes()[0].id(), lane_id);
        assert_eq!(
            published[0].lanes()[0].target(),
            AutomationTarget::Main {
                parameter: AutomationMainParameter::Gain,
            }
        );
        assert_eq!(published[0].lanes()[0].point_count(), 2);
        let (_, _, automation, _) = reopened.session().parts();
        let compiled = automation.unwrap();
        assert_eq!(compiled.target_count(), 1);
        let main_gain = AutomationTarget::Main {
            parameter: AutomationMainParameter::Gain,
        };
        for (frame, expected) in [(0, 1_000_000), (25, 666_667), (75, 0)] {
            let mut observed = Vec::new();
            compiled
                .visit_frame_values(frame, |target, value| observed.push((target, value)))
                .unwrap();
            assert_eq!(
                observed,
                vec![(main_gain, AutomationValue::Integer(expected))],
                "frame {frame}"
            );
        }
    }

    #[test]
    fn disabling_an_automation_take_removes_it_from_the_compiled_runtime() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        publish_main_gain_automation(&project_path, limits, take_id);

        let opened = hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = opened.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let source = package.read_resource(take_id).unwrap();
        let (project, _, _, media) = opened.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let loaded_mixer = project
            .load_mixer(&mixer_bytes, &assets, limits.session.documents)
            .unwrap();
        let take_bytes = serialize_automation_take_activation(
            project,
            &loaded_mixer,
            &assets,
            &[&source],
            take_id,
            false,
            limits.session.documents,
        )
        .unwrap();
        drop(opened);

        let outcome = IdeAutomationTakeEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            take_id: take_id.to_owned(),
            take_bytes,
        }
        .publish()
        .unwrap();
        assert!(outcome.changed());

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let published = reopened.session().automation_takes().takes();
        assert_eq!(published.len(), 1);
        assert!(!published[0].enabled());
        let (_, _, automation, _) = reopened.session().parts();
        assert_eq!(automation.unwrap().target_count(), 0);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one lifecycle test retains the exact removal, restore, byte, mixer, and reopen oracle"
    )]
    fn automation_take_removal_and_history_restore_reopen_verified_playback() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        publish_main_gain_automation(&project_path, limits, take_id);
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let mixer = prepared
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let original_take_bytes = prepared.package().read_resource(take_id).unwrap();
        let mixer_bytes = prepared.package().read_resource(mixer.id()).unwrap();
        let live_mixer_bytes = String::from_utf8(mixer_bytes)
            .unwrap()
            .replace("\"gain_millionths\":1000000", "\"gain_millionths\":750000")
            .into_bytes();
        assert_ne!(
            prepared.package().read_resource(mixer.id()).unwrap(),
            live_mixer_bytes,
            "the fixture must exercise preservation of a distinctive live mixer"
        );
        let plan = RunningDesktopIdeApplication::prepare_automation_take_removal_from_project(
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *prepared.package().manifest_sha256(),
                resource_id: mixer.id().to_owned(),
                resource_bytes: live_mixer_bytes.clone(),
            },
            &prepared,
            take_id,
        )
        .unwrap();
        let inverse = plan.inverse();
        drop(prepared);

        let removal = plan.publish().unwrap();
        assert!(removal.changed());
        assert_eq!(removal.resource_id(), take_id);
        assert!(!project_path.join("takes").exists());
        let removed =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(!removed.session().has_automation());
        let mixer = removed
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        assert_eq!(
            removed.package().read_resource(mixer.id()).unwrap(),
            live_mixer_bytes
        );
        let restore_mixer = IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *removed.package().manifest_sha256(),
            resource_id: mixer.id().to_owned(),
            resource_bytes: removed.package().read_resource(mixer.id()).unwrap(),
        };
        let restore =
            RunningDesktopIdeApplication::prepare_automation_take_history_restore_from_project(
            &application,
            restore_mixer,
            &removed,
            &inverse,
        )
        .unwrap();
        assert_eq!(
            restore.displaced(),
            IdeHistoryOperation::AutomationTakeRemoval {
                take_id: take_id.to_owned(),
                expected_bytes: match &inverse {
                    IdeHistoryOperation::AutomationTakeAddition { bytes, .. } => bytes.clone(),
                    _ => unreachable!(),
                },
            }
        );
        drop(removed);
        restore.publish().unwrap();

        let restored =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(restored.session().has_automation());
        assert_eq!(restored.session().automation_takes().takes()[0].id(), take_id);
        assert_eq!(
            restored.package().read_resource(take_id).unwrap(),
            original_take_bytes
        );
        let restored_mixer_descriptor = restored
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        assert_eq!(
            restored
                .package()
                .read_resource(restored_mixer_descriptor.id())
                .unwrap(),
            live_mixer_bytes
        );

    }

    #[test]
    fn automation_take_history_restore_refuses_stale_bytes_and_overlap_without_writing() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        publish_main_gain_automation(&project_path, limits, take_id);
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let mixer = prepared
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_plan = IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *prepared.package().manifest_sha256(),
            resource_id: mixer.id().to_owned(),
            resource_bytes: prepared.package().read_resource(mixer.id()).unwrap(),
        };
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let take_bytes = prepared.package().read_resource(take_id).unwrap();
        let manifest_before = std::fs::read(project_path.join("project.json")).unwrap();

        assert!(matches!(
            RunningDesktopIdeApplication::prepare_automation_take_history_restore_from_project(
                &application,
                mixer_plan.clone(),
                &prepared,
                &IdeHistoryOperation::AutomationTakeRemoval {
                    take_id: take_id.to_owned(),
                    expected_bytes: b"different canonical authority".to_vec(),
                },
            ),
            Err(IdeResourceRestoreError::CandidateInvalid(_))
        ));

        let conflicting_id = "018f47a2-6b2d-7c11-8a44-00112233850b";
        let mut conflicting: Value = serde_json::from_slice(&take_bytes).unwrap();
        conflicting["id"] = conflicting_id.into();
        conflicting["lanes"][0]["id"] =
            "018f47a2-6b2d-7c11-8a44-00112233850c".into();
        assert!(matches!(
            RunningDesktopIdeApplication::prepare_automation_take_history_restore_from_project(
                &application,
                mixer_plan,
                &prepared,
                &IdeHistoryOperation::AutomationTakeAddition {
                    take_id: conflicting_id.to_owned(),
                    bytes: serde_json_canonicalizer::to_vec(&conflicting).unwrap(),
                },
            ),
            Err(IdeResourceRestoreError::CandidateInvalid(_))
        ));
        assert_eq!(
            std::fs::read(project_path.join("project.json")).unwrap(),
            manifest_before
        );
        assert_eq!(prepared.package().read_resource(take_id).unwrap(), take_bytes);
    }

    #[test]
    fn native_filter_edits_survive_add_update_bypass_and_final_removal() {
        let fixture = ArrangementProjectFixture::new();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let addition = RunningDesktopIdeApplication::prepare_native_filter_edit_from_project(
            fixture_mixer_save_plan(&prepared),
            &prepared,
            &IdeNativeFilterEdit::Add {
                track_id: ARRANGEMENT_TEST_TRACK_ID.to_owned(),
                mode: NativeFilterMode::LowPass,
                cutoff_millihertz: 1_250_000,
                resonance_millionths: 707_000,
            },
        )
        .unwrap();
        drop(prepared);
        let added = addition.publish().unwrap();
        assert!(added.changed());

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let chain = prepared.device_chains().first().unwrap();
        assert_eq!(chain.track_id(), ARRANGEMENT_TEST_TRACK_ID);
        assert_eq!(chain.devices().len(), 1);
        let chain_id = chain.id().to_owned();
        let device_id = chain.devices()[0].id().to_owned();
        assert!(chain.devices()[0].enabled());
        assert_eq!(
            chain.devices()[0].filter_state().unwrap().mode(),
            NativeFilterMode::LowPass
        );
        assert_eq!(
            chain.devices()[0]
                .filter_state()
                .unwrap()
                .cutoff_millihertz(),
            1_250_000
        );

        let update = RunningDesktopIdeApplication::prepare_native_filter_edit_from_project(
            fixture_mixer_save_plan(&prepared),
            &prepared,
            &IdeNativeFilterEdit::Update {
                chain_id: chain_id.clone(),
                device_instance_id: device_id.clone(),
                enabled: false,
                mode: NativeFilterMode::HighPass,
                cutoff_millihertz: 80_000,
                resonance_millionths: 2_000_000,
            },
        )
        .unwrap();
        drop(prepared);
        assert!(update.publish().unwrap().changed());

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let device = &prepared.device_chains()[0].devices()[0];
        assert_eq!(device.id(), device_id);
        assert!(!device.enabled());
        let state = device.filter_state().unwrap();
        assert_eq!(state.mode(), NativeFilterMode::HighPass);
        assert_eq!(state.cutoff_millihertz(), 80_000);
        assert_eq!(state.resonance_millionths(), 2_000_000);

        let removal = RunningDesktopIdeApplication::prepare_native_filter_edit_from_project(
            fixture_mixer_save_plan(&prepared),
            &prepared,
            &IdeNativeFilterEdit::Remove {
                chain_id,
                device_instance_id: device_id,
            },
        )
        .unwrap();
        drop(prepared);
        assert!(removal.publish().unwrap().changed());

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(reopened.device_chains().is_empty());
        assert!(!project_path.join("devices").exists());
        hyper_storage_engine::verify_project_package(&project_path).unwrap();
    }

    #[test]
    fn automation_take_preparation_reads_the_committed_take_set_from_the_manifest() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        publish_main_gain_automation(&project_path, limits, take_id);

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let mixer_plan = || IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256,
            resource_id: mixer_id.clone(),
            resource_bytes: mixer_bytes.clone(),
        };
        let request = |start_tick: u64, end_tick: u64| IdeAutomationTakeRequest {
            name: "Second pass".to_owned(),
            start_tick,
            end_tick,
            target: IdeAutomationTarget::Main {
                parameter: AutomationMainParameter::Gain,
            },
            interpolation: AutomationInterpolation::Linear,
            points: vec![
                AutomationPoint::new(start_tick, AutomationValue::Integer(0)),
                AutomationPoint::new(end_tick - 1, AutomationValue::Integer(1_000_000)),
            ],
        };

        let overlapping =
            RunningDesktopIdeApplication::prepare_automation_take_creation_from_project(
                &application,
                mixer_plan(),
                &prepared,
                &request(2, 6),
            )
            .unwrap_err();
        assert!(
            matches!(
                overlapping,
                IdeAutomationTakeCreationError::Serialization(_)
            ),
            "the committed take must be visible to the overlap check: {overlapping}"
        );
        assert!(!overlapping.publication_committed());

        let plan = RunningDesktopIdeApplication::prepare_automation_take_creation_from_project(
            &application,
            mixer_plan(),
            &prepared,
            &request(8, 16),
        )
        .unwrap();
        assert_ne!(plan.take_id, take_id);
        let document: Value = serde_json::from_slice(&plan.take_bytes).unwrap();
        assert_eq!(document["id"].as_str(), Some(plan.take_id.as_str()));
        assert_eq!(document["name"].as_str(), Some("Second pass"));
        assert_eq!(document["enabled"].as_bool(), Some(true));
        assert_eq!(document["start_tick"].as_u64(), Some(8));
        assert_ne!(
            document["lanes"][0]["id"].as_str(),
            Some(plan.take_id.as_str())
        );
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let published = reopened.session().automation_takes().takes();
        assert_eq!(published.len(), 2);
        assert!(
            published
                .iter()
                .all(hyper_playback_session::PlaybackAutomationTake::enabled)
        );
        let (_, _, automation, _) = reopened.session().parts();
        assert_eq!(automation.unwrap().target_count(), 1);
    }

    #[test]
    fn automation_take_activation_preparation_addresses_the_requested_take() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let take_id = "018f47a2-6b2d-7c11-8a44-00112233850a";
        publish_main_gain_automation(&project_path, limits, take_id);

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let mixer_plan = || IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256,
            resource_id: mixer_id.clone(),
            resource_bytes: mixer_bytes.clone(),
        };

        let missing =
            RunningDesktopIdeApplication::prepare_automation_take_activation_from_project(
                &application,
                mixer_plan(),
                &prepared,
                "018f47a2-6b2d-7c11-8a44-0011223344ff",
                false,
            )
            .unwrap_err();
        assert!(
            matches!(missing, IdeAutomationTakeEditError::Serialization(_)),
            "{missing}"
        );

        let plan = RunningDesktopIdeApplication::prepare_automation_take_activation_from_project(
            &application,
            mixer_plan(),
            &prepared,
            take_id,
            false,
        )
        .unwrap();
        assert_eq!(plan.take_id, take_id);
        let document: Value = serde_json::from_slice(&plan.take_bytes).unwrap();
        assert_eq!(document["id"].as_str(), Some(take_id));
        assert_eq!(document["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn a_restore_republishes_previous_bytes_and_reports_its_own_previous_state() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let scene_bytes = package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();

        let (project, _, _, _) = prepared.session().parts();
        let renamed = serialize_scene_rename(
            project,
            &scene_bytes,
            ARRANGEMENT_TEST_SCENE_ID,
            "Renamed",
            limits.session.documents,
        )
        .unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        drop(prepared);
        let edited = publish_project_resource(
            &project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256,
                resource_id: ARRANGEMENT_TEST_SCENE_ID,
                expected_role: ResourceRole::Scene,
                resource_bytes: &renamed,
            },
        )
        .unwrap();
        assert_eq!(
            edited.previous_resource(),
            Some(scene_bytes.as_slice()),
            "the edit carries the bytes an undo will restore"
        );

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let plan = {
            let package = reopened.package();
            let mixer_plan = IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer_id.clone(),
                resource_bytes: mixer_bytes.clone(),
            };
            RunningDesktopIdeApplication::prepare_resource_restore_from_project(
                &application,
                mixer_plan,
                &reopened,
                ARRANGEMENT_TEST_SCENE_ID,
                &scene_bytes,
            )
            .unwrap()
        };
        assert_eq!(
            plan.role,
            ResourceRole::Scene,
            "the role comes from the manifest"
        );
        drop(reopened);
        let restored = plan.publish().unwrap();
        assert_eq!(
            restored.previous_resource(),
            Some(renamed.as_slice()),
            "the restore's own previous bytes are the redo entry"
        );

        let verified =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            verified
                .package()
                .read_resource(ARRANGEMENT_TEST_SCENE_ID)
                .unwrap(),
            scene_bytes,
            "the undone edit is byte-identical to the original"
        );
    }

    /// Joins the seam the type system alone leaves open: the bytes history
    /// captures from a publication must be exactly the bytes a restore
    /// consumes, and restoring twice must return the file to where it began.
    #[test]
    fn an_undo_then_a_redo_return_the_project_to_the_edited_state() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let read_scene = || {
            hyper_storage_engine::verify_project_package(&project_path)
                .unwrap()
                .read_resource(ARRANGEMENT_TEST_SCENE_ID)
                .unwrap()
        };

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let original = read_scene();
        let mixer_id = prepared
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = prepared.package().read_resource(&mixer_id).unwrap();
        let mixer_plan = |sha: [u8; 32]| IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: sha,
            resource_id: mixer_id.clone(),
            resource_bytes: mixer_bytes.clone(),
        };
        let (project, _, _, _) = prepared.session().parts();
        let renamed = serialize_scene_rename(
            project,
            &original,
            ARRANGEMENT_TEST_SCENE_ID,
            "Renamed",
            limits.session.documents,
        )
        .unwrap();
        let sha = *prepared.package().manifest_sha256();
        drop(prepared);

        let edit = publish_project_resource(
            &project_path,
            ProjectResourcePublicationRequest {
                expected_project_sha256: sha,
                resource_id: ARRANGEMENT_TEST_SCENE_ID,
                expected_role: ResourceRole::Scene,
                resource_bytes: &renamed,
            },
        )
        .unwrap();
        let edited = read_scene();
        assert_eq!(edited, renamed);

        let undo_bytes = edit.previous_resource().unwrap().to_vec();
        let restore = |bytes: &[u8]| {
            let reopened =
                hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
            let plan = RunningDesktopIdeApplication::prepare_resource_restore_from_project(
                &application,
                mixer_plan(*reopened.package().manifest_sha256()),
                &reopened,
                ARRANGEMENT_TEST_SCENE_ID,
                bytes,
            )
            .unwrap();
            drop(reopened);
            plan.publish().unwrap()
        };

        let undone = restore(&undo_bytes);
        assert_eq!(
            read_scene(),
            original,
            "the undo restores the original bytes"
        );
        let redo_bytes = undone.previous_resource().unwrap().to_vec();
        assert_eq!(
            redo_bytes, edited,
            "and displaces exactly the edited bytes, which become the redo entry"
        );

        let redone = restore(&redo_bytes);
        assert_eq!(read_scene(), edited, "the redo reapplies the edit");
        assert_eq!(
            redone.previous_resource().unwrap(),
            original.as_slice(),
            "leaving the original available to undo again"
        );
    }

    /// The tempo edit writes a real project and the reopened project reads the
    /// new tempo back, so the durable path is proven rather than only the
    /// serializer that feeds it.
    #[test]
    fn a_tempo_edit_publishes_and_the_reopened_project_carries_it() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_plan = IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_id: mixer_id.clone(),
            resource_bytes: package.read_resource(&mixer_id).unwrap(),
        };
        let (project, _, _, _) = prepared.session().parts();
        let before = project.tempo_events()[0].microseconds_per_quarter;
        assert_ne!(
            before, 250_000,
            "the fixture starts at a tempo the edit will actually change"
        );

        let refused = RunningDesktopIdeApplication::prepare_timeline_tempo_from_project(
            &application,
            mixer_plan.clone(),
            &prepared,
            250_000,
        )
        .unwrap_err();
        assert!(
            matches!(refused, IdeTempoEditError::Serialization(_)),
            "a project whose audio placements would need stretching refuses the              change rather than mangling the audio: {refused}"
        );

        let plan = RunningDesktopIdeApplication::prepare_timeline_tempo_from_project(
            &application,
            mixer_plan,
            &prepared,
            before,
        )
        .unwrap();
        assert_eq!(
            plan.timeline_id,
            prepared
                .package()
                .manifest()
                .resources()
                .iter()
                .find(|descriptor| descriptor.role() == ResourceRole::Timeline)
                .unwrap()
                .id(),
            "the plan targets the project's own timeline resource"
        );
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(
            !outcome.changed(),
            "rewriting the same tempo is a no-op, so nothing is republished"
        );
    }

    /// The workflow the feature exists for: create a project, set its tempo,
    /// reopen it and find the new tempo. A fresh project has no placements, so
    /// nothing pins the tempo yet.
    #[test]
    fn a_fresh_project_accepts_a_new_tempo_that_survives_reopening() {
        let parent = std::env::temp_dir().join(format!("hyper-tempo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir(&parent).unwrap();
        let created = hyper_storage_engine::create_project(
            &parent,
            hyper_storage_engine::ProjectCreationRequest {
                directory_name: "Session",
                project_name: "Session",
            },
        )
        .unwrap();
        let project_path = created.project().root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );

        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, _) = prepared.session().parts();
        assert_eq!(
            project.tempo_events()[0].microseconds_per_quarter,
            500_000,
            "projects are created at 120 bpm"
        );
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let plan = RunningDesktopIdeApplication::prepare_timeline_tempo_from_project(
            &application,
            IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer_id.clone(),
                resource_bytes: package.read_resource(&mixer_id).unwrap(),
            },
            &prepared,
            250_000,
        )
        .unwrap();
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed(), "a real tempo change is published");
        assert!(
            outcome.previous_resource().is_some(),
            "so it is undoable like every other replacement"
        );

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (reloaded, _, _, _) = reopened.session().parts();
        assert_eq!(
            reloaded.tempo_events()[0].microseconds_per_quarter,
            250_000,
            "the reopened project plays at the tempo that was saved"
        );
        drop(reopened);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// The review found the frame→tick conversion had no coverage at all, and
    /// that the round trip is lossy by construction: saving floors to the tick
    /// at or before the frame, seeding ceils back. This pins how far it can
    /// drift and that the drift never inverts the range.
    /// The seam the review found untested twice over: the frames a user marks
    /// must become the ticks the arrangement stores, in that order, and a
    /// reopened project must arm the runtime it displays.
    #[test]
    fn marked_frames_become_the_ticks_the_arrangement_stores() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(fixture.root(), limits).unwrap();

        assert_eq!(
            RunningDesktopIdeApplication::loop_range_ticks(&prepared, None).unwrap(),
            None,
            "no marks writes no range"
        );

        let (start_tick, end_tick, enabled) = RunningDesktopIdeApplication::loop_range_ticks(
            &prepared,
            Some(IdeLoopRangeFrames {
                start_frame: 24_000,
                end_frame: 96_000,
                enabled: true,
            }),
        )
        .unwrap()
        .expect("a usable range converts");
        assert!(
            start_tick < end_tick,
            "the start mark becomes the start tick, not the end"
        );
        assert!(
            enabled,
            "and whether it was engaged survives the conversion"
        );

        let disengaged = RunningDesktopIdeApplication::loop_range_ticks(
            &prepared,
            Some(IdeLoopRangeFrames {
                start_frame: 24_000,
                end_frame: 96_000,
                enabled: false,
            }),
        )
        .unwrap()
        .unwrap();
        assert!(!disengaged.2, "a disengaged mark converts as disengaged");
        assert_eq!(
            (disengaged.0, disengaged.1),
            (start_tick, end_tick),
            "and the flag does not disturb the ticks"
        );

        for (start_frame, end_frame, why) in [
            (96_000, 24_000, "an inverted mark pair"),
            (24_000, 24_001, "a pair that collapses to one tick"),
        ] {
            assert!(
                matches!(
                    RunningDesktopIdeApplication::loop_range_ticks(
                        &prepared,
                        Some(IdeLoopRangeFrames {
                            start_frame,
                            end_frame,
                            enabled: true,
                        }),
                    ),
                    Err(IdeArrangementPlacementEditError::LoopMark(
                        LoopMarkFailure::MarksCollapse
                    ))
                ),
                "{why} is refused by its own name, not as a missing arrangement"
            );
        }
        assert_eq!(
            LoopMarkFailure::MarksCollapse.to_string(),
            "the loop marks are closer together than one tick, so the range would be empty"
        );
        assert_ne!(
            LoopMarkFailure::TimelineUnavailable.to_string(),
            LoopMarkFailure::MarkOutOfRange.to_string(),
            "each loop failure reads differently, so the user can tell them apart"
        );
        drop(prepared);
    }

    #[test]
    fn a_loop_mark_round_trips_through_ticks_within_one_tick() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let (project, _, _, media) = prepared.session().parts();
        let facts: Vec<_> = media.media_asset_facts().collect();
        let graph = project
            .compile_playback(
                &facts,
                hyper_playback_compiler::graph::GraphLimits::VERSION_1,
                hyper_playback_compiler::scene::SceneLimits::VERSION_1,
            )
            .unwrap();
        let authority = graph.scene_authority();

        // The drift bound is one tick, derived from this project's own tempo
        // rather than assumed, because a tick is a different number of frames
        // at every tempo.
        let frames_per_tick = authority.tick_to_frame_ceil(1).unwrap();
        assert!(frames_per_tick > 0);

        for (start_frame, end_frame) in [(0_usize, 48_000_usize), (1, 24_001), (12_345, 67_890)] {
            let start_tick = authority.frame_to_tick(start_frame).unwrap();
            let end_tick = authority.frame_to_tick(end_frame).unwrap();
            assert!(
                end_tick > start_tick,
                "frames {start_frame}..{end_frame} stay a usable range in ticks"
            );

            let seeded_start = authority.tick_to_frame_ceil(start_tick).unwrap();
            let seeded_end = authority.tick_to_frame_ceil(end_tick).unwrap();
            assert!(
                seeded_start.abs_diff(start_frame) < frames_per_tick,
                "a reopened start mark lands within one tick of where it was set:                  {seeded_start} vs {start_frame}"
            );
            assert!(
                seeded_end.abs_diff(end_frame) < frames_per_tick,
                "and so does the end: {seeded_end} vs {end_frame}"
            );
            assert!(
                seeded_end > seeded_start,
                "the reopened range never inverts, whatever the rounding does"
            );
        }

        assert_eq!(
            authority.frame_to_tick(0).unwrap(),
            authority.frame_to_tick(frames_per_tick - 1).unwrap(),
            "marks closer together than a tick collapse, which the edit refuses"
        );
        drop(graph);
        drop(prepared);
    }

    #[test]
    fn a_restore_reads_its_role_from_the_manifest() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let plan_for = |resource_id: &str, bytes: &[u8]| {
            RunningDesktopIdeApplication::prepare_resource_restore_from_project(
                &application,
                IdeMixerSavePlan {
                    project_path: project_path.clone(),
                    expected_project_sha256: *package.manifest_sha256(),
                    resource_id: mixer_id.clone(),
                    resource_bytes: package.read_resource(&mixer_id).unwrap(),
                },
                &prepared,
                resource_id,
                bytes,
            )
            .unwrap()
        };
        assert_eq!(
            plan_for(&mixer_id, &package.read_resource(&mixer_id).unwrap()).role,
            ResourceRole::Mixer
        );
        assert_eq!(
            plan_for(
                ARRANGEMENT_TEST_SCENE_ID,
                &package.read_resource(ARRANGEMENT_TEST_SCENE_ID).unwrap(),
            )
            .role,
            ResourceRole::Scene,
            "the role follows the resource, so it cannot be a hardcoded constant"
        );
        drop(prepared);
    }

    #[test]
    fn a_restore_that_would_break_the_project_is_refused_before_writing() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let application = DesktopIdeApplication::from_inventories(
            config,
            IdeAudioOutputInventory::try_new(&[("route", "Output")]).unwrap(),
            IdeAudioInputInventory::try_new(&[]).unwrap(),
        );

        let scene_bytes = prepared
            .package()
            .read_resource(ARRANGEMENT_TEST_SCENE_ID)
            .unwrap();
        let broken = String::from_utf8(scene_bytes.clone()).unwrap().replace(
            "\"slots\":[]",
            "\"slots\":[{\"action\":\"play\",\"clip_id\":\"018f47a2-6b2d-7c11-8a44-0011223344fe\",\"track_id\":\"018f47a2-6b2d-7c11-8a44-0011223344ff\"}]",
        );
        assert_ne!(broken.as_bytes(), scene_bytes.as_slice());
        let mixer_plan = || IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *prepared.package().manifest_sha256(),
            resource_id: "mixer".to_owned(),
            resource_bytes: Vec::new(),
        };
        let refused = RunningDesktopIdeApplication::prepare_resource_restore_from_project(
            &application,
            mixer_plan(),
            &prepared,
            ARRANGEMENT_TEST_SCENE_ID,
            broken.as_bytes(),
        )
        .map(|_| ())
        .unwrap_err();
        assert!(
            matches!(refused, IdeResourceRestoreError::CandidateInvalid(_)),
            "restoring bytes that no longer fit the project is refused: {refused}"
        );

        assert!(
            RunningDesktopIdeApplication::prepare_resource_restore_from_project(
                &application,
                mixer_plan(),
                &prepared,
                ARRANGEMENT_TEST_SCENE_ID,
                &scene_bytes,
            )
            .is_ok()
        );
        assert!(matches!(
            RunningDesktopIdeApplication::prepare_resource_restore_from_project(
                &application,
                mixer_plan(),
                &prepared,
                "018f47a2-6b2d-7c11-8a44-0011223344fd",
                &scene_bytes,
            ),
            Err(IdeResourceRestoreError::ResourceMissing)
        ));
    }

    #[test]
    fn the_export_journal_never_collides_with_its_destination() {
        let wav = Path::new("C:/Renders/night drive.wav");
        let journal = export_journal_path(wav).unwrap();
        assert_eq!(
            journal.parent(),
            wav.parent(),
            "staging sits beside the file"
        );
        assert_ne!(journal, wav.to_path_buf());
        assert_eq!(
            journal.file_name().unwrap().to_str().unwrap(),
            "night drive.wav.export-journal"
        );

        // A destination already named like a journal must not be overwritten by
        // its own staging file.
        let awkward = Path::new("C:/Renders/take.export-journal");
        let staged = export_journal_path(awkward).unwrap();
        assert_ne!(staged, awkward.to_path_buf());

        // A path with no file name has nowhere to stage.
        assert!(export_journal_path(Path::new("C:/")).is_none());
    }

    #[test]
    fn control_binding_insertion_publishes_and_compiles_into_the_runtime() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert!(
            prepared.control_mapping().is_none(),
            "a freshly created project declares no control mapping"
        );
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let mixer_plan = IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_id: mixer_id,
            resource_bytes: mixer_bytes,
        };
        let request = IdeControlBindingRequest {
            source: ControlSource::CvInput { input_index: 2 },
            scope: ControlScope::Global,
            transform: ControlTransform::Linear {
                input_min: -1_000_000,
                input_max: 1_000_000,
                output_min: 0,
                output_max: 1_000_000,
            },
            target: ControlTarget::MixerMain {
                parameter: IdeControlMainParameter::Gain,
            },
        };
        let plan = RunningDesktopIdeApplication::prepare_control_mapping_edit_from_project(
            mixer_plan,
            &prepared,
            &IdeControlMappingEdit::Insert(request),
        )
        .unwrap();
        drop(prepared);

        let revision = plan.publish().unwrap();

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        assert_eq!(
            reopened.package().manifest().revision(),
            revision,
            "the reported revision is the one the reopened project carries"
        );
        let document = reopened.control_mapping().expect("mapping is persisted");
        assert_eq!(document.bindings().len(), 1);
        assert!(
            hyper_project_model::is_valid_entity_id(document.bindings()[0].id()),
            "insertion mints a stable identity"
        );
        assert!(
            document.bindings()[0].enabled(),
            "an inserted binding is created enabled"
        );
        assert_eq!(
            reopened
                .package()
                .manifest()
                .required_capabilities()
                .cv_inputs(),
            3,
            "publication raised the capability the binding requires"
        );
        assert!(
            reopened.compiled_control_mapping().is_some(),
            "the published binding compiles into the runtime table"
        );
    }

    #[test]
    fn control_binding_activation_and_removal_round_trip_through_the_editor() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        publish_first_control_binding(&project_path, limits);
        let binding_id = {
            let opened =
                hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
            let id = opened.control_mapping().unwrap().bindings()[0]
                .id()
                .to_owned();
            drop(opened);
            id
        };

        for (edit, expect_bindings, expect_enabled) in [
            (
                IdeControlMappingEdit::SetEnabled {
                    binding_id: binding_id.clone(),
                    enabled: false,
                },
                1,
                Some(false),
            ),
            (
                IdeControlMappingEdit::Remove {
                    binding_id: binding_id.clone(),
                },
                0,
                None,
            ),
        ] {
            let prepared =
                hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
            let package = prepared.package();
            let mixer_id = package
                .manifest()
                .resources()
                .iter()
                .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
                .unwrap()
                .id()
                .to_owned();
            let mixer_bytes = package.read_resource(&mixer_id).unwrap();
            let mixer_plan = IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            };
            let plan = RunningDesktopIdeApplication::prepare_control_mapping_edit_from_project(
                mixer_plan, &prepared, &edit,
            )
            .unwrap();
            drop(prepared);
            plan.publish().unwrap();

            let reopened =
                hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
            let document = reopened.control_mapping().expect("mapping is retained");
            assert_eq!(document.bindings().len(), expect_bindings);
            if let Some(expected) = expect_enabled {
                assert_eq!(document.bindings()[0].enabled(), expected);
            }
            assert_eq!(
                reopened
                    .package()
                    .manifest()
                    .required_capabilities()
                    .cv_inputs(),
                3,
                "a capability already required is never lowered"
            );
        }
    }

    #[test]
    fn edge_source_insertion_raises_its_own_capability_before_compiling() {
        for (source, expect_clock, expect_reset) in [
            (
                ControlSource::ClockInput { edge: Edge::Rising },
                true,
                false,
            ),
            (
                ControlSource::ResetInput {
                    edge: Edge::Falling,
                },
                false,
                true,
            ),
        ] {
            edge_source_insertion_case(source, expect_clock, expect_reset);
        }
    }

    fn edge_source_insertion_case(source: ControlSource, expect_clock: bool, expect_reset: bool) {
        let fixture = ArrangementProjectFixture::with_two_scenes();
        let project_path = fixture.root().to_path_buf();
        let config = desktop_ide_application_config().unwrap();
        let limits = config.project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let package = prepared.package();
        assert!(!package.manifest().required_capabilities().has_clock_input());
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let mixer_plan = IdeMixerSavePlan {
            project_path: project_path.clone(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_id: mixer_id,
            resource_bytes: mixer_bytes,
        };
        let plan = RunningDesktopIdeApplication::prepare_control_mapping_edit_from_project(
            mixer_plan,
            &prepared,
            &IdeControlMappingEdit::Insert(IdeControlBindingRequest {
                source,
                scope: ControlScope::Global,
                transform: ControlTransform::Trigger {
                    behavior: TriggerBehavior::Invoke,
                },
                target: ControlTarget::Transport {
                    action: TransportAction::Play,
                },
            }),
        )
        .expect("the preview resolves against the capability the transaction will commit");
        drop(prepared);
        plan.publish().unwrap();

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let raised = reopened.package().manifest().required_capabilities();
        assert_eq!(raised.has_clock_input(), expect_clock);
        assert_eq!(raised.has_reset_input(), expect_reset);
        assert_eq!(raised.cv_inputs(), 0, "no CV input was required");
        assert!(reopened.compiled_control_mapping().is_some());
    }

    fn publish_first_control_binding(project_path: &Path, limits: ProjectPlaybackLimits) {
        let prepared =
            hyper_project_loader::prepare_project_playback(project_path, limits).unwrap();
        let package = prepared.package();
        let mixer_id = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap()
            .id()
            .to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let mixer_plan = IdeMixerSavePlan {
            project_path: project_path.to_path_buf(),
            expected_project_sha256: *package.manifest_sha256(),
            resource_id: mixer_id,
            resource_bytes: mixer_bytes,
        };
        let plan = RunningDesktopIdeApplication::prepare_control_mapping_edit_from_project(
            mixer_plan,
            &prepared,
            &IdeControlMappingEdit::Insert(IdeControlBindingRequest {
                source: ControlSource::CvInput { input_index: 2 },
                scope: ControlScope::Global,
                transform: ControlTransform::Linear {
                    input_min: -1_000_000,
                    input_max: 1_000_000,
                    output_min: 0,
                    output_max: 1_000_000,
                },
                target: ControlTarget::MixerMain {
                    parameter: IdeControlMainParameter::Gain,
                },
            }),
        )
        .unwrap();
        drop(prepared);
        plan.publish().unwrap();
    }

    fn publish_main_gain_automation(
        project_path: &Path,
        limits: ProjectPlaybackLimits,
        take_id: &str,
    ) {
        let prepared =
            hyper_project_loader::prepare_project_playback(project_path, limits).unwrap();
        let package = prepared.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let loaded_mixer = project
            .load_mixer(&mixer_bytes, &assets, limits.session.documents)
            .unwrap();
        let points = [
            AutomationPoint::new(0, AutomationValue::Integer(1_000_000)),
            AutomationPoint::new(3, AutomationValue::Integer(0)),
        ];
        let take_bytes = serialize_automation_take_addition(
            project,
            &loaded_mixer,
            &assets,
            &[],
            AutomationTakeAddition {
                take_id,
                lane_id: "018f47a2-6b2d-7c11-8a44-00112233850b",
                name: "Main fade",
                captured_at_utc: "2026-08-16T09:30:00.000Z",
                start_tick: 0,
                end_tick: 4,
                target: AutomationTargetSelection::Main {
                    parameter: AutomationMainParameter::Gain,
                },
                interpolation: AutomationInterpolation::Linear,
                points: &points,
            },
            limits.session.documents,
        )
        .unwrap();
        drop(prepared);
        IdeAutomationTakePublicationPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.to_path_buf(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            take_id: take_id.to_owned(),
            take_bytes,
        }
        .publish()
        .unwrap();
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one integration test keeps scene-marker add, move, reassignment, removal, and each durable reopen together"
    )]
    fn arrangement_scene_launch_edits_publish_and_reopen_exactly() {
        let fixture = ArrangementProjectFixture::with_two_scenes();
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
        let original_mixer_id = mixer_id.clone();
        let original_mixer_bytes = mixer_bytes.clone();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let launch_id = "018f47a2-6b2d-7c11-8a44-001122338509";
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let arrangement_bytes = serialize_arrangement_scene_launch_addition(
            project,
            &assets,
            Some(&source_arrangement),
            ARRANGEMENT_TEST_RESOURCE_ID,
            launch_id,
            ARRANGEMENT_TEST_SCENE_ID,
            4,
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
            arrangement_id: ARRANGEMENT_TEST_RESOURCE_ID.to_owned(),
            arrangement_bytes,
            addition: false,
            created_placement_id: None,
        }
        .publish()
        .unwrap();
        assert_eq!(outcome.project_revision(), 2);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.scene_launch_count(), 1);
        assert_eq!(loaded.scene_launch_id_at(0), Some(launch_id));
        assert_eq!(loaded.scene_launch_tick_at(0), Some(4));
        assert_eq!(loaded.scene_launch_scene_index_at(0), Some(0));
        let published = reopened.session().arrangement();
        assert_eq!(published.scene_launches().len(), 1);
        assert_eq!(published.scene_launches()[0].id(), launch_id);
        assert_eq!(published.scene_launches()[0].tick(), 4);
        assert_eq!(published.scene_launches()[0].start_frame(), 100);
        assert_eq!(published.scene_name(0), Some("Intro"));

        let package = reopened.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let (project, _, _, media) = reopened.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let moved_arrangement = serialize_arrangement_scene_launch_move(
            project,
            &assets,
            &source_arrangement,
            launch_id,
            7,
            limits.session.documents,
        )
        .unwrap();
        drop(reopened);

        let moved = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id: ARRANGEMENT_TEST_RESOURCE_ID.to_owned(),
            arrangement_bytes: moved_arrangement,
            addition: false,
            created_placement_id: None,
        }
        .publish()
        .unwrap();
        assert_eq!(moved.project_revision(), 3);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let arrangement = reopened.session().arrangement();
        assert_eq!(arrangement.scene_launches()[0].tick(), 7);
        assert_eq!(arrangement.scene_launches()[0].start_frame(), 175);
        assert_arrangement_fixture_placement_is_unchanged(arrangement);
        assert_eq!(
            reopened
                .package()
                .read_resource(&original_mixer_id)
                .unwrap(),
            original_mixer_bytes
        );
        let package = reopened.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let (project, _, _, media) = reopened.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let reassigned_arrangement = serialize_arrangement_scene_launch_scene_edit(
            project,
            &assets,
            &source_arrangement,
            launch_id,
            ARRANGEMENT_TEST_SECOND_SCENE_ID,
            limits.session.documents,
        )
        .unwrap();
        drop(reopened);

        let reassigned = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id: ARRANGEMENT_TEST_RESOURCE_ID.to_owned(),
            arrangement_bytes: reassigned_arrangement,
            addition: false,
            created_placement_id: None,
        }
        .publish()
        .unwrap();
        assert_eq!(reassigned.project_revision(), 4);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.scene_launch_id_at(0), Some(launch_id));
        assert_eq!(loaded.scene_launch_tick_at(0), Some(7));
        assert_eq!(loaded.scene_launch_scene_index_at(0), Some(1));
        let arrangement = reopened.session().arrangement();
        assert_eq!(arrangement.scene_launches()[0].id(), launch_id);
        assert_eq!(arrangement.scene_launches()[0].tick(), 7);
        assert_eq!(arrangement.scene_launches()[0].start_frame(), 175);
        assert_eq!(arrangement.scene_name(1), Some("Chorus"));
        assert_arrangement_fixture_placement_is_unchanged(arrangement);
        assert_eq!(
            reopened
                .package()
                .read_resource(&original_mixer_id)
                .unwrap(),
            original_mixer_bytes
        );
        let package = reopened.package();
        let mixer = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Mixer)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(&mixer_id).unwrap();
        let source_arrangement = package.read_resource(ARRANGEMENT_TEST_RESOURCE_ID).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let (project, _, _, media) = reopened.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let removed_arrangement = serialize_arrangement_scene_launch_removal(
            project,
            &assets,
            &source_arrangement,
            launch_id,
            limits.session.documents,
        )
        .unwrap();
        drop(reopened);

        let removed = IdeArrangementPlacementEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id,
                resource_bytes: mixer_bytes,
            },
            arrangement_id: ARRANGEMENT_TEST_RESOURCE_ID.to_owned(),
            arrangement_bytes: removed_arrangement,
            addition: false,
            created_placement_id: None,
        }
        .publish()
        .unwrap();
        assert_eq!(removed.project_revision(), 5);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let arrangement = reopened.session().arrangement();
        assert_eq!(arrangement.scene_launches(), &[]);
        assert_arrangement_fixture_placement_is_unchanged(arrangement);
        assert_eq!(
            reopened
                .package()
                .read_resource(&original_mixer_id)
                .unwrap(),
            original_mixer_bytes
        );
        assert!(
            reopened
                .package()
                .manifest()
                .resources()
                .iter()
                .any(|descriptor| descriptor.role() == ResourceRole::Arrangement)
        );
    }

    fn assert_arrangement_fixture_placement_is_unchanged(arrangement: &IdePlaybackArrangement) {
        let placements = arrangement.placements();
        assert_eq!(placements.len(), 1);
        assert_eq!(placements[0].id(), ARRANGEMENT_TEST_PLACEMENT_ID);
        assert_eq!(placements[0].start_tick(), 0);
        assert_eq!(placements[0].clip_start_tick(), 0);
        assert_eq!(placements[0].duration_ticks(), 4);
        assert!(placements[0].enabled());
        assert_eq!(
            arrangement.track_name(placements[0].track_index()),
            Some("Audio")
        );
        assert_eq!(
            arrangement.clip_name(placements[0].clip_index()),
            Some("Disabled Clip")
        );
    }

    #[test]
    fn arrangement_scene_launch_publishes_the_first_arrangement_resource() {
        let fixture = ArrangementProjectFixture::with_scene_without_arrangement();
        let project_path = fixture.root().to_path_buf();
        let limits = desktop_ide_application_config().unwrap().project_limits();
        let prepared =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
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
        let launch_id = "018f47a2-6b2d-7c11-8a44-001122338509";
        let (project, _, _, media) = prepared.session().parts();
        let assets: Vec<_> = media.media_asset_facts().collect();
        let arrangement_bytes = serialize_arrangement_scene_launch_addition(
            project,
            &assets,
            None,
            ARRANGEMENT_TEST_RESOURCE_ID,
            launch_id,
            ARRANGEMENT_TEST_SCENE_ID,
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
            arrangement_id: ARRANGEMENT_TEST_RESOURCE_ID.to_owned(),
            arrangement_bytes: arrangement_bytes.clone(),
            addition: true,
            created_placement_id: None,
        }
        .publish()
        .unwrap();
        assert_eq!(outcome.project_revision(), 2);

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let arrangement_descriptor = reopened
            .package()
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .unwrap();
        assert_eq!(arrangement_descriptor.id(), ARRANGEMENT_TEST_RESOURCE_ID);
        assert_eq!(
            reopened
                .package()
                .read_resource(ARRANGEMENT_TEST_RESOURCE_ID)
                .unwrap(),
            arrangement_bytes
        );
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.scene_launch_count(), 1);
        assert_eq!(loaded.scene_launch_id_at(0), Some(launch_id));
        assert_eq!(loaded.scene_launch_tick_at(0), Some(7));
        assert_eq!(loaded.scene_launch_scene_index_at(0), Some(0));
        let arrangement = reopened.session().arrangement();
        assert_eq!(arrangement.scene_launches()[0].id(), launch_id);
        assert_eq!(arrangement.scene_launches()[0].tick(), 7);
        assert_eq!(arrangement.scene_name(0), Some("Intro"));
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
    fn audio_clip_fades_publish_and_reopen_in_the_exact_tick_domain() {
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
        let clip_bytes = serialize_audio_clip_fades(
            prepared.session().parts().0,
            &source_clip,
            clip.id(),
            1,
            2,
            limits.session.documents,
        )
        .unwrap();
        let plan = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer.id().to_owned(),
                resource_bytes: package.read_resource(mixer.id()).unwrap(),
            },
            clip_id: clip.id().to_owned(),
            clip_bytes: clip_bytes.clone(),
        };
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let committed_warning = IdeAudioClipFadeEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed_warning.publication_committed());
        assert!(matches!(
            worker::audio_clip_fade_edit_failure_event(committed_warning),
            IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipFades,
                warning: IdeAudioClipEditCommittedWarning::Fades(reason),
            } if reason.publication_committed()
        ));
        assert!(matches!(
            worker::audio_clip_fade_edit_failure_event(
                IdeAudioClipFadeEditError::InvalidClipIdentity
            ),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipFades,
                reason: IdeWorkerCommandFailure::AudioClipFades(reason),
            } if matches!(reason.as_ref(), IdeAudioClipFadeEditError::InvalidClipIdentity)
        ));
        assert!(
            IdeAudioClipRenameEditError::ReopenAfterPublication {
                outcome: Box::new(outcome.clone()),
                source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
            }
            .publication_committed()
        );
        assert!(
            IdeAudioClipActivationEditError::ReopenAfterPublication {
                outcome: Box::new(outcome.clone()),
                source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
            }
            .publication_committed()
        );
        assert!(
            IdeAudioClipGainEditError::ReopenAfterPublication {
                outcome: Box::new(outcome.clone()),
                source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
            }
            .publication_committed()
        );
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.clip_fade_in_ticks_at(0), Some(1));
        assert_eq!(loaded.clip_fade_out_ticks_at(0), Some(2));
        assert_eq!(
            reopened
                .package()
                .read_resource(outcome.resource_id())
                .unwrap(),
            clip_bytes
        );
    }

    #[test]
    fn audio_clip_loop_publishes_and_reopens_in_the_exact_tick_domain() {
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
        let clip_bytes = serialize_audio_clip_loop(
            prepared.session().parts().0,
            &source_clip,
            clip.id(),
            true,
            1,
            3,
            limits.session.documents,
        )
        .unwrap();
        let plan = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer.id().to_owned(),
                resource_bytes: package.read_resource(mixer.id()).unwrap(),
            },
            clip_id: clip.id().to_owned(),
            clip_bytes: clip_bytes.clone(),
        };
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let committed_warning = IdeAudioClipLoopEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed_warning.publication_committed());
        assert!(matches!(
            worker::audio_clip_loop_edit_failure_event(committed_warning),
            IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipLoop,
                warning: IdeAudioClipEditCommittedWarning::Loop(reason),
            } if reason.publication_committed()
        ));
        assert!(matches!(
            worker::audio_clip_loop_edit_failure_event(
                IdeAudioClipLoopEditError::InvalidClipIdentity
            ),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipLoop,
                reason: IdeWorkerCommandFailure::AudioClipLoop(reason),
            } if matches!(reason.as_ref(), IdeAudioClipLoopEditError::InvalidClipIdentity)
        ));
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.clip_loop_enabled_at(0), Some(true));
        assert_eq!(loaded.clip_loop_start_tick_at(0), Some(1));
        assert_eq!(loaded.clip_loop_end_tick_at(0), Some(3));
        assert_eq!(
            reopened
                .package()
                .read_resource(outcome.resource_id())
                .unwrap(),
            clip_bytes
        );
    }

    #[test]
    fn audio_clip_reverse_publishes_and_reopens_with_native_direction() {
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
        let clip_bytes = serialize_audio_clip_reverse(
            prepared.session().parts().0,
            &source_clip,
            clip.id(),
            true,
            limits.session.documents,
        )
        .unwrap();
        let plan = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256: *package.manifest_sha256(),
                resource_id: mixer.id().to_owned(),
                resource_bytes: package.read_resource(mixer.id()).unwrap(),
            },
            clip_id: clip.id().to_owned(),
            clip_bytes: clip_bytes.clone(),
        };
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let committed_warning = IdeAudioClipReverseEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed_warning.publication_committed());
        assert!(matches!(
            worker::audio_clip_reverse_edit_failure_event(committed_warning),
            IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipReverse,
                warning: IdeAudioClipEditCommittedWarning::Reverse(reason),
            } if reason.publication_committed()
        ));
        assert!(matches!(
            worker::audio_clip_reverse_edit_failure_event(
                IdeAudioClipReverseEditError::InvalidClipIdentity
            ),
            IdeWorkerEvent::CommandFailed {
                command: IdeWorkerCommandKind::SetAudioClipReverse,
                reason: IdeWorkerCommandFailure::AudioClipReverse(reason),
            } if matches!(reason.as_ref(), IdeAudioClipReverseEditError::InvalidClipIdentity)
        ));
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.clip_reverse_at(0), Some(true));
        assert_eq!(
            reopened
                .package()
                .read_resource(outcome.resource_id())
                .unwrap(),
            clip_bytes
        );
    }

    #[test]
    fn audio_clip_source_slip_publishes_and_reopens_exact_decoded_view() {
        let fixture = ArrangementProjectFixture::with_audio(&arrangement_test_wav(&[0; 125]));
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
        let arrangement = package
            .manifest()
            .resources()
            .iter()
            .find(|descriptor| descriptor.role() == ResourceRole::Arrangement)
            .unwrap();
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(mixer.id()).unwrap();
        let arrangement_id = arrangement.id().to_owned();
        let arrangement_bytes = package.read_resource(arrangement.id()).unwrap();
        let source_audio = std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let (clip_id, clip_bytes) = prepare_audio_clip_source_slip_resource(
            &prepared,
            clip.id(),
            25,
            limits.session.documents,
        )
        .unwrap();
        let plan = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: mixer_bytes.clone(),
            },
            clip_id,
            clip_bytes: clip_bytes.clone(),
        };
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let committed_warning = IdeAudioClipSourceSlipEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed_warning.publication_committed());
        assert!(matches!(
            worker::audio_clip_source_slip_edit_failure_event(committed_warning),
            IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::SetAudioClipSourceStart,
                warning: IdeAudioClipEditCommittedWarning::SourceSlip(reason),
            } if reason.publication_committed()
        ));
        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.clip_source_start_frame_at(0), Some(25));
        assert_eq!(loaded.clip_source_frame_count_at(0), Some(100));
        assert_eq!(
            reopened
                .package()
                .read_resource(outcome.resource_id())
                .unwrap(),
            clip_bytes
        );
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            mixer_bytes
        );
        assert_eq!(
            reopened.package().read_resource(&arrangement_id).unwrap(),
            arrangement_bytes
        );
        assert_eq!(
            std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap(),
            source_audio
        );
    }

    #[test]
    fn audio_clip_source_trim_publishes_exact_view_and_reopens_without_arrangement_retiming() {
        let fixture =
            ArrangementProjectFixture::without_arrangement().with_trim_compatible_source();
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
        let mixer_id = mixer.id().to_owned();
        let mixer_bytes = package.read_resource(mixer.id()).unwrap();
        let source_audio = std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap();
        let expected_project_sha256 = *package.manifest_sha256();
        let resource = prepare_audio_clip_source_trim_resource(
            &prepared,
            clip.id(),
            25,
            50,
            limits.session.documents,
        )
        .unwrap();
        let plan = IdeAudioClipEditPlan {
            mixer: IdeMixerSavePlan {
                project_path: project_path.clone(),
                expected_project_sha256,
                resource_id: mixer_id.clone(),
                resource_bytes: mixer_bytes.clone(),
            },
            clip_id: resource.clip_id,
            clip_bytes: resource.clip_bytes.clone(),
        };
        let clip_bytes = resource.clip_bytes;
        drop(prepared);

        let outcome = plan.publish().unwrap();
        assert!(outcome.changed());
        assert_eq!(outcome.project_revision(), 2);
        let committed_warning = IdeAudioClipSourceTrimEditError::ReopenAfterPublication {
            outcome: Box::new(outcome.clone()),
            source: Box::new(IdeProjectOpenError::SelectedOutputUnavailable),
        };
        assert!(committed_warning.publication_committed());
        assert!(matches!(
            worker::audio_clip_source_trim_edit_failure_event(committed_warning),
            IdeWorkerEvent::AudioClipEditCommittedWithWarning {
                command: IdeWorkerCommandKind::TrimAudioClipSource,
                warning: IdeAudioClipEditCommittedWarning::SourceTrim(reason),
            } if reason.publication_committed()
        ));

        let reopened =
            hyper_project_loader::prepare_project_playback(&project_path, limits).unwrap();
        let loaded = reopened.session().parts().0;
        assert_eq!(loaded.clip_source_start_frame_at(0), Some(25));
        assert_eq!(loaded.clip_source_frame_count_at(0), Some(50));
        assert_eq!(loaded.clip_duration_ticks_at(0), Some(2));
        assert_eq!(
            reopened
                .package()
                .read_resource(outcome.resource_id())
                .unwrap(),
            clip_bytes
        );
        assert_eq!(
            reopened.package().read_resource(&mixer_id).unwrap(),
            mixer_bytes
        );
        assert!(
            reopened
                .package()
                .manifest()
                .resources()
                .iter()
                .all(|descriptor| descriptor.role() != ResourceRole::Arrangement),
            "source trim must not create or retime an arrangement"
        );
        assert_eq!(
            std::fs::read(project_path.join("assets/samples/sample.wav")).unwrap(),
            source_audio
        );
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
    fn prepared_audio_import_publishes_only_while_the_source_is_unchanged() {
        let source_directory = TestDirectory::new();
        let source = source_directory.0.join("unchanged.wav");
        std::fs::write(&source, arrangement_test_wav(&[1; 100])).unwrap();
        let (fixture, plan, original_identity) = arrangement_test_import_plan(&source);

        let outcome = plan.publish().unwrap();

        assert_eq!(outcome.project_revision(), 2);
        let verified = hyper_storage_engine::verify_project_package(fixture.root()).unwrap();
        assert_ne!(verified.manifest_sha256(), &original_identity);
    }

    #[test]
    fn prepared_audio_import_rejects_same_length_source_mutation_without_project_change() {
        let source_directory = TestDirectory::new();
        let source = source_directory.0.join("mutated.wav");
        std::fs::write(&source, arrangement_test_wav(&[1; 100])).unwrap();
        let (fixture, plan, original_identity) = arrangement_test_import_plan(&source);
        std::fs::write(&source, arrangement_test_wav(&[2; 100])).unwrap();

        assert!(matches!(
            plan.publish(),
            Err(IdeAudioImportError::SourceChanged(_))
        ));
        let verified = hyper_storage_engine::verify_project_package(fixture.root()).unwrap();
        assert_eq!(verified.manifest().revision(), 1);
        assert_eq!(verified.manifest_sha256(), &original_identity);
    }

    #[test]
    fn prepared_audio_import_rejects_source_entry_replacement_without_project_change() {
        let source_directory = TestDirectory::new();
        let source = source_directory.0.join("replaced.wav");
        let replacement = source_directory.0.join("replacement.wav");
        let bytes = arrangement_test_wav(&[1; 100]);
        std::fs::write(&source, &bytes).unwrap();
        let (fixture, plan, original_identity) = arrangement_test_import_plan(&source);
        std::fs::write(&replacement, &bytes).unwrap();
        std::fs::remove_file(&source).unwrap();
        std::fs::rename(&replacement, &source).unwrap();

        assert!(matches!(
            plan.publish(),
            Err(IdeAudioImportError::SourceChanged(_))
        ));
        let verified = hyper_storage_engine::verify_project_package(fixture.root()).unwrap();
        assert_eq!(verified.manifest().revision(), 1);
        assert_eq!(verified.manifest_sha256(), &original_identity);
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
    fn arrangement_navigation_validates_identity_and_uses_exact_compiled_frames() {
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
                arrangement_scene_launches: vec![("marker-a".to_owned(), 960)],
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
        assert!(matches!(
            running.try_locate_arrangement_scene_launch(
                "project-b",
                0,
                "marker-a",
                LocateState::Preserve,
            ),
            Err(IdeArrangementLocateError::StaleProject)
        ));
        assert!(matches!(
            running.try_locate_arrangement_scene_launch(
                "project-a",
                0,
                "stale-marker",
                LocateState::Preserve,
            ),
            Err(IdeArrangementLocateError::StaleSceneLaunch { requested_index: 0 })
        ));
        assert_eq!(
            running
                .try_locate_arrangement_scene_launch(
                    "project-a",
                    0,
                    "marker-a",
                    LocateState::Rolling,
                )
                .unwrap(),
            960
        );
        let IdePlaybackSessionOwner::Test(session) = &running.session else {
            unreachable!("test constructed the deterministic session owner");
        };
        assert_eq!(
            session.commands,
            [
                TransportCommand::Locate {
                    frame: 480,
                    state: LocateState::Preserve,
                },
                TransportCommand::Locate {
                    frame: 960,
                    state: LocateState::Rolling,
                }
            ]
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
