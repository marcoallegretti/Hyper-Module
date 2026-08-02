use super::{
    BackendThreadCompletion, BackendThreadJoin, BoundedBackendThread, CaptureCallback,
    CaptureCallbackStatus, CaptureStreamCounters, InterleavedStereoCaptureProcessor,
    PendingNativeShutdown, PipeWireInputTarget, PipeWireStartError, PipeWireStartupStage,
    PipeWireStopError, STEREO_CHANNELS, StreamState, StreamStatus, acquire_backend_thread_permit,
};
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread;
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FRAME_STRIDE: usize = STEREO_CHANNELS * size_of::<f32>();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeWireInputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    target: Option<String>,
}

impl PipeWireInputConfig {
    pub(crate) const fn from_desktop(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
        target: Option<String>,
    ) -> Self {
        Self {
            sample_rate_hz,
            maximum_block_frames,
            target,
        }
    }

    /// Creates a fixed-rate stereo `PipeWire` capture configuration.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireInputConfigError`] for a zero sample rate or invalid
    /// callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, PipeWireInputConfigError> {
        if sample_rate_hz == 0 {
            return Err(PipeWireInputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(PipeWireInputConfigError::InvalidMaximumBlockFrames {
                actual: maximum_block_frames,
                maximum: super::MAXIMUM_BLOCK_FRAMES,
            });
        }
        Ok(Self {
            sample_rate_hz,
            maximum_block_frames,
            target: None,
        })
    }

    /// Pins capture to one `PipeWire` source instead of the policy default.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireInputConfigError`] for an empty target or embedded
    /// NUL.
    pub fn with_target(mut self, target: &str) -> Result<Self, PipeWireInputConfigError> {
        if target.is_empty() {
            return Err(PipeWireInputConfigError::EmptyTarget);
        }
        if target.as_bytes().contains(&0) {
            return Err(PipeWireInputConfigError::TargetContainsNul);
        }
        self.target = Some(target.to_owned());
        Ok(self)
    }

    /// Selects one source returned by input discovery.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireInputConfigError`] if its routing identity is invalid.
    pub fn with_input_target(
        self,
        target: &PipeWireInputTarget,
    ) -> Result<Self, PipeWireInputConfigError> {
        self.with_target(target.routing_id())
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
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeWireInputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyTarget,
    TargetContainsNul,
}

impl fmt::Display for PipeWireInputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => {
                formatter.write_str("PipeWire input sample rate must be nonzero")
            }
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "PipeWire input maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyTarget => formatter.write_str("PipeWire input target must not be empty"),
            Self::TargetContainsNul => {
                formatter.write_str("PipeWire input target contains an embedded NUL")
            }
        }
    }
}

impl Error for PipeWireInputConfigError {}

#[derive(Debug)]
pub struct PipeWireInputStream {
    stop_requested: Arc<AtomicBool>,
    thread: Option<BoundedBackendThread>,
    counters: CaptureStreamCounters,
    status: StreamStatus,
    shutdown_timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct PipeWireInputThreadRunner {
    config: PipeWireInputConfig,
    stop_requested: Arc<AtomicBool>,
    startup: SyncSender<Result<(), PipeWireStartError>>,
    counters: CaptureStreamCounters,
    status: StreamStatus,
}

impl PipeWireInputThreadRunner {
    fn run<P>(self, processor: P) -> Result<(), PipeWireStartError>
    where
        P: InterleavedStereoCaptureProcessor + 'static,
    {
        let callback = CaptureCallback::try_new_with_counters(
            processor,
            self.config.maximum_block_frames,
            self.counters,
        )
        .map_err(|source| PipeWireStartError::Backend {
            stage: PipeWireStartupStage::Callback,
            detail: source.to_string(),
        })?;
        run_pipewire_input(
            self.config,
            callback,
            self.stop_requested,
            &self.startup,
            &self.status,
        )
    }
}

impl PipeWireInputStream {
    /// Connects a stereo `f32` input node and owns its `PipeWire` loop thread.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStartError`] when the thread, `PipeWire` objects,
    /// format, connection, or initial stream state cannot be established.
    pub fn start<P>(config: PipeWireInputConfig, processor: P) -> Result<Self, PipeWireStartError>
    where
        P: InterleavedStereoCaptureProcessor + Send + 'static,
    {
        Self::start_with_thread_initializer(config, move |runner| runner.run(processor))
    }

