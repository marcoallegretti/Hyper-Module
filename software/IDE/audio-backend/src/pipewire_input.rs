use super::{
    BackendThreadCompletion, BackendThreadJoin, BoundedBackendThread, CaptureCallback,
    CaptureCallbackStatus, CaptureStreamCounters, InterleavedStereoCaptureProcessor,
    PipeWireInputTarget, PipeWireStartError, PipeWireStartupStage, PipeWireStopError,
    STEREO_CHANNELS, StreamState, StreamStatus,
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
    thread: BoundedBackendThread,
    counters: CaptureStreamCounters,
    status: StreamStatus,
    shutdown_timeout: Duration,
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
        Self::start_with_timeouts(config, processor, STARTUP_TIMEOUT, SHUTDOWN_TIMEOUT)
    }

    fn start_with_timeouts<P>(
        config: PipeWireInputConfig,
        processor: P,
        startup_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<Self, PipeWireStartError>
    where
        P: InterleavedStereoCaptureProcessor + Send + 'static,
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
        let handle = thread::Builder::new()
            .name("hyper-pipewire-input".to_owned())
            .spawn(move || {
                let _completion = BackendThreadCompletion {
                    sender: completion_sender,
                    status: completion_thread_status,
                };
                let callback = match CaptureCallback::try_new_with_counters(
                    processor,
                    config.maximum_block_frames,
                    thread_counters,
                ) {
                    Ok(callback) => callback,
                    Err(source) => {
                        completion_status.mark_failed();
                        let _ = failure_sender.try_send(Err(PipeWireStartError::Backend {
                            stage: PipeWireStartupStage::Callback,
                            detail: source.to_string(),
                        }));
                        return;
                    }
                };
                if let Err(error) = run_pipewire_input(
                    config,
                    callback,
                    thread_stop,
                    &startup_sender,
                    &thread_status,
                ) {
                    completion_status.mark_failed();
                    let _ = failure_sender.try_send(Err(error));
                } else {
                    completion_status.mark_stopped();
                }
            })
            .map_err(PipeWireStartError::ThreadSpawn)?;
        let mut thread = BoundedBackendThread::new(handle, completion_receiver);

        match wait_for_startup(&startup_receiver, startup_timeout) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread,
                counters,
                status,
                shutdown_timeout,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.wait(shutdown_timeout) {
                    BackendThreadJoin::Panicked => Err(PipeWireStartError::ThreadPanicked),
                    BackendThreadJoin::Completed | BackendThreadJoin::TimedOut => Err(error),
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
        match self.thread.wait(self.shutdown_timeout) {
            BackendThreadJoin::Completed if self.status.state() == StreamState::Failed => {
                Err(PipeWireStopError::RuntimeFailed)
            }
            BackendThreadJoin::Completed => Ok(()),
            BackendThreadJoin::Panicked => Err(PipeWireStopError::ThreadPanicked),
            BackendThreadJoin::TimedOut => Err(PipeWireStopError::ShutdownTimeout),
        }
    }
}

impl Drop for PipeWireInputStream {
    fn drop(&mut self) {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.thread.detach();
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
        .process(process_pipewire_input)
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

fn process_pipewire_input<P>(stream: &pw::stream::Stream, state: &mut PipeWireInputCallback<P>)
where
    P: InterleavedStereoCaptureProcessor,
{
    let Some(mut buffer) = stream.dequeue_buffer() else {
        state.input.counters().record_unavailable_buffer();
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        state.input.counters().record_malformed_buffer(0);
        return;
    };
    let offset = usize::try_from(data.chunk().offset()).unwrap_or(usize::MAX);
    let size = usize::try_from(data.chunk().size()).unwrap_or(usize::MAX);
    let stride = data.chunk().stride();
    let Some(bytes) = data.data() else {
        state.input.counters().record_malformed_buffer(0);
        return;
    };
    let Some(end) = offset.checked_add(size) else {
        state.input.counters().record_malformed_buffer(0);
        return;
    };
    let frames = size / FRAME_STRIDE;
    if size == 0 {
        return;
    }
    if end > bytes.len()
        || size % FRAME_STRIDE != 0
        || (stride != 0 && usize::try_from(stride).ok() != Some(FRAME_STRIDE))
    {
        state.input.counters().record_malformed_buffer(frames);
        return;
    }
    let Ok(samples) = bytemuck::try_cast_slice::<u8, f32>(&bytes[offset..end]) else {
        state.input.counters().record_malformed_buffer(frames);
        return;
    };

    match state.input.process(samples) {
        CaptureCallbackStatus::Captured | CaptureCallbackStatus::NonFiniteInput => {}
        CaptureCallbackStatus::ProcessorFailed
        | CaptureCallbackStatus::SampleCountNotStereoAligned { .. }
        | CaptureCallbackStatus::BlockTooLarge { .. } => {
            state.status.mark_failed();
            state.main_loop.quit();
        }
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
    use core::sync::atomic::AtomicUsize;
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

        let deadline = Instant::now() + Duration::from_secs(2);
        while captured.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        stream.stop().unwrap();

        assert!(captured.load(Ordering::Relaxed) > 0);
    }
}
