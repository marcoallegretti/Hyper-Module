use core::error::Error;
use core::fmt;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::StereoOutput;
use hyper_audio_source::{DecodeError, DecodedAudio};
use hyper_media_cache::PreparePlaybackError;
use hyper_mixer_runtime::{
    RoutedMainRuntime, RoutedTransportRuntime, RuntimeCreationError, RuntimeProcessError,
    TransportCommand, TransportState,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::AdapterCompileError;
use hyper_playback_session::{PlaybackExtent, PreparedPlaybackSession};
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
    ShortRender { rendered: usize, expected: usize },
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
            Self::ShortRender { rendered, expected } => write!(
                formatter,
                "offline export produced {rendered} of {expected} frames"
            ),
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
            Self::UnboundedExtent
            | Self::EmptyExtent
            | Self::InvalidBlockFrames
            | Self::CapacityUnavailable
            | Self::ShortRender { .. } => None,
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
    let PlaybackExtent::Bounded { end_frame } = session.extent() else {
        return Err(OfflineExportError::UnboundedExtent);
    };
    if end_frame == 0 {
        return Err(OfflineExportError::EmptyExtent);
    }
    if spec.block_frames == 0 || spec.block_frames > hyper_audio_engine::MAX_PROCESS_FRAMES {
        return Err(OfflineExportError::InvalidBlockFrames);
    }

    let (project, mixer, automation, media) = session.parts();
    let facts: Vec<_> = media.media_asset_facts().collect();
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .map_err(OfflineExportError::Playback)?;
    let prepared = media
        .prepare_playback(&graph)
        .map_err(OfflineExportError::Media)?;
    let runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated(
            &prepared,
            mixer,
            automation.clone(),
            spec.block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, spec.block_frames),
    }
    .map_err(OfflineExportError::Runtime)?;

    let slot_count = mixer.slots().len();
    let mut scratch_left = allocate_planes(slot_count, spec.block_frames)?;
    let mut scratch_right = allocate_planes(slot_count, spec.block_frames)?;
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

    let (mut sender, mut transport) = RoutedTransportRuntime::with_command_queue(runtime);
    sender
        .try_send(TransportCommand::Start)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let mut left = allocate_silence(end_frame)?;
    let mut right = allocate_silence(end_frame)?;
    let mut block_left = vec![0.0_f32; spec.block_frames];
    let mut block_right = vec![0.0_f32; spec.block_frames];

    let mut rendered = 0_usize;
    while rendered < end_frame {
        let frames = spec.block_frames.min(end_frame - rendered);
        let cycle = transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut block_left[..frames],
                    right: &mut block_right[..frames],
                },
            )
            .map_err(OfflineExportError::Process)?;
        if cycle.state() != TransportState::Rolling {
            break;
        }
        left[rendered..rendered + frames].copy_from_slice(&block_left[..frames]);
        right[rendered..rendered + frames].copy_from_slice(&block_right[..frames]);
        rendered += frames;
    }
    if rendered != end_frame {
        return Err(OfflineExportError::ShortRender {
            rendered,
            expected: end_frame,
        });
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
}

/// Renders the project and publishes it as one float WAV.
///
/// The audio is staged through the recording journal so publication reuses the
/// proven durable path, including its recovery. The journal stores `f32`, so
/// the published file carries the rendered samples without quantization.
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
    let rendered = render_project_offline(session, spec)?;
    let frame_count = rendered.frame_count();
    let sample_rate_hz = rendered.sample_rate_hz();
    let left = rendered.plane(0).ok_or(OfflineExportError::EmptyExtent)?;
    let right = rendered.plane(1).unwrap_or(left);

    let max_chunk_frames = u32::try_from(spec.block_frames)
        .map_err(|_| OfflineExportError::InvalidBlockFrames)?;
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
        .try_reserve_exact(spec.block_frames * 2)
        .map_err(|_| OfflineExportError::CapacityUnavailable)?;
    let mut sample_peak = 0.0_f32;
    for start in (0..frame_count).step_by(spec.block_frames) {
        let end = (start + spec.block_frames).min(frame_count);
        interleaved.clear();
        for index in start..end {
            interleaved.push(left[index]);
            interleaved.push(right[index]);
            sample_peak = sample_peak.max(left[index].abs()).max(right[index].abs());
        }
        writer
            .append(&interleaved)
            .map_err(OfflineExportError::Journal)?;
    }
    writer
        .publish_wav(wav_path)
        .map_err(OfflineExportError::Journal)?;

    Ok(OfflineExportReport {
        frame_count,
        sample_rate_hz,
        sample_peak,
    })
}
