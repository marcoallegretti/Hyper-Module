use super::input_meter::{InputMeterSender, try_input_meter_channel};
use super::{
    CaptureStreamCounters, DesktopInputConfig, DesktopOutputConfig, DesktopOutputStartError,
    DesktopOutputStopError, InputMeterReceiver, OutputStreamCounters, OutputStreamStatus,
    PendingNativeShutdown, STEREO_CHANNELS, StreamStatus,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
use super::{
    PipeWireInputConfig as NativeInputConfig, PipeWireInputStream as NativeInputStream,
    PipeWireOutputStream as NativeOutputStream, PipeWireStartError as NativeInputStartError,
    PipeWireStopError as NativeInputStopError,
};
#[cfg(target_os = "windows")]
use super::{
    WasapiDisconnectReason, WasapiInputConfig as NativeInputConfig,
    WasapiInputStream as NativeInputStream, WasapiOutputStream as NativeOutputStream,
    WasapiStartError as NativeInputStartError, WasapiStopError as NativeInputStopError,
};
use core::convert::Infallible;
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Adjustable, FixedAsync, Resampler, Slip};
use std::sync::Arc;

const MINIMUM_PROCESSING_CHUNK_FRAMES: usize = 4;
const DEFAULT_MAXIMUM_CLOCK_DRIFT_PPM: u32 = 1_000;
const MAXIMUM_CLOCK_DRIFT_PPM: u32 = 10_000;
const MAXIMUM_MONITOR_GAIN_LINEAR: f32 = 1.0;
const GAIN_RAMP_DURATION_DIVISOR: u32 = 200;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopMonitoringConfig {
    input: DesktopInputConfig,
    output: DesktopOutputConfig,
    transport_capacity_frames: usize,
    target_latency_frames: usize,
    processing_chunk_frames: usize,
    maximum_clock_drift_ppm: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopMonitoringConfigError {
    SampleRateMismatch {
        input_hz: u32,
        output_hz: u32,
    },
    ZeroTransportCapacity,
    TransportSampleCapacityOverflow,
    InputBlockExceedsTransport {
        input_frames: usize,
        transport_frames: usize,
    },
    InvalidProcessingChunk {
        actual: usize,
        maximum: usize,
    },
    ProcessingChunkExceedsTransport {
        chunk_frames: usize,
        transport_frames: usize,
    },
    InvalidTargetLatency {
        target_frames: usize,
        minimum: usize,
        maximum: usize,
    },
    ZeroMaximumClockDrift,
    ExcessiveMaximumClockDrift {
        actual_ppm: u32,
        maximum_ppm: u32,
    },
}

impl fmt::Display for DesktopMonitoringConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleRateMismatch {
                input_hz,
                output_hz,
            } => write!(
                formatter,
                "desktop monitoring input is {input_hz} Hz; output is {output_hz} Hz"
            ),
            Self::ZeroTransportCapacity => {
                formatter.write_str("desktop monitoring transport capacity must be nonzero")
            }
            Self::TransportSampleCapacityOverflow => {
                formatter.write_str("desktop monitoring sample capacity overflowed")
            }
            Self::InputBlockExceedsTransport {
                input_frames,
                transport_frames,
            } => write!(
                formatter,
                "desktop monitoring input block has {input_frames} frames; transport capacity is {transport_frames}"
            ),
            Self::InvalidProcessingChunk { actual, maximum } => write!(
                formatter,
                "desktop monitoring processing chunk has {actual} frames; expected {MINIMUM_PROCESSING_CHUNK_FRAMES} through {maximum}"
            ),
            Self::ProcessingChunkExceedsTransport {
                chunk_frames,
                transport_frames,
            } => write!(
                formatter,
                "desktop monitoring processing chunk has {chunk_frames} frames; transport capacity is {transport_frames}"
            ),
            Self::InvalidTargetLatency {
                target_frames,
                minimum,
                maximum,
            } => write!(
                formatter,
                "desktop monitoring target latency is {target_frames} frames; expected {minimum} through {maximum}"
            ),
            Self::ZeroMaximumClockDrift => {
                formatter.write_str("desktop monitoring maximum clock drift must be nonzero")
            }
            Self::ExcessiveMaximumClockDrift {
                actual_ppm,
                maximum_ppm,
            } => write!(
                formatter,
                "desktop monitoring maximum clock drift is {actual_ppm} ppm; maximum is {maximum_ppm} ppm"
            ),
        }
    }
}

impl Error for DesktopMonitoringConfigError {}

impl DesktopMonitoringConfig {
    /// Defines one bounded stereo input-monitoring session.
    ///
    /// Input and output must use the same nominal sample rate. The target is
    /// the software bridge latency; native device buffers add their own fixed
    /// delay. The transport absorbs callback jitter while adaptive frame
    /// slipping compensates only the small long-term drift between independent
    /// device clocks.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopMonitoringConfigError`] when the latency, capacity,
    /// callback, or clock contracts are inconsistent.
    pub fn try_new(
        input: DesktopInputConfig,
        output: DesktopOutputConfig,
        transport_capacity_frames: usize,
        target_latency_frames: usize,
        processing_chunk_frames: usize,
    ) -> Result<Self, DesktopMonitoringConfigError> {
        let config = Self {
            input,
            output,
            transport_capacity_frames,
            target_latency_frames,
            processing_chunk_frames,
            maximum_clock_drift_ppm: DEFAULT_MAXIMUM_CLOCK_DRIFT_PPM,
        };
        config.validate()?;
        Ok(config)
    }

