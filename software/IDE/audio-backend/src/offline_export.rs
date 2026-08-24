use core::error::Error;
use core::fmt;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{
    StereoLoudnessError, StereoLoudnessMeter, StereoOutput, StereoTruePeakError,
    StereoTruePeakMeter,
};
use hyper_audio_source::{DecodeError, DecodedAudio};
#[cfg(test)]
use hyper_audio_source::{AudioContainer, DecodeLimits, decode_bytes, encode_float_wav};
use hyper_fs_authority::{
    create_syncable_directory_at, open_syncable_directory_ambient, remove_directory_entry,
    sync_directory,
};
#[cfg(target_os = "linux")]
use hyper_fs_authority::{publish_bytes_no_replace, rename_no_replace};
#[cfg(windows)]
use hyper_fs_authority::{identity, read_stable_file_snapshot, rename_no_replace};
#[cfg(windows)]
use hyper_fs_authority::remove_file_if_identity;
use hyper_media_cache::PreparePlaybackError;
use hyper_mixer_runtime::{
    RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError, RuntimeProcessError,
    TrackWindowRenderer, TransportCommand, TransportState,
};
use hyper_native_device::DeviceChain;
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, CompiledMixerGraph, CompiledMixerTrackKind};
use hyper_playback_session::{PlaybackExtent, PreparedPlaybackSession};
use hyper_project_loader::PreparedProjectPlayback;
use hyper_recording_journal::{JournalConfig, JournalError, JournalWriter};
use std::{
    cell::RefCell,
    fs::File,
    io::{BufWriter, Cursor, Seek, Write},
    path::{Path, PathBuf},
    rc::Rc,
};
use cap_std::fs::OpenOptions;
use flac_codec::encode::{FlacSampleWriter, Options as FlacOptions};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflineExportSpec {
    pub block_frames: usize,
}

/// One aligned direct audio-track render from a verified project.
///
/// `audio` includes the track's device chain, automation, mute, fader, and
/// pan. It deliberately excludes sends, groups, returns, and master
/// processing, so every stem is independently usable while remaining aligned
/// to the project extent.
pub struct OfflineTrackStem {
    pub track_id: String,
    pub track_name: String,
    pub audio: DecodedAudio,
}

/// One explicitly supported container for a complete aligned stem export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineTrackStemExportFormat {
    Wav,
    Flac,
}

impl OfflineTrackStemExportFormat {
    const fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
        }
    }
}

/// One published file in an aligned stem export.
///
/// `file_name` is generated from the canonical track order plus a portable
/// encoding of the verified track name. It is never derived as a raw path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineTrackStemExportFile {
    pub ordinal: usize,
    pub track_id: String,
    pub track_name: String,
    pub file_name: String,
}

/// The complete result of publishing an aligned stem set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineTrackStemExportReport {
    pub frame_count: usize,
    pub sample_rate_hz: u32,
    pub format: OfflineTrackStemExportFormat,
    pub files: Vec<OfflineTrackStemExportFile>,
}

#[derive(Debug)]
pub enum OfflineExportError {
    UnboundedExtent,
    EmptyExtent,
    InvalidBlockFrames,
    CapacityUnavailable,
    Playback(AdapterCompileError),
    Media(PreparePlaybackError),
    Runtime(RuntimeCreationError),
    Process(RuntimeProcessError),
    Output(DecodeError),
    Journal(JournalError),
    TruePeak(StereoTruePeakError),
    Loudness(StereoLoudnessError),
    MixerLayoutMismatch,
    StemTrackMetadataUnavailable { channel_index: usize },
    NoAudioTrackStems,
    Destination(std::io::Error),
    StemDestination(std::io::Error),
    StemPublicationCommittedWithWarning {
        source: std::io::Error,
        report: Box<OfflineTrackStemExportReport>,
    },
    FlacEncoding(String),
    FlacVerification,
    FlacMinimumFrames,
}

impl fmt::Display for OfflineExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnboundedExtent => {
                formatter.write_str("an unbounded project has no end to export")
            }
            Self::EmptyExtent => formatter.write_str("this project renders no audio"),
            Self::InvalidBlockFrames => {
                formatter.write_str("the export block size is outside the supported range")
            }
            Self::CapacityUnavailable => {
                formatter.write_str("offline export capacity is unavailable")
            }
            Self::Playback(source) => source.fmt(formatter),
            Self::Media(source) => source.fmt(formatter),
            Self::Runtime(source) => source.fmt(formatter),
            Self::Process(source) => source.fmt(formatter),
            Self::Output(source) => source.fmt(formatter),
            Self::Journal(source) => source.fmt(formatter),
            Self::TruePeak(source) => source.fmt(formatter),
            Self::Loudness(source) => source.fmt(formatter),
            Self::MixerLayoutMismatch => {
                formatter.write_str("the live mixer does not match the prepared mixer graph")
            }
            Self::StemTrackMetadataUnavailable { channel_index } => write!(
                formatter,
                "compiled audio channel {channel_index} has no verified track metadata"
            ),
            Self::NoAudioTrackStems => {
                formatter.write_str("this project has no audio tracks to export as stems")
            }
            Self::Destination(source) => {
                write!(formatter, "the export destination is unusable: {source}")
            }
            Self::StemDestination(source) => {
                write!(formatter, "the stem export destination is unusable: {source}")
            }
            Self::StemPublicationCommittedWithWarning { source, .. } => write!(
                formatter,
                "the complete stem export was published but its parent durability barrier failed: {source}"
            ),
            Self::FlacEncoding(source) => write!(formatter, "FLAC encoding failed: {source}"),
            Self::FlacVerification => formatter.write_str("the staged FLAC did not decode to its exact PCM"),
            Self::FlacMinimumFrames => {
                formatter.write_str("a FLAC export requires at least 32 rendered frames")
            }
        }
    }
}

impl Error for OfflineExportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Playback(source) => Some(source),
            Self::Media(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Process(source) => Some(source),
            Self::Output(source) => Some(source),
            Self::Journal(source) => Some(source),
            Self::TruePeak(source) => Some(source),
            Self::Loudness(source) => Some(source),
            Self::Destination(source)
            | Self::StemDestination(source)
            | Self::StemPublicationCommittedWithWarning { source, .. } => Some(source),
            Self::UnboundedExtent
            | Self::EmptyExtent
            | Self::InvalidBlockFrames
            | Self::CapacityUnavailable
            | Self::MixerLayoutMismatch
            | Self::StemTrackMetadataUnavailable { .. }
            | Self::NoAudioTrackStems
            | Self::FlacEncoding(_)
            | Self::FlacVerification
            | Self::FlacMinimumFrames => None,
        }
    }
}

impl OfflineExportError {
    /// Returns the activated stem-set report only when publication completed
    /// but its final parent-directory durability barrier failed.
    #[must_use]
    pub fn committed_track_stem_report(&self) -> Option<&OfflineTrackStemExportReport> {
        match self {
            Self::StemPublicationCommittedWithWarning { report, .. } => Some(report),
            _ => None,
        }
    }
}

/// Renders one prepared project to stereo audio through the same compiled
/// runtime the desktop output drives.
///
/// The arrangement graph, scene authority, mixer routing, and automation are
/// the ones prepared for playback, so an export cannot diverge from playback by
/// construction. This performs no device I/O and no file writing.
///
/// # Errors
///
/// Returns a classified rejection for an unbounded or empty extent, an invalid
/// block size, exhausted capacity, or a compile, media, runtime, or process
/// failure.
pub fn render_project_offline(
    session: &PreparedPlaybackSession,
    spec: OfflineExportSpec,
) -> Result<DecodedAudio, OfflineExportError> {
    render_project_offline_with_mixer(session, spec, None)
}

/// Renders one prepared project, optionally against a mixer graph other than
/// the one the project was opened with.
///
/// The desktop passes the graph compiled from live fader, pan, mute, solo, and
/// send values, because those are applied to the audio thread and are folded
/// into the project only by a save. Without the override an export would
/// render the last saved mix rather than the audible one.
///
/// # Errors
///
/// Returns the same classified rejections as [`render_project_offline`], plus
/// [`OfflineExportError::MixerLayoutMismatch`] when the override describes a
/// different slot layout than the prepared graph.
pub fn render_project_offline_with_mixer(
    session: &PreparedPlaybackSession,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<DecodedAudio, OfflineExportError> {
    render_project_offline_with_devices(session, &[], spec, live_mixer)
}

/// Renders a structurally verified project, including its native device chains.
///
/// # Errors
///
/// Returns the same classified rejections as [`render_project_offline_with_mixer`].
pub fn render_prepared_project_offline_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<DecodedAudio, OfflineExportError> {
    render_project_offline_with_devices(
        project.session(),
        project.device_chains(),
        spec,
        live_mixer,
    )
}

fn render_project_offline_with_devices(
    session: &PreparedPlaybackSession,
    device_chains: &[DeviceChain],
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<DecodedAudio, OfflineExportError> {
    let PlaybackExtent::Bounded { end_frame } = session.extent() else {
        return Err(OfflineExportError::UnboundedExtent);
    };
    if end_frame == 0 {
        return Err(OfflineExportError::EmptyExtent);
    }
    if spec.block_frames == 0 || spec.block_frames > hyper_audio_engine::MAX_PROCESS_FRAMES {
        return Err(OfflineExportError::InvalidBlockFrames);
    }
    let block_frames = spec.block_frames.min(end_frame);

    let (project, prepared_mixer, automation, media) = session.parts();
    let mixer = live_mixer.unwrap_or(prepared_mixer);
    if !same_mixer_layout(prepared_mixer, mixer) {
        return Err(OfflineExportError::MixerLayoutMismatch);
    }
    let facts: Vec<_> = media.media_asset_facts().collect();
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(OfflineExportError::Playback)?;
    let prepared = media
        .prepare_playback(&graph)
        .map_err(OfflineExportError::Media)?;
    let runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated_with_devices(
            &prepared,
            mixer,
            automation.clone(),
            device_chains,
            block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback_with_devices(
            &prepared,
            mixer,
            device_chains,
            block_frames,
        ),
    }
    .map_err(OfflineExportError::Runtime)?;

    let slot_count = mixer.slots().len();
    let mut scratch_left = allocate_planes(slot_count, block_frames)?;
    let mut scratch_right = allocate_planes(slot_count, block_frames)?;
    let mut scratch: Vec<RenderScratchSlot<'_>> = Vec::new();
    scratch
        .try_reserve_exact(slot_count)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    scratch.extend(
        scratch_left
            .iter_mut()
            .zip(&mut scratch_right)
            .map(|(left, right)| RenderScratchSlot::new(left, right)),
    );

    let (mut sender, _mixer_sender, mut transport) =
        RoutedTransportRuntime::with_command_queues(runtime);
    sender
        .try_send(TransportCommand::Start)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let mut left = allocate_silence(end_frame)?;
    let mut right = allocate_silence(end_frame)?;
    let mut block_left = vec![0.0_f32; block_frames];
    let mut block_right = vec![0.0_f32; block_frames];

    let mut rendered = 0_usize;
    while rendered < end_frame {
        let frames = block_frames.min(end_frame - rendered);
        let cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut block_left[..frames],
                    right: &mut block_right[..frames],
                },
            )
            .map_err(OfflineExportError::Process)?;
        debug_assert_eq!(
            cycle.state(),
            TransportState::Rolling,
            "the export queues one Start and no command that could stop it"
        );
        left[rendered..rendered + frames].copy_from_slice(&block_left[..frames]);
        right[rendered..rendered + frames].copy_from_slice(&block_right[..frames]);
        rendered += frames;
    }

    DecodedAudio::from_stereo(session.sample_rate_hz(), left, right)
        .map_err(OfflineExportError::Output)
}

