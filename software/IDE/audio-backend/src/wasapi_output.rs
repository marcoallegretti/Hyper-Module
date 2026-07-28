use super::{InterleavedStereoProcessor, OutputCallback, OutputStreamCounters, STEREO_CHANNELS};
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper_mixer_runtime::{TrackWindowRenderer, TransportOutputProcessor};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wasapi::{Direction, SampleType, ShareMode, StreamMode, WaveFormat};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const EVENT_WAIT_MILLISECONDS: u32 = 100;
const MAX_CONSECUTIVE_EVENT_TIMEOUTS: u8 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasapiOutputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    device_id: Option<String>,
}

impl WasapiOutputConfig {
    /// Creates a strict shared-mode stereo `f32` output configuration.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiOutputConfigError`] for a zero sample rate or an
    /// invalid callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, WasapiOutputConfigError> {
        if sample_rate_hz == 0 {
            return Err(WasapiOutputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(WasapiOutputConfigError::InvalidMaximumBlockFrames {
                actual: maximum_block_frames,
                maximum: super::MAXIMUM_BLOCK_FRAMES,
            });
        }
        Ok(Self {
            sample_rate_hz,
            maximum_block_frames,
            device_id: None,
        })
    }

    /// Selects an endpoint identifier instead of the default render endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiOutputConfigError`] for an empty identifier or an
    /// embedded NUL.
    pub fn with_device_id(mut self, device_id: &str) -> Result<Self, WasapiOutputConfigError> {
        if device_id.is_empty() {
            return Err(WasapiOutputConfigError::EmptyDeviceId);
        }
        if device_id.as_bytes().contains(&0) {
            return Err(WasapiOutputConfigError::DeviceIdContainsNul);
        }
        self.device_id = Some(device_id.to_owned());
        Ok(self)
    }

    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn maximum_block_frames(&self) -> usize {
        self.maximum_block_frames
    }

    #[must_use]
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasapiOutputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyDeviceId,
    DeviceIdContainsNul,
}

impl fmt::Display for WasapiOutputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("WASAPI sample rate must be nonzero"),
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "WASAPI maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyDeviceId => {
                formatter.write_str("WASAPI device identifier must not be empty")
            }
            Self::DeviceIdContainsNul => {
                formatter.write_str("WASAPI device identifier contains an embedded NUL")
            }
        }
    }
}

impl Error for WasapiOutputConfigError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasapiStartupStage {
    Com,
    Enumerator,
    Device,
    Client,
    Format,
    Initialize,
    Event,
    RenderClient,
    Buffer,
    Start,
    Callback,
}

impl fmt::Display for WasapiStartupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Com => "COM initialization",
            Self::Enumerator => "device enumeration",
            Self::Device => "device selection",
            Self::Client => "audio client",
            Self::Format => "audio format",
            Self::Initialize => "stream initialization",
            Self::Event => "event handle",
            Self::RenderClient => "render client",
            Self::Buffer => "render buffer",
            Self::Start => "stream start",
            Self::Callback => "callback",
        })
    }
}

#[derive(Debug)]
pub enum WasapiStartError {
    ThreadSpawn(std::io::Error),
    ThreadPanicked,
    StartupTimeout,
    ThreadTerminated,
    Backend {
        stage: WasapiStartupStage,
        detail: String,
    },
}

impl fmt::Display for WasapiStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(source) => write!(formatter, "cannot start WASAPI thread: {source}"),
            Self::ThreadPanicked => formatter.write_str("WASAPI thread panicked during startup"),
            Self::StartupTimeout => formatter.write_str("WASAPI startup timed out"),
            Self::ThreadTerminated => {
                formatter.write_str("WASAPI thread terminated during startup")
            }
            Self::Backend { stage, detail } => {
                write!(formatter, "WASAPI {stage} failed: {detail}")
            }
        }
    }
}

impl Error for WasapiStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ThreadSpawn(source) => Some(source),
            Self::ThreadPanicked
            | Self::StartupTimeout
            | Self::ThreadTerminated
            | Self::Backend { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasapiStopError {
    ThreadPanicked,
}

impl fmt::Display for WasapiStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WASAPI thread panicked during shutdown")
    }
}

impl Error for WasapiStopError {}