    pub(crate) fn start_with_thread_initializer<F>(
        config: PipeWireInputConfig,
        initialize: F,
    ) -> Result<Self, PipeWireStartError>
    where
        F: FnOnce(PipeWireInputThreadRunner) -> Result<(), PipeWireStartError> + Send + 'static,
    {
        Self::start_with_thread_initializer_timeouts(
            config,
            initialize,
            STARTUP_TIMEOUT,
            SHUTDOWN_TIMEOUT,
        )
    }

    fn start_with_thread_initializer_timeouts<F>(
        config: PipeWireInputConfig,
        initialize: F,
        startup_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<Self, PipeWireStartError>
    where
        F: FnOnce(PipeWireInputThreadRunner) -> Result<(), PipeWireStartError> + Send + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let counters = CaptureStreamCounters::default();
        let status = StreamStatus::default();
        let thread_stop = Arc::clone(&stop_requested);
        let thread_counters = counters.clone();
        let thread_status = status.clone();
        let completion_status = status.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let failure_sender = startup_sender.clone();
        let (completion_sender, completion_receiver) = sync_channel(1);
        let completion_thread_status = status.clone();
        let shutdown_permit =
            acquire_backend_thread_permit().map_err(PipeWireStartError::ThreadSpawn)?;
        let handle = thread::Builder::new()
            .name("hyper-pipewire-input".to_owned())
            .spawn(move || {
                let _completion = BackendThreadCompletion {
                    sender: completion_sender,
                    status: completion_thread_status,
                };
                let runner = PipeWireInputThreadRunner {
                    config,
                    stop_requested: thread_stop,
                    startup: startup_sender,
                    counters: thread_counters,
                    status: thread_status,
                };
                if let Err(error) = initialize(runner) {
                    completion_status.mark_failed();
                    let _ = failure_sender.try_send(Err(error));
                } else {
                    completion_status.mark_stopped();
                }
            })
            .map_err(PipeWireStartError::ThreadSpawn)?;
        let mut thread = BoundedBackendThread::new(handle, completion_receiver, shutdown_permit);

        match wait_for_startup(&startup_receiver, startup_timeout) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread: Some(thread),
                counters,
                status,
                shutdown_timeout,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.wait(shutdown_timeout) {
                    BackendThreadJoin::Panicked => Err(PipeWireStartError::ThreadPanicked),
                    BackendThreadJoin::Completed => Err(error),
                    BackendThreadJoin::TimedOut => Err(PipeWireStartError::CancellationPending {
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

    /// Requests loop shutdown and joins the owned backend thread.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStopError`] if the backend thread panicked or did not
    /// terminate before the bounded shutdown deadline.
    pub fn stop(mut self) -> Result<(), PipeWireStopError> {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<(), PipeWireStopError> {
        let result = self
            .thread
            .as_mut()
            .expect("a running PipeWire input owns its backend thread")
            .wait(self.shutdown_timeout);
        match result {
            BackendThreadJoin::Completed if self.status.state() == StreamState::Failed => {
                Err(PipeWireStopError::RuntimeFailed)
            }
            BackendThreadJoin::Completed => Ok(()),
            BackendThreadJoin::Panicked => Err(PipeWireStopError::ThreadPanicked),
            BackendThreadJoin::TimedOut => Err(PipeWireStopError::ShutdownPending {
                shutdown: Box::new(PendingNativeShutdown::new(
                    self.thread
                        .take()
                        .expect("a timed-out PipeWire input retains its backend thread"),
                    self.status.clone(),
                )),
            }),
        }
    }
}

impl Drop for PipeWireInputStream {
    fn drop(&mut self) {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
    }
}

fn wait_for_startup(
    receiver: &Receiver<Result<(), PipeWireStartError>>,
    timeout: Duration,
) -> Result<(), PipeWireStartError> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(PipeWireStartError::StartupTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(PipeWireStartError::ThreadTerminated),
    }
}

struct PipeWireInputCallback<P> {
    input: CaptureCallback<P>,
    startup: Option<SyncSender<Result<(), PipeWireStartError>>>,
    main_loop: pw::main_loop::MainLoopRc,
    status: StreamStatus,
}

fn run_pipewire_input<P>(
    config: PipeWireInputConfig,
    callback: CaptureCallback<P>,
    stop_requested: Arc<AtomicBool>,
    startup: &SyncSender<Result<(), PipeWireStartError>>,
    status: &StreamStatus,
) -> Result<(), PipeWireStartError>
where
    P: InterleavedStereoCaptureProcessor + 'static,
{
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|source| backend_error(PipeWireStartupStage::MainLoop, source))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|source| backend_error(PipeWireStartupStage::Context, source))?;
    let core = context
        .connect_rc(None)
        .map_err(|source| backend_error(PipeWireStartupStage::Core, source))?;

    let latency = format!("{}/{}", config.maximum_block_frames, config.sample_rate_hz);
    let mut stream_properties = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "Hyper Module",
        *pw::keys::NODE_NAME => "hyper-module-input",
        *pw::keys::NODE_DESCRIPTION => "Hyper Module Stereo Input",
        *pw::keys::NODE_LATENCY => latency,
        *pw::keys::AUDIO_CHANNELS => "2",
    };
    let target_is_pinned = config.target.is_some();
    if let Some(target) = config.target {
        stream_properties.insert("target.object", target);
        stream_properties.insert("node.dont-fallback", "true");
        stream_properties.insert("node.dont-move", "true");
        stream_properties.insert("node.dont-reconnect", "true");
    }
    let stream = pw::stream::StreamBox::new(&core, "hyper-module-input", stream_properties)
        .map_err(|source| backend_error(PipeWireStartupStage::Stream, source))?;

    let loop_for_state = main_loop.clone();
    let counters = callback.counters();
    let state_status = status.clone();
    let listener = stream
        .add_local_listener_with_user_data(PipeWireInputCallback {
            input: callback,
            startup: Some(startup.clone()),
            main_loop: main_loop.clone(),
            status: status.clone(),
        })
        .state_changed(move |_, state, _, new| match new {
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                state_status.mark_running();
                if let Some(sender) = state.startup.take() {
                    let _ = sender.try_send(Ok(()));
                }
            }
            pw::stream::StreamState::Error(detail) => {
                state_status.mark_failed();
                counters.record_stream_error();
                if let Some(sender) = state.startup.take() {
                    let _ = sender.try_send(Err(PipeWireStartError::Backend {
                        stage: PipeWireStartupStage::State,
                        detail,
                    }));
                }
                loop_for_state.quit();
            }
            pw::stream::StreamState::Unconnected => {
                if state_status.state() == StreamState::Running {
                    state_status.mark_failed();
                    counters.record_stream_error();
                    loop_for_state.quit();
                }
            }
            pw::stream::StreamState::Connecting => {}
        })
        .process_with_clock(process_pipewire_input)
        .register()
        .map_err(|source| backend_error(PipeWireStartupStage::Listener, source))?;