/// Renders every compiled audio track as an aligned direct stem.
///
/// This uses one routed render pass per block and captures every selected
/// channel after its device and channel controls. Consequently stateful native
/// devices and automation advance exactly once, identically for all stems.
///
/// # Errors
///
/// Returns the usual offline-render refusal, including a layout mismatch when
/// the supplied live mixer cannot describe the prepared project's channels.
pub fn render_project_track_stems_with_mixer(
    session: &PreparedPlaybackSession,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<Vec<OfflineTrackStem>, OfflineExportError> {
    render_project_track_stems_with_devices(session, &[], spec, live_mixer)
}

/// Renders every compiled audio track of a structurally verified project as an
/// aligned direct stem, including its verified native device chains.
///
/// # Errors
///
/// Returns the same classified rejections as
/// [`render_project_track_stems_with_mixer`].
pub fn render_prepared_project_track_stems_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<Vec<OfflineTrackStem>, OfflineExportError> {
    render_project_track_stems_with_devices(
        project.session(),
        project.device_chains(),
        spec,
        live_mixer,
    )
}

/// Renders every verified audio track and publishes the complete aligned set
/// into one new destination directory.
///
/// The destination itself must be absent. Each file is streamed into a private
/// stage, finalized, and synced before the complete stage is activated with one
/// no-replace directory rename. Callers therefore observe either no destination
/// or the whole ordered stem set, never a partially populated final directory.
///
/// # Errors
///
/// Returns the usual render and encoding rejections, a destination refusal
/// when the final directory already exists or cannot be safely created, or a
/// committed warning only after the complete directory was activated and its
/// parent durability barrier failed.
pub fn export_prepared_project_track_stems_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    format: OfflineTrackStemExportFormat,
    destination_directory: &Path,
) -> Result<OfflineTrackStemExportReport, OfflineExportError> {
    preflight_track_stem_destination(destination_directory)?;
    export_project_track_stems_to_directory_with_devices(
        project.session(),
        project.device_chains(),
        spec,
        live_mixer,
        format,
        destination_directory,
    )
}

/// Streams every direct track stem into a private destination stage.
///
/// Unlike the explicitly in-memory stem-rendering API, this path retains only
/// the renderer scratch, codec block state, and one buffered writer per track.
/// The stage is activated only after every file has been finalized and synced.
#[allow(
    clippy::too_many_lines,
    reason = "the streaming transaction owns the one shared runtime and its stage lifecycle"
)]
fn export_project_track_stems_to_directory_with_devices(
    session: &PreparedPlaybackSession,
    device_chains: &[DeviceChain],
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    format: OfflineTrackStemExportFormat,
    destination_directory: &Path,
) -> Result<OfflineTrackStemExportReport, OfflineExportError> {
    let PlaybackExtent::Bounded { end_frame } = session.extent() else {
        return Err(OfflineExportError::UnboundedExtent);
    };
    if end_frame == 0 {
        return Err(OfflineExportError::EmptyExtent);
    }
    if format == OfflineTrackStemExportFormat::Flac && end_frame < FLAC_MINIMUM_FRAMES {
        return Err(OfflineExportError::FlacMinimumFrames);
    }
    if spec.block_frames == 0 || spec.block_frames > hyper_audio_engine::MAX_PROCESS_FRAMES {
        return Err(OfflineExportError::InvalidBlockFrames);
    }
    let block_frames = spec.block_frames.min(end_frame);

    let (project, prepared_mixer, automation, media) = session.parts();
    let mixer = live_mixer.unwrap_or(prepared_mixer);
    if !same_mixer_layout(prepared_mixer, mixer) {
        return Err(OfflineExportError::MixerLayoutMismatch);
    }
    let stems = select_stem_descriptors(session, mixer)?;
    let report = track_stem_export_report(&stems, end_frame, session.sample_rate_hz(), format)?;
    let (parent_path, destination_name) = stem_destination_parts(destination_directory)?;
    let parent = open_track_stem_destination_parent(&parent_path, &destination_name)?;
    let stage_name = format!(".hyper-stem-stage-{}", uuid::Uuid::new_v4());
    let (stage, stage_identity) = create_syncable_directory_at(&parent, &stage_name)
        .map_err(OfflineExportError::StemDestination)?;

    let result = (|| {
        let mut writers = create_track_stem_file_writers(
            &stage,
            &stems,
            &report.files,
            format,
            session.sample_rate_hz(),
            end_frame,
        )?;
        let facts: Vec<_> = media.media_asset_facts().collect();
        let graph = project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .map_err(OfflineExportError::Playback)?;
        let prepared = media
            .prepare_playback(&graph)
            .map_err(OfflineExportError::Media)?;
        let mut runtime = match automation {
            Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated_with_devices(
                &prepared,
                mixer,
                automation.clone(),
                device_chains,
                block_frames,
            ),
            None => RoutedMainRuntime::try_new_prepared_playback_with_devices(
                &prepared,
                mixer,
                device_chains,
                block_frames,
            ),
        }
        .map_err(OfflineExportError::Runtime)?;

        let slot_count = mixer.slots().len();
        let mut scratch_left = allocate_planes(slot_count, block_frames)?;
        let mut scratch_right = allocate_planes(slot_count, block_frames)?;
        let mut scratch = Vec::new();
        scratch
            .try_reserve_exact(slot_count)
            .map_err(|_| OfflineExportError::CapacityUnavailable)?;
        scratch.extend(
            scratch_left
                .iter_mut()
                .zip(&mut scratch_right)
                .map(|(left, right)| RenderScratchSlot::new(left, right)),
        );
        let mut main_left = allocate_silence(block_frames)?;
        let mut main_right = allocate_silence(block_frames)?;
        let mut channel_indices = Vec::new();
        channel_indices
            .try_reserve_exact(stems.len())
            .map_err(|_| OfflineExportError::CapacityUnavailable)?;
        channel_indices.extend(stems.iter().map(|stem| stem.channel_index));

        render_selected_track_stem_windows(
            &mut runtime,
            &mut scratch,
            &mut main_left,
            &mut main_right,
            &channel_indices,
            end_frame,
            block_frames,
            |_, channel_index, left, right| {
                let writer = writers
                    .iter_mut()
                    .find(|writer| writer.channel_index == channel_index)
                    .expect("the runtime emits only the prevalidated selected channels");
                writer.append(left, right)
            },
        )?;
        for writer in &mut writers {
            writer.finalize()?;
        }
        sync_directory(&stage).map_err(OfflineExportError::StemDestination)?;
        Ok::<(), OfflineExportError>(())
    })();
    drop(stage);
    if let Err(primary) = result {
        return cleanup_stem_stage(primary, &parent, &stage_name, stage_identity);
    }

    if let Err(source) = rename_no_replace(
        &parent,
        &stage_name,
        stage_identity,
        &parent,
        &destination_name,
    ) {
        return cleanup_stem_stage(
            OfflineExportError::StemDestination(source),
            &parent,
            &stage_name,
            stage_identity,
        );
    }
    sync_directory(&parent).map_err(|source| {
        OfflineExportError::StemPublicationCommittedWithWarning {
            source,
            report: Box::new(report.clone()),
        }
    })?;
    Ok(report)
}

#[cfg(test)]
fn publish_rendered_track_stems(
    stems: &[OfflineTrackStem],
    format: OfflineTrackStemExportFormat,
    destination_directory: &Path,
) -> Result<OfflineTrackStemExportReport, OfflineExportError> {
    let files = describe_track_stem_files(stems, format)?;
    let frame_count = stems
        .first()
        .ok_or(OfflineExportError::NoAudioTrackStems)?
        .audio
        .frame_count();
    let sample_rate_hz = stems
        .first()
        .ok_or(OfflineExportError::NoAudioTrackStems)?
        .audio
        .sample_rate_hz();
    if stems.iter().any(|stem| {
        stem.audio.frame_count() != frame_count || stem.audio.sample_rate_hz() != sample_rate_hz
    }) {
        return Err(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "aligned stems do not share one sample rate and frame extent",
        )));
    }

    let report = OfflineTrackStemExportReport {
        frame_count,
        sample_rate_hz,
        format,
        files,
    };
    publish_rendered_track_stem_files(stems, &report, destination_directory)?;
    Ok(report)
}

