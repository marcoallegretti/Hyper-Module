#![doc = "Fixed-track arrangement-to-main-mixer execution for Hyper Module."]
#![forbid(unsafe_code)]

use core::error::Error;
use core::fmt;

use hyper_arrangement_renderer::{
    ArrangementRenderer, RenderScratchSlot, RenderWindowError, RenderedWindow,
    RendererCreationError,
};
use hyper_audio_engine::{
    ChannelParameters, InputBlock, LinearGain, MAX_PROCESS_FRAMES, MixerCreationError, Pan,
    ProcessError, StereoMixer, StereoOutput,
};
use hyper_media_cache::PreparedSchedule;
use hyper_playback_json::{
    CompiledMainMixer, CompiledMixerDestination, CompiledMixerGraph, CompiledMixerTap,
    CompiledMixerTrackKind,
};

pub const MAX_MAIN_MIXER_SLOTS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCreationError {
    SlotLimit { actual: usize, maximum: usize },
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    CapacityUnavailable,
    Renderer(RendererCreationError),
    Mixer(MixerCreationError),
}

impl fmt::Display for RuntimeCreationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SlotLimit { actual, maximum } => write!(
                formatter,
                "compiled main mixer has {actual} slots; maximum is {maximum}"
            ),
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "maximum callback block has {actual} frames; valid range is 1..={maximum}"
            ),
            Self::CapacityUnavailable => {
                formatter.write_str("mixer runtime buffer capacity is unavailable")
            }
            Self::Renderer(source) => write!(formatter, "mixer track indexing failed: {source}"),
            Self::Mixer(source) => write!(formatter, "mixer construction failed: {source}"),
        }
    }
}

impl Error for RuntimeCreationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Renderer(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::SlotLimit { .. }
            | Self::InvalidMaximumBlockFrames { .. }
            | Self::CapacityUnavailable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeProcessError {
    OutputLengthMismatch { left: usize, right: usize },
    BlockLimit { actual: usize, maximum: usize },
    Render(RenderWindowError),
    Mixer(ProcessError),
}

impl fmt::Display for RuntimeProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutputLengthMismatch { left, right } => {
                write!(
                    formatter,
                    "runtime output lengths differ: left={left}, right={right}"
                )
            }
            Self::BlockLimit { actual, maximum } => write!(
                formatter,
                "runtime block has {actual} frames; configured maximum is {maximum}"
            ),
            Self::Render(source) => write!(formatter, "track rendering failed: {source}"),
            Self::Mixer(source) => write!(formatter, "main mixing failed: {source}"),
        }
    }
}

impl Error for RuntimeProcessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Render(source) => Some(source),
            Self::Mixer(source) => Some(source),
            Self::OutputLengthMismatch { .. } | Self::BlockLimit { .. } => None,
        }
    }
}

/// A prepared arrangement renderer and mixer whose slot identities cannot
/// diverge after construction.
///
/// Construction performs all indexing and heap allocation. [`Self::process_window`]
/// uses caller-owned scratch and performs no allocation, I/O, locking, logging,
/// or dynamic dispatch.
#[derive(Debug)]
pub struct FlatMainRuntime<'prepared, 'cache, 'schedule> {
    renderer: ArrangementRenderer<'prepared, 'cache, 'schedule>,
    mixer: StereoMixer,
    parameters: [ChannelParameters; MAX_MAIN_MIXER_SLOTS],
    slot_count: usize,
    master_gain: LinearGain,
}

impl<'prepared, 'cache, 'schedule> FlatMainRuntime<'prepared, 'cache, 'schedule> {
    /// Binds schedule tracks to compiled mixer slots in one immutable order.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] if the compiled mixer exceeds the
    /// version-1 slot bound, track indexing fails, or mixer state cannot be
    /// allocated.
    pub fn try_new(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMainMixer,
    ) -> Result<Self, RuntimeCreationError> {
        let slot_count = compiled.slots().len();
        if slot_count > MAX_MAIN_MIXER_SLOTS {
            return Err(RuntimeCreationError::SlotLimit {
                actual: slot_count,
                maximum: MAX_MAIN_MIXER_SLOTS,
            });
        }

        let mut track_ids = [""; MAX_MAIN_MIXER_SLOTS];
        let mut parameters =
            [ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, false); MAX_MAIN_MIXER_SLOTS];
        for (index, slot) in compiled.slots().iter().enumerate() {
            track_ids[index] = slot.track_id();
            parameters[index] = slot.parameters();
        }
        let renderer = ArrangementRenderer::try_new_for_tracks(schedule, &track_ids[..slot_count])
            .map_err(RuntimeCreationError::Renderer)?;
        let mixer = StereoMixer::try_new(&parameters[..slot_count], compiled.master_gain())
            .map_err(RuntimeCreationError::Mixer)?;

