use core::error::Error;
use core::fmt;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{
    StereoLoudnessError, StereoLoudnessMeter, StereoOutput, StereoTruePeakError,
    StereoTruePeakMeter,
};
use hyper_audio_source::{DecodeError, DecodedAudio};
use hyper_fs_authority::{
    identity, open_syncable_directory_ambient, read_stable_file, rename_replace, sync_directory,
};
use hyper_media_cache::PreparePlaybackError;
use hyper_mixer_runtime::{
    RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError, RuntimeProcessError,
    TransportCommand, TransportState,
};
use hyper_native_device::DeviceChain;
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{AdapterCompileError, CompiledMixerGraph};
use hyper_playback_session::{PlaybackExtent, PreparedPlaybackSession};
use hyper_project_loader::PreparedProjectPlayback;
use hyper_recording_journal::{JournalConfig, JournalError, JournalWriter};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use cap_std::fs::OpenOptions;
use flacenc::{
    bitsink::ByteSink,
    component::BitRepr,
    error::Verify,
    source::MemSource,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OfflineExportSpec {
    pub block_frames: usize,
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
    Destination(std::io::Error),
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
            Self::Destination(source) => {
                write!(formatter, "the export destination is unusable: {source}")
            }
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
            Self::Destination(source) => Some(source),
            Self::UnboundedExtent
            | Self::EmptyExtent
            | Self::InvalidBlockFrames
            | Self::CapacityUnavailable
            | Self::MixerLayoutMismatch
            | Self::FlacEncoding(_)
            | Self::FlacVerification
            | Self::FlacMinimumFrames => None,
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
    if mixer.slots().len() != prepared_mixer.slots().len()
        || mixer.project_id() != prepared_mixer.project_id()
    {
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

/// One rendered export written to disk, with the measurements the live meters
/// would report for the same audio.
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
/// 24-bit FLAC export.
///
/// FLAC is lossless with respect to the explicitly quantized signed 24-bit
/// PCM stream. Non-finite and out-of-range rendered samples are rejected;
/// export never normalizes or silently clips the audible mix.
///
/// # Errors
///
/// Returns the usual render failures, a typed output rejection when the
/// rendered signal cannot be represented as 24-bit PCM, or a destination
/// failure. The staged payload is decoded through the project's FLAC reader
/// and compared to the quantized PCM before it can replace the destination.
pub fn export_prepared_project_flac_with_mixer(
    project: &PreparedProjectPlayback,
    spec: OfflineExportSpec,
    live_mixer: Option<&CompiledMixerGraph>,
    flac_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
    let rendered = render_prepared_project_offline_with_mixer(project, spec, live_mixer)?;
    publish_rendered_flac(&rendered, spec, flac_path)
}

const FLAC_BITS_PER_SAMPLE: usize = 24;
const FLAC_MINIMUM_FRAMES: usize = 32;
const FLAC_PCM_SCALE: f32 = 8_388_607.0;
const FLAC_DECODE_SCALE: f32 = 8_388_608.0;

fn publish_rendered_flac(
    rendered: &DecodedAudio,
    spec: OfflineExportSpec,
    flac_path: &Path,
) -> Result<OfflineExportReport, OfflineExportError> {
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
    publish_verified_flac(flac_path, &encoded)?;
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
    let config = flacenc::config::Encoder::default()
        .into_verified()
        .map_err(|(_, source)| OfflineExportError::FlacEncoding(source.to_string()))?;
    let source = MemSource::from_samples(pcm, 2, FLAC_BITS_PER_SAMPLE, sample_rate_hz as usize);
    let frames = pcm.len() / 2;
    let block_size = frames.min(config.block_size).max(FLAC_MINIMUM_FRAMES);
    let stream = flacenc::encode_with_fixed_block_size(&config, source, block_size)
        .map_err(|source| OfflineExportError::FlacEncoding(source.to_string()))?;
    let mut sink = ByteSink::new();
    stream
        .write(&mut sink)
        .map_err(|source| OfflineExportError::FlacEncoding(source.to_string()))?;
    Ok(sink.into_inner())
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

fn publish_verified_flac(flac_path: &Path, encoded: &[u8]) -> Result<(), OfflineExportError> {
    let (parent_path, destination_name) = flac_destination_parts(flac_path)?;
    let parent = open_syncable_directory_ambient(&parent_path).map_err(OfflineExportError::Destination)?;
    let stage_name = format!(".{destination_name}.flac-stage-{}", uuid::Uuid::new_v4());
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut staged = parent
        .open_with(&stage_name, &options)
        .map_err(OfflineExportError::Destination)?;
    let result = (|| {
        staged.write_all(encoded).map_err(OfflineExportError::Destination)?;
        staged.sync_all().map_err(OfflineExportError::Destination)?;
        drop(staged);
        let maximum_stage_bytes = u64::try_from(encoded.len())
            .map_err(|_| OfflineExportError::CapacityUnavailable)?;
        let staged_bytes = read_stable_file(&parent, &stage_name, maximum_stage_bytes)
            .map_err(OfflineExportError::Destination)?;
        if staged_bytes != encoded {
            return Err(OfflineExportError::FlacVerification);
        }
        let staged_identity = identity(
            &parent
                .symlink_metadata(&stage_name)
                .map_err(OfflineExportError::Destination)?,
        );
        rename_replace(
            &parent,
            &stage_name,
            staged_identity,
            &parent,
            &destination_name,
        )
        .map_err(OfflineExportError::Destination)?;
        sync_directory(&parent).map_err(OfflineExportError::Destination)
    })();
    if result.is_err() {
        let _ = parent.remove_file(&stage_name);
    }
    result
}

fn flac_destination_parts(flac_path: &Path) -> Result<(PathBuf, String), OfflineExportError> {
    let parent = flac_path.parent().filter(|parent| !parent.as_os_str().is_empty()).ok_or(
        OfflineExportError::Destination(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "FLAC destination has no parent directory",
        )),
    )?;
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
        OfflineExportError, OfflineExportSpec, publish_rendered_flac, quantize_flac_pcm,
        verify_flac_pcm,
    };
    use hyper_audio_source::DecodedAudio;

    #[test]
    fn flac_export_round_trips_exact_quantized_stereo_pcm_and_replaces_only_after_verification() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("mix.flac");
        std::fs::write(&destination, b"previous export").unwrap();
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
}