#[cfg(test)]
fn describe_track_stem_files(
    stems: &[OfflineTrackStem],
    format: OfflineTrackStemExportFormat,
) -> Result<Vec<OfflineTrackStemExportFile>, OfflineExportError> {
    if stems.is_empty() {
        return Err(OfflineExportError::NoAudioTrackStems);
    }
    let mut files = Vec::new();
    files
        .try_reserve_exact(stems.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for (index, stem) in stems.iter().enumerate() {
        let ordinal = index
            .checked_add(1)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        files.push(OfflineTrackStemExportFile {
            ordinal,
            track_id: stem.track_id.clone(),
            track_name: stem.track_name.clone(),
            file_name: stem_file_name(ordinal, &stem.track_name, format)?,
        });
    }
    Ok(files)
}

fn stem_file_name(
    ordinal: usize,
    track_name: &str,
    format: OfflineTrackStemExportFormat,
) -> Result<String, OfflineExportError> {
    const MAX_PORTABLE_FILE_NAME_BYTES: usize = 200;
    let prefix = format!("{ordinal:03}-");
    let suffix = format!(".{}", format.extension());
    let available = MAX_PORTABLE_FILE_NAME_BYTES
        .checked_sub(prefix.len())
        .and_then(|available| available.checked_sub(suffix.len()))
        .ok_or(OfflineExportError::CapacityUnavailable)?;
    let mut encoded_name = String::new();
    for byte in track_name.bytes() {
        let encoded = match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => {
                char::from(byte).to_string()
            }
            _ => format!("%{byte:02X}"),
        };
        if encoded_name.len().saturating_add(encoded.len()) > available {
            break;
        }
        encoded_name.push_str(&encoded);
    }
    if encoded_name.is_empty() {
        encoded_name.push_str("track");
    }
    Ok(format!("{prefix}{encoded_name}{suffix}"))
}

#[cfg(test)]
fn encode_verified_wav(audio: &DecodedAudio) -> Result<Vec<u8>, OfflineExportError> {
    let encoded = encode_float_wav(audio).map_err(OfflineExportError::Output)?;
    let maximum_file_bytes =
        u64::try_from(encoded.len()).map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let maximum_decoded_bytes = audio
        .frame_count()
        .checked_mul(audio.channels())
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(OfflineExportError::CapacityUnavailable)?;
    let limits = DecodeLimits::new(
        maximum_file_bytes,
        u64::try_from(audio.frame_count()).map_err(|_| OfflineExportError::CapacityUnavailable)?,
        maximum_decoded_bytes,
    )
    .map_err(OfflineExportError::Output)?;
    let decoded = decode_bytes(
        encoded.clone(),
        AudioContainer::Wav,
        audio.sample_rate_hz(),
        limits,
    )
    .map_err(OfflineExportError::Output)?;
    let exact_planes = (0..audio.channels()).all(|channel| {
        let Some(expected) = audio.plane(channel) else {
            return false;
        };
        let Some(actual) = decoded.plane(channel) else {
            return false;
        };
        expected
            .iter()
            .zip(actual)
            .all(|(&expected, &actual)| expected.to_bits() == actual.to_bits())
    });
    if decoded.sample_rate_hz() != audio.sample_rate_hz()
        || decoded.frame_count() != audio.frame_count()
        || decoded.channels() != audio.channels()
        || !exact_planes
    {
        return Err(OfflineExportError::Output(DecodeError::InvalidDecodedAudio(
            "encoded WAV does not exactly reproduce its rendered samples",
        )));
    }
    Ok(encoded)
}

#[cfg(test)]
fn publish_rendered_track_stem_files(
    stems: &[OfflineTrackStem],
    report: &OfflineTrackStemExportReport,
    destination_directory: &Path,
) -> Result<(), OfflineExportError> {
    if stems.len() != report.files.len() {
        return Err(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "every rendered stem must have exactly one generated output file",
        )));
    }
    let (parent_path, destination_name) = stem_destination_parts(destination_directory)?;
    let parent = open_track_stem_destination_parent(&parent_path, &destination_name)?;

    let stage_name = format!(".hyper-stem-stage-{}", uuid::Uuid::new_v4());
    let (stage, stage_identity) = create_syncable_directory_at(&parent, &stage_name)
        .map_err(OfflineExportError::StemDestination)?;
    let result = (|| {
        for (stem, file) in stems.iter().zip(&report.files) {
            let bytes = match report.format {
                OfflineTrackStemExportFormat::Wav => encode_verified_wav(&stem.audio)?,
                OfflineTrackStemExportFormat::Flac => encode_verified_flac(&stem.audio)?,
            };
            publish_stem_bytes(&stage, &file.file_name, &bytes)?;
        }
        sync_directory(&stage).map_err(OfflineExportError::StemDestination)?;
        Ok::<(), OfflineExportError>(())
    })();
    drop(stage);
    if let Err(primary) = result {
        return cleanup_stem_stage(primary, &parent, &stage_name, stage_identity);
    }

    rename_no_replace(
        &parent,
        &stage_name,
        stage_identity,
        &parent,
        &destination_name,
    )
    .map_err(OfflineExportError::StemDestination)?;
    sync_directory(&parent).map_err(|source| {
        OfflineExportError::StemPublicationCommittedWithWarning {
            source,
            report: Box::new(report.clone()),
        }
    })
}

const WAV_MAX_DATA_BYTES: u64 = u32::MAX as u64 - 36;

struct StemDescriptor {
    channel_index: usize,
    track_id: String,
    track_name: String,
}

struct StemFileWriter {
    channel_index: usize,
    writer: StemContainerWriter,
}

enum StemContainerWriter {
    Wav(WavStemWriter),
    Flac(Box<FlacStemWriter>),
}

struct WavStemWriter {
    file: BufWriter<File>,
    expected_frames: usize,
    written_frames: usize,
}

struct FlacStemWriter {
    writer: Option<FlacSampleWriter<File>>,
    sync_file: File,
    pending_pcm: Vec<i32>,
    expected_frames: usize,
    written_frames: usize,
}

impl StemFileWriter {
    fn append(&mut self, left: &[f32], right: &[f32]) -> Result<(), OfflineExportError> {
        match &mut self.writer {
            StemContainerWriter::Wav(writer) => writer.append(left, right),
            StemContainerWriter::Flac(writer) => writer.append(left, right),
        }
    }

    fn finalize(&mut self) -> Result<(), OfflineExportError> {
        match &mut self.writer {
            StemContainerWriter::Wav(writer) => writer.finalize(),
            StemContainerWriter::Flac(writer) => writer.finalize(),
        }
    }
}

impl WavStemWriter {
    fn create(
        file: File,
        sample_rate_hz: u32,
        expected_frames: usize,
    ) -> Result<Self, OfflineExportError> {
        let data_bytes = wav_data_bytes(expected_frames)?;
        let riff_bytes = data_bytes
            .checked_add(36)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        let riff_bytes = u32::try_from(riff_bytes).map_err(|_| OfflineExportError::CapacityUnavailable)?;
        let data_bytes = u32::try_from(data_bytes).map_err(|_| OfflineExportError::CapacityUnavailable)?;
        let byte_rate = sample_rate_hz
            .checked_mul(8)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        let mut file = BufWriter::new(file);
        file.write_all(b"RIFF")
            .and_then(|()| file.write_all(&riff_bytes.to_le_bytes()))
            .and_then(|()| file.write_all(b"WAVEfmt "))
            .and_then(|()| file.write_all(&16_u32.to_le_bytes()))
            .and_then(|()| file.write_all(&3_u16.to_le_bytes()))
            .and_then(|()| file.write_all(&2_u16.to_le_bytes()))
            .and_then(|()| file.write_all(&sample_rate_hz.to_le_bytes()))
            .and_then(|()| file.write_all(&byte_rate.to_le_bytes()))
            .and_then(|()| file.write_all(&8_u16.to_le_bytes()))
            .and_then(|()| file.write_all(&32_u16.to_le_bytes()))
            .and_then(|()| file.write_all(b"data"))
            .and_then(|()| file.write_all(&data_bytes.to_le_bytes()))
            .map_err(OfflineExportError::StemDestination)?;
        Ok(Self {
            file,
            expected_frames,
            written_frames: 0,
        })
    }

    fn append(&mut self, left: &[f32], right: &[f32]) -> Result<(), OfflineExportError> {
        if left.len() != right.len() {
            return Err(OfflineExportError::StemDestination(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a stem window has unequal stereo planes",
            )));
        }
        let next = self
            .written_frames
            .checked_add(left.len())
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        if next > self.expected_frames {
            return Err(OfflineExportError::StemDestination(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a stem writer received frames beyond its verified extent",
            )));
        }
        for (&left, &right) in left.iter().zip(right) {
            if !left.is_finite() || !right.is_finite() {
                return Err(OfflineExportError::Output(DecodeError::InvalidDecodedAudio(
                    "the rendered stem contains a non-finite sample",
                )));
            }
            self.file
                .write_all(&left.to_le_bytes())
                .and_then(|()| self.file.write_all(&right.to_le_bytes()))
                .map_err(OfflineExportError::StemDestination)?;
        }
        self.written_frames = next;
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), OfflineExportError> {
        if self.written_frames != self.expected_frames {
            return Err(OfflineExportError::StemDestination(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "a stem writer did not receive the verified project extent",
            )));
        }
        self.file
            .flush()
            .and_then(|()| self.file.get_ref().sync_all())
            .map_err(OfflineExportError::StemDestination)
    }
}

