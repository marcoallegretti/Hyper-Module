use super::wasapi_output::{decode_disconnect_reason, publish_disconnect};
use super::{
    BackendThreadCompletion, BackendThreadJoin, BoundedBackendThread, CaptureCallback,
    CaptureCallbackStatus, CaptureStreamCounters, InterleavedStereoCaptureProcessor,
    PendingNativeShutdown, STEREO_CHANNELS, StreamState, StreamStatus, WasapiDisconnectReason,
    WasapiInputEndpoint, WasapiStartError, WasapiStartupStage, WasapiStopError,
    acquire_backend_thread_permit,
};
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Duration;
use wasapi::{Direction, EventCallbacks, SampleType, ShareMode, StreamMode, WaveFormat};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const EVENT_WAIT_MILLISECONDS: u32 = 100;
const MAX_CONSECUTIVE_EVENT_TIMEOUTS: u8 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasapiInputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    device_id: Option<String>,
}

impl WasapiInputConfig {
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

    /// Creates a strict shared-mode stereo `f32` capture configuration.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiInputConfigError`] for a zero sample rate or invalid
    /// callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, WasapiInputConfigError> {
        if sample_rate_hz == 0 {
            return Err(WasapiInputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(WasapiInputConfigError::InvalidMaximumBlockFrames {
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

    /// Selects an endpoint identifier instead of the default capture endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiInputConfigError`] for an empty identifier or embedded
    /// NUL.
    pub fn with_device_id(mut self, device_id: &str) -> Result<Self, WasapiInputConfigError> {
        if device_id.is_empty() {
            return Err(WasapiInputConfigError::EmptyDeviceId);
        }
        if device_id.as_bytes().contains(&0) {
            return Err(WasapiInputConfigError::DeviceIdContainsNul);
        }
        self.device_id = Some(device_id.to_owned());
        Ok(self)
    }

    /// Selects one endpoint returned by input discovery.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiInputConfigError`] if its routing identity is invalid.
    pub fn with_input_target(
        self,
        target: &WasapiInputEndpoint,
    ) -> Result<Self, WasapiInputConfigError> {
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
pub enum WasapiInputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyDeviceId,
    DeviceIdContainsNul,
}

impl fmt::Display for WasapiInputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("WASAPI input sample rate must be nonzero"),
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "WASAPI input maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyDeviceId => {
                formatter.write_str("WASAPI input device identifier must not be empty")
            }
            Self::DeviceIdContainsNul => {
                formatter.write_str("WASAPI input device identifier contains an embedded NUL")
            }
        }
    }
}

impl Error for WasapiInputConfigError {}

#[derive(Debug)]
pub struct WasapiInputStream {
    stop_requested: Arc<AtomicBool>,
    thread: Option<BoundedBackendThread>,
    counters: CaptureStreamCounters,
    status: StreamStatus,
    disconnect_reason: Arc<AtomicU8>,
    shutdown_timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct WasapiInputThreadRunner {
    config: WasapiInputConfig,
    stop_requested: Arc<AtomicBool>,
    startup: SyncSender<Result<(), WasapiStartError>>,
    counters: CaptureStreamCounters,
    status: StreamStatus,
    disconnect_reason: Arc<AtomicU8>,
}

impl WasapiInputThreadRunner {
    fn run<P>(self, processor: P) -> Result<(), WasapiStartError>
    where
        P: InterleavedStereoCaptureProcessor,
    {
        let callback = CaptureCallback::try_new_with_counters(
            processor,
            self.config.maximum_block_frames,
            self.counters.clone(),
        )
        .map_err(|source| WasapiStartError::Backend {
            stage: WasapiStartupStage::Callback,
            detail: source.to_string(),
        })?;
        run_wasapi_input(
            &self.config,
            callback,
            WasapiInputSignals {
                stop_requested: &self.stop_requested,
                startup: &self.startup,
                counters: &self.counters,
                status: &self.status,
                disconnect_reason: &self.disconnect_reason,
            },
        )
    }
}

impl WasapiInputStream {
    /// Starts a strict event-driven shared-mode stereo capture stream.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStartError`] when the thread, endpoint, exact format,
    /// callback, or event-driven stream cannot start.
    pub fn start<P>(config: WasapiInputConfig, processor: P) -> Result<Self, WasapiStartError>
    where
        P: InterleavedStereoCaptureProcessor + Send + 'static,
    {
        Self::start_with_thread_initializer(config, move |runner| runner.run(processor))
    }