    /// Sets the maximum correction applied by the buffer-fill controller.
    ///
    /// # Errors
    ///
    /// Returns [`DesktopMonitoringConfigError`] for zero or more than one
    /// percent of nominal rate. Larger mismatches require true sample-rate
    /// conversion rather than clock-drift compensation.
    pub fn with_maximum_clock_drift_ppm(
        mut self,
        maximum_clock_drift_ppm: u32,
    ) -> Result<Self, DesktopMonitoringConfigError> {
        self.maximum_clock_drift_ppm = maximum_clock_drift_ppm;
        self.validate()?;
        Ok(self)
    }

    /// Rebinds monitoring to a replacement output while preserving its input,
    /// latency, capacity, chunk, and drift contracts.
    ///
    /// # Errors
    ///
    /// Returns an error when the replacement output is incompatible with the
    /// existing monitoring contract.
    pub fn with_output(
        mut self,
        output: DesktopOutputConfig,
    ) -> Result<Self, DesktopMonitoringConfigError> {
        self.output = output;
        self.validate()?;
        Ok(self)
    }

    #[must_use]
    pub const fn input(&self) -> &DesktopInputConfig {
        &self.input
    }

    #[must_use]
    pub const fn output(&self) -> &DesktopOutputConfig {
        &self.output
    }

    #[must_use]
    pub const fn transport_capacity_frames(&self) -> usize {
        self.transport_capacity_frames
    }

    #[must_use]
    pub const fn target_latency_frames(&self) -> usize {
        self.target_latency_frames
    }

    #[must_use]
    pub const fn processing_chunk_frames(&self) -> usize {
        self.processing_chunk_frames
    }

    #[must_use]
    pub const fn maximum_clock_drift_ppm(&self) -> u32 {
        self.maximum_clock_drift_ppm
    }

    fn validate(&self) -> Result<(), DesktopMonitoringConfigError> {
        if self.input.sample_rate_hz() != self.output.sample_rate_hz() {
            return Err(DesktopMonitoringConfigError::SampleRateMismatch {
                input_hz: self.input.sample_rate_hz(),
                output_hz: self.output.sample_rate_hz(),
            });
        }
        if self.transport_capacity_frames == 0 {
            return Err(DesktopMonitoringConfigError::ZeroTransportCapacity);
        }
        if self
            .transport_capacity_frames
            .checked_mul(STEREO_CHANNELS)
            .is_none()
        {
            return Err(DesktopMonitoringConfigError::TransportSampleCapacityOverflow);
        }
        if self.input.maximum_block_frames() > self.transport_capacity_frames {
            return Err(DesktopMonitoringConfigError::InputBlockExceedsTransport {
                input_frames: self.input.maximum_block_frames(),
                transport_frames: self.transport_capacity_frames,
            });
        }
        if !(MINIMUM_PROCESSING_CHUNK_FRAMES..=super::MAXIMUM_BLOCK_FRAMES)
            .contains(&self.processing_chunk_frames)
        {
            return Err(DesktopMonitoringConfigError::InvalidProcessingChunk {
                actual: self.processing_chunk_frames,
                maximum: super::MAXIMUM_BLOCK_FRAMES,
            });
        }
        if self.processing_chunk_frames >= self.transport_capacity_frames {
            return Err(
                DesktopMonitoringConfigError::ProcessingChunkExceedsTransport {
                    chunk_frames: self.processing_chunk_frames,
                    transport_frames: self.transport_capacity_frames,
                },
            );
        }
        let minimum_latency = self.processing_chunk_frames + 1;
        let maximum_latency = self.transport_capacity_frames - 1;
        if !(minimum_latency..=maximum_latency).contains(&self.target_latency_frames) {
            return Err(DesktopMonitoringConfigError::InvalidTargetLatency {
                target_frames: self.target_latency_frames,
                minimum: minimum_latency,
                maximum: maximum_latency,
            });
        }
        if self.maximum_clock_drift_ppm == 0 {
            return Err(DesktopMonitoringConfigError::ZeroMaximumClockDrift);
        }
        if self.maximum_clock_drift_ppm > MAXIMUM_CLOCK_DRIFT_PPM {
            return Err(DesktopMonitoringConfigError::ExcessiveMaximumClockDrift {
                actual_ppm: self.maximum_clock_drift_ppm,
                maximum_ppm: MAXIMUM_CLOCK_DRIFT_PPM,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopMonitoringControlError {
    NonFiniteGain,
    NegativeGain,
    ExcessiveGain,
}

impl fmt::Display for DesktopMonitoringControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NonFiniteGain => "desktop monitoring gain must be finite",
            Self::NegativeGain => "desktop monitoring gain must not be negative",
            Self::ExcessiveGain => "desktop monitoring gain must not exceed unity",
        })
    }
}

impl Error for DesktopMonitoringControlError {}

#[derive(Debug)]
struct MonitoringControlState {
    gain_bits: AtomicU32,
    muted: AtomicBool,
}

#[derive(Clone, Debug)]
pub struct DesktopMonitoringControl {
    state: Arc<MonitoringControlState>,
}

impl DesktopMonitoringControl {
    fn new() -> Self {
        Self {
            state: Arc::new(MonitoringControlState {
                gain_bits: AtomicU32::new(1.0_f32.to_bits()),
                muted: AtomicBool::new(false),
            }),
        }
    }

