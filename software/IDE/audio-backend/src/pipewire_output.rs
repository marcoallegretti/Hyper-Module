use super::{
    InterleavedStereoProcessor, OutputCallback, OutputStreamCounters, OutputStreamState,
    OutputStreamStatus, PipeWireOutputTarget, PlaybackRecoveryCheckpoint, STEREO_CHANNELS,
};
use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicBool, Ordering};
use hyper_mixer_runtime::{TrackWindowRenderer, TransportOutputProcessor};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::pod::Pod;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_POLL_INTERVAL: Duration = Duration::from_millis(10);
const BYTES_PER_SAMPLE: usize = size_of::<f32>();
const FRAME_STRIDE: usize = STEREO_CHANNELS * BYTES_PER_SAMPLE;
type PipeWireProcessor<'processor> = Box<dyn FnMut(&mut [f32]) -> Result<(), ()> + 'processor>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeWireOutputConfig {
    sample_rate_hz: u32,
    maximum_block_frames: usize,
    target: Option<String>,
}

impl PipeWireOutputConfig {
    /// Creates a fixed-rate stereo `PipeWire` output configuration.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireOutputConfigError`] for a zero sample rate or an
    /// invalid callback capacity.
    pub fn try_new(
        sample_rate_hz: u32,
        maximum_block_frames: usize,
    ) -> Result<Self, PipeWireOutputConfigError> {
        if sample_rate_hz == 0 {
            return Err(PipeWireOutputConfigError::ZeroSampleRate);
        }
        if !(1..=super::MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(PipeWireOutputConfigError::InvalidMaximumBlockFrames {
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

    /// Selects a `PipeWire` target object instead of the policy default.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireOutputConfigError`] for an empty target or an
    /// embedded NUL.
    pub fn with_target(mut self, target: &str) -> Result<Self, PipeWireOutputConfigError> {
        if target.is_empty() {
            return Err(PipeWireOutputConfigError::EmptyTarget);
        }
        if target.as_bytes().contains(&0) {
            return Err(PipeWireOutputConfigError::TargetContainsNul);
        }
        self.target = Some(target.to_owned());
        Ok(self)
    }

    /// Selects one target returned by desktop output discovery.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireOutputConfigError`] if its routing identity is invalid.
    pub fn with_output_target(
        self,
        target: &PipeWireOutputTarget,
    ) -> Result<Self, PipeWireOutputConfigError> {
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
pub enum PipeWireOutputConfigError {
    ZeroSampleRate,
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
    EmptyTarget,
    TargetContainsNul,
}

impl fmt::Display for PipeWireOutputConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSampleRate => formatter.write_str("PipeWire sample rate must be nonzero"),
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "PipeWire maximum block has {actual} frames; expected 1 through {maximum}"
            ),
            Self::EmptyTarget => formatter.write_str("PipeWire target must not be empty"),
            Self::TargetContainsNul => {
                formatter.write_str("PipeWire target contains an embedded NUL")
            }
        }
    }
}

impl Error for PipeWireOutputConfigError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipeWireStartupStage {
    Callback,
    MainLoop,
    Context,
    Core,
    Stream,
    Listener,
    Format,
    Connect,
    State,
}

impl fmt::Display for PipeWireStartupStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Callback => "callback",
            Self::MainLoop => "main loop",
            Self::Context => "context",
            Self::Core => "core",
            Self::Stream => "stream",
            Self::Listener => "listener",
            Self::Format => "format",
            Self::Connect => "connection",
            Self::State => "state",
        })
    }
}

#[derive(Debug)]
pub enum PipeWireStartError {
    ThreadSpawn(std::io::Error),
    ThreadPanicked,
    StartupTimeout,
    ThreadTerminated,
    Backend {
        stage: PipeWireStartupStage,
        detail: String,
    },
}

impl fmt::Display for PipeWireStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ThreadSpawn(source) => {
                write!(formatter, "cannot start PipeWire thread: {source}")
            }
            Self::ThreadPanicked => formatter.write_str("PipeWire thread panicked during startup"),
            Self::StartupTimeout => formatter.write_str("PipeWire startup timed out"),
            Self::ThreadTerminated => {
                formatter.write_str("PipeWire thread terminated during startup")
            }
            Self::Backend { stage, detail } => {
                write!(formatter, "PipeWire {stage} failed: {detail}")
            }
        }
    }
}

impl Error for PipeWireStartError {
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
pub enum PipeWireStopError {
    ThreadPanicked,
}

impl fmt::Display for PipeWireStopError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PipeWire thread panicked during shutdown")
    }
}

impl Error for PipeWireStopError {}

#[derive(Debug)]
pub struct PipeWireOutputStream {
    stop_requested: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counters: OutputStreamCounters,
    status: OutputStreamStatus,
}

