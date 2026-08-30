use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use hyper_audio_engine::StereoOutput;
use hyper_media_cache::DecodedMediaCache;
use hyper_playback_json::LoadedPlaybackProject;
use hyper_recording_metadata::RecordingMetadataDocument;
use std::sync::Arc;

const INITIAL_PREVIEW_GAIN: f32 = 0.25;
const GAIN_RAMP_MILLISECONDS: u32 = 5;
const NO_PREVIEW_INDEX: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreviewCommand {
    Start {
        index: usize,
        source_start_frame: usize,
        source_frame_count: usize,
    },
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum DesktopAudioPreviewState {
    Idle,
    Starting,
    Playing,
    Stopping,
    Complete,
    Failed,
}

impl DesktopAudioPreviewState {
    const fn from_u8(value: u8) -> Self {
        match value {
            value if value == Self::Starting as u8 => Self::Starting,
            value if value == Self::Playing as u8 => Self::Playing,
            value if value == Self::Stopping as u8 => Self::Stopping,
            value if value == Self::Complete as u8 => Self::Complete,
            value if value == Self::Failed as u8 => Self::Failed,
            _ => Self::Idle,
        }
    }

    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Playing | Self::Stopping)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopAudioPreviewControlError {
    NonFiniteGain,
    NegativeGain,
    ExcessiveGain,
}

impl fmt::Display for DesktopAudioPreviewControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonFiniteGain => "audio preview gain must be finite",
            Self::NegativeGain => "audio preview gain must not be negative",
            Self::ExcessiveGain => "audio preview gain must not exceed unity",
        })
    }
}

impl Error for DesktopAudioPreviewControlError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopAudioPreviewCommandError {
    ProjectUnavailable,
    RecordingUnavailable,
    ClipUnavailable,
    SourceRangeUnavailable,
    PreviewBusy,
    PreviewInactive,
    SequenceExhausted,
}

impl fmt::Display for DesktopAudioPreviewCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ProjectUnavailable => "audio preview requires a verified project",
            Self::RecordingUnavailable => "audio preview target is not a verified recording",
            Self::ClipUnavailable => "audio preview target is not a verified audio clip",
            Self::SourceRangeUnavailable => "audio preview source range is unavailable",
            Self::PreviewBusy => "another audio preview is active",
            Self::PreviewInactive => "no audio preview is active",
            Self::SequenceExhausted => "audio preview command sequence is exhausted",
        })
    }
}

impl Error for DesktopAudioPreviewCommandError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopAudioPreviewBuildError {
    CapacityUnavailable,
    MediaUnavailable {
        recording_index: usize,
    },
    UnsupportedChannels {
        recording_index: usize,
        channels: usize,
    },
    SampleRateMismatch {
        recording_index: usize,
        source_hz: u32,
        output_hz: u32,
    },
    ClipMediaUnavailable {
        clip_index: usize,
    },
    ClipSourceViewUnavailable {
        clip_index: usize,
    },
    ClipUnsupportedChannels {
        clip_index: usize,
        channels: usize,
    },
    ClipSampleRateMismatch {
        clip_index: usize,
        source_hz: u32,
        output_hz: u32,
    },
}

impl fmt::Display for DesktopAudioPreviewBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityUnavailable => {
                formatter.write_str("audio preview catalog capacity is unavailable")
            }
            Self::MediaUnavailable { recording_index } => write!(
                formatter,
                "audio preview recording {recording_index} has no decoded media"
            ),
            Self::UnsupportedChannels {
                recording_index,
                channels,
            } => write!(
                formatter,
                "audio preview recording {recording_index} has unsupported channel count {channels}"
            ),
            Self::SampleRateMismatch {
                recording_index,
                source_hz,
                output_hz,
            } => write!(
                formatter,
                "audio preview recording {recording_index} is {source_hz} Hz; output is {output_hz} Hz"
            ),
            Self::ClipMediaUnavailable { clip_index } => write!(
                formatter,
                "audio preview clip {clip_index} has no decoded media"
            ),
            Self::ClipSourceViewUnavailable { clip_index } => write!(
                formatter,
                "audio preview clip {clip_index} has no valid source view"
            ),
            Self::ClipUnsupportedChannels {
                clip_index,
                channels,
            } => write!(
                formatter,
                "audio preview clip {clip_index} has unsupported channel count {channels}"
            ),
            Self::ClipSampleRateMismatch {
                clip_index,
                source_hz,
                output_hz,
            } => write!(
                formatter,
                "audio preview clip {clip_index} is {source_hz} Hz; output is {output_hz} Hz"
            ),
        }
    }
}