    /// Changes the monitor gain without blocking either native callback. The
    /// output processor ramps the change over five milliseconds.
    ///
    /// # Errors
    ///
    /// Rejects non-finite or negative gain values without changing state.
    pub fn set_gain_linear(&self, gain: f32) -> Result<(), DesktopMonitoringControlError> {
        if !gain.is_finite() {
            return Err(DesktopMonitoringControlError::NonFiniteGain);
        }
        if gain < 0.0 {
            return Err(DesktopMonitoringControlError::NegativeGain);
        }
        if gain > MAXIMUM_MONITOR_GAIN_LINEAR {
            return Err(DesktopMonitoringControlError::ExcessiveGain);
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
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MonitoringSnapshot {
    pub accepted_frame_count: u64,
    pub overflow_frame_count: u64,
    pub consumer_closed_frame_count: u64,
    pub rendered_frame_count: u64,
    pub silent_frame_count: u64,
    pub underflow_count: u64,
    pub rebuffer_count: u64,
    pub discarded_stale_frame_count: u64,
    pub buffered_frames: u64,
    pub correction_ppm: i32,
    pub processor_failure_count: u64,
}

#[derive(Debug, Default)]
struct MonitoringCounterState {
    accepted_frames: AtomicU64,
    overflow_frames: AtomicU64,
    consumer_closed_frames: AtomicU64,
    rendered_frames: AtomicU64,
    silent_frames: AtomicU64,
    underflows: AtomicU64,
    rebuffers: AtomicU64,
    discarded_stale_frames: AtomicU64,
    buffered_frames: AtomicU64,
    queued_output_frames: AtomicU64,
    correction_ppm: AtomicI32,
    processor_failures: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MonitoringCounters {
    state: Arc<MonitoringCounterState>,
}

impl MonitoringCounters {
    fn add(counter: &AtomicU64, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }

    pub(crate) fn snapshot(&self) -> MonitoringSnapshot {
        MonitoringSnapshot {
            accepted_frame_count: self.state.accepted_frames.load(Ordering::Relaxed),
            overflow_frame_count: self.state.overflow_frames.load(Ordering::Relaxed),
            consumer_closed_frame_count: self.state.consumer_closed_frames.load(Ordering::Relaxed),
            rendered_frame_count: self.state.rendered_frames.load(Ordering::Relaxed),
            silent_frame_count: self.state.silent_frames.load(Ordering::Relaxed),
            underflow_count: self.state.underflows.load(Ordering::Relaxed),
            rebuffer_count: self.state.rebuffers.load(Ordering::Relaxed),
            discarded_stale_frame_count: self.state.discarded_stale_frames.load(Ordering::Relaxed),
            buffered_frames: self.state.buffered_frames.load(Ordering::Relaxed),
            correction_ppm: self.state.correction_ppm.load(Ordering::Relaxed),
            processor_failure_count: self.state.processor_failures.load(Ordering::Relaxed),
        }
    }

    fn publish_buffered(&self, samples: usize) {
        let queued = self.state.queued_output_frames.load(Ordering::Relaxed);
        let transport = u64::try_from(samples / STEREO_CHANNELS).unwrap_or(u64::MAX);
        self.state
            .buffered_frames
            .store(transport.saturating_add(queued), Ordering::Relaxed);
    }

    fn publish_queued_output(&self, samples: usize) {
        self.state.queued_output_frames.store(
            u64::try_from(samples / STEREO_CHANNELS).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

pub(crate) struct MonitoringProducer {
    inner: HeapProd<f32>,
    counters: MonitoringCounters,
    meter: InputMeterSender,
}

impl MonitoringProducer {
    pub(crate) fn push(&mut self, samples: &[f32]) {
        let _ = self.meter.publish(samples);
        let frames = samples.len() / STEREO_CHANNELS;
        if !self.inner.read_is_held() {
            MonitoringCounters::add(&self.counters.state.consumer_closed_frames, frames);
            self.counters.publish_buffered(0);
            return;
        }
        if self.inner.vacant_len() < samples.len() {
            MonitoringCounters::add(&self.counters.state.overflow_frames, frames);
            self.counters.publish_buffered(self.inner.occupied_len());
            return;
        }
        let pushed = self.inner.push_slice(samples);
        debug_assert_eq!(pushed, samples.len());
        MonitoringCounters::add(&self.counters.state.accepted_frames, frames);
        self.counters.publish_buffered(self.inner.occupied_len());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitoringRenderError {
    AdapterInvariant,
    ResamplerInvariant,
}

pub(crate) struct MonitoringConsumer {
    inner: HeapCons<f32>,
    counters: MonitoringCounters,
    control: DesktopMonitoringControl,
    slip: Slip<f32>,
    input: Vec<f32>,
    chunk: Vec<f32>,
    chunk_offset_samples: usize,
    target_latency_frames: usize,
    processing_chunk_frames: usize,
    maximum_clock_drift_ppm: u32,
    primed: bool,
    applied_gain: f32,
    ramp_target_gain: f32,
    ramp_remaining_frames: u16,
    gain_ramp_frames: u16,
}

impl MonitoringConsumer {
    pub(crate) fn process(&mut self, output: &mut [f32]) -> Result<(), MonitoringRenderError> {
        let result = self.process_inner(output);
        if result.is_err() {
            self.record_processor_failure();
        }
        result
    }

    pub(crate) fn record_processor_failure(&self) {
        MonitoringCounters::add(&self.counters.state.processor_failures, 1);
    }

    fn process_inner(&mut self, output: &mut [f32]) -> Result<(), MonitoringRenderError> {
        output.fill(0.0);
        let mut output_offset = 0;
        while output_offset < output.len() {
            if self.chunk_offset_samples < self.chunk.len() {
                let available = self.chunk.len() - self.chunk_offset_samples;
                let count = available.min(output.len() - output_offset);
                output[output_offset..output_offset + count].copy_from_slice(
                    &self.chunk[self.chunk_offset_samples..self.chunk_offset_samples + count],
                );
                self.chunk_offset_samples += count;
                output_offset += count;
                continue;
            }
            if !self.prepare_chunk()? {
                let silent_frames = (output.len() - output_offset) / STEREO_CHANNELS;
                MonitoringCounters::add(&self.counters.state.silent_frames, silent_frames);
                break;
            }
        }
        let target_gain = if self.control.muted() {
            0.0
        } else {
            self.control.gain_linear()
        };
        if target_gain.to_bits() != self.ramp_target_gain.to_bits() {
            self.ramp_target_gain = target_gain;
            self.ramp_remaining_frames = self.gain_ramp_frames;
        }
        for frame in output.chunks_exact_mut(STEREO_CHANNELS) {
            if self.ramp_remaining_frames > 0 {
                self.applied_gain += (self.ramp_target_gain - self.applied_gain)
                    / f32::from(self.ramp_remaining_frames);
                self.ramp_remaining_frames -= 1;
            }
            for sample in frame {
                *sample *= self.applied_gain;
            }
        }
        MonitoringCounters::add(
            &self.counters.state.rendered_frames,
            output_offset / STEREO_CHANNELS,
        );
        self.counters
            .publish_queued_output(self.chunk.len() - self.chunk_offset_samples);
        self.counters.publish_buffered(self.inner.occupied_len());
        Ok(())
    }

    fn prepare_chunk(&mut self) -> Result<bool, MonitoringRenderError> {
        let mut buffered_frames = self.inner.occupied_len() / STEREO_CHANNELS;
        if !self.primed {
            if buffered_frames < self.target_latency_frames {
                self.counters.publish_buffered(self.inner.occupied_len());
                return Ok(false);
            }
            if self.counters.state.rendered_frames.load(Ordering::Relaxed) > 0 {
                MonitoringCounters::add(&self.counters.state.rebuffers, 1);
            }
            self.primed = true;
        }

        let stale_threshold = self
            .target_latency_frames
            .saturating_add(self.processing_chunk_frames.saturating_mul(2));
        if buffered_frames > stale_threshold {
            let discard_frames = buffered_frames - self.target_latency_frames;
            let discarded = self.discard_frames(discard_frames);
            MonitoringCounters::add(&self.counters.state.discarded_stale_frames, discarded);
            self.slip.reset();
            buffered_frames = self.inner.occupied_len() / STEREO_CHANNELS;
        }

        self.update_ratio(buffered_frames)?;
        let input_frames = self.slip.input_frames_next();
        if buffered_frames < input_frames {
            self.primed = false;
            self.slip.reset();
            MonitoringCounters::add(&self.counters.state.underflows, 1);
            self.counters.publish_buffered(self.inner.occupied_len());
            return Ok(false);
        }
        let input_samples = input_frames * STEREO_CHANNELS;
        let popped = self.inner.pop_slice(&mut self.input[..input_samples]);
        debug_assert_eq!(popped, input_samples);
        let input = InterleavedSlice::new(&self.input, STEREO_CHANNELS, input_frames)
            .map_err(|_| MonitoringRenderError::AdapterInvariant)?;
        let mut output = InterleavedSlice::new_mut(
            &mut self.chunk,
            STEREO_CHANNELS,
            self.processing_chunk_frames,
        )
        .map_err(|_| MonitoringRenderError::AdapterInvariant)?;
        let (_, written) = self
            .slip
            .process_into_buffer(&input, &mut output, None)
            .map_err(|_| MonitoringRenderError::ResamplerInvariant)?;
        if written != self.processing_chunk_frames {
            return Err(MonitoringRenderError::ResamplerInvariant);
        }
        self.chunk_offset_samples = 0;
        self.counters.publish_queued_output(self.chunk.len());
        self.counters.publish_buffered(self.inner.occupied_len());
        Ok(true)
    }

    fn update_ratio(&mut self, buffered_frames: usize) -> Result<(), MonitoringRenderError> {
        let buffered = i64::try_from(buffered_frames)
            .map_err(|_| MonitoringRenderError::ResamplerInvariant)?;
        let target = i64::try_from(self.target_latency_frames)
            .map_err(|_| MonitoringRenderError::ResamplerInvariant)?;
        let maximum = i64::from(self.maximum_clock_drift_ppm);
        let correction_ppm = buffered
            .saturating_sub(target)
            .saturating_mul(maximum)
            .checked_div(target)
            .ok_or(MonitoringRenderError::ResamplerInvariant)?
            .clamp(-maximum, maximum)
            .saturating_neg();
        let correction_ppm =
            i32::try_from(correction_ppm).map_err(|_| MonitoringRenderError::ResamplerInvariant)?;
        let ratio = 1.0 + f64::from(correction_ppm) / 1_000_000.0;
        self.slip
            .set_resample_ratio(ratio, false)
            .map_err(|_| MonitoringRenderError::ResamplerInvariant)?;
        self.counters
            .state
            .correction_ppm
            .store(correction_ppm, Ordering::Relaxed);
        Ok(())
    }

    fn discard_frames(&mut self, mut frames: usize) -> usize {
        let requested = frames;
        while frames > 0 {
            let chunk_frames = frames.min(self.input.len() / STEREO_CHANNELS);
            let samples = chunk_frames * STEREO_CHANNELS;
            let popped = self.inner.pop_slice(&mut self.input[..samples]);
            let popped_frames = popped / STEREO_CHANNELS;
            frames -= popped_frames;
            if popped_frames < chunk_frames {
                break;
            }
        }
        requested - frames
    }
}

#[derive(Debug)]
pub(crate) enum MonitoringTransportBuildError {
    Allocation,
    RingAllocation,
    MeterRingAllocation,
    Resampler,
}

pub(crate) fn try_monitoring_transport(
    config: &DesktopMonitoringConfig,
) -> Result<
    (
        MonitoringProducer,
        MonitoringConsumer,
        MonitoringCounters,
        DesktopMonitoringControl,
        InputMeterReceiver,
    ),
    MonitoringTransportBuildError,
> {
    let sample_capacity = config.transport_capacity_frames * STEREO_CHANNELS;
    let ring = HeapRb::<f32>::try_new(sample_capacity)
        .map_err(|_| MonitoringTransportBuildError::RingAllocation)?;
    let (producer, consumer) = ring.split();
    let (meter, meter_receiver) = try_input_meter_channel()
        .map_err(|_| MonitoringTransportBuildError::MeterRingAllocation)?;
    let slip = Slip::new(
        config.processing_chunk_frames,
        STEREO_CHANNELS,
        FixedAsync::Output,
    )
    .map_err(|_| MonitoringTransportBuildError::Resampler)?;
    let input_frames = slip.input_frames_max();
    let mut input = Vec::new();
    input
        .try_reserve_exact(input_frames * STEREO_CHANNELS)
        .map_err(|_| MonitoringTransportBuildError::Allocation)?;
    input.resize(input_frames * STEREO_CHANNELS, 0.0);
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(config.processing_chunk_frames * STEREO_CHANNELS)
        .map_err(|_| MonitoringTransportBuildError::Allocation)?;
    chunk.resize(config.processing_chunk_frames * STEREO_CHANNELS, 0.0);
    let counters = MonitoringCounters::default();
    let control = DesktopMonitoringControl::new();
    let gain_ramp_frames = u16::try_from(
        config
            .input
            .sample_rate_hz()
            .div_ceil(GAIN_RAMP_DURATION_DIVISOR),
    )
    .unwrap_or(u16::MAX);
    Ok((
        MonitoringProducer {
            inner: producer,
            counters: counters.clone(),
            meter,
        },
        MonitoringConsumer {
            inner: consumer,
            counters: counters.clone(),
            control: control.clone(),
            slip,
            input,
            chunk,
            chunk_offset_samples: config.processing_chunk_frames * STEREO_CHANNELS,
            target_latency_frames: config.target_latency_frames,
            processing_chunk_frames: config.processing_chunk_frames,
            maximum_clock_drift_ppm: config.maximum_clock_drift_ppm,
            primed: false,
            applied_gain: 1.0,
            ramp_target_gain: 1.0,
            ramp_remaining_frames: 0,
            gain_ramp_frames,
        },
        counters,
        control,
        meter_receiver,
    ))
}

#[derive(Debug)]
pub enum DesktopInputStartError {
    ThreadSpawn(std::io::Error),
    ThreadPanicked,
    StartupTimeout,
    CancellationPending {
        shutdown: Box<PendingNativeShutdown>,
    },
    ThreadTerminated,
    Backend {
        source: Box<dyn Error + Send + Sync>,
    },
}

impl fmt::Display for DesktopInputStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(_) => formatter.write_str("desktop input thread could not start"),
            Self::ThreadPanicked => formatter.write_str("desktop input thread panicked"),
            Self::StartupTimeout => formatter.write_str("desktop input startup timed out"),
            Self::CancellationPending { .. } => {
                formatter.write_str("desktop input startup cancellation is still pending")
            }
            Self::ThreadTerminated => {
                formatter.write_str("desktop input thread terminated during startup")
            }
            Self::Backend { source } => source.fmt(formatter),
        }
    }
}

impl Error for DesktopInputStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(source) => Some(source),
            Self::Backend { source } => Some(source.as_ref()),
            Self::ThreadPanicked
            | Self::StartupTimeout
            | Self::CancellationPending { .. }
            | Self::ThreadTerminated => None,
        }
    }
}

impl From<NativeInputStartError> for DesktopInputStartError {
    fn from(source: NativeInputStartError) -> Self {
        match source {
            NativeInputStartError::ThreadSpawn(source) => Self::ThreadSpawn(source),
            NativeInputStartError::ThreadPanicked => Self::ThreadPanicked,
            NativeInputStartError::StartupTimeout => Self::StartupTimeout,
            NativeInputStartError::CancellationPending { shutdown } => {
                Self::CancellationPending { shutdown }
            }
            NativeInputStartError::ThreadTerminated => Self::ThreadTerminated,
            source @ (NativeInputStartError::Backend { .. }
            | NativeInputStartError::BackendSource { .. }) => Self::Backend {
                source: Box::new(source),
            },
        }
    }
}

#[derive(Debug)]
pub enum DesktopInputStopError {
    RuntimeFailed,
    ThreadPanicked,
    ShutdownPending {
        shutdown: Box<PendingNativeShutdown>,
    },
}

impl fmt::Display for DesktopInputStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::RuntimeFailed => "desktop input failed before shutdown completed",
            Self::ThreadPanicked => "desktop input thread panicked during shutdown",
            Self::ShutdownPending { .. } => "desktop input shutdown is still pending",
        })
    }
}