    pub(crate) fn start_with_thread_initializer<F>(
        config: WasapiInputConfig,
        initialize: F,
    ) -> Result<Self, WasapiStartError>
    where
        F: FnOnce(WasapiInputThreadRunner) -> Result<(), WasapiStartError> + Send + 'static,
    {
        Self::start_with_thread_initializer_timeouts(
            config,
            initialize,
            STARTUP_TIMEOUT,
            SHUTDOWN_TIMEOUT,
        )
    }

    fn start_with_thread_initializer_timeouts<F>(
        config: WasapiInputConfig,
        initialize: F,
        startup_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<Self, WasapiStartError>
    where
        F: FnOnce(WasapiInputThreadRunner) -> Result<(), WasapiStartError> + Send + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let counters = CaptureStreamCounters::default();
        let status = StreamStatus::default();
        let disconnect_reason = Arc::new(AtomicU8::new(0));
        let thread_stop = Arc::clone(&stop_requested);
        let thread_counters = counters.clone();
        let thread_status = status.clone();
        let thread_disconnect = Arc::clone(&disconnect_reason);
        let completion_status = status.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let failure_sender = startup_sender.clone();
        let (completion_sender, completion_receiver) = sync_channel(1);
        let completion_thread_status = status.clone();
        let shutdown_permit =
            acquire_backend_thread_permit().map_err(WasapiStartError::ThreadSpawn)?;
        let handle = thread::Builder::new()
            .name("hyper-wasapi-input".to_owned())
            .spawn(move || {
                let _completion = BackendThreadCompletion {
                    sender: completion_sender,
                    status: completion_thread_status,
                };
                let runner = WasapiInputThreadRunner {
                    config,
                    stop_requested: thread_stop,
                    startup: startup_sender,
                    counters: thread_counters,
                    status: thread_status,
                    disconnect_reason: thread_disconnect,
                };
                if let Err(error) = initialize(runner) {
                    completion_status.mark_failed();
                    let _ = failure_sender.try_send(Err(error));
                } else {
                    completion_status.mark_stopped();
                }
            })
            .map_err(WasapiStartError::ThreadSpawn)?;
        let mut thread = BoundedBackendThread::new(handle, completion_receiver, shutdown_permit);

        match wait_for_startup(&startup_receiver, startup_timeout) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread: Some(thread),
                counters,
                status,
                disconnect_reason,
                shutdown_timeout,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.wait(shutdown_timeout) {
                    BackendThreadJoin::Panicked => Err(WasapiStartError::ThreadPanicked),
                    BackendThreadJoin::Completed => Err(error),
                    BackendThreadJoin::TimedOut => Err(WasapiStartError::CancellationPending {
                        shutdown: Box::new(PendingNativeShutdown::new(thread, status)),
                    }),
                }
            }
        }
    }

    #[must_use]
    pub fn counters(&self) -> CaptureStreamCounters {
        self.counters.clone()
    }

    #[must_use]
    pub fn status(&self) -> StreamStatus {
        self.status.clone()
    }

    #[must_use]
    pub fn disconnect_reason(&self) -> Option<WasapiDisconnectReason> {
        decode_disconnect_reason(self.disconnect_reason.load(Ordering::Acquire))
    }