/// Backend-thread context for constructing callback state from owned inputs.
///
/// The initializer passed to [`PipeWireOutputStream::start_with_thread_initializer`] owns
/// project inputs moved to the backend thread. It can borrow those local inputs
/// while constructing a processor, then pass that processor to [`Self::run`].
/// This avoids requiring self-referential or leaked prepared playback state.
#[derive(Debug)]
pub struct PipeWireThreadRunner {
    config: PipeWireOutputConfig,
    stop_requested: Arc<AtomicBool>,
    startup: SyncSender<Result<(), PipeWireStartError>>,
    counters: OutputStreamCounters,
    status: OutputStreamStatus,
}

impl PipeWireThreadRunner {
    /// Runs the `PipeWire` loop with processor state constructed on this thread.
    ///
    /// The processor may borrow values owned by the thread initializer. It is
    /// dropped before the initializer returns.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStartError`] for callback construction, backend
    /// construction, format, connection, or stream-state failure.
    pub fn run<P>(self, processor: P) -> Result<(), PipeWireStartError>
    where
        P: InterleavedStereoProcessor,
    {
        let mut processor = processor;
        let processor: PipeWireProcessor<'_> =
            Box::new(move |output| processor.process(output).map_err(|_| ()));
        let callback = OutputCallback::try_new_with_counters(
            processor,
            self.config.maximum_block_frames,
            self.counters,
        )
        .map_err(|source| PipeWireStartError::Backend {
            stage: PipeWireStartupStage::Callback,
            detail: source.to_string(),
        })?;
        run_pipewire_inner(
            self.config,
            callback,
            self.stop_requested,
            &self.startup,
            &self.status,
        )
    }

    /// Runs one prepared transport output processor on this `PipeWire` thread.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStartError`] when the transport and stream callback
    /// capacities differ or when backend startup or lifecycle fails. Transport
    /// processing failures follow the stream's fail-silent counter policy.
    pub fn run_transport_output<R>(
        self,
        mut processor: TransportOutputProcessor<'_, '_, R>,
    ) -> Result<(), PipeWireStartError>
    where
        R: TrackWindowRenderer,
    {
        validate_transport_capacity(
            self.config.maximum_block_frames,
            processor.maximum_block_frames(),
        )?;
        self.run(move |output: &mut [f32]| processor.process(output).map(|_| ()))
    }

    pub(crate) fn run_transport_output_with_checkpoint<R>(
        self,
        mut processor: TransportOutputProcessor<'_, '_, R>,
        checkpoint: PlaybackRecoveryCheckpoint,
    ) -> Result<(), PipeWireStartError>
    where
        R: TrackWindowRenderer,
    {
        validate_transport_capacity(
            self.config.maximum_block_frames,
            processor.maximum_block_frames(),
        )?;
        self.run(move |output: &mut [f32]| {
            processor.process(output).map(|rendered| {
                checkpoint.publish_cycle(rendered.snapshot().cycle());
            })
        })
    }

    /// Returns whether stream startup or shutdown cancellation was requested.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }
}

impl PipeWireOutputStream {
    /// Connects a stereo `f32` output node and owns its `PipeWire` loop thread.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStartError`] when the thread, `PipeWire` objects,
    /// format, connection, or initial stream state cannot be established.
    pub fn start<P>(config: PipeWireOutputConfig, processor: P) -> Result<Self, PipeWireStartError>
    where
        P: InterleavedStereoProcessor + Send + 'static,
    {
        Self::start_with_thread_initializer(config, move |thread| thread.run(processor))
    }