impl Error for DesktopInputStopError {}

impl DesktopInputStopError {
    /// Returns the sole native-thread owner when shutdown has not completed.
    #[must_use]
    pub fn into_pending_shutdown(self) -> Option<PendingNativeShutdown> {
        match self {
            Self::ShutdownPending { shutdown } => Some(*shutdown),
            Self::RuntimeFailed | Self::ThreadPanicked => None,
        }
    }
}

impl From<NativeInputStopError> for DesktopInputStopError {
    fn from(source: NativeInputStopError) -> Self {
        match source {
            NativeInputStopError::RuntimeFailed => Self::RuntimeFailed,
            NativeInputStopError::ThreadPanicked => Self::ThreadPanicked,
            NativeInputStopError::ShutdownPending { shutdown } => {
                Self::ShutdownPending { shutdown }
            }
        }
    }
}

#[derive(Debug)]
pub enum DesktopMonitoringSessionStartError {
    Config(DesktopMonitoringConfigError),
    CapacityUnavailable,
    Input(DesktopInputStartError),
    Output {
        source: DesktopOutputStartError,
        input_cleanup: Option<DesktopInputStopError>,
    },
}

impl fmt::Display for DesktopMonitoringSessionStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(source) => source.fmt(formatter),
            Self::CapacityUnavailable => {
                formatter.write_str("desktop monitoring capacity is unavailable")
            }
            Self::Input(source) => write!(formatter, "desktop monitoring input failed: {source}"),
            Self::Output {
                source,
                input_cleanup: None,
            } => {
                write!(formatter, "desktop monitoring output failed: {source}")
            }
            Self::Output {
                source,
                input_cleanup: Some(cleanup),
            } => write!(
                formatter,
                "desktop monitoring output failed: {source}; input cleanup also failed: {cleanup}"
            ),
        }
    }
}

impl Error for DesktopMonitoringSessionStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(source) => Some(source),
            Self::Input(source) => Some(source),
            Self::Output { source, .. } => Some(source),
            Self::CapacityUnavailable => None,
        }
    }
}

#[derive(Debug)]
pub struct DesktopMonitoringSessionStopError {
    pub input: Option<DesktopInputStopError>,
    pub output: Option<DesktopOutputStopError>,
}

impl fmt::Display for DesktopMonitoringSessionStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.input, &self.output) {
            (Some(input), Some(output)) => write!(
                formatter,
                "desktop monitoring input shutdown failed: {input}; output shutdown failed: {output}"
            ),
            (Some(input), None) => write!(
                formatter,
                "desktop monitoring input shutdown failed: {input}"
            ),
            (None, Some(output)) => write!(
                formatter,
                "desktop monitoring output shutdown failed: {output}"
            ),
            (None, None) => formatter.write_str("desktop monitoring shutdown failed"),
        }
    }
}

impl Error for DesktopMonitoringSessionStopError {}

/// Exclusive native stereo input-to-output monitoring ownership.
#[derive(Debug)]
pub struct DesktopMonitoringSession {
    input: NativeInputStream,
    output: NativeOutputStream,
    config: DesktopMonitoringConfig,
    counters: MonitoringCounters,
    control: DesktopMonitoringControl,
    meter: InputMeterReceiver,
}