#[derive(Debug)]
pub struct WasapiOutputStream {
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counters: OutputStreamCounters,
}

#[derive(Debug)]
pub struct WasapiThreadRunner {
    config: WasapiOutputConfig,
    stop_requested: Arc<AtomicBool>,
    startup: SyncSender<Result<(), WasapiStartError>>,
    counters: OutputStreamCounters,
}

impl WasapiThreadRunner {
    /// Runs the event-driven shared stream with thread-local processor state.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] for callback, COM, endpoint, format,
    /// stream, or buffer initialization failure.
    pub fn run<P>(self, processor: P) -> Result<(), WasapiStartError>
    where
        P: InterleavedStereoProcessor,
    {
        let callback = OutputCallback::try_new_with_counters(
            processor,
            self.config.maximum_block_frames,
            self.counters.clone(),
        )
        .map_err(|source| WasapiStartError::Backend {
            stage: WasapiStartupStage::Callback,
            detail: source.to_string(),
        })?;
        run_wasapi_inner(
            &self.config,
            callback,
            &self.stop_requested,
            &self.startup,
            &self.counters,
        )
    }

    /// Runs one prepared transport output processor on this WASAPI thread.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] when the transport and stream callback
    /// capacities differ or when backend startup or lifecycle fails. Transport
    /// processing failures follow the stream's fail-silent counter policy.
    pub fn run_transport_output<R>(
        self,
        mut processor: TransportOutputProcessor<'_, '_, R>,
    ) -> Result<(), WasapiStartError>
    where
        R: TrackWindowRenderer,
    {
        validate_transport_capacity(
            self.config.maximum_block_frames,
            processor.maximum_block_frames(),
        )?;
        self.run(move |output: &mut [f32]| processor.process(output).map(|_| ()))
    }

    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }
}

impl WasapiOutputStream {
    /// Starts a strict shared-mode stereo render stream.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] when the thread, endpoint, exact format,
    /// callback, event-driven stream, or initial render buffer cannot start.
    pub fn start<P>(config: WasapiOutputConfig, processor: P) -> Result<Self, WasapiStartError>
    where
        P: InterleavedStereoProcessor + Send + 'static,
    {
        Self::start_with_thread_initializer(config, move |thread| thread.run(processor))
    }

    /// Constructs borrowed processor state and runs it on the owning COM thread.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] when initialization panics, terminates,
    /// times out, or reports a backend failure.
    pub fn start_with_thread_initializer<F>(
        config: WasapiOutputConfig,
        initialize: F,
    ) -> Result<Self, WasapiStartError>
    where
        F: FnOnce(WasapiThreadRunner) -> Result<(), WasapiStartError> + Send + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let counters = OutputStreamCounters::default();
        let thread_stop = Arc::clone(&stop_requested);
        let thread_counters = counters.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let failure_sender = startup_sender.clone();
        let thread = thread::Builder::new()
            .name("hyper-wasapi-output".to_owned())
            .spawn(move || {
                let thread = WasapiThreadRunner {
                    config,
                    stop_requested: thread_stop,
                    startup: startup_sender,
                    counters: thread_counters,
                };
                if let Err(error) = initialize(thread) {
                    let _ = failure_sender.try_send(Err(error));
                }
            })
            .map_err(WasapiStartError::ThreadSpawn)?;

        match wait_for_startup(&startup_receiver) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread: Some(thread),
                counters,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.join() {
                    Ok(()) => Err(error),
                    Err(_) => Err(WasapiStartError::ThreadPanicked),
                }
            }
        }
    }

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.counters.clone()
    }

    /// Stops and joins the owned render thread.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStopError`] if the render thread panicked.
    pub fn stop(mut self) -> Result<(), WasapiStopError> {
        self.stop_requested.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<(), WasapiStopError> {
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| WasapiStopError::ThreadPanicked)?;
        }
        Ok(())
    }
}

impl Drop for WasapiOutputStream {
    fn drop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        let _ = self.join();
    }
}