        Ok(Self {
            renderer,
            mixer,
            parameters,
            slot_count,
            master_gain: compiled.master_gain(),
        })
    }

    #[must_use]
    pub const fn slot_count(&self) -> usize {
        self.slot_count
    }

    /// Renders and mixes one half-open arrangement frame window.
    ///
    /// The output length defines the callback frame count. Exactly one scratch
    /// slot per configured mixer input is required.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] before output is touched when output
    /// planes differ, or when track rendering or mixer processing rejects the
    /// supplied block.
    pub fn process_window(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(RuntimeProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        let rendered = self
            .renderer
            .render_track_window(arrangement_start_frame, frame_count, scratch)
            .map_err(RuntimeProcessError::Render)?;

        let empty: &[f32] = &[];
        let mut inputs = [InputBlock::Mono(empty); MAX_MAIN_MIXER_SLOTS];
        for (input, slot) in inputs[..self.slot_count].iter_mut().zip(scratch.iter()) {
            *input = slot.input_block();
        }
        self.mixer
            .process(
                &inputs[..self.slot_count],
                &self.parameters[..self.slot_count],
                self.master_gain,
                output,
            )
            .map_err(RuntimeProcessError::Mixer)?;
        Ok(rendered)
    }
}

#[derive(Debug)]
struct StereoBuffer {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl StereoBuffer {
    fn try_new(frame_count: usize) -> Result<Self, RuntimeCreationError> {
        let mut left = Vec::new();
        left.try_reserve_exact(frame_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        left.resize(frame_count, 0.0);
        let mut right = Vec::new();
        right
            .try_reserve_exact(frame_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        right.resize(frame_count, 0.0);
        Ok(Self { left, right })
    }

    fn clear_prefix(&mut self, frame_count: usize) {
        self.left[..frame_count].fill(0.0);
        self.right[..frame_count].fill(0.0);
    }
}

#[derive(Debug)]
struct GraphSendRuntime {
    target_slot: usize,
    tap: CompiledMixerTap,
    parameters: ChannelParameters,
    processor: StereoMixer,
}

#[derive(Debug)]
struct GraphSlotRuntime {
    kind: CompiledMixerTrackKind,
    destination: CompiledMixerDestination,
    parameters: ChannelParameters,
    processor: StereoMixer,
    sends: Vec<GraphSendRuntime>,
}

/// A bounded primary/send/return graph whose route order and buffers are
/// completely compiled before callback execution.
#[derive(Debug)]
pub struct GraphMainRuntime<'prepared, 'cache, 'schedule> {
    renderer: ArrangementRenderer<'prepared, 'cache, 'schedule>,
    slots: Vec<GraphSlotRuntime>,
    processing_order: Vec<usize>,
    route_buffers: Vec<StereoBuffer>,
    channel_output: StereoBuffer,
    send_output: StereoBuffer,
    main_input: StereoBuffer,
    main_processor: StereoMixer,
    main_parameters: [ChannelParameters; 1],
    master_gain: LinearGain,
    max_block_frames: usize,
}

impl<'prepared, 'cache, 'schedule> GraphMainRuntime<'prepared, 'cache, 'schedule> {
    /// Binds a prepared schedule to a fully resolved mixer graph.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeCreationError`] for a slot or block bound violation,
    /// unavailable buffer capacity, track-index construction failure, or mixer
    /// state construction failure.
    pub fn try_new(
        schedule: &'prepared PreparedSchedule<'cache, 'schedule>,
        compiled: &CompiledMixerGraph,
        max_block_frames: usize,
    ) -> Result<Self, RuntimeCreationError> {
        let slot_count = compiled.slots().len();
        if slot_count > MAX_MAIN_MIXER_SLOTS {
            return Err(RuntimeCreationError::SlotLimit {
                actual: slot_count,
                maximum: MAX_MAIN_MIXER_SLOTS,
            });
        }
        if max_block_frames == 0 || max_block_frames > MAX_PROCESS_FRAMES {
            return Err(RuntimeCreationError::InvalidMaximumBlockFrames {
                actual: max_block_frames,
                maximum: MAX_PROCESS_FRAMES,
            });
        }

        let mut track_ids = [""; MAX_MAIN_MIXER_SLOTS];
        for (index, slot) in compiled.slots().iter().enumerate() {
            track_ids[index] = slot.track_id();
        }
        let renderer = ArrangementRenderer::try_new_for_tracks(schedule, &track_ids[..slot_count])
            .map_err(RuntimeCreationError::Renderer)?;

        let mut slots = Vec::new();
        slots
            .try_reserve_exact(slot_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        for compiled_slot in compiled.slots() {
            let parameters = compiled_slot.parameters();
            let processor = StereoMixer::try_new(&[parameters], LinearGain::UNITY)
                .map_err(RuntimeCreationError::Mixer)?;
            let mut sends = Vec::new();
            sends
                .try_reserve_exact(compiled_slot.sends().len())
                .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
            for compiled_send in compiled_slot.sends() {
                let parameters = compiled_send.parameters();
                sends.push(GraphSendRuntime {
                    target_slot: compiled_send.target_slot(),
                    tap: compiled_send.tap(),
                    parameters,
                    processor: StereoMixer::try_new(&[parameters], LinearGain::UNITY)
                        .map_err(RuntimeCreationError::Mixer)?,
                });
            }
            slots.push(GraphSlotRuntime {
                kind: compiled_slot.kind(),
                destination: compiled_slot.destination(),
                parameters,
                processor,
                sends,
            });
        }

        let mut processing_order = Vec::new();
        processing_order
            .try_reserve_exact(compiled.processing_order().len())
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        processing_order.extend_from_slice(compiled.processing_order());

        let mut route_buffers = Vec::new();
        route_buffers
            .try_reserve_exact(slot_count)
            .map_err(|_| RuntimeCreationError::CapacityUnavailable)?;
        for _ in 0..slot_count {
            route_buffers.push(StereoBuffer::try_new(max_block_frames)?);
        }
        let main_parameters = [ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true)];
        let main_processor = StereoMixer::try_new(&main_parameters, compiled.master_gain())
            .map_err(RuntimeCreationError::Mixer)?;

        Ok(Self {
            renderer,
            slots,
            processing_order,
            route_buffers,
            channel_output: StereoBuffer::try_new(max_block_frames)?,
            send_output: StereoBuffer::try_new(max_block_frames)?,
            main_input: StereoBuffer::try_new(max_block_frames)?,
            main_processor,
            main_parameters,
            master_gain: compiled.master_gain(),
            max_block_frames,
        })
    }

    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub const fn max_block_frames(&self) -> usize {
        self.max_block_frames
    }

