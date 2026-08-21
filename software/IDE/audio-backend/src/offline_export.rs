use core::error::Error;
use core::fmt;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{
    StereoLoudnessError, StereoLoudnessMeter, StereoOutput, StereoTruePeakError,
    StereoTruePeakMeter,
};
use hyper_audio_source::{DecodeError, DecodedAudio};
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
use std::path::Path;

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
            | Self::MixerLayoutMismatch => None,
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