impl DesktopMonitoringSession {
    /// Starts input first, then output, so the first audible block is emitted
    /// only after the configured latency target has accumulated.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, unavailable fixed capacity,
    /// or native startup failure. Output failure also reports any input cleanup
    /// failure instead of discarding its shutdown authority.
    pub fn start(
        config: DesktopMonitoringConfig,
    ) -> Result<Self, DesktopMonitoringSessionStartError> {
        config
            .validate()
            .map_err(DesktopMonitoringSessionStartError::Config)?;
        let (mut producer, mut consumer, counters, control, meter) =
            try_monitoring_transport(&config)
                .map_err(|_| DesktopMonitoringSessionStartError::CapacityUnavailable)?;
        let input = NativeInputStream::start(
            NativeInputConfig::from(&config.input),
            move |samples: &[f32]| {
                producer.push(samples);
                Ok::<(), Infallible>(())
            },
        )
        .map_err(|source| DesktopMonitoringSessionStartError::Input(source.into()))?;
        let output =
            match NativeOutputStream::start(config.output.native(), move |samples: &mut [f32]| {
                consumer.process(samples)
            }) {
                Ok(output) => output,
                Err(source) => {
                    let input_cleanup = input.stop().err().map(DesktopInputStopError::from);
                    return Err(DesktopMonitoringSessionStartError::Output {
                        source: source.into(),
                        input_cleanup,
                    });
                }
            };
        Ok(Self {
            input,
            output,
            config,
            counters,
            control,
            meter,
        })
    }

    #[must_use]
    pub const fn config(&self) -> &DesktopMonitoringConfig {
        &self.config
    }

    #[must_use]
    pub fn control(&self) -> DesktopMonitoringControl {
        self.control.clone()
    }

    #[must_use]
    pub fn monitoring_snapshot(&self) -> MonitoringSnapshot {
        self.counters.snapshot()
    }

    /// Returns the accepted-input meter receiver for this live monitor owner.
    #[must_use]
    pub fn input_meter_receiver(&mut self) -> &mut InputMeterReceiver {
        &mut self.meter
    }

    #[must_use]
    pub fn input_counters(&self) -> CaptureStreamCounters {
        self.input.counters()
    }

    #[must_use]
    pub fn output_counters(&self) -> OutputStreamCounters {
        self.output.counters()
    }

    #[must_use]
    pub fn input_status(&self) -> StreamStatus {
        self.input.status()
    }

    #[must_use]
    pub fn output_status(&self) -> OutputStreamStatus {
        self.output.status()
    }

    /// Stops capture before playback and attempts both bounded joins even if
    /// one side fails.
    ///
    /// # Errors
    ///
    /// Returns every native shutdown failure with pending ownership retained.
    pub fn stop(self) -> Result<(), DesktopMonitoringSessionStopError> {
        let input = self.input.stop().err().map(DesktopInputStopError::from);
        let output = self.output.stop().err().map(DesktopOutputStopError::from);
        if input.is_none() && output.is_none() {
            Ok(())
        } else {
            Err(DesktopMonitoringSessionStopError { input, output })
        }
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn input_disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.input.disconnect_reason()
    }