    let loop_for_stop = main_loop.clone();
    let stop_timer = main_loop.loop_().add_timer(move |_| {
        if stop_requested.load(Ordering::Acquire) {
            loop_for_stop.quit();
        }
    });
    stop_timer
        .update_timer(Some(STOP_POLL_INTERVAL), Some(STOP_POLL_INTERVAL))
        .into_result()
        .map_err(|source| backend_error(PipeWireStartupStage::MainLoop, source))?;

    let format = build_format(config.sample_rate_hz)?;
    let pod = Pod::from_bytes(&format).ok_or_else(|| PipeWireStartError::Backend {
        stage: PipeWireStartupStage::Format,
        detail: "serialized input format is not a valid SPA pod".to_owned(),
    })?;
    let mut stream_flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;
    if target_is_pinned {
        stream_flags |= pw::stream::StreamFlags::DONT_RECONNECT;
    }
    stream
        .connect(spa::utils::Direction::Input, None, stream_flags, &mut [pod])
        .map_err(|source| backend_error(PipeWireStartupStage::Connect, source))?;

    main_loop.run();
    drop(stop_timer);
    drop(listener);
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PreparedPipeWireInput {
    Continue,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PipeWireInputSubmission {
    Continued,
    Terminal,
    QueueRejected,
}

fn submit_pipewire_input<E, Q>(prepared: PreparedPipeWireInput, queue: Q) -> PipeWireInputSubmission
where
    Q: FnOnce() -> Result<(), E>,
{
    if queue().is_err() {
        return PipeWireInputSubmission::QueueRejected;
    }
    match prepared {
        PreparedPipeWireInput::Continue => PipeWireInputSubmission::Continued,
        PreparedPipeWireInput::Terminal => PipeWireInputSubmission::Terminal,
    }
}

fn prepare_pipewire_input<P>(
    datas: &mut [spa::buffer::Data],
    state: &mut PipeWireInputCallback<P>,
) -> PreparedPipeWireInput
where
    P: InterleavedStereoCaptureProcessor,
{
    let Some(data) = datas.first_mut() else {
        state.input.counters().record_malformed_buffer(0);
        return PreparedPipeWireInput::Continue;
    };
    let (offset, size, stride) = {
        let chunk = data.chunk();
        (
            usize::try_from(chunk.offset()).unwrap_or(usize::MAX),
            usize::try_from(chunk.size()).unwrap_or(usize::MAX),
            chunk.stride(),
        )
    };
    let Some(bytes) = data.data() else {
        state.input.counters().record_malformed_buffer(0);
        return PreparedPipeWireInput::Continue;
    };
    let Some(end) = offset.checked_add(size) else {
        state.input.counters().record_malformed_buffer(0);
        return PreparedPipeWireInput::Continue;
    };
    let frames = size / FRAME_STRIDE;
    if size == 0 {
        return PreparedPipeWireInput::Continue;
    }
    if end > bytes.len()
        || size % FRAME_STRIDE != 0
        || (stride != 0 && usize::try_from(stride).ok() != Some(FRAME_STRIDE))
    {
        state.input.counters().record_malformed_buffer(frames);
        return PreparedPipeWireInput::Continue;
    }
    let Ok(samples) = bytemuck::try_cast_slice::<u8, f32>(&bytes[offset..end]) else {
        state.input.counters().record_malformed_buffer(frames);
        return PreparedPipeWireInput::Continue;
    };

    match state.input.process(samples) {
        CaptureCallbackStatus::Captured | CaptureCallbackStatus::NonFiniteInput => {
            PreparedPipeWireInput::Continue
        }
        CaptureCallbackStatus::ProcessorFailed
        | CaptureCallbackStatus::SampleCountNotStereoAligned { .. }
        | CaptureCallbackStatus::BlockTooLarge { .. } => PreparedPipeWireInput::Terminal,
    }
}

fn fail_pipewire_input<P>(state: &mut PipeWireInputCallback<P>, queue_rejected: bool)
where
    P: InterleavedStereoCaptureProcessor,
{
    if queue_rejected {
        state.input.counters().record_stream_error();
    }
    state.status.mark_failed();
    state.main_loop.quit();
}

fn process_pipewire_input<P>(
    stream: &pw::stream::Stream,
    state: &mut PipeWireInputCallback<P>,
    clock: Option<pw::stream::ProcessClock>,
) where
    P: InterleavedStereoCaptureProcessor,
{
    state.input.observe_pipewire_clock(clock);
    let Some(mut buffer) = stream.dequeue_buffer() else {
        state.input.counters().record_unavailable_buffer();
        return;
    };
    let prepared = prepare_pipewire_input(buffer.datas_mut(), state);
    match submit_pipewire_input(prepared, || buffer.queue()) {
        PipeWireInputSubmission::Continued => {}
        PipeWireInputSubmission::Terminal => fail_pipewire_input(state, false),
        PipeWireInputSubmission::QueueRejected => fail_pipewire_input(state, true),
    }
}

fn build_format(sample_rate_hz: u32) -> Result<Vec<u8>, PipeWireStartError> {
    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    audio_info.set_rate(sample_rate_hz);
    audio_info.set_channels(u32::try_from(STEREO_CHANNELS).expect("stereo channel count fits u32"));
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa_sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = spa_sys::SPA_AUDIO_CHANNEL_FR;
    audio_info.set_position(positions);

    pw::spa::pod::serialize::PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(pw::spa::pod::Object {
            type_: spa_sys::SPA_TYPE_OBJECT_Format,
            id: spa_sys::SPA_PARAM_EnumFormat,
            properties: audio_info.into(),
        }),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|source| PipeWireStartError::Backend {
        stage: PipeWireStartupStage::Format,
        detail: source.to_string(),
    })
}

fn backend_error(
    stage: PipeWireStartupStage,
    source: impl Error + Send + Sync + 'static,
) -> PipeWireStartError {
    PipeWireStartError::BackendSource {
        stage,
        source: Box::new(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PendingNativeShutdownWait;
    use core::sync::atomic::AtomicUsize;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    #[test]
    fn config_rejects_invalid_domains() {
        assert_eq!(
            PipeWireInputConfig::try_new(0, 64),
            Err(PipeWireInputConfigError::ZeroSampleRate)
        );
        assert!(matches!(
            PipeWireInputConfig::try_new(48_000, 0),
            Err(PipeWireInputConfigError::InvalidMaximumBlockFrames { actual: 0, .. })
        ));
        assert_eq!(
            PipeWireInputConfig::try_new(48_000, 64)
                .unwrap()
                .with_target(""),
            Err(PipeWireInputConfigError::EmptyTarget)
        );
        assert_eq!(
            PipeWireInputConfig::try_new(48_000, 64)
                .unwrap()
                .with_target("target\0object"),
            Err(PipeWireInputConfigError::TargetContainsNul)
        );
    }

    #[test]
    fn stereo_format_is_a_valid_spa_pod() {
        let format = build_format(48_000).unwrap();
        assert!(Pod::from_bytes(&format).is_some());
    }

    #[test]
    fn input_submission_reports_native_queue_rejection() {
        assert_eq!(
            submit_pipewire_input(PreparedPipeWireInput::Continue, || Err::<(), _>("rejected")),
            PipeWireInputSubmission::QueueRejected
        );
        assert_eq!(
            submit_pipewire_input(PreparedPipeWireInput::Terminal, || Ok::<_, ()>(())),
            PipeWireInputSubmission::Terminal
        );
        assert_eq!(
            submit_pipewire_input(PreparedPipeWireInput::Continue, || Ok::<_, ()>(())),
            PipeWireInputSubmission::Continued
        );
    }

    #[test]
    fn thread_local_initializer_failure_is_reported_before_backend_start() {
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireInputStream::start_with_thread_initializer(config, |_| {
            Err(PipeWireStartError::Backend {
                stage: PipeWireStartupStage::Callback,
                detail: "fixture rejected".to_owned(),
            })
        });

        assert!(matches!(
            result,
            Err(PipeWireStartError::Backend {
                stage: PipeWireStartupStage::Callback,
                detail
            }) if detail == "fixture rejected"
        ));
    }

    #[test]
    fn thread_local_initializer_must_run_its_context() {
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireInputStream::start_with_thread_initializer(config, |_| Ok(()));

        assert!(matches!(result, Err(PipeWireStartError::ThreadTerminated)));
    }

    #[test]
    fn thread_local_initializer_panic_is_distinct() {
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireInputStream::start_with_thread_initializer(config, |_| {
            panic!("initializer panic");
        });

        assert!(matches!(result, Err(PipeWireStartError::ThreadPanicked)));
    }

    #[test]
    fn uncooperative_initializer_retains_eventual_shutdown() {
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let started = Instant::now();

        let result = PipeWireInputStream::start_with_thread_initializer_timeouts(
            config,
            move |_| {
                release_receiver.recv().unwrap();
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );

        let Err(PipeWireStartError::CancellationPending { mut shutdown }) = result else {
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
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let (release_sender, release_receiver) = sync_channel(1);
        let stream = PipeWireInputStream::start_with_thread_initializer_timeouts(
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

        let Err(PipeWireStopError::ShutdownPending { mut shutdown }) = stream.stop() else {
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
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let stream = PipeWireInputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                runner.status.mark_failed();
                Ok(())
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(matches!(
            stream.stop(),
            Err(PipeWireStopError::RuntimeFailed)
        ));
    }

    #[test]
    fn runtime_panic_is_reported_during_stop() {
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let stream = PipeWireInputStream::start_with_thread_initializer_timeouts(
            config,
            |runner| {
                runner.status.mark_running();
                let _ = runner.startup.try_send(Ok(()));
                panic!("runtime panic");
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        )
        .unwrap();

        assert!(matches!(
            stream.stop(),
            Err(PipeWireStopError::ThreadPanicked)
        ));
    }

    #[test]
    fn uncooperative_lifecycle_is_bounded_in_a_subprocess() {
        let executable = std::env::current_exe().unwrap();
        let mut child = Command::new(executable)
            .args([
                "--ignored",
                "--exact",
                "pipewire_input::tests::uncooperative_lifecycle_subprocess_fixture",
            ])
            .env("HYPER_UNCOOPERATIVE_PIPEWIRE_INPUT_FIXTURE", "1")
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
        if std::env::var_os("HYPER_UNCOOPERATIVE_PIPEWIRE_INPUT_FIXTURE").is_none() {
            return;
        }
        let config = PipeWireInputConfig::try_new(48_000, 64).unwrap();
        let startup = PipeWireInputStream::start_with_thread_initializer_timeouts(
            config.clone(),
            |_| loop {
                thread::park();
            },
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        assert!(matches!(
            startup,
            Err(PipeWireStartError::CancellationPending { .. })
        ));

        let stream = PipeWireInputStream::start_with_thread_initializer_timeouts(
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
        assert!(matches!(
            stream.stop(),
            Err(PipeWireStopError::ShutdownPending { .. })
        ));
    }

    #[test]
    #[ignore = "requires a live PipeWire daemon with the hyper-ci-source virtual source"]
    fn live_pipewire_source_captures_audio() {
        let targets = super::super::discover_pipewire_input_targets().unwrap();
        let target = targets
            .targets()
            .iter()
            .find(|target| target.display_name() == "Hyper CI Source")
            .expect("CI virtual source was not discovered");
        let captured = Arc::new(AtomicUsize::new(0));
        let callback_captured = Arc::clone(&captured);
        let config = PipeWireInputConfig::try_new(48_000, 256)
            .unwrap()
            .with_input_target(target)
            .unwrap();
        let stream = PipeWireInputStream::start(config, move |input: &[f32]| {
            callback_captured.fetch_add(input.len(), Ordering::Relaxed);
            Ok::<_, u8>(())
        })
        .unwrap();
        let counters = stream.counters();

        let deadline = Instant::now() + Duration::from_secs(2);
        while captured.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        stream.stop().unwrap();

        assert!(captured.load(Ordering::Relaxed) > 0);
        assert!(counters.snapshot().native_xruns.is_some());
    }
}
