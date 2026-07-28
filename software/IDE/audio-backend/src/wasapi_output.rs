use super::{
    BackendThreadCompletion, BackendThreadJoin, BoundedBackendThread, InterleavedStereoProcessor,
    OutputCallback, OutputCallbackStatus, OutputStreamCounters, OutputStreamState,
    OutputStreamStatus, PlaybackRecoveryCheckpoint, STEREO_CHANNELS, WasapiOutputEndpoint,
};
use core::cell::Cell;
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use hyper_mixer_runtime::{TrackWindowRenderer, TransportOutputProcessor};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Duration;
use wasapi::{
    Direction, DisconnectReason, EventCallbacks, SampleType, ShareMode, StreamMode, WaveFormat,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const EVENT_WAIT_MILLISECONDS: u32 = 100;
const MAX_CONSECUTIVE_EVENT_TIMEOUTS: u8 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasapiOutputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    device_id: Option<String>,
}

impl WasapiOutputConfig {
    pub(crate) const fn from_desktop(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
        device_id: Option<String>,
    ) -> Self {
        Self {
            sample_rate_hz,
            maximum_block_frames,
            device_id,
        }
    }

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

    /// Selects one endpoint returned by desktop output discovery.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiOutputConfigError`] if its routing identity is invalid.
    pub fn with_output_target(
        self,
        target: &WasapiOutputEndpoint,
    ) -> Result<Self, WasapiOutputConfigError> {
        self.with_device_id(target.routing_id())
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
    SessionControl,
    Notifications,
    State,
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
            Self::SessionControl => "audio session control",
            Self::Notifications => "audio session notifications",
            Self::State => "stream state",
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
    ShutdownTimeout,
}

impl fmt::Display for WasapiStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ThreadPanicked => "WASAPI thread panicked during shutdown",
            Self::ShutdownTimeout => "WASAPI shutdown timed out",
        })
    }
}

impl Error for WasapiStopError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum WasapiDisconnectReason {
    /// The selected endpoint was removed.
    DeviceRemoval = 1,
    /// The Windows audio service stopped.
    AudioServiceStopped = 2,
    /// The selected endpoint's shared stream format changed.
    FormatChanged = 3,
    /// The owning Windows session logged off.
    SessionLogoff = 4,
    /// The owning Windows session disconnected.
    SessionDisconnected = 5,
    /// An exclusive-mode client displaced this shared-mode stream.
    ExclusiveModeOverride = 6,
    /// Windows supplied a disconnect reason unknown to the binding.
    Unknown = 7,
}

#[derive(Debug)]
pub struct WasapiOutputStream {
    stop_requested: Arc<AtomicBool>,
    thread: BoundedBackendThread,
    counters: OutputStreamCounters,
    status: OutputStreamStatus,
    disconnect_reason: Arc<AtomicU8>,
    shutdown_timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct WasapiThreadRunner {
    config: WasapiOutputConfig,
    stop_requested: Arc<AtomicBool>,
    startup: SyncSender<Result<(), WasapiStartError>>,
    counters: OutputStreamCounters,
    status: OutputStreamStatus,
    disconnect_reason: Arc<AtomicU8>,
}

impl WasapiThreadRunner {
    /// Runs the event-driven shared stream with thread-local processor state.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] for callback, COM, endpoint, format,
    /// stream, or buffer initialization failure.
    pub(crate) fn run<P>(self, processor: P) -> Result<(), WasapiStartError>
    where
        P: InterleavedStereoProcessor,
    {
        self.run_with_commit(processor, || {})
    }