fn wait_for_startup(
    receiver: &Receiver<Result<(), WasapiStartError>>,
) -> Result<(), WasapiStartError> {
    match receiver.recv_timeout(STARTUP_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(WasapiStartError::StartupTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(WasapiStartError::ThreadTerminated),
    }
}

fn run_wasapi_inner<P: InterleavedStereoProcessor>(
    config: &WasapiOutputConfig,
    mut callback: OutputCallback<P>,
    stop_requested: &AtomicBool,
    startup: &SyncSender<Result<(), WasapiStartError>>,
    counters: &OutputStreamCounters,
) -> Result<(), WasapiStartError> {
    let _com = initialize_com()?;
    let mut output = open_render_output(config)?;
    let initial_frames = usize::try_from(
        output
            .client
            .get_buffer_size()
            .map_err(|source| backend_error(WasapiStartupStage::Buffer, source))?,
    )
    .map_err(|_| WasapiStartError::Backend {
        stage: WasapiStartupStage::Buffer,
        detail: "endpoint buffer size does not fit the platform address space".to_owned(),
    })?;
    write_frames(
        initial_frames,
        config.maximum_block_frames,
        &mut output.samples,
        &mut callback,
        &output.render,
    )
    .map_err(|source| backend_error(WasapiStartupStage::Buffer, source))?;
    output
        .client
        .start_stream()
        .map_err(|source| backend_error(WasapiStartupStage::Start, source))?;
    let _started = StartedStream(&output.client);
    let _ = startup.try_send(Ok(()));

    let mut consecutive_event_timeouts = 0_u8;
    while !stop_requested.load(Ordering::Acquire) {
        if output
            .event
            .wait_for_event(EVENT_WAIT_MILLISECONDS)
            .is_err()
        {
            consecutive_event_timeouts = consecutive_event_timeouts.saturating_add(1);
            if consecutive_event_timeouts >= MAX_CONSECUTIVE_EVENT_TIMEOUTS {
                counters.record_stream_error();
                break;
            }
            continue;
        }
        consecutive_event_timeouts = 0;
        let Ok(available) = output.client.get_available_space_in_frames() else {
            counters.record_stream_error();
            break;
        };
        if write_frames(
            usize::try_from(available).unwrap_or(usize::MAX),
            config.maximum_block_frames,
            &mut output.samples,
            &mut callback,
            &output.render,
        )
        .is_err()
        {
            counters.record_stream_error();
            break;
        }
    }
    Ok(())
}

struct WasapiRenderOutput {
    client: wasapi::AudioClient,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    samples: Vec<f32>,
}

fn initialize_com() -> Result<ComGuard, WasapiStartError> {
    let result = wasapi::initialize_mta();
    if result.is_err() {
        return Err(WasapiStartError::Backend {
            stage: WasapiStartupStage::Com,
            detail: format!("{result:?}"),
        });
    }
    Ok(ComGuard)
}

fn open_render_output(config: &WasapiOutputConfig) -> Result<WasapiRenderOutput, WasapiStartError> {
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|source| backend_error(WasapiStartupStage::Enumerator, source))?;
    let device = match &config.device_id {
        Some(device_id) => enumerator
            .get_device(device_id)
            .map_err(|source| backend_error(WasapiStartupStage::Device, source))?,
        None => enumerator
            .get_default_device(&Direction::Render)
            .map_err(|source| backend_error(WasapiStartupStage::Device, source))?,
    };
    let mut client = device
        .get_iaudioclient()
        .map_err(|source| backend_error(WasapiStartupStage::Client, source))?;
    let sample_rate_hz =
        usize::try_from(config.sample_rate_hz).map_err(|_| WasapiStartError::Backend {
            stage: WasapiStartupStage::Format,
            detail: "sample rate does not fit the platform address space".to_owned(),
        })?;
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        sample_rate_hz,
        STEREO_CHANNELS,
        None,
    );
    match client
        .is_supported(&format, &ShareMode::Shared)
        .map_err(|source| backend_error(WasapiStartupStage::Format, source))?
    {
        None => {}
        Some(_) => {
            return Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Format,
                detail: "endpoint does not support exact stereo f32 at the project sample rate"
                    .to_owned(),
            });
        }
    }
    client
        .initialize_client(
            &format,
            &Direction::Render,
            &StreamMode::EventsShared {
                autoconvert: false,
                buffer_duration_hns: 0,
            },
        )
        .map_err(|source| backend_error(WasapiStartupStage::Initialize, source))?;
    let event = client
        .set_get_eventhandle()
        .map_err(|source| backend_error(WasapiStartupStage::Event, source))?;
    let render = client
        .get_audiorenderclient()
        .map_err(|source| backend_error(WasapiStartupStage::RenderClient, source))?;
    let mut samples = Vec::new();
    let sample_capacity = config
        .maximum_block_frames
        .checked_mul(STEREO_CHANNELS)
        .ok_or_else(|| WasapiStartError::Backend {
            stage: WasapiStartupStage::Buffer,
            detail: "render sample capacity overflowed".to_owned(),
        })?;
    samples
        .try_reserve_exact(sample_capacity)
        .map_err(|_| WasapiStartError::Backend {
            stage: WasapiStartupStage::Buffer,
            detail: "render sample capacity is unavailable".to_owned(),
        })?;
    samples.resize(sample_capacity, 0.0_f32);
    Ok(WasapiRenderOutput {
        client,
        render,
        event,
        samples,
    })
}