    /// Stops and joins the owned capture thread.
    ///
    /// # Errors
    ///
    /// Returns [`WasapiStopError`] if the thread panicked or did not terminate
    /// before the bounded shutdown deadline.
    pub fn stop(mut self) -> Result<(), WasapiStopError> {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<(), WasapiStopError> {
        let result = self
            .thread
            .as_mut()
            .expect("a running WASAPI input owns its backend thread")
            .wait(self.shutdown_timeout);
        match result {
            BackendThreadJoin::Completed if self.status.state() == StreamState::Failed => {
                Err(WasapiStopError::RuntimeFailed)
            }
            BackendThreadJoin::Completed => Ok(()),
            BackendThreadJoin::Panicked => Err(WasapiStopError::ThreadPanicked),
            BackendThreadJoin::TimedOut => Err(WasapiStopError::ShutdownPending {
                shutdown: Box::new(PendingNativeShutdown::new(
                    self.thread
                        .take()
                        .expect("a timed-out WASAPI input retains its backend thread"),
                    self.status.clone(),
                )),
            }),
        }
    }
}

impl Drop for WasapiInputStream {
    fn drop(&mut self) {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
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
struct WasapiInputSignals<'state> {
    stop_requested: &'state AtomicBool,
    startup: &'state SyncSender<Result<(), WasapiStartError>>,
    counters: &'state CaptureStreamCounters,
    status: &'state StreamStatus,
    disconnect_reason: &'state Arc<AtomicU8>,
}

fn run_wasapi_input<P>(
    config: &WasapiInputConfig,
    mut callback: CaptureCallback<P>,
    signals: WasapiInputSignals<'_>,
) -> Result<(), WasapiStartError>
where
    P: InterleavedStereoCaptureProcessor,
{
    let _com = initialize_com()?;
    let mut input = open_capture_input(config, signals.status, signals.disconnect_reason)?;
    input
        .client
        .start_stream()
        .map_err(|source| backend_error(WasapiStartupStage::Start, source))?;
    let _started = StartedStream(&input.client);
    signals.status.mark_running();
    if signals.status.state() != StreamState::Running {
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
        && signals.status.state() != StreamState::Failed
    {
        if input.event.wait_for_event(EVENT_WAIT_MILLISECONDS).is_err() {
            consecutive_event_timeouts = consecutive_event_timeouts.saturating_add(1);
            if consecutive_event_timeouts >= MAX_CONSECUTIVE_EVENT_TIMEOUTS {
                signals.status.mark_failed();
                signals.counters.record_stream_error();
                break;
            }
            continue;
        }
        consecutive_event_timeouts = 0;
        if drain_capture_packets(
            &input.capture,
            &mut input.samples,
            &mut callback,
            signals.counters,
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

struct WasapiCaptureInput {
    _notifications: wasapi::EventRegistration,
    capture: wasapi::AudioCaptureClient,
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

fn open_capture_input(
    config: &WasapiInputConfig,
    status: &StreamStatus,
    disconnect_reason: &Arc<AtomicU8>,
) -> Result<WasapiCaptureInput, WasapiStartError> {
    let enumerator = wasapi::DeviceEnumerator::new()
        .map_err(|source| backend_error(WasapiStartupStage::Enumerator, source))?;
    let device = match &config.device_id {
        Some(device_id) => enumerator
            .get_device(device_id)
            .map_err(|source| backend_error(WasapiStartupStage::Device, source))?,
        None => enumerator
            .get_default_device(&Direction::Capture)
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
            &Direction::Capture,
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
    let capture = client
        .get_audiocaptureclient()
        .map_err(|source| backend_error(WasapiStartupStage::CaptureClient, source))?;
    let buffer_frames = usize::try_from(
        client
            .get_buffer_size()
            .map_err(|source| backend_error(WasapiStartupStage::Buffer, source))?,
    )
    .map_err(|_| WasapiStartError::Backend {
        stage: WasapiStartupStage::Buffer,
        detail: "capture buffer size does not fit the platform address space".to_owned(),
    })?;
    let sample_capacity =
        buffer_frames
            .checked_mul(STEREO_CHANNELS)
            .ok_or_else(|| WasapiStartError::Backend {
                stage: WasapiStartupStage::Buffer,
                detail: "capture sample capacity overflowed".to_owned(),
            })?;
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_capacity)
        .map_err(|_| WasapiStartError::Backend {
            stage: WasapiStartupStage::Buffer,
            detail: "capture buffer capacity is unavailable".to_owned(),
        })?;
    samples.resize(sample_capacity, 0.0);

    Ok(WasapiCaptureInput {
        _notifications: notifications,
        capture,
        event,
        samples,
        client,
    })
}

fn drain_capture_packets<P>(
    capture: &wasapi::AudioCaptureClient,
    samples: &mut [f32],
    callback: &mut CaptureCallback<P>,
    counters: &CaptureStreamCounters,
) -> Result<(), CapturePacketError<wasapi::WasapiError>>
where
    P: InterleavedStereoCaptureProcessor,
{
    loop {
        let Some(packet_frames) = capture
            .get_next_packet_size()
            .map_err(CapturePacketError::Device)?
        else {
            return Err(CapturePacketError::ExclusiveMode);
        };
        if packet_frames == 0 {
            return Ok(());
        }
        let packet_frames = usize::try_from(packet_frames).unwrap_or(usize::MAX);
        let packet_samples = packet_frames
            .checked_mul(STEREO_CHANNELS)
            .ok_or(CapturePacketError::MalformedPacket)?;
        if packet_samples > samples.len() {
            counters.record_malformed_buffer(packet_frames);
            return Err(CapturePacketError::MalformedPacket);
        }
        let packet = &mut samples[..packet_samples];
        let (frames_read, info) = capture
            .read_from_device(bytemuck::cast_slice_mut(packet))
            .map_err(CapturePacketError::Device)?;
        let frames_read = usize::try_from(frames_read).unwrap_or(usize::MAX);
        if frames_read != packet_frames {
            counters.record_malformed_buffer(frames_read);
            return Err(CapturePacketError::MalformedPacket);
        }
        if info.flags.data_discontinuity {
            counters.record_discontinuity();
        }
        if info.flags.timestamp_error {
            counters.record_timestamp_error();
        }
        if info.flags.silent {
            packet.fill(0.0);
        }
        process_samples(packet, callback)?;
    }
}

fn process_samples<P>(
    mut samples: &[f32],
    callback: &mut CaptureCallback<P>,
) -> Result<(), CapturePacketError<wasapi::WasapiError>>
where
    P: InterleavedStereoCaptureProcessor,
{
    let maximum_samples = callback.maximum_block_frames() * STEREO_CHANNELS;
    while !samples.is_empty() {
        let block_samples = samples.len().min(maximum_samples);
        let block = &samples[..block_samples];
        match callback.process(block) {
            CaptureCallbackStatus::Captured | CaptureCallbackStatus::NonFiniteInput => {}
            status => return Err(CapturePacketError::Callback(status)),
        }
        samples = &samples[block_samples..];
    }
    Ok(())
}

#[derive(Debug)]
enum CapturePacketError<E> {
    Callback(CaptureCallbackStatus),
    Device(E),
    ExclusiveMode,
    MalformedPacket,
}

impl<E: fmt::Display> fmt::Display for CapturePacketError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Callback(status) => write!(formatter, "audio callback failed: {status:?}"),
            Self::Device(source) => write!(formatter, "device capture failed: {source}"),
            Self::ExclusiveMode => {
                formatter.write_str("shared capture client reported exclusive-mode packet sizing")
            }
            Self::MalformedPacket => formatter.write_str("capture packet is malformed"),
        }
    }
}

impl<E: Error + 'static> Error for CapturePacketError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Device(source) => Some(source),
            Self::Callback(_) | Self::ExclusiveMode | Self::MalformedPacket => None,
        }
    }
}

fn backend_error(
    stage: WasapiStartupStage,
    source: impl Error + Send + Sync + 'static,
) -> WasapiStartError {
    WasapiStartError::BackendSource {
        stage,
        source: Box::new(source),
    }
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
    use crate::PendingNativeShutdownWait;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    const TEST_STARTUP_TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn config_rejects_invalid_domains() {
        assert_eq!(
            WasapiInputConfig::try_new(0, 64),
            Err(WasapiInputConfigError::ZeroSampleRate)
        );
        assert!(matches!(
            WasapiInputConfig::try_new(48_000, 0),
            Err(WasapiInputConfigError::InvalidMaximumBlockFrames { actual: 0, .. })
        ));
        assert_eq!(
            WasapiInputConfig::try_new(48_000, 64)
                .unwrap()
                .with_device_id(""),
            Err(WasapiInputConfigError::EmptyDeviceId)
        );
        assert_eq!(
            WasapiInputConfig::try_new(48_000, 64)
                .unwrap()
                .with_device_id("device\0id"),
            Err(WasapiInputConfigError::DeviceIdContainsNul)
        );
    }

    #[test]
    fn packets_are_split_without_losing_order() {
        let mut captured = Vec::new();
        let mut callback = CaptureCallback::try_new(
            |input: &[f32]| {
                captured.extend_from_slice(input);
                Ok::<_, ()>(())
            },
            2,
        )
        .unwrap();
        let counters = callback.counters();
        let input = [0.0, 1.0, 2.0, 3.0, 4.0, 5.0];

        process_samples(&input, &mut callback).unwrap();
        drop(callback);

        assert_eq!(captured, input);
        assert_eq!(counters.snapshot().captured_callback_count, 2);
    }

    #[test]
    fn thread_local_initializer_failure_is_reported_before_device_start() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiInputStream::start_with_thread_initializer(config, |_| {
            Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Callback,
                detail: "fixture rejected".to_owned(),
            })
        });

        assert!(matches!(
            result,
            Err(WasapiStartError::Backend {
                stage: WasapiStartupStage::Callback,
                detail
            }) if detail == "fixture rejected"
        ));
    }

    #[test]
    fn thread_local_initializer_must_run_its_context() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiInputStream::start_with_thread_initializer(config, |_| Ok(()));

        assert!(matches!(result, Err(WasapiStartError::ThreadTerminated)));
    }

    #[test]
    fn thread_local_initializer_panic_is_distinct() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let result = WasapiInputStream::start_with_thread_initializer(config, |_| {
            panic!("initializer panic");
        });

        assert!(matches!(result, Err(WasapiStartError::ThreadPanicked)));
    }

    #[test]
    fn uncooperative_initializer_retains_eventual_shutdown() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let started = Instant::now();

        let result = WasapiInputStream::start_with_thread_initializer_timeouts(
            config,
            move |_| {
                release_receiver.recv().unwrap();
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );

        let Err(WasapiStartError::CancellationPending { mut shutdown }) = result else {
            panic!("startup cancellation must retain native thread ownership");
        };
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            shutdown.wait(Duration::from_millis(1)),
            PendingNativeShutdownWait::TimedOut
        );
        release_sender.send(()).unwrap();
        assert_eq!(
            shutdown.wait(Duration::from_secs(1)),
            PendingNativeShutdownWait::Completed
        );
    }

    #[test]
    fn uncooperative_shutdown_retains_eventual_completion() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let stream = WasapiInputStream::start_with_thread_initializer_timeouts(
            config,
            move |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                release_receiver.recv().unwrap();
                Ok(())
            },
            TEST_STARTUP_TIMEOUT,
            Duration::from_millis(20),
        )
        .unwrap();
        let started = Instant::now();

        let Err(WasapiStopError::ShutdownPending { mut shutdown }) = stream.stop() else {
            panic!("shutdown timeout must retain native thread ownership");
        };
        assert!(started.elapsed() < Duration::from_secs(1));
        release_sender.send(()).unwrap();
        assert_eq!(
            shutdown.wait(Duration::from_secs(1)),
            PendingNativeShutdownWait::Completed
        );
    }

    #[test]
    fn completed_runtime_failure_is_not_reported_as_clean_shutdown() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let stream = WasapiInputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                runner.status.mark_failed();
                Ok(())
            },
            TEST_STARTUP_TIMEOUT,
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(matches!(stream.stop(), Err(WasapiStopError::RuntimeFailed)));
    }

    #[test]
    fn runtime_panic_is_reported_during_stop() {
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let stream = WasapiInputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                panic!("runtime panic");
            },
            TEST_STARTUP_TIMEOUT,
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(matches!(
            stream.stop(),
            Err(WasapiStopError::ThreadPanicked)
        ));
    }

    #[test]
    fn uncooperative_lifecycle_is_bounded_in_a_subprocess() {
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "wasapi_input::tests::uncooperative_lifecycle_subprocess_fixture",
            ])
            .env("HYPER_UNCOOPERATIVE_WASAPI_INPUT_FIXTURE", "1")
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
                panic!("uncooperative input lifecycle subprocess exceeded its deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn uncooperative_lifecycle_subprocess_fixture() {
        if std::env::var_os("HYPER_UNCOOPERATIVE_WASAPI_INPUT_FIXTURE").is_none() {
            return;
        }
        let config = WasapiInputConfig::try_new(48_000, 64).unwrap();
        let startup = WasapiInputStream::start_with_thread_initializer_timeouts(
            config.clone(),
            |_| loop {
                thread::park();
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        assert!(matches!(
            startup,
            Err(WasapiStartError::CancellationPending { .. })
        ));

        let stream = WasapiInputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                loop {
                    thread::park();
                }
            },
            TEST_STARTUP_TIMEOUT,
            Duration::from_millis(20),
        )
        .unwrap();
        assert!(matches!(
            stream.stop(),
            Err(WasapiStopError::ShutdownPending { .. })
        ));
    }
}