    fn run_with_commit<P, C>(self, processor: P, commit: C) -> Result<(), WasapiStartError>
    where
        P: InterleavedStereoProcessor,
        C: FnMut(),
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
            WasapiRuntimeSignals {
                stop_requested: &self.stop_requested,
                startup: &self.startup,
                counters: &self.counters,
                status: &self.status,
                disconnect_reason: &self.disconnect_reason,
            },
            commit,
        )
    }

    pub(crate) fn run_transport_output_with_checkpoint<R>(
        self,
        mut processor: TransportOutputProcessor<'_, '_, R>,
        checkpoint: &PlaybackRecoveryCheckpoint,
    ) -> Result<(), WasapiStartError>
    where
        R: TrackWindowRenderer,
    {
        validate_transport_capacity(
            self.config.maximum_block_frames,
            processor.maximum_block_frames(),
        )?;
        let pending_cycle = Cell::new(None);
        self.run_with_commit(
            |output: &mut [f32]| {
                pending_cycle.set(None);
                processor.process(output).map(|rendered| {
                    pending_cycle.set(Some(rendered.snapshot().cycle()));
                })
            },
            || {
                let cycle = pending_cycle
                    .take()
                    .expect("a rendered WASAPI block has a pending recovery cycle");
                checkpoint.publish_cycle(cycle);
            },
        )
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn stop_requested(&self) -> bool {
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
    pub(crate) fn start_with_thread_initializer<F>(
        config: WasapiOutputConfig,
        initialize: F,
    ) -> Result<Self, WasapiStartError>
    where
        F: FnOnce(WasapiThreadRunner) -> Result<(), WasapiStartError> + Send + 'static,
    {
        Self::start_with_thread_initializer_timeouts(
            config,
            initialize,
            STARTUP_TIMEOUT,
            SHUTDOWN_TIMEOUT,
        )
    }

    fn start_with_thread_initializer_timeouts<F>(
        config: WasapiOutputConfig,
        initialize: F,
        startup_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<Self, WasapiStartError>
    where
        F: FnOnce(WasapiThreadRunner) -> Result<(), WasapiStartError> + Send + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let counters = OutputStreamCounters::default();
        let status = OutputStreamStatus::default();
        let disconnect_reason = Arc::new(AtomicU8::new(0));
        let thread_stop = Arc::clone(&stop_requested);
        let thread_counters = counters.clone();
        let thread_status = status.clone();
        let completion_status = status.clone();
        let thread_disconnect_reason = Arc::clone(&disconnect_reason);
        let (startup_sender, startup_receiver) = sync_channel(1);
        let failure_sender = startup_sender.clone();
        let (completion_sender, completion_receiver) = sync_channel(1);
        let completion_thread_status = status.clone();
        let handle = thread::Builder::new()
            .name("hyper-wasapi-output".to_owned())
            .spawn(move || {
                let _completion = BackendThreadCompletion {
                    sender: completion_sender,
                    status: completion_thread_status,
                };
                let thread = WasapiThreadRunner {
                    config,
                    stop_requested: thread_stop,
                    startup: startup_sender,
                    counters: thread_counters,
                    status: thread_status,
                    disconnect_reason: thread_disconnect_reason,
                };
                if let Err(error) = initialize(thread) {
                    completion_status.mark_failed();
                    let _ = failure_sender.try_send(Err(error));
                } else {
                    completion_status.mark_stopped();
                }
            })
            .map_err(WasapiStartError::ThreadSpawn)?;
        let mut thread = BoundedBackendThread::new(handle, completion_receiver);

        match wait_for_startup(&startup_receiver, startup_timeout) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread,
                counters,
                status,
                disconnect_reason,
                shutdown_timeout,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.wait(shutdown_timeout) {
                    BackendThreadJoin::Panicked => Err(WasapiStartError::ThreadPanicked),
                    BackendThreadJoin::Completed | BackendThreadJoin::TimedOut => Err(error),
                }
            }
        }
    }

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.counters.clone()
    }

    #[must_use]
    pub fn status(&self) -> OutputStreamStatus {
        self.status.clone()
    }

    /// Returns the asynchronous Windows session-disconnect reason, if one was
    /// published. A generic runtime failure can have no disconnect reason.
    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        decode_disconnect_reason(self.disconnect_reason.load(Ordering::Acquire))
    }

    /// Stops and joins the owned render thread.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStopError`] if the render thread panicked.
    pub fn stop(mut self) -> Result<(), WasapiStopError> {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<(), WasapiStopError> {
        match self.thread.wait(self.shutdown_timeout) {
            BackendThreadJoin::Completed => Ok(()),
            BackendThreadJoin::Panicked => Err(WasapiStopError::ThreadPanicked),
            BackendThreadJoin::TimedOut => Err(WasapiStopError::ShutdownTimeout),
        }
    }
}

impl Drop for WasapiOutputStream {
    fn drop(&mut self) {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.thread.detach();
    }
}

fn wait_for_startup(
    receiver: &Receiver<Result<(), WasapiStartError>>,
    timeout: Duration,
) -> Result<(), WasapiStartError> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(WasapiStartError::StartupTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(WasapiStartError::ThreadTerminated),
    }
}

#[derive(Clone, Copy)]
struct WasapiRuntimeSignals<'state> {
    stop_requested: &'state AtomicBool,
    startup: &'state SyncSender<Result<(), WasapiStartError>>,
    counters: &'state OutputStreamCounters,
    status: &'state OutputStreamStatus,
    disconnect_reason: &'state Arc<AtomicU8>,
}