impl FlacStemWriter {
    fn create(
        file: File,
        sample_rate_hz: u32,
        expected_frames: usize,
    ) -> Result<Self, OfflineExportError> {
        let total_samples = u64::try_from(expected_frames)
            .map_err(|_| OfflineExportError::CapacityUnavailable)?
            .checked_mul(2)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        let sync_file = file
            .try_clone()
            .map_err(OfflineExportError::StemDestination)?;
        let writer = FlacSampleWriter::new(
            file,
            FlacOptions::default(),
            sample_rate_hz,
            FLAC_BITS_PER_SAMPLE,
            2,
            Some(total_samples),
        )
        .map_err(flac_encoding_error)?;
        let mut pending_pcm = Vec::new();
        pending_pcm
            .try_reserve_exact(hyper_audio_engine::MAX_PROCESS_FRAMES * 2)
            .map_err(|_| OfflineExportError::CapacityUnavailable)?;
        Ok(Self {
            writer: Some(writer),
            sync_file,
            pending_pcm,
            expected_frames,
            written_frames: 0,
        })
    }

    fn append(&mut self, left: &[f32], right: &[f32]) -> Result<(), OfflineExportError> {
        if left.len() != right.len() {
            return Err(OfflineExportError::FlacVerification);
        }
        let next = self
            .written_frames
            .checked_add(left.len())
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        if next > self.expected_frames {
            return Err(OfflineExportError::FlacVerification);
        }
        self.pending_pcm.clear();
        for (&left, &right) in left.iter().zip(right) {
            self.pending_pcm.push(quantize_flac_sample(left)?);
            self.pending_pcm.push(quantize_flac_sample(right)?);
        }
        self.writer
            .as_mut()
            .ok_or(OfflineExportError::FlacVerification)?
            .write(&self.pending_pcm)
            .map_err(flac_encoding_error)?;
        self.written_frames = next;
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), OfflineExportError> {
        if self.written_frames != self.expected_frames || self.written_frames < FLAC_MINIMUM_FRAMES {
            return Err(OfflineExportError::FlacMinimumFrames);
        }
        self.writer
            .take()
            .ok_or(OfflineExportError::FlacVerification)?
            .finalize()
            .map_err(flac_encoding_error)?;
        self.sync_file
            .sync_all()
            .map_err(OfflineExportError::StemDestination)
    }
}

fn wav_data_bytes(frame_count: usize) -> Result<u64, OfflineExportError> {
    let data_bytes = u64::try_from(frame_count)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?
        .checked_mul(8)
        .ok_or(OfflineExportError::CapacityUnavailable)?;
    if data_bytes > WAV_MAX_DATA_BYTES {
        return Err(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the stem exceeds the standard WAV container data limit",
        )));
    }
    Ok(data_bytes)
}


fn create_track_stem_file_writers(
    stage: &cap_std::fs::Dir,
    stems: &[StemDescriptor],
    files: &[OfflineTrackStemExportFile],
    format: OfflineTrackStemExportFormat,
    sample_rate_hz: u32,
    expected_frames: usize,
) -> Result<Vec<StemFileWriter>, OfflineExportError> {
    if stems.len() != files.len() {
        return Err(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "every selected stem must have exactly one generated output file",
        )));
    }
    let mut writers = Vec::new();
    writers
        .try_reserve_exact(files.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for (stem, file) in stems.iter().zip(files) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let output = stage
            .open_with(&file.file_name, &options)
            .map_err(OfflineExportError::StemDestination)?
            .into_std();
        let writer = match format {
            OfflineTrackStemExportFormat::Wav => {
                StemContainerWriter::Wav(WavStemWriter::create(output, sample_rate_hz, expected_frames)?)
            }
            OfflineTrackStemExportFormat::Flac => {
                StemContainerWriter::Flac(Box::new(FlacStemWriter::create(
                    output,
                    sample_rate_hz,
                    expected_frames,
                )?))
            }
        };
        writers.push(StemFileWriter {
            channel_index: stem.channel_index,
            writer,
        });
    }
    Ok(writers)
}

fn preflight_track_stem_destination(destination_directory: &Path) -> Result<(), OfflineExportError> {
    let (parent_path, destination_name) = stem_destination_parts(destination_directory)?;
    let _ = open_track_stem_destination_parent(&parent_path, &destination_name)?;
    Ok(())
}

fn open_track_stem_destination_parent(
    parent_path: &Path,
    destination_name: &str,
) -> Result<cap_std::fs::Dir, OfflineExportError> {
    let parent = open_syncable_directory_ambient(parent_path)
        .map_err(OfflineExportError::StemDestination)?;
    match parent.symlink_metadata(destination_name) {
        Ok(_) => Err(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "stem export destination already exists",
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(parent),
        Err(error) => Err(OfflineExportError::StemDestination(error)),
    }
}

fn cleanup_stem_stage<T>(
    primary: OfflineExportError,
    parent: &cap_std::fs::Dir,
    stage_name: &str,
    stage_identity: (u64, u64),
) -> Result<T, OfflineExportError> {
    match remove_directory_entry(parent, stage_name, stage_identity) {
        Ok(()) => {
            let _ = sync_directory(parent);
            Err(primary)
        }
        Err(cleanup) => Err(OfflineExportError::StemDestination(std::io::Error::new(
            cleanup.kind(),
            format!("{primary}; stem staging cleanup also failed: {cleanup}"),
        ))),
    }
}

#[cfg(all(test, target_os = "linux"))]
fn publish_stem_bytes(
    directory: &cap_std::fs::Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), OfflineExportError> {
    publish_bytes_no_replace(directory, name, bytes).map_err(OfflineExportError::StemDestination)
}

#[cfg(all(test, windows))]
fn publish_stem_bytes(
    directory: &cap_std::fs::Dir,
    name: &str,
    bytes: &[u8],
) -> Result<(), OfflineExportError> {
    publish_verified_bytes_in_directory(directory, name, bytes)
}

#[cfg(all(test, not(any(target_os = "linux", windows))))]
fn publish_stem_bytes(
    _: &cap_std::fs::Dir,
    _: &str,
    _: &[u8],
) -> Result<(), OfflineExportError> {
    Err(OfflineExportError::StemDestination(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "verified stem publication is supported only on Linux and Windows",
    )))
}

fn stem_destination_parts(
    destination_directory: &Path,
) -> Result<(PathBuf, String), OfflineExportError> {
    let parent = destination_directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = destination_directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or(OfflineExportError::StemDestination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "stem export directory name is not valid Unicode",
        )))?;
    Ok((parent.to_path_buf(), name.to_owned()))
}

struct StemRenderBuffer {
    channel_index: usize,
    track_id: String,
    track_name: String,
    left: Vec<f32>,
    right: Vec<f32>,
}