    /// Renders and executes one bounded graph window into the main output.
    ///
    /// Primary routes, pre/post-fader sends, returns, groups, channel controls,
    /// solo eligibility, and master gain use precompiled state. Callback work
    /// performs no allocation, I/O, locking, logging, or dynamic dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeProcessError`] for output or configured block bounds,
    /// arrangement rendering failure, or an unexpected mixer invariant
    /// failure.
    pub fn process_window(
        &mut self,
        arrangement_start_frame: usize,
        scratch: &mut [RenderScratchSlot<'_>],
        output: StereoOutput<'_>,
    ) -> Result<RenderedWindow, RuntimeProcessError> {
        let frame_count = output.left.len();
        if frame_count != output.right.len() {
            return Err(RuntimeProcessError::OutputLengthMismatch {
                left: frame_count,
                right: output.right.len(),
            });
        }
        if frame_count > self.max_block_frames {
            return Err(RuntimeProcessError::BlockLimit {
                actual: frame_count,
                maximum: self.max_block_frames,
            });
        }
        let rendered = self
            .renderer
            .render_track_window(arrangement_start_frame, frame_count, scratch)
            .map_err(RuntimeProcessError::Render)?;
        if frame_count == 0 {
            return Ok(rendered);
        }

        for buffer in &mut self.route_buffers {
            buffer.clear_prefix(frame_count);
        }
        self.main_input.clear_prefix(frame_count);

        for order_index in 0..self.processing_order.len() {
            let slot_index = self.processing_order[order_index];
            self.process_sends(slot_index, CompiledMixerTap::PreFader, frame_count, scratch)?;
            self.process_channel(slot_index, frame_count, scratch)?;
            self.route_channel_output(slot_index, frame_count);
            self.process_sends(
                slot_index,
                CompiledMixerTap::PostFader,
                frame_count,
                scratch,
            )?;
        }

        let main_input = [InputBlock::Stereo {
            left: &self.main_input.left[..frame_count],
            right: &self.main_input.right[..frame_count],
        }];
        self.main_processor
            .process(&main_input, &self.main_parameters, self.master_gain, output)
            .map_err(RuntimeProcessError::Mixer)?;
        Ok(rendered)
    }

    fn process_sends(
        &mut self,
        slot_index: usize,
        tap: CompiledMixerTap,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) -> Result<(), RuntimeProcessError> {
        let send_count = self.slots[slot_index].sends.len();
        for send_index in 0..send_count {
            if self.slots[slot_index].sends[send_index].tap != tap {
                continue;
            }
            let input = if tap == CompiledMixerTap::PostFader {
                InputBlock::Stereo {
                    left: &self.channel_output.left[..frame_count],
                    right: &self.channel_output.right[..frame_count],
                }
            } else if self.slots[slot_index].kind == CompiledMixerTrackKind::Audio {
                scratch[slot_index].input_block()
            } else {
                InputBlock::Stereo {
                    left: &self.route_buffers[slot_index].left[..frame_count],
                    right: &self.route_buffers[slot_index].right[..frame_count],
                }
            };
            let inputs = [input];
            let parameters = [self.slots[slot_index].sends[send_index].parameters];
            self.slots[slot_index].sends[send_index]
                .processor
                .process(
                    &inputs,
                    &parameters,
                    LinearGain::UNITY,
                    StereoOutput {
                        left: &mut self.send_output.left[..frame_count],
                        right: &mut self.send_output.right[..frame_count],
                    },
                )
                .map_err(RuntimeProcessError::Mixer)?;
            let target = self.slots[slot_index].sends[send_index].target_slot;
            accumulate(
                &mut self.route_buffers[target],
                &self.send_output,
                frame_count,
            );
        }
        Ok(())
    }

    fn process_channel(
        &mut self,
        slot_index: usize,
        frame_count: usize,
        scratch: &[RenderScratchSlot<'_>],
    ) -> Result<(), RuntimeProcessError> {
        let input = if self.slots[slot_index].kind == CompiledMixerTrackKind::Audio {
            scratch[slot_index].input_block()
        } else {
            InputBlock::Stereo {
                left: &self.route_buffers[slot_index].left[..frame_count],
                right: &self.route_buffers[slot_index].right[..frame_count],
            }
        };
        let inputs = [input];
        let parameters = [self.slots[slot_index].parameters];
        self.slots[slot_index]
            .processor
            .process(
                &inputs,
                &parameters,
                LinearGain::UNITY,
                StereoOutput {
                    left: &mut self.channel_output.left[..frame_count],
                    right: &mut self.channel_output.right[..frame_count],
                },
            )
            .map_err(RuntimeProcessError::Mixer)
    }

    fn route_channel_output(&mut self, slot_index: usize, frame_count: usize) {
        match self.slots[slot_index].destination {
            CompiledMixerDestination::Main => {
                accumulate(&mut self.main_input, &self.channel_output, frame_count);
            }
            CompiledMixerDestination::Slot(target) => {
                accumulate(
                    &mut self.route_buffers[target],
                    &self.channel_output,
                    frame_count,
                );
            }
        }
    }
}

fn accumulate(destination: &mut StereoBuffer, source: &StereoBuffer, frame_count: usize) {
    for frame in 0..frame_count {
        destination.left[frame] += source.left[frame];
        destination.right[frame] += source.right[frame];
    }
}