    /// Starts a backend thread that constructs and runs its processor locally.
    ///
    /// The initializer is moved to the backend thread. It may own project
    /// documents, compiled graphs, decoded media, and other session inputs;
    /// borrowed prepared state can then remain valid for the complete
    /// [`PipeWireThreadRunner::run`] call.
    ///
    /// The initializer must either call [`PipeWireThreadRunner::run`] or return an
    /// error. Returning success without running the context is reported as
    /// [`PipeWireStartError::ThreadTerminated`].
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStartError`] when the thread cannot be created, the
    /// initializer or backend fails, startup times out, or the thread
    /// terminates before reaching a usable stream state.
    pub fn start_with_thread_initializer<F>(
        config: PipeWireOutputConfig,
        initialize: F,
    ) -> Result<Self, PipeWireStartError>
    where
        F: FnOnce(PipeWireThreadRunner) -> Result<(), PipeWireStartError> + Send + 'static,
    {
        let stop_requested = Arc::new(AtomicBool::new(false));
        let counters = OutputStreamCounters::default();
        let status = OutputStreamStatus::default();
        let thread_stop = Arc::clone(&stop_requested);
        let thread_counters = counters.clone();
        let thread_status = status.clone();
        let completion_status = status.clone();
        let (startup_sender, startup_receiver) = sync_channel(1);
        let failure_sender = startup_sender.clone();
        let thread = thread::Builder::new()
            .name("hyper-pipewire-output".to_owned())
            .spawn(move || {
                let thread = PipeWireThreadRunner {
                    config,
                    stop_requested: thread_stop,
                    startup: startup_sender,
                    counters: thread_counters,
                    status: thread_status,
                };
                if let Err(error) = initialize(thread) {
                    completion_status.mark_failed();
                    let _ = failure_sender.try_send(Err(error));
                } else {
                    completion_status.mark_stopped();
                }
            })
            .map_err(PipeWireStartError::ThreadSpawn)?;

        match wait_for_startup(&startup_receiver) {
            Ok(()) => Ok(Self {
                stop_requested,
                thread: Some(thread),
                counters,
                status,
            }),
            Err(error) => {
                stop_requested.store(true, Ordering::Release);
                match thread.join() {
                    Ok(()) => Err(error),
                    Err(_) => Err(PipeWireStartError::ThreadPanicked),
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

    /// Requests loop shutdown and joins the owned backend thread.
    ///
    /// # Errors
    ///
    /// Returns [`PipeWireStopError`] if the backend thread panicked.
    pub fn stop(mut self) -> Result<(), PipeWireStopError> {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        self.join()
    }

    fn join(&mut self) -> Result<(), PipeWireStopError> {
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| PipeWireStopError::ThreadPanicked)?;
        }
        Ok(())
    }
}

impl Drop for PipeWireOutputStream {
    fn drop(&mut self) {
        self.status.request_stop();
        self.stop_requested.store(true, Ordering::Release);
        let _ = self.join();
    }
}

fn wait_for_startup(
    receiver: &Receiver<Result<(), PipeWireStartError>>,
) -> Result<(), PipeWireStartError> {
    match receiver.recv_timeout(STARTUP_TIMEOUT) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => Err(PipeWireStartError::StartupTimeout),
        Err(RecvTimeoutError::Disconnected) => Err(PipeWireStartError::ThreadTerminated),
    }
}

struct PipeWireCallback<'processor> {
    output: OutputCallback<PipeWireProcessor<'processor>>,
    startup: Option<SyncSender<Result<(), PipeWireStartError>>>,
}

fn run_pipewire_inner(
    config: PipeWireOutputConfig,
    callback: OutputCallback<PipeWireProcessor<'_>>,
    stop_requested: Arc<AtomicBool>,
    startup: &SyncSender<Result<(), PipeWireStartError>>,
    status: &OutputStreamStatus,
) -> Result<(), PipeWireStartError> {
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
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::APP_NAME => "Hyper Module",
        *pw::keys::NODE_NAME => "hyper-module-output",
        *pw::keys::NODE_DESCRIPTION => "Hyper Module Stereo Output",
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
    let stream = pw::stream::StreamBox::new(&core, "hyper-module-output", stream_properties)
        .map_err(|source| backend_error(PipeWireStartupStage::Stream, source))?;

    let loop_for_state = main_loop.clone();
    let counters = callback.counters();
    let state_status = status.clone();
    let listener = stream
        .add_local_listener_with_user_data(PipeWireCallback {
            output: callback,
            startup: Some(startup.clone()),
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
                if state_status.state() == OutputStreamState::Running {
                    state_status.mark_failed();
                    counters.record_stream_error();
                    loop_for_state.quit();
                }
            }
            pw::stream::StreamState::Connecting => {}
        })
        .process(process_pipewire_buffer)
        .register()
        .map_err(|source| backend_error(PipeWireStartupStage::Listener, source))?;

    let loop_for_stop = main_loop.clone();
    let stop_timer = main_loop.loop_().add_timer(move |_| {
        if stop_requested.load(Ordering::Acquire) {
            loop_for_stop.quit();
        }
    });
    stop_timer.update_timer(Some(STOP_POLL_INTERVAL), Some(STOP_POLL_INTERVAL));

    let format = build_format(config.sample_rate_hz)?;
    let pod = Pod::from_bytes(&format).ok_or_else(|| PipeWireStartError::Backend {
        stage: PipeWireStartupStage::Format,
        detail: "serialized format is not a valid SPA pod".to_owned(),
    })?;
    let mut stream_flags = pw::stream::StreamFlags::AUTOCONNECT
        | pw::stream::StreamFlags::MAP_BUFFERS
        | pw::stream::StreamFlags::RT_PROCESS;
    if target_is_pinned {
        stream_flags |= pw::stream::StreamFlags::DONT_RECONNECT;
    }
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            stream_flags,
            &mut [pod],
        )
        .map_err(|source| backend_error(PipeWireStartupStage::Connect, source))?;

    main_loop.run();
    drop(stop_timer);
    drop(listener);
    Ok(())
}

fn process_pipewire_buffer(stream: &pw::stream::Stream, state: &mut PipeWireCallback<'_>) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        state.output.counters().record_unavailable_buffer();
        return;
    };
    let Some(data) = buffer.datas_mut().first_mut() else {
        state.output.counters().record_malformed_buffer(0);
        return;
    };
    let requested_bytes = usize::try_from(data.chunk().size()).unwrap_or(usize::MAX);
    let maximum_bytes = state
        .output
        .maximum_block_frames()
        .saturating_mul(FRAME_STRIDE);
    let Some(bytes) = data.data() else {
        state.output.counters().record_malformed_buffer(0);
        return;
    };
    let callback_bytes = if requested_bytes == 0 {
        bytes.len().min(maximum_bytes)
    } else {
        requested_bytes.min(bytes.len())
    };
    let aligned_bytes = callback_bytes - (callback_bytes % FRAME_STRIDE);
    if requested_bytes > bytes.len() || aligned_bytes != callback_bytes || aligned_bytes == 0 {
        bytes[..callback_bytes].fill(0);
        state
            .output
            .counters()
            .record_malformed_buffer(aligned_bytes / FRAME_STRIDE);
    } else if let Ok(samples) = bytemuck::try_cast_slice_mut::<u8, f32>(&mut bytes[..aligned_bytes])
    {
        let _ = state.output.process(samples);
    } else {
        bytes[..aligned_bytes].fill(0);
        state
            .output
            .counters()
            .record_malformed_buffer(aligned_bytes / FRAME_STRIDE);
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.size_mut() = u32::try_from(aligned_bytes).unwrap_or(u32::MAX);
    *chunk.stride_mut() = i32::try_from(FRAME_STRIDE).expect("stereo f32 stride fits i32");
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

fn backend_error(stage: PipeWireStartupStage, source: impl fmt::Display) -> PipeWireStartError {
    PipeWireStartError::Backend {
        stage,
        detail: source.to_string(),
    }
}

fn validate_transport_capacity(
    pipewire_frames: usize,
    transport_frames: usize,
) -> Result<(), PipeWireStartError> {
    if pipewire_frames != transport_frames {
        return Err(PipeWireStartError::Backend {
            stage: PipeWireStartupStage::Callback,
            detail: format!(
                "PipeWire capacity is {pipewire_frames} frames; transport capacity is {transport_frames}"
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_invalid_domains() {
        assert_eq!(
            PipeWireOutputConfig::try_new(0, 64),
            Err(PipeWireOutputConfigError::ZeroSampleRate)
        );
        assert!(matches!(
            PipeWireOutputConfig::try_new(48_000, 0),
            Err(PipeWireOutputConfigError::InvalidMaximumBlockFrames { actual: 0, .. })
        ));
        assert_eq!(
            PipeWireOutputConfig::try_new(48_000, 64)
                .unwrap()
                .with_target(""),
            Err(PipeWireOutputConfigError::EmptyTarget)
        );
        assert_eq!(
            PipeWireOutputConfig::try_new(48_000, 64)
                .unwrap()
                .with_target("target\0object"),
            Err(PipeWireOutputConfigError::TargetContainsNul)
        );
    }

    #[test]
    fn stereo_format_is_a_valid_spa_pod() {
        let format = build_format(48_000).unwrap();
        assert!(Pod::from_bytes(&format).is_some());
    }

    #[test]
    fn thread_local_initializer_failure_is_reported_before_backend_start() {
        let config = PipeWireOutputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireOutputStream::start_with_thread_initializer(config, |_| {
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
        let config = PipeWireOutputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireOutputStream::start_with_thread_initializer(config, |_| Ok(()));

        assert!(matches!(result, Err(PipeWireStartError::ThreadTerminated)));
    }

    #[test]
    fn thread_local_initializer_panic_is_distinct() {
        let config = PipeWireOutputConfig::try_new(48_000, 64).unwrap();
        let result = PipeWireOutputStream::start_with_thread_initializer(config, |_| {
            panic!("initializer panic");
        });

        assert!(matches!(result, Err(PipeWireStartError::ThreadPanicked)));
    }

    #[test]
    fn transport_and_pipewire_capacities_must_match() {
        assert!(validate_transport_capacity(64, 64).is_ok());
        assert!(matches!(
            validate_transport_capacity(64, 128),
            Err(PipeWireStartError::Backend {
                stage: PipeWireStartupStage::Callback,
                detail
            }) if detail
                == "PipeWire capacity is 64 frames; transport capacity is 128"
        ));
    }
}