fn select_stem_descriptors(
    session: &PreparedPlaybackSession,
    mixer: &CompiledMixerGraph,
) -> Result<Vec<StemDescriptor>, OfflineExportError> {
    let mut stems = Vec::new();
    stems
        .try_reserve_exact(mixer.slots().len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for (channel_index, slot) in mixer.slots().iter().enumerate() {
        if slot.kind() != CompiledMixerTrackKind::Audio {
            continue;
        }
        let track_name = session
            .mixer_channel_name_at(channel_index)
            .ok_or(OfflineExportError::StemTrackMetadataUnavailable { channel_index })?;
        stems.push(StemDescriptor {
            channel_index,
            track_id: slot.track_id().to_owned(),
            track_name: track_name.to_owned(),
        });
    }
    if stems.is_empty() {
        return Err(OfflineExportError::NoAudioTrackStems);
    }
    Ok(stems)
}

fn track_stem_export_report(
    stems: &[StemDescriptor],
    frame_count: usize,
    sample_rate_hz: u32,
    format: OfflineTrackStemExportFormat,
) -> Result<OfflineTrackStemExportReport, OfflineExportError> {
    if stems.is_empty() {
        return Err(OfflineExportError::NoAudioTrackStems);
    }
    let mut files = Vec::new();
    files
        .try_reserve_exact(stems.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for (index, stem) in stems.iter().enumerate() {
        let ordinal = index
            .checked_add(1)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
        files.push(OfflineTrackStemExportFile {
            ordinal,
            track_id: stem.track_id.clone(),
            track_name: stem.track_name.clone(),
            file_name: stem_file_name(ordinal, &stem.track_name, format)?,
        });
    }
    Ok(OfflineTrackStemExportReport {
        frame_count,
        sample_rate_hz,
        format,
        files,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one pass retains the required shared renderer/device state for every aligned stem"
)]
fn render_project_track_stems_with_devices(
    session: &PreparedPlaybackSession,
    device_chains: &[DeviceChain],
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
) -> Result<Vec<OfflineTrackStem>, OfflineExportError> {
    let PlaybackExtent::Bounded { end_frame } = session.extent() else {
        return Err(OfflineExportError::UnboundedExtent);
    };
    if end_frame == 0 {
        return Err(OfflineExportError::EmptyExtent);
    }
    if spec.block_frames == 0 || spec.block_frames > hyper_audio_engine::MAX_PROCESS_FRAMES {
        return Err(OfflineExportError::InvalidBlockFrames);
    }
    let block_frames = spec.block_frames.min(end_frame);

    let (project, prepared_mixer, automation, media) = session.parts();
    let mixer = live_mixer.unwrap_or(prepared_mixer);
    if !same_mixer_layout(prepared_mixer, mixer) {
        return Err(OfflineExportError::MixerLayoutMismatch);
    }
    let mut stems = select_stem_buffers(session, mixer, end_frame)?;
    let facts: Vec<_> = media.media_asset_facts().collect();
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(OfflineExportError::Playback)?;
    let prepared = media
        .prepare_playback(&graph)
        .map_err(OfflineExportError::Media)?;
    let mut runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated_with_devices(
            &prepared,
            mixer,
            automation.clone(),
            device_chains,
            block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback_with_devices(
            &prepared,
            mixer,
            device_chains,
            block_frames,
        ),
    }
    .map_err(OfflineExportError::Runtime)?;

    let slot_count = mixer.slots().len();
    let mut scratch_left = allocate_planes(slot_count, block_frames)?;
    let mut scratch_right = allocate_planes(slot_count, block_frames)?;
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(slot_count)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    scratch.extend(
        scratch_left
            .iter_mut()
            .zip(&mut scratch_right)
            .map(|(left, right)| RenderScratchSlot::new(left, right)),
    );
    let mut main_left = allocate_silence(block_frames)?;
    let mut main_right = allocate_silence(block_frames)?;
    let mut channel_indices = Vec::new();
    channel_indices
        .try_reserve_exact(stems.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    channel_indices.extend(stems.iter().map(|stem| stem.channel_index));

    render_selected_track_stem_windows(
        &mut runtime,
        &mut scratch,
        &mut main_left,
        &mut main_right,
        &channel_indices,
        end_frame,
        block_frames,
        |start_frame, channel_index, left, right| {
            let stem = stems
                .iter_mut()
                .find(|stem| stem.channel_index == channel_index)
                .expect("the runtime emits only the prevalidated selected channels");
            let end_frame = start_frame
                .checked_add(left.len())
                .expect("the bounded renderer prevalidated the export extent");
            stem.left[start_frame..end_frame].copy_from_slice(left);
            stem.right[start_frame..end_frame].copy_from_slice(right);
            Ok(())
        },
    )?;

    let mut output = Vec::new();
    output
        .try_reserve_exact(stems.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for stem in stems {
        output.push(OfflineTrackStem {
            track_id: stem.track_id,
            track_name: stem.track_name,
            audio: DecodedAudio::from_stereo(session.sample_rate_hz(), stem.left, stem.right)
                .map_err(OfflineExportError::Output)?,
        });
    }
    Ok(output)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one bounded renderer owns the shared runtime and scratch state for every stem sink"
)]
fn render_selected_track_stem_windows<R: TrackWindowRenderer>(
    runtime: &mut RoutedMainRuntime<R>,
    scratch: &mut [RenderScratchSlot<'_>],
    main_left: &mut [f32],
    main_right: &mut [f32],
    channel_indices: &[usize],
    end_frame: usize,
    block_frames: usize,
    mut sink: impl FnMut(usize, usize, &[f32], &[f32]) -> Result<(), OfflineExportError>,
) -> Result<(), OfflineExportError> {
    let mut rendered = 0_usize;
    while rendered < end_frame {
        let frames = block_frames.min(end_frame - rendered);
        let mut sink_error = None;
        runtime
            .process_window_with_channel_tap_sink(
                rendered,
                scratch,
                StereoOutput {
                    left: &mut main_left[..frames],
                    right: &mut main_right[..frames],
                },
                channel_indices,
                |channel_index, left, right| {
                    if sink_error.is_none() {
                        sink_error = sink(rendered, channel_index, left, right).err();
                    }
                },
            )
            .map_err(OfflineExportError::Process)?;
        if let Some(error) = sink_error {
            return Err(error);
        }
        rendered = rendered
            .checked_add(frames)
            .ok_or(OfflineExportError::CapacityUnavailable)?;
    }
    Ok(())
}

fn same_mixer_layout(
    prepared: &CompiledMixerGraph,
    candidate: &CompiledMixerGraph,
) -> bool {
    prepared.project_id() == candidate.project_id()
        && prepared.slots().len() == candidate.slots().len()
        && prepared
            .slots()
            .iter()
            .zip(candidate.slots())
            .all(|(prepared, candidate)| {
                prepared.track_id() == candidate.track_id() && prepared.kind() == candidate.kind()
            })
}

fn select_stem_buffers(
    session: &PreparedPlaybackSession,
    mixer: &CompiledMixerGraph,
    end_frame: usize,
) -> Result<Vec<StemRenderBuffer>, OfflineExportError> {
    let descriptors = select_stem_descriptors(session, mixer)?;
    let mut stems = Vec::new();
    stems
        .try_reserve_exact(descriptors.len())
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for descriptor in descriptors {
        stems.push(StemRenderBuffer {
            channel_index: descriptor.channel_index,
            track_id: descriptor.track_id,
            track_name: descriptor.track_name,
            left: allocate_silence(end_frame)?,
            right: allocate_silence(end_frame)?,
        });
    }
    Ok(stems)
}

fn allocate_planes(
    count: usize,
    frames: usize,
) -> Result<Vec<Vec<f32>>, OfflineExportError> {
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(count)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for _ in 0..count {
        planes.push(allocate_silence(frames)?);
    }
    Ok(planes)
}

fn allocate_silence(frames: usize) -> Result<Vec<f32>, OfflineExportError> {
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(frames)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    plane.resize(frames, 0.0);
    Ok(plane)
}

/// One rendered export written to disk, with render-domain measurements the
/// live meters would report for that f32 mix.
///
/// A float WAV contains those samples directly. A FLAC export deliberately
/// quantizes them to its documented 24-bit PCM contract, so these values
/// describe the source render rather than a second measurement of the
/// serialized delivery file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OfflineExportReport {
    pub frame_count: usize,
    pub sample_rate_hz: u32,
    pub sample_peak: f32,
    pub true_peak: f32,
    pub integrated_lufs: Option<f32>,
}

/// Renders the project, publishes it as one float WAV, and measures it.
///
/// The audio is staged through the recording journal so publication reuses the
/// proven durable path, including its recovery. The journal stores `f32`, so
/// the published file carries the rendered samples without quantization.
///
/// Measurement runs the same 4x-oversampled true-peak and EBU loudness meters
/// the live output uses, over the rendered audio in the same block order.
///
/// # Errors
///
/// Returns a classified rejection for any render failure, or a journal
/// staging, append, or publication failure.
pub fn export_project_wav(
    session: &PreparedPlaybackSession,
    spec: OfflineExportSpec,
    journal_path: &Path,
    wav_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    export_project_wav_with_mixer(session, spec, None, journal_path, wav_path)
}

/// Exports against a mixer graph other than the prepared one.
///
/// # Errors
///
/// Returns the same classified rejections as [`export_project_wav`].
pub fn export_project_wav_with_mixer(
    session: &PreparedPlaybackSession,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    journal_path: &Path,
    wav_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let rendered = render_project_offline_with_mixer(session, spec, live_mixer)?;
    publish_rendered_wav(&rendered, spec, journal_path, wav_path)
}

/// Exports a structurally verified project, including its native device chains.
///
/// # Errors
///
/// Returns the same classified rejections as [`export_project_wav_with_mixer`].
pub fn export_prepared_project_wav_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    journal_path: &Path,
    wav_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let rendered = render_prepared_project_offline_with_mixer(project, spec, live_mixer)?;
    publish_rendered_wav(&rendered, spec, journal_path, wav_path)
}

/// Renders a structurally verified project and atomically publishes a native
/// 24-bit FLAC export to a new destination.
///
/// FLAC is lossless with respect to the explicitly quantized signed 24-bit
/// PCM stream: every finite sample in `[-1.0, 1.0]` is multiplied by
/// `8_388_607` and rounded with [`f32::round`]. Thus both `-1.0` and `1.0`
/// map to the signed 24-bit extrema representable by that symmetric scale.
/// Non-finite and out-of-range rendered samples are rejected; export never
/// normalizes or silently clips the audible mix.
///
/// # Errors
///
/// Returns the usual render failures, a typed output rejection when the
/// rendered signal cannot be represented as 24-bit PCM, or a destination
/// failure. The staged payload is decoded through the project's FLAC reader
/// and compared to the quantized PCM before publication. The destination must
/// be absent: FLAC export fails closed instead of using a pathname overwrite
/// that cannot retain verified source authority on Linux.
pub fn export_prepared_project_flac_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    flac_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let rendered = render_prepared_project_offline_with_mixer(project, spec, live_mixer)?;
    publish_rendered_flac(&rendered, spec, flac_path)
}

const FLAC_BITS_PER_SAMPLE: u32 = 24;
const FLAC_MINIMUM_FRAMES: usize = 32;
const FLAC_PCM_SCALE: f32 = 8_388_607.0;
const FLAC_DECODE_SCALE: f32 = 8_388_608.0;

fn publish_rendered_flac(
    rendered: &DecodedAudio,
    spec: OfflineExportSpec,
    flac_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let encoded = encode_verified_flac(rendered)?;
    publish_verified_flac(flac_path, &encoded)?;
    measure_rendered_audio(rendered, spec)
}

fn encode_verified_flac(rendered: &DecodedAudio) -> Result<Vec<u8>, OfflineExportError> {
    let frame_count = rendered.frame_count();
    if frame_count < FLAC_MINIMUM_FRAMES {
        return Err(OfflineExportError::FlacMinimumFrames);
    }
    let sample_rate_hz = rendered.sample_rate_hz();
    let left = rendered.plane(0).ok_or(OfflineExportError::EmptyExtent)?;
    let right = rendered.plane(1).unwrap_or(left);
    let pcm = quantize_flac_pcm(left, right)?;
    let encoded = encode_flac_pcm(&pcm, sample_rate_hz)?;
    verify_flac_pcm(&encoded, sample_rate_hz, frame_count, &pcm)?;
    Ok(encoded)
}

#[cfg(test)]
fn verify_and_publish_flac(
    rendered: &DecodedAudio,
    spec: OfflineExportSpec,
    flac_path: &Path,
    expected_pcm: &[i32],
    encoded: &[u8],
) -> Result<OfflineExportReport, OfflineExportError> {
    verify_flac_pcm(
        encoded,
        rendered.sample_rate_hz(),
        rendered.frame_count(),
        expected_pcm,
    )?;
    publish_verified_flac(flac_path, encoded)?;
    measure_rendered_audio(rendered, spec)
}

fn quantize_flac_pcm(left: &[f32], right: &[f32]) -> Result<Vec<i32>, OfflineExportError> {
    if left.len() != right.len() {
        return Err(OfflineExportError::FlacVerification);
    }
    let sample_count = left
        .len()
        .checked_mul(2)
        .ok_or(OfflineExportError::CapacityUnavailable)?;
    let mut pcm = Vec::new();
    pcm.try_reserve_exact(sample_count)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    for (&left, &right) in left.iter().zip(right) {
        pcm.push(quantize_flac_sample(left)?);
        pcm.push(quantize_flac_sample(right)?);
    }
    Ok(pcm)
}

fn quantize_flac_sample(sample: f32) -> Result<i32, OfflineExportError> {
    if !sample.is_finite() || !(-1.0..=1.0).contains(&sample) {
        return Err(OfflineExportError::FlacVerification);
    }
    rounded_flac_pcm(sample * FLAC_PCM_SCALE)
}

fn encode_flac_pcm(pcm: &[i32], sample_rate_hz: u32) -> Result<Vec<u8>, OfflineExportError> {
    if !pcm.len().is_multiple_of(2) {
        return Err(OfflineExportError::FlacVerification);
    }
    let total_samples = u64::try_from(pcm.len()).map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let sink = MemoryFlacSink::new();
    let mut writer = FlacSampleWriter::new(
        sink.clone(),
        FlacOptions::default(),
        sample_rate_hz,
        FLAC_BITS_PER_SAMPLE,
        2,
        Some(total_samples),
    )
    .map_err(flac_encoding_error)?;
    writer.write(pcm).map_err(flac_encoding_error)?;
    writer.finalize().map_err(flac_encoding_error)?;
    sink.into_bytes()
}

#[derive(Clone)]
struct MemoryFlacSink(Rc<RefCell<Cursor<Vec<u8>>>>);

impl MemoryFlacSink {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Cursor::new(Vec::new()))))
    }

    fn into_bytes(self) -> Result<Vec<u8>, OfflineExportError> {
        Rc::try_unwrap(self.0)
            .map_err(|_| {
                OfflineExportError::FlacEncoding(
                    "the in-memory FLAC sink remained retained after finalization".to_owned(),
                )
            })
            .map(|sink| sink.into_inner().into_inner())
    }
}