    #[cfg(target_os = "windows")]
    #[must_use]
    pub fn output_disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        self.output.disconnect_reason()
    }
}

#[cfg(test)]
#[path = "../../../../test-support/realtime_allocator.rs"]
pub(crate) mod realtime_allocator;

#[cfg(test)]
mod tests {
    use crate::{CaptureCallback, CaptureCallbackStatus};

    use super::realtime_allocator::HeapMeasurement;
    use super::*;

    fn config() -> DesktopMonitoringConfig {
        DesktopMonitoringConfig::try_new(
            DesktopInputConfig::try_new(48_000, 128).unwrap(),
            DesktopOutputConfig::try_new(48_000, 128).unwrap(),
            1_024,
            256,
            64,
        )
        .unwrap()
    }

    fn transport() -> (
        MonitoringProducer,
        MonitoringConsumer,
        MonitoringCounters,
        DesktopMonitoringControl,
        InputMeterReceiver,
    ) {
        try_monitoring_transport(&config()).unwrap()
    }

    #[test]
    fn configuration_rejects_inconsistent_domains() {
        let input = DesktopInputConfig::try_new(44_100, 128).unwrap();
        let output = DesktopOutputConfig::try_new(48_000, 128).unwrap();
        assert_eq!(
            DesktopMonitoringConfig::try_new(input, output, 1_024, 256, 64),
            Err(DesktopMonitoringConfigError::SampleRateMismatch {
                input_hz: 44_100,
                output_hz: 48_000,
            })
        );
        assert!(matches!(
            DesktopMonitoringConfig::try_new(
                DesktopInputConfig::try_new(48_000, 128).unwrap(),
                DesktopOutputConfig::try_new(48_000, 128).unwrap(),
                256,
                32,
                256,
            ),
            Err(DesktopMonitoringConfigError::ProcessingChunkExceedsTransport { .. })
        ));
    }

    #[test]
    fn replacement_output_preserves_the_monitoring_contract() {
        let original = config().with_maximum_clock_drift_ppm(750).unwrap();
        let replacement = DesktopOutputConfig::try_new(48_000, 256)
            .unwrap()
            .with_routing_id("replacement-output")
            .unwrap();
        let rebound = original.clone().with_output(replacement.clone()).unwrap();

        assert_eq!(rebound.input(), original.input());
        assert_eq!(rebound.output(), &replacement);
        assert_eq!(
            rebound.transport_capacity_frames(),
            original.transport_capacity_frames()
        );
        assert_eq!(
            rebound.target_latency_frames(),
            original.target_latency_frames()
        );
        assert_eq!(
            rebound.processing_chunk_frames(),
            original.processing_chunk_frames()
        );
        assert_eq!(rebound.maximum_clock_drift_ppm(), 750);

        let incompatible = DesktopOutputConfig::try_new(44_100, 128).unwrap();
        assert_eq!(
            original.with_output(incompatible),
            Err(DesktopMonitoringConfigError::SampleRateMismatch {
                input_hz: 48_000,
                output_hz: 44_100,
            })
        );
    }

    #[test]
    fn output_stays_silent_until_target_latency_is_available() {
        let (mut producer, mut consumer, counters, _, _) = transport();
        producer.push(&vec![0.25; 128 * STEREO_CHANNELS]);
        let mut output = [1.0; 32];
        consumer.process(&mut output).unwrap();
        assert!(
            output
                .iter()
                .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
        );
        assert_eq!(counters.snapshot().silent_frame_count, 16);

        producer.push(&vec![0.25; 128 * STEREO_CHANNELS]);
        consumer.process(&mut output).unwrap();
        assert!(
            output
                .iter()
                .all(|sample| sample.to_bits() == 0.25_f32.to_bits())
        );
    }

    #[test]
    fn gain_and_mute_are_applied_without_touching_transport_data() {
        let (mut producer, mut consumer, _, control, _) = transport();
        producer.push(&vec![0.5; 256 * STEREO_CHANNELS]);
        control.set_gain_linear(0.25).unwrap();
        let mut output = vec![0.0; 240 * STEREO_CHANNELS];
        consumer.process(&mut output).unwrap();
        assert!(output[0] > 0.125 && output[0] < 0.5);
        assert!((output[output.len() - 1] - 0.125).abs() <= f32::EPSILON);
        producer.push(&vec![0.5; 256 * STEREO_CHANNELS]);
        control.set_muted(true);
        consumer.process(&mut output).unwrap();
        assert!(output[0] > 0.0 && output[0] < 0.125);
        assert!(output[output.len() - 1].abs() <= f32::EPSILON);
        assert_eq!(
            control.set_gain_linear(f32::NAN),
            Err(DesktopMonitoringControlError::NonFiniteGain)
        );
        assert_eq!(
            control.set_gain_linear(1.01),
            Err(DesktopMonitoringControlError::ExcessiveGain)
        );
        assert_eq!(control.gain_linear().to_bits(), 0.25_f32.to_bits());
    }