fn run_wasapi_inner<P, C>(
    config: &WasapiOutputConfig,
    mut callback: OutputCallback<P>,
    signals: WasapiRuntimeSignals<'_>,
    mut commit: C,
) -> Result<(), WasapiStartError>
where
    P: InterleavedStereoProcessor,
    C: FnMut(),
{
    let _com = initialize_com()?;
    let mut output = open_render_output(config, signals.status, signals.disconnect_reason)?;
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
        &mut commit,
    )
    .map_err(|source| backend_error(WasapiStartupStage::Buffer, source))?;
    output
        .client
        .start_stream()
        .map_err(|source| backend_error(WasapiStartupStage::Start, source))?;
    let _started = StartedStream(&output.client);
    signals.status.mark_running();
    if signals.status.state() != OutputStreamState::Running {
        return Err(WasapiStartError::Backend {
            stage: WasapiStartupStage::State,
            detail: match decode_disconnect_reason(
                signals.disconnect_reason.load(Ordering::Acquire),
            ) {
                Some(reason) => format!("audio session disconnected during startup: {reason:?}"),
                None => "audio session failed during startup".to_owned(),
            },
        });
    }
    let _ = signals.startup.try_send(Ok(()));

    let mut consecutive_event_timeouts = 0_u8;
    while !signals.stop_requested.load(Ordering::Acquire)
        && signals.status.state() != OutputStreamState::Failed
    {
        if output
            .event
            .wait_for_event(EVENT_WAIT_MILLISECONDS)
            .is_err()
        {
            consecutive_event_timeouts = consecutive_event_timeouts.saturating_add(1);
            if consecutive_event_timeouts >= MAX_CONSECUTIVE_EVENT_TIMEOUTS {
                signals.status.mark_failed();
                signals.counters.record_stream_error();
                break;
            }
            continue;
        }
        consecutive_event_timeouts = 0;
        let Ok(available) = output.client.get_available_space_in_frames() else {
            signals.status.mark_failed();
            signals.counters.record_stream_error();
            break;
        };
        if write_frames(
            usize::try_from(available).unwrap_or(usize::MAX),
            config.maximum_block_frames,
            &mut output.samples,
            &mut callback,
            &output.render,
            &mut commit,
        )
        .is_err()
        {
            signals.status.mark_failed();
            signals.counters.record_stream_error();
            break;
        }
    }
    Ok(())
}

struct WasapiRenderOutput {
    _notifications: wasapi::EventRegistration,
    render: wasapi::AudioRenderClient,
    event: wasapi::Handle,
    samples: Vec<f32>,
    client: wasapi::AudioClient,
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

fn open_render_output(
    config: &WasapiOutputConfig,
    status: &OutputStreamStatus,
    disconnect_reason: &Arc<AtomicU8>,
) -> Result<WasapiRenderOutput, WasapiStartError> {
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
    let session_control = client
        .get_audiosessioncontrol()
        .map_err(|source| backend_error(WasapiStartupStage::SessionControl, source))?;
    let mut callbacks = EventCallbacks::new();
    let callback_status = status.clone();
    let callback_reason = Arc::clone(disconnect_reason);
    callbacks.set_disconnected_callback(move |reason| {
        publish_disconnect(&reason, &callback_status, &callback_reason);
    });
    let notifications = session_control
        .register_session_notification(callbacks)
        .map_err(|source| backend_error(WasapiStartupStage::Notifications, source))?;
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
        _notifications: notifications,
        render,
        event,
        samples,
        client,
    })
}

#[derive(Debug)]
enum WriteFramesError<E> {
    Callback(OutputCallbackStatus),
    Submission(E),
}

impl<E: fmt::Display> fmt::Display for WriteFramesError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Callback(status) => write!(formatter, "audio callback failed: {status:?}"),
            Self::Submission(source) => write!(formatter, "device submission failed: {source}"),
        }
    }
}

fn write_frames<P, C>(
    remaining_frames: usize,
    maximum_block_frames: usize,
    samples: &mut [f32],
    callback: &mut OutputCallback<P>,
    render: &wasapi::AudioRenderClient,
    commit: &mut C,
) -> Result<(), WriteFramesError<wasapi::WasapiError>>
where
    P: InterleavedStereoProcessor,
    C: FnMut(),
{
    render_frames(
        remaining_frames,
        maximum_block_frames,
        samples,
        callback,
        |frames, bytes| render.write_to_device(frames, bytes, None),
        commit,
    )
}