impl Write for MemoryFlacSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.borrow_mut().flush()
    }
}

impl Seek for MemoryFlacSink {
    fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.borrow_mut().seek(position)
    }
}

fn flac_encoding_error(source: flac_codec::Error) -> OfflineExportError {
    match source {
        flac_codec::Error::Io(source) => OfflineExportError::StemDestination(source),
        source => OfflineExportError::FlacEncoding(source.to_string()),
    }
}

fn verify_flac_pcm(
    encoded: &[u8],
    sample_rate_hz: u32,
    frame_count: usize,
    expected_pcm: &[i32],
) -> Result<(), OfflineExportError> {
    let maximum_file_bytes = u64::try_from(encoded.len()).map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let maximum_decoded_bytes = frame_count
        .checked_mul(2)
        .and_then(|samples| samples.checked_mul(std::mem::size_of::<f32>()))
        .ok_or(OfflineExportError::CapacityUnavailable)?;
    let limits = hyper_audio_source::DecodeLimits::new(
        maximum_file_bytes,
        u64::try_from(frame_count).map_err(|_| OfflineExportError::CapacityUnavailable)?,
        maximum_decoded_bytes,
    )
    .map_err(OfflineExportError::Output)?;
    let decoded = hyper_audio_source::decode_bytes(
        encoded.to_vec(),
        hyper_audio_source::AudioContainer::Flac,
        sample_rate_hz,
        limits,
    )
    .map_err(OfflineExportError::Output)?;
    if decoded.frame_count() != frame_count {
        return Err(OfflineExportError::FlacVerification);
    }
    let left = decoded.plane(0).ok_or(OfflineExportError::FlacVerification)?;
    let right = decoded.plane(1).unwrap_or(left);
    for ((&left, &right), expected) in left.iter().zip(right).zip(expected_pcm.chunks_exact(2)) {
        if rounded_flac_pcm(left * FLAC_DECODE_SCALE)? != expected[0]
            || rounded_flac_pcm(right * FLAC_DECODE_SCALE)? != expected[1]
        {
            return Err(OfflineExportError::FlacVerification);
        }
    }
    Ok(())
}

fn rounded_flac_pcm(value: f32) -> Result<i32, OfflineExportError> {
    let rounded = value.round();
    if !rounded.is_finite() || rounded.abs() > FLAC_DECODE_SCALE {
        return Err(OfflineExportError::FlacVerification);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the preceding finite inclusive i32-domain proof makes this conversion exact"
    )]
    Ok(rounded as i32)
}

#[cfg(target_os = "linux")]
fn publish_verified_flac(flac_path: &Path, encoded: &[u8]) -> Result<(), OfflineExportError> {
    publish_verified_flac_linux(flac_path, encoded)
}

#[cfg(windows)]
fn publish_verified_flac(flac_path: &Path, encoded: &[u8]) -> Result<(), OfflineExportError> {
    publish_verified_flac_observed(flac_path, encoded, |_| Ok(()))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn publish_verified_flac(_: &Path, _: &[u8]) -> Result<(), OfflineExportError> {
    Err(OfflineExportError::Destination(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "verified FLAC publication is supported only on Linux and Windows",
    )))
}

#[cfg(target_os = "linux")]
fn publish_verified_flac_linux(
    flac_path: &Path,
    encoded: &[u8],
) -> Result<(), OfflineExportError> {
    let (parent_path, destination_name) = flac_destination_parts(flac_path)?;
    let parent = open_syncable_directory_ambient(&parent_path).map_err(OfflineExportError::Destination)?;
    publish_bytes_no_replace(&parent, &destination_name, encoded)
        .map_err(OfflineExportError::Destination)?;
    sync_directory(&parent).map_err(OfflineExportError::Destination)
}