fn write_frames<P: InterleavedStereoProcessor>(
    mut remaining_frames: usize,
    maximum_block_frames: usize,
    samples: &mut [f32],
    callback: &mut OutputCallback<P>,
    render: &wasapi::AudioRenderClient,
) -> Result<(), wasapi::WasapiError> {
    while remaining_frames > 0 {
        let frames = remaining_frames.min(maximum_block_frames);
        let sample_count = frames * STEREO_CHANNELS;
        let block = &mut samples[..sample_count];
        let _ = callback.process(block);
        render.write_to_device(frames, bytemuck::cast_slice(block), None)?;
        remaining_frames -= frames;
    }
    Ok(())
}

fn backend_error(stage: WasapiStartupStage, source: impl fmt::Display) -> WasapiStartError {
    WasapiStartError::Backend {
        stage,
        detail: source.to_string(),
    }
}

fn validate_transport_capacity(
    wasapi_frames: usize,
    transport_frames: usize,
) -> Result<(), WasapiStartError> {
    if wasapi_frames != transport_frames {
        return Err(WasapiStartError::Backend {
            stage: WasapiStartupStage::Callback,
            detail: format!(
                "WASAPI capacity is {wasapi_frames} frames; transport capacity is {transport_frames}"
            ),
        });
    }
    Ok(())
}

struct ComGuard;

impl Drop for ComGuard {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

struct StartedStream<'client>(&'client wasapi::AudioClient);

impl Drop for StartedStream<'_> {
    fn drop(&mut self) {
        let _ = self.0.stop_stream();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_invalid_domains() {
        assert_eq!(
            WasapiOutputConfig::try_new(0, 64),
            Err(WasapiOutputConfigError::ZeroSampleRate)
        );
        assert!(matches!(
            WasapiOutputConfig::try_new(48_000, 0),
            Err(WasapiOutputConfigError::InvalidMaximumBlockFrames { actual: 0, .. })
        ));
        assert_eq!(
            WasapiOutputConfig::try_new(48_000, 64)
                .unwrap()
                .with_device_id(""),
            Err(WasapiOutputConfigError::EmptyDeviceId)
        );
    }

    #[test]
    fn thread_local_initializer_failure_is_reported_before_device_start() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiOutputStream::start_with_thread_initializer(config, |_| {
            Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Callback,
                detail: "fixture rejected".to_owned(),
            })
        });

        assert!(matches!(
            result,
            Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Callback,
                ..
            })
        ));
    }

    #[test]
    fn thread_local_initializer_must_run_its_context() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiOutputStream::start_with_thread_initializer(config, |_| Ok(()));

        assert!(matches!(result, Err(WasapiStartError::ThreadTerminated)));
    }

    #[test]
    fn thread_local_initializer_panic_is_distinct() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiOutputStream::start_with_thread_initializer(config, |_| {
            panic!("initializer panic");
        });

        assert!(matches!(result, Err(WasapiStartError::ThreadPanicked)));
    }

    #[test]
    fn transport_and_wasapi_capacities_must_match() {
        assert!(matches!(
            validate_transport_capacity(64, 128),
            Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Callback,
                detail,
            }) if detail == "WASAPI capacity is 64 frames; transport capacity is 128"
        ));
    }
}