fn render_frames<P, C, S, E>(
    mut remaining_frames: usize,
    maximum_block_frames: usize,
    samples: &mut [f32],
    callback: &mut OutputCallback<P>,
    mut submit: S,
    commit: &mut C,
) -> Result<(), WriteFramesError<E>>
where
    P: InterleavedStereoProcessor,
    C: FnMut(),
    S: FnMut(usize, &[u8]) -> Result<(), E>,
{
    while remaining_frames > 0 {
        let frames = remaining_frames.min(maximum_block_frames);
        let sample_count = frames * STEREO_CHANNELS;
        let block = &mut samples[..sample_count];
        let status = callback.process(block);
        if status != OutputCallbackStatus::Rendered {
            return Err(WriteFramesError::Callback(status));
        }
        submit(frames, bytemuck::cast_slice(block)).map_err(WriteFramesError::Submission)?;
        commit();
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

const fn map_disconnect_reason(reason: &DisconnectReason) -> WasapiDisconnectReason {
    match reason {
        DisconnectReason::DeviceRemoval => WasapiDisconnectReason::DeviceRemoval,
        DisconnectReason::ServerShutdown => WasapiDisconnectReason::AudioServiceStopped,
        DisconnectReason::FormatChanged => WasapiDisconnectReason::FormatChanged,
        DisconnectReason::SessionLogoff => WasapiDisconnectReason::SessionLogoff,
        DisconnectReason::SessionDisconnected => WasapiDisconnectReason::SessionDisconnected,
        DisconnectReason::ExclusiveModeOverride => WasapiDisconnectReason::ExclusiveModeOverride,
        DisconnectReason::Unknown => WasapiDisconnectReason::Unknown,
    }
}

const fn decode_disconnect_reason(value: u8) -> Option<WasapiDisconnectReason> {
    match value {
        value if value == WasapiDisconnectReason::DeviceRemoval as u8 => {
            Some(WasapiDisconnectReason::DeviceRemoval)
        }
        value if value == WasapiDisconnectReason::AudioServiceStopped as u8 => {
            Some(WasapiDisconnectReason::AudioServiceStopped)
        }
        value if value == WasapiDisconnectReason::FormatChanged as u8 => {
            Some(WasapiDisconnectReason::FormatChanged)
        }
        value if value == WasapiDisconnectReason::SessionLogoff as u8 => {
            Some(WasapiDisconnectReason::SessionLogoff)
        }
        value if value == WasapiDisconnectReason::SessionDisconnected as u8 => {
            Some(WasapiDisconnectReason::SessionDisconnected)
        }
        value if value == WasapiDisconnectReason::ExclusiveModeOverride as u8 => {
            Some(WasapiDisconnectReason::ExclusiveModeOverride)
        }
        value if value == WasapiDisconnectReason::Unknown as u8 => {
            Some(WasapiDisconnectReason::Unknown)
        }
        _ => None,
    }
}

fn publish_disconnect(
    reason: &DisconnectReason,
    status: &OutputStreamStatus,
    destination: &AtomicU8,
) {
    destination.store(map_disconnect_reason(reason) as u8, Ordering::Release);
    status.mark_failed();
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
    use std::process::{Command, Stdio};
    use std::time::Instant;

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
    fn uncooperative_initializer_cannot_extend_startup_deadline() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let started = Instant::now();

        let result = WasapiOutputStream::start_with_thread_initializer_timeouts(
            config,
            move |_| {
                release_receiver.recv().unwrap();
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );

        assert!(matches!(result, Err(WasapiStartError::StartupTimeout)));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender.send(()).unwrap();
    }

    #[test]
    fn uncooperative_shutdown_returns_a_timeout() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let stream = WasapiOutputStream::start_with_thread_initializer_timeouts(
            config,
            move |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                release_receiver.recv().unwrap();
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();
        let started = Instant::now();

        let result = stream.stop();

        assert_eq!(result, Err(WasapiStopError::ShutdownTimeout));
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender.send(()).unwrap();
    }

    #[test]
    fn uncooperative_lifecycle_is_bounded_in_a_subprocess() {
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "wasapi_output::tests::uncooperative_lifecycle_subprocess_fixture",
            ])
            .env("HYPER_UNCOOPERATIVE_LIFECYCLE_FIXTURE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("uncooperative lifecycle subprocess exceeded its deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn uncooperative_lifecycle_subprocess_fixture() {
        if std::env::var_os("HYPER_UNCOOPERATIVE_LIFECYCLE_FIXTURE").is_none() {
            return;
        }
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let startup = WasapiOutputStream::start_with_thread_initializer_timeouts(
            config.clone(),
            |_| loop {
                thread::park();
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        assert!(matches!(startup, Err(WasapiStartError::StartupTimeout)));

        let stream = WasapiOutputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                loop {
                    thread::park();
                }
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();
        assert_eq!(stream.stop(), Err(WasapiStopError::ShutdownTimeout));
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

    #[test]
    fn recovery_commit_follows_successful_device_submission() {
        let mut callback = OutputCallback::try_new(
            |output: &mut [f32]| {
                output.fill(0.25);
                Ok::<_, ()>(())
            },
            2,
        )
        .unwrap();
        let mut samples = [0.0_f32; 4];
        let mut submissions = 0;
        let mut commits = 0;

        let result = render_frames(
            2,
            1,
            &mut samples,
            &mut callback,
            |_, _| {
                submissions += 1;
                if submissions == 2 {
                    Err("submission rejected")
                } else {
                    Ok(())
                }
            },
            &mut || commits += 1,
        );

        assert!(matches!(
            result,
            Err(WriteFramesError::Submission("submission rejected"))
        ));
        assert_eq!(submissions, 2);
        assert_eq!(commits, 1);
    }

    #[test]
    fn failed_callback_is_not_submitted_or_committed() {
        let mut callback =
            OutputCallback::try_new(|_: &mut [f32]| Err::<(), _>("render failed"), 1).unwrap();
        let mut samples = [1.0_f32; 2];
        let mut submissions = 0;
        let mut commits = 0;

        let result = render_frames(
            1,
            1,
            &mut samples,
            &mut callback,
            |_, _| {
                submissions += 1;
                Ok::<_, ()>(())
            },
            &mut || commits += 1,
        );

        assert!(matches!(
            result,
            Err(WriteFramesError::Callback(
                OutputCallbackStatus::ProcessorFailed
            ))
        ));
        assert_eq!(submissions, 0);
        assert_eq!(commits, 0);
    }

    #[test]
    fn every_wasapi_disconnect_reason_has_a_stable_public_mapping() {
        let mappings = [
            (
                DisconnectReason::DeviceRemoval,
                WasapiDisconnectReason::DeviceRemoval,
            ),
            (
                DisconnectReason::ServerShutdown,
                WasapiDisconnectReason::AudioServiceStopped,
            ),
            (
                DisconnectReason::FormatChanged,
                WasapiDisconnectReason::FormatChanged,
            ),
            (
                DisconnectReason::SessionLogoff,
                WasapiDisconnectReason::SessionLogoff,
            ),
            (
                DisconnectReason::SessionDisconnected,
                WasapiDisconnectReason::SessionDisconnected,
            ),
            (
                DisconnectReason::ExclusiveModeOverride,
                WasapiDisconnectReason::ExclusiveModeOverride,
            ),
            (DisconnectReason::Unknown, WasapiDisconnectReason::Unknown),
        ];

        for (source, expected) in mappings {
            assert_eq!(map_disconnect_reason(&source), expected);
            assert_eq!(decode_disconnect_reason(expected as u8), Some(expected));
        }
        assert_eq!(decode_disconnect_reason(0), None);
    }

    #[test]
    fn disconnect_reason_is_published_before_terminal_failure() {
        let status = OutputStreamStatus::default();
        let reason = AtomicU8::new(0);
        status.mark_running();

        publish_disconnect(&DisconnectReason::DeviceRemoval, &status, &reason);

        assert_eq!(status.state(), OutputStreamState::Failed);
        assert_eq!(
            decode_disconnect_reason(reason.load(Ordering::Acquire)),
            Some(WasapiDisconnectReason::DeviceRemoval)
        );
    }

    #[test]
    fn owned_stream_publishes_requested_shutdown_completion() {
        let config = WasapiOutputConfig::try_new(48_000, 64).unwrap();
        let stream = WasapiOutputStream::start_with_thread_initializer(config, |runner| {
            runner.status.mark_running();
            let _ = runner.startup.try_send(Ok(()));
            while !runner.stop_requested() {
                thread::yield_now();
            }
            Ok(())
        })
        .unwrap();
        let status = stream.status();

        assert_eq!(status.state(), OutputStreamState::Running);
        stream.stop().unwrap();
        assert_eq!(status.state(), OutputStreamState::Stopped);
    }
}