#[cfg(windows)]
fn publish_verified_flac_observed(
    flac_path: &Path,
    encoded: &[u8],
    after_stage_verified: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), OfflineExportError> {
    let (parent_path, destination_name) = flac_destination_parts(flac_path)?;
    let parent = open_syncable_directory_ambient(&parent_path).map_err(OfflineExportError::Destination)?;
    match parent.symlink_metadata(&destination_name) {
        Ok(_) => {
            return Err(OfflineExportError::Destination(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "FLAC destination already exists",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(OfflineExportError::Destination(error)),
    }
    let stage_name = format!(".{destination_name}.flac-stage-{}", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut staged = parent
        .open_with(&stage_name, &options)
        .map_err(OfflineExportError::Destination)?;
    #[cfg(windows)]
    let stage_identity = identity(
        &staged
            .metadata()
            .map_err(OfflineExportError::Destination)?,
    );
    let result = (|| {
        staged.write_all(encoded).map_err(OfflineExportError::Destination)?;
        staged.sync_all().map_err(OfflineExportError::Destination)?;
        drop(staged);
        let maximum_stage_bytes = u64::try_from(encoded.len())
            .map_err(|_| OfflineExportError::CapacityUnavailable)?;
        let snapshot = read_stable_file_snapshot(&parent, &stage_name, maximum_stage_bytes)
            .map_err(OfflineExportError::Destination)?;
        let (staged_bytes, staged_evidence) = snapshot.into_parts();
        if staged_bytes != encoded {
            return Err(OfflineExportError::FlacVerification);
        }
        after_stage_verified(&parent_path.join(&stage_name)).map_err(OfflineExportError::Destination)?;
        rename_no_replace(
            &parent,
            &stage_name,
            staged_evidence.identity(),
            &parent,
            &destination_name,
        )
        .map_err(OfflineExportError::Destination)?;
        sync_directory(&parent).map_err(OfflineExportError::Destination)
    })();
    if result.is_err() {
        #[cfg(windows)]
        let _ = remove_file_if_identity(&parent, &stage_name, stage_identity);
    }
    result
}

#[cfg(all(test, windows))]
fn publish_verified_bytes_in_directory(
    directory: &cap_std::fs::Dir,
    destination_name: &str,
    bytes: &[u8],
) -> Result<(), OfflineExportError> {
    match directory.symlink_metadata(destination_name) {
        Ok(_) => {
            return Err(OfflineExportError::StemDestination(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "staged stem file already exists",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(OfflineExportError::StemDestination(error)),
    }
    let stage_name = format!(".hyper-stem-file-stage-{}", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut staged = directory
        .open_with(&stage_name, &options)
        .map_err(OfflineExportError::StemDestination)?;
    let stage_identity = identity(
        &staged
            .metadata()
            .map_err(OfflineExportError::StemDestination)?,
    );
    let result = (|| {
        staged
            .write_all(bytes)
            .map_err(OfflineExportError::StemDestination)?;
        staged
            .sync_all()
            .map_err(OfflineExportError::StemDestination)?;
        drop(staged);
        let maximum_stage_bytes =
            u64::try_from(bytes.len()).map_err(|_| OfflineExportError::CapacityUnavailable)?;
        let snapshot = read_stable_file_snapshot(directory, &stage_name, maximum_stage_bytes)
            .map_err(OfflineExportError::StemDestination)?;
        let (staged_bytes, staged_evidence) = snapshot.into_parts();
        if staged_bytes != bytes {
            return Err(OfflineExportError::StemDestination(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "staged stem file does not match its verified bytes",
            )));
        }
        rename_no_replace(
            directory,
            &stage_name,
            staged_evidence.identity(),
            directory,
            destination_name,
        )
        .map_err(OfflineExportError::StemDestination)
    })();
    if result.is_err() {
        let _ = remove_file_if_identity(directory, &stage_name, stage_identity);
    }
    result
}

fn flac_destination_parts(flac_path: &Path) -> Result<(PathBuf, String), OfflineExportError> {
    let parent = flac_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = flac_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .ok_or(OfflineExportError::Destination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "FLAC destination name is not valid Unicode",
        )))?;
    Ok((parent.to_path_buf(), name.to_owned()))
}

fn measure_rendered_audio(
    rendered: &DecodedAudio,
    spec: OfflineExportSpec,
) -> Result<OfflineExportReport, OfflineExportError> {
    let frame_count = rendered.frame_count();
    let sample_rate_hz = rendered.sample_rate_hz();
    let left = rendered.plane(0).ok_or(OfflineExportError::EmptyExtent)?;
    let right = rendered.plane(1).unwrap_or(left);
    let block_frames = spec.block_frames.min(frame_count).max(1);
    let mut true_peak_meter =
        StereoTruePeakMeter::try_new(sample_rate_hz).map_err(OfflineExportError::TruePeak)?;
    let mut loudness_meter =
        StereoLoudnessMeter::try_new(sample_rate_hz).map_err(OfflineExportError::Loudness)?;
    let mut sample_peak = 0.0_f32;
    let mut true_peak = 0.0_f32;
    for start in (0..frame_count).step_by(block_frames) {
        let end = (start + block_frames).min(frame_count);
        for index in start..end {
            sample_peak = sample_peak.max(left[index].abs()).max(right[index].abs());
        }
        let block = true_peak_meter
            .process(&left[start..end], &right[start..end])
            .map_err(OfflineExportError::TruePeak)?;
        let block_true_peak = block.left_linear().max(block.right_linear());
        true_peak = true_peak.max(block_true_peak);
        loudness_meter
            .process(&left[start..end], &right[start..end], block_true_peak)
            .map_err(OfflineExportError::Loudness)?;
    }
    Ok(OfflineExportReport {
        frame_count,
        sample_rate_hz,
        sample_peak,
        true_peak,
        integrated_lufs: loudness_meter.snapshot().integrated_lufs(),
    })
}

fn publish_rendered_wav(
    rendered: &DecodedAudio,
    spec: OfflineExportSpec,
    journal_path: &Path,
    wav_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let frame_count = rendered.frame_count();
    let sample_rate_hz = rendered.sample_rate_hz();
    let left = rendered.plane(0).ok_or(OfflineExportError::EmptyExtent)?;
    let right = rendered.plane(1).unwrap_or(left);
    // The journal rejects a chunk bound above the total, so a project shorter
    // than one block still exports.
    let block_frames = spec.block_frames.min(frame_count).max(1);

    let max_chunk_frames = u32::try_from(block_frames)
        .map_err(|_| OfflineExportError::InvalidBlockFrames)?;
    // A journal left by a failed export, and a destination from an earlier
    // one, both belong to this caller's chosen paths. Publication refuses to
    // replace existing media, so re-exporting has to clear them first.
    if journal_path.exists() {
        std::fs::remove_file(journal_path).map_err(OfflineExportError::Destination)?;
    }
    if wav_path.exists() {
        std::fs::remove_file(wav_path).map_err(OfflineExportError::Destination)?;
    }
    let mut writer = JournalWriter::create(
        journal_path,
        JournalConfig {
            sample_rate: sample_rate_hz,
            max_chunk_frames,
            max_total_frames: frame_count as u64,
        },
    )
    .map_err(OfflineExportError::Journal)?;

    let mut interleaved = Vec::new();
    interleaved
        .try_reserve_exact(block_frames * 2)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let mut true_peak_meter =
        StereoTruePeakMeter::try_new(sample_rate_hz).map_err(OfflineExportError::TruePeak)?;
    let mut loudness_meter =
        StereoLoudnessMeter::try_new(sample_rate_hz).map_err(OfflineExportError::Loudness)?;

    let mut sample_peak = 0.0_f32;
    let mut true_peak = 0.0_f32;
    for start in (0..frame_count).step_by(block_frames) {
        let end = (start + block_frames).min(frame_count);
        interleaved.clear();
        for index in start..end {
            interleaved.push(left[index]);
            interleaved.push(right[index]);
            sample_peak = sample_peak.max(left[index].abs()).max(right[index].abs());
        }
        let block = true_peak_meter
            .process(&left[start..end], &right[start..end])
            .map_err(OfflineExportError::TruePeak)?;
        let block_true_peak = block.left_linear().max(block.right_linear());
        true_peak = true_peak.max(block_true_peak);
        loudness_meter
            .process(&left[start..end], &right[start..end], block_true_peak)
            .map_err(OfflineExportError::Loudness)?;
        writer
            .append(&interleaved)
            .map_err(OfflineExportError::Journal)?;
    }
    writer
        .publish_wav(wav_path)
        .map_err(OfflineExportError::Journal)?;
    drop(writer);
    // The staging journal is this function's own temporary. Leaving it behind
    // would make the next export to the same destination fail before it starts.
    std::fs::remove_file(journal_path).map_err(OfflineExportError::Destination)?;

    Ok(OfflineExportReport {
        frame_count,
        sample_rate_hz,
        sample_peak,
        true_peak,
        integrated_lufs: loudness_meter.snapshot().integrated_lufs(),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        OfflineExportError, OfflineExportSpec, OfflineTrackStem, OfflineTrackStemExportFormat,
        OfflineTrackStemExportFile, StemDescriptor, create_track_stem_file_writers,
        encode_flac_pcm, flac_destination_parts, publish_rendered_flac,
        preflight_track_stem_destination, publish_rendered_track_stems, quantize_flac_pcm, verify_and_publish_flac,
        verify_flac_pcm,
    };
    #[cfg(windows)]
    use super::publish_verified_flac_observed;
    use hyper_audio_source::{AudioContainer, DecodeLimits, DecodedAudio, decode_bytes};
    use std::path::Path;

    #[test]
    fn bounded_stem_writers_finalize_exact_wav_and_quantized_flac() {
        let directory = tempfile::tempdir().unwrap();
        let stage = hyper_fs_authority::open_syncable_directory_ambient(directory.path()).unwrap();
        let left: Vec<_> = (0..4_097_i16)
            .map(|frame| f32::from(frame % 200) / 200.0 - 0.5)
            .collect();
        let right: Vec<_> = left.iter().map(|sample| -*sample).collect();
        let stems = vec![StemDescriptor {
            channel_index: 3,
            track_id: "track".to_owned(),
            track_name: "Track".to_owned(),
        }];

        for (format, name) in [
            (OfflineTrackStemExportFormat::Wav, "stem.wav"),
            (OfflineTrackStemExportFormat::Flac, "stem.flac"),
        ] {
            let files = vec![OfflineTrackStemExportFile {
                ordinal: 1,
                track_id: "track".to_owned(),
                track_name: "Track".to_owned(),
                file_name: name.to_owned(),
            }];
            let mut writers = create_track_stem_file_writers(
                &stage,
                &stems,
                &files,
                format,
                48_000,
                left.len(),
            )
            .unwrap();
            writers[0].append(&left[..7], &right[..7]).unwrap();
            writers[0].append(&left[7..], &right[7..]).unwrap();
            writers[0].finalize().unwrap();
            drop(writers);

            let encoded = std::fs::read(directory.path().join(name)).unwrap();
            match format {
                OfflineTrackStemExportFormat::Wav => {
                    let decoded = decode_bytes(
                        encoded,
                        AudioContainer::Wav,
                        48_000,
                        DecodeLimits::new(1_048_576, 4_097, 65_536).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(decoded.plane(0), Some(left.as_slice()));
                    assert_eq!(decoded.plane(1), Some(right.as_slice()));
                }
                OfflineTrackStemExportFormat::Flac => {
                    let expected = quantize_flac_pcm(&left, &right).unwrap();
                    let reference = encode_flac_pcm(&expected, 48_000).unwrap();
                    verify_flac_pcm(&reference, 48_000, 4_097, &expected).unwrap();
                    verify_flac_pcm(&encoded, 48_000, 4_097, &expected).unwrap();
                }
            }
        }
    }

    #[test]
    fn stem_export_activates_only_a_complete_verified_directory() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("aligned-stems");
        let stems = vec![
            OfflineTrackStem {
                track_id: "track-lead".to_owned(),
                track_name: "Lead / Voice".to_owned(),
                audio: DecodedAudio::from_stereo(
                    48_000,
                    (0..32_i16).map(|frame| f32::from(frame) / 100.0).collect(),
                    (0..32_i16).map(|frame| -f32::from(frame) / 100.0).collect(),
                )
                .unwrap(),
            },
            OfflineTrackStem {
                track_id: "track-drums".to_owned(),
                track_name: "Drums".to_owned(),
                audio: DecodedAudio::from_stereo(
                    48_000,
                    vec![0.25; 32],
                    vec![-0.5; 32],
                )
                .unwrap(),
            },
        ];

        let report = publish_rendered_track_stems(
            &stems,
            OfflineTrackStemExportFormat::Wav,
            &destination,
        )
        .unwrap();

        assert_eq!(report.frame_count, 32);
        assert_eq!(report.sample_rate_hz, 48_000);
        assert_eq!(
            report.files.iter().map(|file| file.file_name.as_str()).collect::<Vec<_>>(),
            vec!["001-Lead%20%2F%20Voice.wav", "002-Drums.wav"]
        );
        for (stem, file) in stems.iter().zip(&report.files) {
            let bytes = std::fs::read(destination.join(&file.file_name)).unwrap();
            let decoded = decode_bytes(
                bytes,
                AudioContainer::Wav,
                48_000,
                DecodeLimits::new(1_048_576, 32, 1_024).unwrap(),
            )
            .unwrap();
            for channel in 0..2 {
                assert_eq!(decoded.plane(channel), stem.audio.plane(channel));
            }
        }
        assert_eq!(
            std::fs::read_dir(&destination).unwrap().count(),
            report.files.len(),
            "the activated directory contains only the complete ordered stem set"
        );
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("hyper-stem-stage")));
    }

    #[test]
    fn stem_export_refuses_an_existing_destination_without_staging_or_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("aligned-stems");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("preserve"), b"existing export").unwrap();
        let stems = vec![OfflineTrackStem {
            track_id: "track".to_owned(),
            track_name: "Track".to_owned(),
            audio: DecodedAudio::from_stereo(48_000, vec![0.0; 32], vec![0.0; 32]).unwrap(),
        }];

        let refused = publish_rendered_track_stems(
            &stems,
            OfflineTrackStemExportFormat::Wav,
            &destination,
        );

        assert!(matches!(refused, Err(OfflineExportError::StemDestination(_))));
        assert_eq!(std::fs::read(destination.join("preserve")).unwrap(), b"existing export");
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("hyper-stem-stage")));
    }

    #[test]
    fn stem_export_preflight_refuses_an_existing_destination_before_rendering() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("aligned-stems");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("preserve"), b"existing export").unwrap();

        let refusal = preflight_track_stem_destination(&destination);

        assert!(matches!(refusal, Err(OfflineExportError::StemDestination(error)) if error.kind() == std::io::ErrorKind::AlreadyExists));
        assert_eq!(std::fs::read(destination.join("preserve")).unwrap(), b"existing export");
    }

    #[test]
    fn stem_export_refuses_invalid_files_without_activation_or_stage_residue() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("aligned-stems");
        let stems = vec![OfflineTrackStem {
            track_id: "track".to_owned(),
            track_name: "Track".to_owned(),
            audio: DecodedAudio::from_stereo(48_000, vec![0.0; 31], vec![0.0; 31]).unwrap(),
        }];

        let refused = publish_rendered_track_stems(
            &stems,
            OfflineTrackStemExportFormat::Flac,
            &destination,
        );

        assert!(matches!(refused, Err(OfflineExportError::FlacMinimumFrames)));
        assert!(!destination.exists());
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("hyper-stem-stage")));
    }

    #[test]
    fn flac_export_round_trips_exact_quantized_stereo_pcm_to_a_new_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        let audio = DecodedAudio::from_stereo(
            48_000,
            [-1.0, -0.25, 0.2, 1.0].repeat(8),
            [0.75, -0.5, 0.125, -0.875].repeat(8),
        )
        .unwrap();

        let report = publish_rendered_flac(
            &audio,
            OfflineExportSpec { block_frames: 2 },
            &destination,
        )
        .unwrap();
        assert_eq!(report.frame_count, 32);
        assert_eq!(report.sample_rate_hz, 48_000);
        let encoded = std::fs::read(&destination).unwrap();
        assert!(encoded.starts_with(b"fLaC"));
        let expected = quantize_flac_pcm(
            audio.plane(0).unwrap(),
            audio.plane(1).unwrap(),
        )
        .unwrap();
        verify_flac_pcm(&encoded, 48_000, 32, &expected).unwrap();
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("flac-stage")));
    }

    #[test]
    fn flac_export_refuses_nonfinite_and_out_of_range_samples_before_publication() {
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.000_001, -1.000_001] {
            let refused = quantize_flac_pcm(&[sample], &[0.0]).unwrap_err();
            assert!(matches!(refused, OfflineExportError::FlacVerification));
        }
    }

    #[test]
    fn flac_export_refuses_short_render_without_replacing_an_existing_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("short.flac");
        std::fs::write(&destination, b"previous export").unwrap();
        let audio = DecodedAudio::from_stereo(48_000, vec![0.0; 31], vec![0.0; 31]).unwrap();
        let refused = publish_rendered_flac(
            &audio,
            OfflineExportSpec { block_frames: 31 },
            &destination,
        )
        .unwrap_err();
        assert!(matches!(refused, OfflineExportError::FlacMinimumFrames));
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous export");
    }

    #[test]
    fn flac_export_refuses_an_existing_destination_without_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        std::fs::write(&destination, b"previous export").unwrap();
        let audio = DecodedAudio::from_stereo(48_000, vec![0.0; 32], vec![0.0; 32]).unwrap();

        let refused = publish_rendered_flac(
            &audio,
            OfflineExportSpec { block_frames: 32 },
            &destination,
        );

        assert!(matches!(refused, Err(OfflineExportError::Destination(_))));
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous export");
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("flac-stage")));
    }

    #[test]
    fn flac_quantization_has_a_literal_signed_24_bit_stereo_contract() {
        let left = [-1.0, 0.0, 0.5, 0.25].repeat(8);
        let right = [1.0, -0.5, -0.25, 0.0].repeat(8);
        let expected_frame = [
            -8_388_607,
            8_388_607,
            0,
            -4_194_304,
            4_194_304,
            -2_097_152,
            2_097_152,
            0,
        ];
        let expected = expected_frame.repeat(8);
        let quantized = quantize_flac_pcm(&left, &right).unwrap();
        assert_eq!(quantized, expected);

        let encoded = encode_flac_pcm(&expected, 48_000).unwrap();
        let decoded = decode_flac_pcm_for_test(encoded, 48_000, 32);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn flac_quantization_rounds_half_lsb_ties_and_adjacent_f32_values() {
        let half_lsb = 0.5_f32 / 8_388_607.0;
        let below_half_lsb = f32::from_bits(half_lsb.to_bits() - 1);
        let above_half_lsb = f32::from_bits(half_lsb.to_bits() + 1);
        assert!(below_half_lsb * 8_388_607.0 < 0.5);
        assert_eq!((half_lsb * 8_388_607.0).to_bits(), 0.5_f32.to_bits());
        assert!(above_half_lsb * 8_388_607.0 > 0.5);

        let left = [
            below_half_lsb,
            half_lsb,
            above_half_lsb,
            -below_half_lsb,
            -half_lsb,
            -above_half_lsb,
        ]
        .repeat(6);
        let right = [
            -below_half_lsb,
            -half_lsb,
            -above_half_lsb,
            below_half_lsb,
            half_lsb,
            above_half_lsb,
        ]
        .repeat(6);
        let expected_frame = [0, 0, 1, -1, 1, -1, 0, 0, -1, 1, -1, 1];
        let expected = expected_frame.repeat(6);

        assert_eq!(quantize_flac_pcm(&left, &right).unwrap(), expected);
        let encoded = encode_flac_pcm(&expected, 48_000).unwrap();
        assert_eq!(decode_flac_pcm_for_test(encoded, 48_000, 36), expected);
    }

    #[cfg(windows)]
    #[test]
    fn flac_publication_rejects_a_stage_replacement_after_verification() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        let pcm = [0_i32, 0_i32].repeat(16);
        let encoded = encode_flac_pcm(&pcm, 48_000).unwrap();
        let refused = publish_verified_flac_observed(&destination, &encoded, |stage| {
            std::fs::remove_file(stage)?;
            std::fs::write(stage, b"replacement")
        });
        assert!(matches!(refused, Err(OfflineExportError::Destination(_))));
        assert!(!destination.exists());
        let replacement = std::fs::read_dir(directory.path())
            .unwrap()
            .find_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("flac-stage")
                    .then_some(entry.path())
            })
            .expect("replacement stage must remain for its owner");
        assert_eq!(std::fs::read(replacement).unwrap(), b"replacement");
    }

    #[cfg(windows)]
    #[test]
    fn flac_publication_removes_only_an_owned_stage_after_verification_failure() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        let pcm = [0_i32, 0_i32].repeat(16);
        let encoded = encode_flac_pcm(&pcm, 48_000).unwrap();

        let refused = publish_verified_flac_observed(&destination, &encoded, |_| {
            Err(std::io::Error::other("post-verification failure"))
        });

        assert!(matches!(refused, Err(OfflineExportError::Destination(_))));
        assert!(!destination.exists());
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("flac-stage")));
    }

    #[test]
    fn flac_publication_cleans_up_after_an_unreplaceable_destination_failure() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("preserve"), b"existing directory").unwrap();
        let audio = DecodedAudio::from_stereo(48_000, vec![0.0; 32], vec![0.0; 32]).unwrap();
        let refused = publish_rendered_flac(
            &audio,
            OfflineExportSpec { block_frames: 32 },
            &destination,
        );
        assert!(matches!(refused, Err(OfflineExportError::Destination(_))));
        assert_eq!(std::fs::read(destination.join("preserve")).unwrap(), b"existing directory");
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("flac-stage")));
    }

    #[test]
    fn flac_verification_refusal_preserves_the_existing_destination_before_staging() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        std::fs::write(&destination, b"previous export").unwrap();
        let audio = DecodedAudio::from_stereo(48_000, vec![0.0; 32], vec![0.0; 32]).unwrap();
        let expected_pcm = [0_i32, 0_i32].repeat(16);
        let refused = verify_and_publish_flac(
            &audio,
            OfflineExportSpec { block_frames: 32 },
            &destination,
            &expected_pcm,
            b"not a FLAC stream",
        );
        assert!(matches!(refused, Err(OfflineExportError::Output(_))));
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous export");
        assert!(std::fs::read_dir(directory.path())
            .unwrap()
            .all(|entry| !entry.unwrap().file_name().to_string_lossy().contains("flac-stage")));
    }

    #[test]
    fn relative_flac_destination_uses_the_current_directory_parent() {
        assert_eq!(flac_destination_parts(Path::new("export.flac")).unwrap().0, Path::new("."));
    }

    #[cfg(unix)]
    #[test]
    fn flac_destination_refuses_a_non_unicode_filename_before_publication() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = tempfile::tempdir().unwrap();
        let destination = directory
            .path()
            .join(OsString::from_vec(b"mix-\xFF.flac".to_vec()));
        let refused = flac_destination_parts(&destination).unwrap_err();
        assert!(matches!(refused, OfflineExportError::Destination(_)));
        assert!(std::fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    fn decode_flac_pcm_for_test(encoded: Vec<u8>, sample_rate_hz: u32, frames: usize) -> Vec<i32> {
        let limits = hyper_audio_source::DecodeLimits::new(
            u64::try_from(encoded.len()).unwrap(),
            u64::try_from(frames).unwrap(),
            frames * 2 * std::mem::size_of::<f32>(),
        )
        .unwrap();
        let decoded = hyper_audio_source::decode_bytes(
            encoded,
            hyper_audio_source::AudioContainer::Flac,
            sample_rate_hz,
            limits,
        )
        .unwrap();
        decoded
            .plane(0)
            .unwrap()
            .iter()
            .zip(decoded.plane(1).unwrap())
            .flat_map(|(&left, &right)| [decode_sample_for_test(left), decode_sample_for_test(right)])
            .collect()
    }

    fn decode_sample_for_test(sample: f32) -> i32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the literal test vectors are within the signed 24-bit domain"
        )]
        {
            (sample * 8_388_608.0).round() as i32
        }
    }
}