impl Error for DesktopAudioPreviewBuildError {}

#[derive(Debug)]
struct PreviewControlState {
    generation: AtomicU64,
    requested_index: AtomicUsize,
    requested_source_start_frame: AtomicUsize,
    requested_source_frame_count: AtomicUsize,
    active_index: AtomicUsize,
    state: AtomicU8,
    played_frames: AtomicU64,
    total_frames: AtomicU64,
    gain_bits: AtomicU32,
    muted: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct DesktopAudioPreviewControl {
    state: Arc<PreviewControlState>,
}

impl DesktopAudioPreviewControl {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(PreviewControlState {
                generation: AtomicU64::new(0),
                requested_index: AtomicUsize::new(NO_PREVIEW_INDEX),
                requested_source_start_frame: AtomicUsize::new(0),
                requested_source_frame_count: AtomicUsize::new(0),
                active_index: AtomicUsize::new(NO_PREVIEW_INDEX),
                state: AtomicU8::new(DesktopAudioPreviewState::Idle as u8),
                played_frames: AtomicU64::new(0),
                total_frames: AtomicU64::new(0),
                gain_bits: AtomicU32::new(INITIAL_PREVIEW_GAIN.to_bits()),
                muted: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) fn request_start(
        &self,
        source_index: usize,
        source_start_frame: usize,
        source_frame_count: usize,
    ) -> Result<(), DesktopAudioPreviewCommandError> {
        if self.snapshot().state().is_active() {
            return Err(DesktopAudioPreviewCommandError::PreviewBusy);
        }
        if source_frame_count == 0 || source_start_frame.checked_add(source_frame_count).is_none() {
            return Err(DesktopAudioPreviewCommandError::SourceRangeUnavailable);
        }
        let total_frames = u64::try_from(source_frame_count)
            .map_err(|_| DesktopAudioPreviewCommandError::SourceRangeUnavailable)?;
        let next = self.next_generation()?;
        self.state
            .requested_index
            .store(source_index, Ordering::Relaxed);
        self.state
            .requested_source_start_frame
            .store(source_start_frame, Ordering::Relaxed);
        self.state
            .requested_source_frame_count
            .store(source_frame_count, Ordering::Relaxed);
        self.state
            .active_index
            .store(source_index, Ordering::Relaxed);
        self.state.played_frames.store(0, Ordering::Relaxed);
        self.state
            .total_frames
            .store(total_frames, Ordering::Relaxed);
        self.state
            .state
            .store(DesktopAudioPreviewState::Starting as u8, Ordering::Relaxed);
        self.state.generation.store(next, Ordering::Release);
        Ok(())
    }

    pub(crate) fn request_stop(&self) -> Result<(), DesktopAudioPreviewCommandError> {
        if !self.snapshot().state().is_active() {
            return Err(DesktopAudioPreviewCommandError::PreviewInactive);
        }
        let next = self.next_generation()?;
        self.state
            .requested_index
            .store(NO_PREVIEW_INDEX, Ordering::Relaxed);
        self.state
            .state
            .store(DesktopAudioPreviewState::Stopping as u8, Ordering::Relaxed);
        self.state.generation.store(next, Ordering::Release);
        Ok(())
    }

    fn next_generation(&self) -> Result<u64, DesktopAudioPreviewCommandError> {
        self.state
            .generation
            .load(Ordering::Relaxed)
            .checked_add(1)
            .ok_or(DesktopAudioPreviewCommandError::SequenceExhausted)
    }

    /// Changes dry audition gain without blocking the native output callback.
    /// The processor ramps each change over five milliseconds.
    ///
    /// # Errors
    ///
    /// Rejects non-finite, negative, and above-unity values without mutation.
    pub fn set_gain_linear(&self, gain: f32) -> Result<(), DesktopAudioPreviewControlError> {
        if !gain.is_finite() {
            return Err(DesktopAudioPreviewControlError::NonFiniteGain);
        }
        if gain < 0.0 {
            return Err(DesktopAudioPreviewControlError::NegativeGain);
        }
        if gain > 1.0 {
            return Err(DesktopAudioPreviewControlError::ExcessiveGain);
        }
        self.state
            .gain_bits
            .store(gain.to_bits(), Ordering::Release);
        Ok(())
    }

    #[must_use]
    pub fn gain_linear(&self) -> f32 {
        f32::from_bits(self.state.gain_bits.load(Ordering::Acquire))
    }

    pub fn set_muted(&self, muted: bool) {
        self.state.muted.store(muted, Ordering::Release);
    }

    #[must_use]
    pub fn muted(&self) -> bool {
        self.state.muted.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn snapshot(&self) -> DesktopAudioPreviewSnapshot {
        let state = DesktopAudioPreviewState::from_u8(self.state.state.load(Ordering::Acquire));
        DesktopAudioPreviewSnapshot {
            state,
            recording_index: match self.state.active_index.load(Ordering::Relaxed) {
                NO_PREVIEW_INDEX => None,
                index => Some(index),
            },
            played_frames: self.state.played_frames.load(Ordering::Relaxed),
            total_frames: self.state.total_frames.load(Ordering::Relaxed),
            gain: self.gain_linear(),
            muted: self.muted(),
        }
    }

    fn take_command(&self, observed_generation: &mut u64) -> Option<PreviewCommand> {
        let generation = self.state.generation.load(Ordering::Acquire);
        if generation == *observed_generation {
            return None;
        }
        *observed_generation = generation;
        Some(match self.state.requested_index.load(Ordering::Relaxed) {
            NO_PREVIEW_INDEX => PreviewCommand::Stop,
            index => PreviewCommand::Start {
                index,
                source_start_frame: self
                    .state
                    .requested_source_start_frame
                    .load(Ordering::Relaxed),
                source_frame_count: self
                    .state
                    .requested_source_frame_count
                    .load(Ordering::Relaxed),
            },
        })
    }

    fn publish_playing(&self) {
        self.state
            .state
            .store(DesktopAudioPreviewState::Playing as u8, Ordering::Release);
    }

    fn publish_progress(&self, played_frames: usize) {
        self.state.played_frames.store(
            u64::try_from(played_frames).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn publish_terminal(&self, state: DesktopAudioPreviewState) {
        if state == DesktopAudioPreviewState::Idle {
            self.state
                .active_index
                .store(NO_PREVIEW_INDEX, Ordering::Relaxed);
            self.state.played_frames.store(0, Ordering::Relaxed);
            self.state.total_frames.store(0, Ordering::Relaxed);
        }
        self.state.state.store(state as u8, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DesktopAudioPreviewSnapshot {
    state: DesktopAudioPreviewState,
    recording_index: Option<usize>,
    played_frames: u64,
    total_frames: u64,
    gain: f32,
    muted: bool,
}

impl DesktopAudioPreviewSnapshot {
    /// Returns the idle preview state before any take is selected.
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            state: DesktopAudioPreviewState::Idle,
            recording_index: None,
            played_frames: 0,
            total_frames: 0,
            gain: INITIAL_PREVIEW_GAIN,
            muted: false,
        }
    }

    #[must_use]
    pub const fn state(self) -> DesktopAudioPreviewState {
        self.state
    }

    #[must_use]
    pub const fn target_index(self) -> Option<usize> {
        self.recording_index
    }

    #[must_use]
    pub const fn played_frames(self) -> u64 {
        self.played_frames
    }

    #[must_use]
    pub const fn total_frames(self) -> u64 {
        self.total_frames
    }

    #[must_use]
    pub const fn gain(self) -> f32 {
        self.gain
    }

    #[must_use]
    pub const fn muted(self) -> bool {
        self.muted
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StereoPreviewSource<'a> {
    Mono(&'a [f32]),
    Stereo { left: &'a [f32], right: &'a [f32] },
}

impl StereoPreviewSource<'_> {
    fn frame_count(self) -> Option<usize> {
        match self {
            Self::Mono(samples) => Some(samples.len()),
            Self::Stereo { left, right } => (left.len() == right.len()).then_some(left.len()),
        }
    }

    fn frame(self, index: usize) -> (f32, f32) {
        match self {
            Self::Mono(samples) => (samples[index], samples[index]),
            Self::Stereo { left, right } => (left[index], right[index]),
        }
    }

    fn range(self, start_frame: usize, frame_count: usize) -> Option<Self> {
        let end_frame = start_frame.checked_add(frame_count)?;
        match self {
            Self::Mono(samples) => samples.get(start_frame..end_frame).map(Self::Mono),
            Self::Stereo { left, right } => Some(Self::Stereo {
                left: left.get(start_frame..end_frame)?,
                right: right.get(start_frame..end_frame)?,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StereoPreviewProcessorError {
    InvalidSampleRate,
    EmptySource,
    SourceShapeChanged,
    InvalidOutputShape,
    NonFiniteGain,
    GainOutOfRange,
}

impl fmt::Display for StereoPreviewProcessorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidSampleRate => "audio preview sample rate must be positive",
            Self::EmptySource => "audio preview source must contain at least one frame",
            Self::SourceShapeChanged => "audio preview source shape changed after preparation",
            Self::InvalidOutputShape => "audio preview output must contain matching stereo planes",
            Self::NonFiniteGain => "audio preview gain must be finite",
            Self::GainOutOfRange => "audio preview gain must remain between silence and unity",
        })
    }
}

impl Error for StereoPreviewProcessorError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StereoPreviewRender {
    rendered_frames: usize,
    complete: bool,
    silent: bool,
}

impl StereoPreviewRender {
    #[must_use]
    pub const fn rendered_frames(self) -> usize {
        self.rendered_frames
    }

    #[must_use]
    pub const fn complete(self) -> bool {
        self.complete
    }

    #[must_use]
    pub const fn silent(self) -> bool {
        self.silent
    }
}

#[derive(Debug)]
pub struct StereoPreviewProcessor {
    total_frames: usize,
    position_frames: usize,
    applied_gain: f32,
    ramp_target_gain: f32,
    ramp_remaining_frames: u16,
    gain_ramp_frames: u16,
}

impl StereoPreviewProcessor {
    /// Creates a source-bound dry audition processor.
    ///
    /// # Errors
    ///
    /// Rejects a zero sample rate, empty source, or unrepresentable ramp.
    pub fn try_new(
        sample_rate_hz: u32,
        total_frames: usize,
    ) -> Result<Self, StereoPreviewProcessorError> {
        if sample_rate_hz == 0 {
            return Err(StereoPreviewProcessorError::InvalidSampleRate);
        }
        if total_frames == 0 {
            return Err(StereoPreviewProcessorError::EmptySource);
        }
        let ramp_frames = sample_rate_hz
            .checked_mul(GAIN_RAMP_MILLISECONDS)
            .and_then(|frames| frames.checked_div(1_000))
            .and_then(|frames| u16::try_from(frames).ok())
            .filter(|frames| *frames > 0)
            .ok_or(StereoPreviewProcessorError::InvalidSampleRate)?;
        Ok(Self {
            total_frames,
            position_frames: 0,
            applied_gain: INITIAL_PREVIEW_GAIN,
            ramp_target_gain: INITIAL_PREVIEW_GAIN,
            ramp_remaining_frames: 0,
            gain_ramp_frames: ramp_frames,
        })
    }

    #[must_use]
    pub const fn position_frames(&self) -> usize {
        self.position_frames
    }

    /// Renders the next source frames as planar stereo and silences any tail.
    /// Source boundaries and control changes receive five-millisecond ramps;
    /// validation is failure-atomic.
    ///
    /// # Errors
    ///
    /// Rejects changed source shape, mismatched output planes, or invalid gain
    /// before changing processor state or output samples.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "boundary frames are capped by the validated u16 ramp before conversion"
    )]
    pub fn render(
        &mut self,
        source: StereoPreviewSource<'_>,
        output: &mut StereoOutput<'_>,
        gain: f32,
        muted: bool,
    ) -> Result<StereoPreviewRender, StereoPreviewProcessorError> {
        if source.frame_count() != Some(self.total_frames) {
            return Err(StereoPreviewProcessorError::SourceShapeChanged);
        }
        if output.left.len() != output.right.len() {
            return Err(StereoPreviewProcessorError::InvalidOutputShape);
        }
        if !gain.is_finite() {
            return Err(StereoPreviewProcessorError::NonFiniteGain);
        }
        if !(0.0..=1.0).contains(&gain) {
            return Err(StereoPreviewProcessorError::GainOutOfRange);
        }
        let target_gain = if muted { 0.0 } else { gain };
        if target_gain.to_bits() != self.ramp_target_gain.to_bits() {
            self.ramp_target_gain = target_gain;
            self.ramp_remaining_frames = self.gain_ramp_frames;
        }
        let requested_frames = output.left.len();
        let rendered_frames = requested_frames.min(self.total_frames - self.position_frames);
        let ramp_frames = usize::from(self.gain_ramp_frames);
        for frame_offset in 0..rendered_frames {
            if self.ramp_remaining_frames > 0 {
                self.applied_gain += (self.ramp_target_gain - self.applied_gain)
                    / f32::from(self.ramp_remaining_frames);
                self.ramp_remaining_frames -= 1;
            }
            let source_frame = self.position_frames + frame_offset;
            let boundary_frames = (source_frame + 1)
                .min(self.total_frames - source_frame)
                .min(ramp_frames);
            let boundary_frames = boundary_frames as u16;
            let boundary_gain = f32::from(boundary_frames) / f32::from(self.gain_ramp_frames);
            let (left, right) = source.frame(source_frame);
            output.left[frame_offset] = left * self.applied_gain * boundary_gain;
            output.right[frame_offset] = right * self.applied_gain * boundary_gain;
        }
        output.left[rendered_frames..].fill(0.0);
        output.right[rendered_frames..].fill(0.0);
        self.position_frames += rendered_frames;
        Ok(StereoPreviewRender {
            rendered_frames,
            complete: self.position_frames == self.total_frames,
            silent: self.applied_gain.to_bits() == 0.0_f32.to_bits(),
        })
    }
}

pub(crate) struct DesktopAudioPreviewProcessor<'a> {
    sample_rate_hz: u32,
    sources: Vec<StereoPreviewSource<'a>>,
    control: DesktopAudioPreviewControl,
    observed_generation: u64,
    current: Option<(StereoPreviewSource<'a>, StereoPreviewProcessor)>,
    stopping: bool,
}

impl fmt::Debug for DesktopAudioPreviewProcessor<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DesktopAudioPreviewProcessor")
            .field("sample_rate_hz", &self.sample_rate_hz)
            .field("source_count", &self.sources.len())
            .field("observed_generation", &self.observed_generation)
            .field("current", &self.current)
            .field("stopping", &self.stopping)
            .finish()
    }
}

impl<'a> DesktopAudioPreviewProcessor<'a> {
    pub(crate) fn try_new(
        media: &'a DecodedMediaCache,
        recordings: &[RecordingMetadataDocument],
        project: &LoadedPlaybackProject,
        sample_rate_hz: u32,
        control: DesktopAudioPreviewControl,
    ) -> Result<Self, DesktopAudioPreviewBuildError> {
        let mut sources = Vec::new();
        let source_count = recordings
            .len()
            .checked_add(project.clip_count())
            .ok_or(DesktopAudioPreviewBuildError::CapacityUnavailable)?;
        sources
            .try_reserve_exact(source_count)
            .map_err(|_| DesktopAudioPreviewBuildError::CapacityUnavailable)?;
        for (recording_index, recording) in recordings.iter().enumerate() {
            let audio = media
                .decoded_audio(recording.asset_id())
                .ok_or(DesktopAudioPreviewBuildError::MediaUnavailable { recording_index })?;
            if audio.sample_rate_hz() != sample_rate_hz {
                return Err(DesktopAudioPreviewBuildError::SampleRateMismatch {
                    recording_index,
                    source_hz: audio.sample_rate_hz(),
                    output_hz: sample_rate_hz,
                });
            }
            let source = match (audio.channels(), audio.plane(0), audio.plane(1)) {
                (1, Some(mono), None) => StereoPreviewSource::Mono(mono),
                (2, Some(left), Some(right)) => StereoPreviewSource::Stereo { left, right },
                (channels, _, _) => {
                    return Err(DesktopAudioPreviewBuildError::UnsupportedChannels {
                        recording_index,
                        channels,
                    });
                }
            };
            sources.push(source);
        }
        for clip_index in 0..project.clip_count() {
            let asset_id = project
                .clip_asset_id_at(clip_index)
                .ok_or(DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index })?;
            let source_start = project
                .clip_source_start_frame_at(clip_index)
                .ok_or(DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index })?;
            let source_frame_count = project
                .clip_source_frame_count_at(clip_index)
                .ok_or(DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index })?;
            let source_end = source_start
                .checked_add(source_frame_count)
                .ok_or(DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index })?;
            let audio = media
                .decoded_audio(asset_id)
                .ok_or(DesktopAudioPreviewBuildError::ClipMediaUnavailable { clip_index })?;
            if audio.sample_rate_hz() != sample_rate_hz {
                return Err(DesktopAudioPreviewBuildError::ClipSampleRateMismatch {
                    clip_index,
                    source_hz: audio.sample_rate_hz(),
                    output_hz: sample_rate_hz,
                });
            }
            let source = match (audio.channels(), audio.plane(0), audio.plane(1)) {
                (1, Some(mono), None) => {
                    StereoPreviewSource::Mono(mono.get(source_start..source_end).ok_or(
                        DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index },
                    )?)
                }
                (2, Some(left), Some(right)) => StereoPreviewSource::Stereo {
                    left: left.get(source_start..source_end).ok_or(
                        DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index },
                    )?,
                    right: right.get(source_start..source_end).ok_or(
                        DesktopAudioPreviewBuildError::ClipSourceViewUnavailable { clip_index },
                    )?,
                },
                (channels, _, _) => {
                    return Err(DesktopAudioPreviewBuildError::ClipUnsupportedChannels {
                        clip_index,
                        channels,
                    });
                }
            };
            if source.frame_count() != Some(source_frame_count) || source_frame_count == 0 {
                return Err(DesktopAudioPreviewBuildError::ClipSourceViewUnavailable {
                    clip_index,
                });
            }
            sources.push(source);
        }
        Ok(Self {
            sample_rate_hz,
            sources,
            control,
            observed_generation: 0,
            current: None,
            stopping: false,
        })
    }

    /// Overwrites the final project mix while a dry audition is active.
    /// Returns whether monitoring must remain suppressed for this block.
    pub(crate) fn process(&mut self, output: &mut StereoOutput<'_>) -> bool {
        if let Some(command) = self.control.take_command(&mut self.observed_generation) {
            match command {
                PreviewCommand::Start {
                    index,
                    source_start_frame,
                    source_frame_count,
                } => self.start(index, source_start_frame, source_frame_count),
                PreviewCommand::Stop if self.current.is_some() => self.stopping = true,
                PreviewCommand::Stop => self
                    .control
                    .publish_terminal(DesktopAudioPreviewState::Idle),
            }
        }
        let Some((source, processor)) = self.current.as_mut() else {
            if self.stopping {
                self.stopping = false;
                self.control
                    .publish_terminal(DesktopAudioPreviewState::Idle);
            }
            return false;
        };
        if let Ok(rendered) = processor.render(
            *source,
            output,
            self.control.gain_linear(),
            self.stopping || self.control.muted(),
        ) {
            self.control.publish_progress(processor.position_frames());
            if self.stopping && (rendered.silent() || rendered.complete()) {
                self.current = None;
                self.stopping = false;
                self.control
                    .publish_terminal(DesktopAudioPreviewState::Idle);
            } else if rendered.complete() {
                self.current = None;
                self.control
                    .publish_terminal(DesktopAudioPreviewState::Complete);
            } else if !self.stopping {
                self.control.publish_playing();
            }
            true
        } else {
            output.left.fill(0.0);
            output.right.fill(0.0);
            self.current = None;
            self.stopping = false;
            self.control
                .publish_terminal(DesktopAudioPreviewState::Failed);
            true
        }
    }

    fn start(&mut self, index: usize, source_start_frame: usize, source_frame_count: usize) {
        let Some(source) = self.sources.get(index).copied() else {
            self.fail();
            return;
        };
        let Some(source) = source.range(source_start_frame, source_frame_count) else {
            self.fail();
            return;
        };
        if source.frame_count() != Some(source_frame_count) {
            self.fail();
            return;
        }
        match StereoPreviewProcessor::try_new(self.sample_rate_hz, source_frame_count) {
            Ok(processor) => {
                self.current = Some((source, processor));
                self.stopping = false;
            }
            Err(_) => self.fail(),
        }
    }

    fn fail(&mut self) {
        self.current = None;
        self.stopping = false;
        self.control
            .publish_terminal(DesktopAudioPreviewState::Failed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_preview_is_dual_mono_boundary_ramped_and_tail_silenced() {
        let source = [1.0_f32, 0.5, -0.5];
        let mut processor = StereoPreviewProcessor::try_new(48_000, source.len()).unwrap();
        let mut left = [9.0_f32; 4];
        let mut right = [9.0_f32; 4];
        let rendered = processor
            .render(
                StereoPreviewSource::Mono(&source),
                &mut StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
                0.25,
                false,
            )
            .unwrap();
        assert_eq!(rendered.rendered_frames(), 3);
        assert!(rendered.complete());
        assert!(left[0] > 0.0 && left[0] < 0.25);
        assert!(
            left.iter()
                .zip(right)
                .all(|(left, right)| left.to_bits() == right.to_bits())
        );
        assert_eq!(left[3].to_bits(), 0.0_f32.to_bits());
    }

    #[test]
    fn preview_validation_is_failure_atomic() {
        let source = [0.5_f32; 4];
        let mut processor = StereoPreviewProcessor::try_new(48_000, source.len()).unwrap();
        let mut left = [7.0_f32; 4];
        let mut right = [7.0_f32; 3];
        assert_eq!(
            processor.render(
                StereoPreviewSource::Mono(&source),
                &mut StereoOutput {
                    left: &mut left,
                    right: &mut right,
                },
                0.25,
                false,
            ),
            Err(StereoPreviewProcessorError::InvalidOutputShape)
        );
        assert_eq!(processor.position_frames(), 0);
        assert!(
            left.iter()
                .all(|sample| sample.to_bits() == 7.0_f32.to_bits())
        );
        assert!(
            right
                .iter()
                .all(|sample| sample.to_bits() == 7.0_f32.to_bits())
        );
    }

    #[test]
    fn preview_control_rejects_unsafe_gain_without_mutation() {
        let control = DesktopAudioPreviewControl::new();
        assert_eq!(control.gain_linear().to_bits(), 0.25_f32.to_bits());
        assert_eq!(
            control.set_gain_linear(f32::NAN),
            Err(DesktopAudioPreviewControlError::NonFiniteGain)
        );
        assert_eq!(
            control.set_gain_linear(1.01),
            Err(DesktopAudioPreviewControlError::ExcessiveGain)
        );
        assert_eq!(control.gain_linear().to_bits(), 0.25_f32.to_bits());
    }

    #[test]
    fn selected_preview_renders_only_the_requested_prepared_source_range() {
        let control = DesktopAudioPreviewControl::new();
        control.request_start(0, 2, 3).unwrap();
        let source = [0.0_f32, 0.0, 0.8, 0.4, -0.6, 0.2];
        let mut processor = DesktopAudioPreviewProcessor {
            sample_rate_hz: 48_000,
            sources: vec![StereoPreviewSource::Mono(&source)],
            control: control.clone(),
            observed_generation: 0,
            current: None,
            stopping: false,
        };
        let mut left = [9.0_f32; 4];
        let mut right = [9.0_f32; 4];

        assert!(processor.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        assert_eq!(control.snapshot().total_frames(), 3);
        assert_eq!(control.snapshot().played_frames(), 3);
        assert_eq!(
            control.snapshot().state(),
            DesktopAudioPreviewState::Complete
        );
        assert!(left[0] > 0.0 && left[1] > 0.0 && left[2] < 0.0);
        assert_eq!(left[3].to_bits(), 0.0_f32.to_bits());
        assert!(
            left.iter()
                .zip(right.iter())
                .all(|(left, right)| left.to_bits() == right.to_bits())
        );
    }

    #[test]
    fn preview_rejects_empty_or_unrepresentable_requested_ranges_without_starting() {
        let control = DesktopAudioPreviewControl::new();
        assert_eq!(
            control.request_start(0, 0, 0),
            Err(DesktopAudioPreviewCommandError::SourceRangeUnavailable)
        );
        assert_eq!(
            control.request_start(0, usize::MAX, 1),
            Err(DesktopAudioPreviewCommandError::SourceRangeUnavailable)
        );
        assert_eq!(control.snapshot().state(), DesktopAudioPreviewState::Idle);
        assert_eq!(control.snapshot().target_index(), None);
    }

    #[test]
    fn stop_command_is_ramped_before_preview_becomes_idle() {
        let control = DesktopAudioPreviewControl::new();
        control.request_start(0, 0, 1_000).unwrap();
        let source = [1.0_f32; 1_000];
        let mut processor = DesktopAudioPreviewProcessor {
            sample_rate_hz: 48_000,
            sources: vec![StereoPreviewSource::Mono(&source)],
            control: control.clone(),
            observed_generation: 0,
            current: None,
            stopping: false,
        };
        let mut left = [0.0_f32; 128];
        let mut right = [0.0_f32; 128];
        assert!(processor.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        control.request_stop().unwrap();
        assert!(processor.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        assert_eq!(
            control.snapshot().state(),
            DesktopAudioPreviewState::Stopping
        );
        assert!(processor.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        assert_eq!(control.snapshot().state(), DesktopAudioPreviewState::Idle);
    }

    #[test]
    fn stop_before_first_callback_does_not_leave_a_stopping_ghost() {
        let control = DesktopAudioPreviewControl::new();
        control.request_start(0, 0, 1_000).unwrap();
        control.request_stop().unwrap();
        let source = [1.0_f32; 1_000];
        let mut processor = DesktopAudioPreviewProcessor {
            sample_rate_hz: 48_000,
            sources: vec![StereoPreviewSource::Mono(&source)],
            control: control.clone(),
            observed_generation: 0,
            current: None,
            stopping: false,
        };
        let mut left = [0.0_f32; 128];
        let mut right = [0.0_f32; 128];
        assert!(!processor.process(&mut StereoOutput {
            left: &mut left,
            right: &mut right,
        }));
        assert_eq!(control.snapshot().state(), DesktopAudioPreviewState::Idle);
    }
}