    #[test]
    fn saturation_is_nonterminal_and_observable() {
        let (mut producer, _consumer, counters, _, _) = transport();
        producer.push(&vec![0.0; 900 * STEREO_CHANNELS]);
        producer.push(&vec![0.0; 128 * STEREO_CHANNELS]);
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.accepted_frame_count, 900);
        assert_eq!(snapshot.overflow_frame_count, 128);
        assert_eq!(snapshot.buffered_frames, 900);
    }

    #[test]
    fn accepted_input_meter_is_independent_from_monitoring_bridge_saturation() {
        let (mut producer, _consumer, counters, _, mut meter) = transport();
        producer.push(&vec![0.25; 900 * STEREO_CHANNELS]);
        producer.push(&vec![0.75; 128 * STEREO_CHANNELS]);

        let snapshot = meter
            .read_latest()
            .snapshot()
            .expect("accepted capture meter snapshot");
        assert_eq!(snapshot.accepted_frame_end(), 1_028);
        assert_eq!(snapshot.frame_count(), 128);
        assert_eq!(snapshot.left_sample_peak().to_bits(), 0.75_f32.to_bits());
        assert_eq!(snapshot.right_sample_peak().to_bits(), 0.75_f32.to_bits());
        assert_eq!(counters.snapshot().accepted_frame_count, 900);
        assert_eq!(counters.snapshot().overflow_frame_count, 128);
    }

    #[test]
    fn capture_validation_blocks_invalid_input_before_accepted_input_meter() {
        let (mut producer, _consumer, counters, _, mut meter) = transport();
        let mut callback = CaptureCallback::try_new(
            move |samples: &[f32]| {
                producer.push(samples);
                Ok::<(), Infallible>(())
            },
            1,
        )
        .unwrap();

        assert_eq!(
            callback.process(&[0.25, -0.5]),
            CaptureCallbackStatus::Captured
        );
        assert_eq!(
            callback.process(&[0.75]),
            CaptureCallbackStatus::SampleCountNotStereoAligned { samples: 1 }
        );
        assert_eq!(
            callback.process(&[0.75, -0.5, 0.25, -0.125]),
            CaptureCallbackStatus::BlockTooLarge {
                frames: 2,
                maximum: 1,
            }
        );
        assert_eq!(
            callback.process(&[f32::NAN, 0.0]),
            CaptureCallbackStatus::NonFiniteInput
        );

        let snapshot = meter
            .read_latest()
            .snapshot()
            .expect("only the validated input block reaches the meter");
        assert_eq!(snapshot.accepted_frame_end(), 1);
        assert_eq!(snapshot.frame_count(), 1);
        assert_eq!(snapshot.left_sample_peak().to_bits(), 0.25_f32.to_bits());
        assert_eq!(snapshot.right_sample_peak().to_bits(), 0.5_f32.to_bits());
        assert_eq!(counters.snapshot().accepted_frame_count, 1);
        assert_eq!(counters.snapshot().overflow_frame_count, 0);
    }

    #[test]
    fn peer_closure_is_fail_silent_for_ordered_shutdown() {
        let (mut producer, consumer, counters, _, _) = transport();
        drop(consumer);
        producer.push(&[0.25; 16]);
        assert_eq!(counters.snapshot().consumer_closed_frame_count, 8);

        let (producer, mut consumer, _, _, _) = transport();
        drop(producer);
        let mut output = [1.0; 16];
        consumer.process(&mut output).unwrap();
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn underflow_rebuffers_before_audio_resumes() {
        let (mut producer, mut consumer, counters, _, _) = transport();
        producer.push(&vec![0.5; 256 * STEREO_CHANNELS]);
        let mut output = vec![0.0; 256 * STEREO_CHANNELS];
        consumer.process(&mut output).unwrap();
        consumer.process(&mut output).unwrap();
        assert!(counters.snapshot().underflow_count > 0);
        producer.push(&vec![0.25; 128 * STEREO_CHANNELS]);
        consumer.process(&mut output[..32]).unwrap();
        assert!(
            output[..32]
                .iter()
                .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
        );
        producer.push(&vec![0.25; 128 * STEREO_CHANNELS]);
        consumer.process(&mut output[..32]).unwrap();
        assert!(output[..32].iter().any(|sample| sample.abs() > 0.0));
        assert_eq!(counters.snapshot().rebuffer_count, 1);
    }

    #[test]
    fn excessive_backlog_is_discarded_to_restore_latency() {
        let (mut producer, mut consumer, counters, _, _) = transport();
        producer.push(&vec![0.5; 900 * STEREO_CHANNELS]);
        let mut output = [0.0; 16];
        consumer.process(&mut output).unwrap();
        let snapshot = counters.snapshot();
        assert_eq!(snapshot.discarded_stale_frame_count, 644);
        assert!(snapshot.buffered_frames <= 256);
    }

    #[test]
    fn buffer_controller_corrects_in_the_required_direction() {
        let (mut producer, mut consumer, counters, _, _) = transport();
        producer.push(&vec![0.25; 320 * STEREO_CHANNELS]);
        let mut output = vec![0.0; 64 * STEREO_CHANNELS];
        consumer.process(&mut output).unwrap();
        assert!(counters.snapshot().correction_ppm < 0);

        consumer.process(&mut output).unwrap();
        assert_eq!(counters.snapshot().correction_ppm, 0);
        consumer.process(&mut output).unwrap();
        assert!(counters.snapshot().correction_ppm > 0);
    }

    #[test]
    fn monitoring_callbacks_perform_zero_heap_operations() {
        let (mut producer, mut consumer, _, _, _) = transport();
        producer.push(&vec![0.25; 256 * STEREO_CHANNELS]);
        let input = [0.5; 64 * STEREO_CHANNELS];
        let mut output = [0.0; 64 * STEREO_CHANNELS];

        let measurement = HeapMeasurement::start_verified();
        consumer.process(&mut output).unwrap();
        producer.push(&input);
        let heap_operations = measurement.finish();

        assert_eq!(heap_operations, 0);
    }
}
