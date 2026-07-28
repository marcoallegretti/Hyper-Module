use core::error::Error;
use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::Arc;
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
use std::thread::JoinHandle;
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
use std::time::Duration;

pub const STEREO_CHANNELS: usize = 2;
pub const MAXIMUM_BLOCK_FRAMES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum OutputStreamState {
    /// Backend construction has not yet published readiness.
    Starting,
    /// The backend published a usable stream and still owns it.
    Running,
    /// Application shutdown won the race with any backend failure.
    StopRequested,
    /// The backend thread completed after an application shutdown request.
    Stopped,
    /// Backend startup or runtime ownership ended unexpectedly.
    Failed,
}

/// Cloneable, lock-free observer for one output thread's lifecycle.
#[derive(Clone, Debug)]
pub struct OutputStreamStatus {
    state: Arc<AtomicU8>,
}

impl Default for OutputStreamStatus {
    fn default() -> Self {
        Self {
            state: Arc::new(AtomicU8::new(OutputStreamState::Starting as u8)),
        }
    }
}

impl OutputStreamStatus {
    /// Returns the latest lifecycle state.
    #[must_use]
    pub fn state(&self) -> OutputStreamState {
        match self.state.load(Ordering::Acquire) {
            value if value == OutputStreamState::Starting as u8 => OutputStreamState::Starting,
            value if value == OutputStreamState::Running as u8 => OutputStreamState::Running,
            value if value == OutputStreamState::StopRequested as u8 => {
                OutputStreamState::StopRequested
            }
            value if value == OutputStreamState::Stopped as u8 => OutputStreamState::Stopped,
            _ => OutputStreamState::Failed,
        }
    }

    fn mark_running(&self) {
        let _ = self.state.compare_exchange(
            OutputStreamState::Starting as u8,
            OutputStreamState::Running as u8,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }

    fn request_stop(&self) {
        let _ =
            self.state.fetch_update(
                Ordering::Release,
                Ordering::Relaxed,
                |current| match current {
                    value
                        if value == OutputStreamState::Starting as u8
                            || value == OutputStreamState::Running as u8 =>
                    {
                        Some(OutputStreamState::StopRequested as u8)
                    }
                    _ => None,
                },
            );
    }

    fn mark_stopped(&self) {
        let _ =
            self.state.fetch_update(
                Ordering::Release,
                Ordering::Relaxed,
                |current| match current {
                    value if value == OutputStreamState::Failed as u8 => None,
                    _ => Some(OutputStreamState::Stopped as u8),
                },
            );
    }

    fn mark_failed(&self) {
        let _ =
            self.state.fetch_update(
                Ordering::Release,
                Ordering::Relaxed,
                |current| match current {
                    value
                        if value == OutputStreamState::Starting as u8
                            || value == OutputStreamState::Running as u8 =>
                    {
                        Some(OutputStreamState::Failed as u8)
                    }
                    _ => None,
                },
            );
    }
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendThreadJoin {
    Completed,
    Panicked,
    TimedOut,
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
#[derive(Debug)]
struct BoundedBackendThread {
    handle: Option<JoinHandle<()>>,
    completed: Receiver<()>,
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
impl BoundedBackendThread {
    fn new(handle: JoinHandle<()>, completed: Receiver<()>) -> Self {
        Self {
            handle: Some(handle),
            completed,
        }
    }

    fn wait(&mut self, timeout: Duration) -> BackendThreadJoin {
        let completion = self.completed.recv_timeout(timeout);
        let Some(handle) = self.handle.take() else {
            return BackendThreadJoin::Completed;
        };
        match completion {
            Ok(()) => match handle.join() {
                Ok(()) => BackendThreadJoin::Completed,
                Err(_) => BackendThreadJoin::Panicked,
            },
            Err(RecvTimeoutError::Disconnected) => {
                if handle.join().is_ok() {
                    BackendThreadJoin::Completed
                } else {
                    BackendThreadJoin::Panicked
                }
            }
            Err(RecvTimeoutError::Timeout) => BackendThreadJoin::TimedOut,
        }
    }

    fn detach(&mut self) {
        let _ = self.handle.take();
    }
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
struct BackendThreadCompletion {
    sender: SyncSender<()>,
    status: OutputStreamStatus,
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
impl Drop for BackendThreadCompletion {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.status.mark_failed();
        }
        let _ = self.sender.try_send(());
    }
}

pub trait InterleavedStereoProcessor {
    type Error;

    /// Renders one interleaved stereo block in place.
    ///
    /// # Errors
    ///
    /// Returns an implementation-defined error when the block cannot be
    /// rendered. The output owner replaces the block with silence.
    ///
    /// # Panics
    ///
    /// Implementations must not panic. Native backends can invoke this method
    /// from a real-time callback, where panic hooks and unwinding are not
    /// bounded operations.
    fn process(&mut self, output: &mut [f32]) -> Result<(), Self::Error>;
}

impl<F, E> InterleavedStereoProcessor for F
where
    F: FnMut(&mut [f32]) -> Result<(), E>,
{
    type Error = E;

    fn process(&mut self, output: &mut [f32]) -> Result<(), Self::Error> {
        self(output)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputCallbackStatus {
    Rendered,
    ProcessorFailed,
    SampleCountNotStereoAligned { samples: usize },
    BlockTooLarge { frames: usize, maximum: usize },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OutputStreamSnapshot {
    pub callback_count: u64,
    pub rendered_callback_count: u64,
    pub rendered_frame_count: u64,
    pub silent_frame_count: u64,
    pub processor_failure_count: u64,
    pub invalid_shape_count: u64,
    pub oversized_block_count: u64,
    pub unavailable_buffer_count: u64,
    pub malformed_buffer_count: u64,
    pub stream_error_count: u64,
}

#[derive(Debug, Default)]
struct OutputStreamCounterState {
    callbacks: AtomicU64,
    rendered_callbacks: AtomicU64,
    rendered_frames: AtomicU64,
    silent_frames: AtomicU64,
    processor_failures: AtomicU64,
    invalid_shapes: AtomicU64,
    oversized_blocks: AtomicU64,
    unavailable_buffers: AtomicU64,
    malformed_buffers: AtomicU64,
    stream_errors: AtomicU64,
}

#[derive(Clone, Debug, Default)]
pub struct OutputStreamCounters {
    state: Arc<OutputStreamCounterState>,
}

impl OutputStreamCounters {
    #[must_use]
    pub fn snapshot(&self) -> OutputStreamSnapshot {
        OutputStreamSnapshot {
            callback_count: self.state.callbacks.load(Ordering::Relaxed),
            rendered_callback_count: self.state.rendered_callbacks.load(Ordering::Relaxed),
            rendered_frame_count: self.state.rendered_frames.load(Ordering::Relaxed),
            silent_frame_count: self.state.silent_frames.load(Ordering::Relaxed),
            processor_failure_count: self.state.processor_failures.load(Ordering::Relaxed),
            invalid_shape_count: self.state.invalid_shapes.load(Ordering::Relaxed),
            oversized_block_count: self.state.oversized_blocks.load(Ordering::Relaxed),
            unavailable_buffer_count: self.state.unavailable_buffers.load(Ordering::Relaxed),
            malformed_buffer_count: self.state.malformed_buffers.load(Ordering::Relaxed),
            stream_error_count: self.state.stream_errors.load(Ordering::Relaxed),
        }
    }

    fn add(counter: &AtomicU64, amount: usize) {
        let amount = u64::try_from(amount).unwrap_or(u64::MAX);
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(amount))
        });
    }

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    fn record_unavailable_buffer(&self) {
        Self::add(&self.state.unavailable_buffers, 1);
    }

    #[cfg(all(target_os = "linux", target_endian = "little"))]
    fn record_malformed_buffer(&self, frames: usize) {
        Self::add(&self.state.callbacks, 1);
        Self::add(&self.state.malformed_buffers, 1);
        Self::add(&self.state.silent_frames, frames);
    }

    #[cfg(any(
        all(target_os = "linux", target_endian = "little"),
        target_os = "windows"
    ))]
    fn record_stream_error(&self) {
        Self::add(&self.state.stream_errors, 1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputCallbackConfigError {
    InvalidMaximumBlockFrames { actual: usize, maximum: usize },
}

impl fmt::Display for OutputCallbackConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMaximumBlockFrames { actual, maximum } => write!(
                formatter,
                "maximum callback block has {actual} frames; expected 1 through {maximum}"
            ),
        }
    }
}

impl Error for OutputCallbackConfigError {}

#[derive(Debug)]
pub struct OutputCallback<P> {
    processor: P,
    maximum_block_frames: usize,
    counters: OutputStreamCounters,
}

impl<P: InterleavedStereoProcessor> OutputCallback<P> {
    /// Creates a fail-silent callback owner for interleaved stereo `f32`.
    ///
    /// # Errors
    ///
    /// Returns [`OutputCallbackConfigError`] when the callback capacity is
    /// zero or exceeds [`MAXIMUM_BLOCK_FRAMES`].
    pub fn try_new(
        processor: P,
        maximum_block_frames: usize,
    ) -> Result<Self, OutputCallbackConfigError> {
        Self::try_new_with_counters(
            processor,
            maximum_block_frames,
            OutputStreamCounters::default(),
        )
    }

    fn try_new_with_counters(
        processor: P,
        maximum_block_frames: usize,
        counters: OutputStreamCounters,
    ) -> Result<Self, OutputCallbackConfigError> {
        if !(1..=MAXIMUM_BLOCK_FRAMES).contains(&maximum_block_frames) {
            return Err(OutputCallbackConfigError::InvalidMaximumBlockFrames {
                actual: maximum_block_frames,
                maximum: MAXIMUM_BLOCK_FRAMES,
            });
        }
        Ok(Self {
            processor,
            maximum_block_frames,
            counters,
        })
    }

    #[must_use]
    pub fn counters(&self) -> OutputStreamCounters {
        self.counters.clone()
    }

    #[must_use]
    pub const fn maximum_block_frames(&self) -> usize {
        self.maximum_block_frames
    }

    pub fn process(&mut self, output: &mut [f32]) -> OutputCallbackStatus {
        let samples = output.len();
        if !samples.is_multiple_of(STEREO_CHANNELS) {
            output.fill(0.0);
            OutputStreamCounters::add(&self.counters.state.callbacks, 1);
            OutputStreamCounters::add(&self.counters.state.invalid_shapes, 1);
            return OutputCallbackStatus::SampleCountNotStereoAligned { samples };
        }
        let frames = samples / STEREO_CHANNELS;
        if frames > self.maximum_block_frames {
            output.fill(0.0);
            OutputStreamCounters::add(&self.counters.state.callbacks, 1);
            OutputStreamCounters::add(&self.counters.state.oversized_blocks, 1);
            OutputStreamCounters::add(&self.counters.state.silent_frames, frames);
            return OutputCallbackStatus::BlockTooLarge {
                frames,
                maximum: self.maximum_block_frames,
            };
        }

        output.fill(0.0);
        OutputStreamCounters::add(&self.counters.state.callbacks, 1);
        if self.processor.process(output).is_ok() {
            OutputStreamCounters::add(&self.counters.state.rendered_callbacks, 1);
            OutputStreamCounters::add(&self.counters.state.rendered_frames, frames);
            OutputCallbackStatus::Rendered
        } else {
            output.fill(0.0);
            OutputStreamCounters::add(&self.counters.state.processor_failures, 1);
            OutputStreamCounters::add(&self.counters.state.silent_frames, frames);
            OutputCallbackStatus::ProcessorFailed
        }
    }
}

#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
mod desktop_output;
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
pub use desktop_output::{
    DesktopOutputConfig, DesktopOutputConfigError, DesktopOutputDiscoveryError,
    DesktopOutputTarget, DesktopOutputTargets, discover_desktop_output_targets,
};

#[cfg(all(target_os = "linux", target_endian = "little"))]
mod pipewire_devices;
#[cfg(all(target_os = "linux", target_endian = "little"))]
pub use pipewire_devices::{
    PipeWireOutputTarget, PipeWireOutputTargets, PipeWireTargetDiscoveryError,
    PipeWireTargetDiscoveryStage, discover_pipewire_output_targets,
};
#[cfg(all(target_os = "linux", target_endian = "little"))]
mod pipewire_output;
#[cfg(all(target_os = "linux", target_endian = "little"))]
pub use pipewire_output::{
    PipeWireOutputConfig, PipeWireOutputConfigError, PipeWireOutputStream, PipeWireStartError,
    PipeWireStartupStage, PipeWireStopError,
};
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
mod playback_checkpoint;
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
pub use playback_checkpoint::{
    PlaybackRecoveryCheckpoint, PlaybackRecoverySnapshot, PlaybackRecoveryTransportState,
};
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
mod playback_session;
#[cfg(any(
    all(target_os = "linux", target_endian = "little"),
    target_os = "windows"
))]
pub use playback_session::{
    DesktopOutputStartError, DesktopOutputStopError, DesktopPlaybackRecovery,
    DesktopPlaybackRecoveryStartError, DesktopPlaybackRecoveryStopError, DesktopPlaybackSession,
    DesktopPlaybackSessionBuildError, DesktopPlaybackSessionInput,
    DesktopPlaybackSessionStartError,
};
#[cfg(target_os = "windows")]
mod wasapi_devices;
#[cfg(target_os = "windows")]
pub use wasapi_devices::{
    WasapiEndpointDiscoveryError, WasapiEndpointDiscoveryStage, WasapiOutputEndpoint,
    WasapiOutputEndpoints, discover_wasapi_output_endpoints,
};
#[cfg(target_os = "windows")]
mod wasapi_output;
#[cfg(target_os = "windows")]
pub use wasapi_output::{
    WasapiDisconnectReason, WasapiOutputConfig, WasapiOutputConfigError, WasapiOutputStream,
    WasapiStartError, WasapiStartupStage, WasapiStopError,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_samples(actual: &[f32], expected: f32) {
        assert!(
            actual
                .iter()
                .all(|sample| (*sample - expected).abs() <= f32::EPSILON)
        );
    }

    #[test]
    fn callback_is_fail_silent_and_counts_every_outcome() {
        let call = 0_u8;
        let mut callback = OutputCallback::try_new(
            {
                let mut call = call;
                move |output: &mut [f32]| -> Result<(), &'static str> {
                    call += 1;
                    output.fill(f32::from(call));
                    if call == 1 { Ok(()) } else { Err("failure") }
                }
            },
            2,
        )
        .unwrap();
        let counters = callback.counters();

        let mut rendered = [9.0_f32; 4];
        assert_eq!(
            callback.process(&mut rendered),
            OutputCallbackStatus::Rendered
        );
        assert_samples(&rendered, 1.0);

        let mut failed = [9.0_f32; 4];
        assert_eq!(
            callback.process(&mut failed),
            OutputCallbackStatus::ProcessorFailed
        );
        assert_samples(&failed, 0.0);

        let mut malformed = [9.0_f32; 3];
        assert_eq!(
            callback.process(&mut malformed),
            OutputCallbackStatus::SampleCountNotStereoAligned { samples: 3 }
        );
        assert_samples(&malformed, 0.0);

        let mut oversized = [9.0_f32; 6];
        assert_eq!(
            callback.process(&mut oversized),
            OutputCallbackStatus::BlockTooLarge {
                frames: 3,
                maximum: 2
            }
        );
        assert_samples(&oversized, 0.0);

        assert_eq!(
            counters.snapshot(),
            OutputStreamSnapshot {
                callback_count: 4,
                rendered_callback_count: 1,
                rendered_frame_count: 2,
                silent_frame_count: 5,
                processor_failure_count: 1,
                invalid_shape_count: 1,
                oversized_block_count: 1,
                ..OutputStreamSnapshot::default()
            }
        );
    }

    #[test]
    fn stream_status_has_terminal_stop_and_failure_states() {
        let stopped = OutputStreamStatus::default();
        assert_eq!(stopped.state(), OutputStreamState::Starting);
        stopped.mark_running();
        assert_eq!(stopped.state(), OutputStreamState::Running);
        stopped.request_stop();
        assert_eq!(stopped.state(), OutputStreamState::StopRequested);
        stopped.mark_failed();
        assert_eq!(stopped.state(), OutputStreamState::StopRequested);
        stopped.mark_stopped();
        assert_eq!(stopped.state(), OutputStreamState::Stopped);

        let failed = OutputStreamStatus::default();
        failed.mark_running();
        failed.mark_failed();
        failed.mark_stopped();
        assert_eq!(failed.state(), OutputStreamState::Failed);
    }

    #[test]
    fn callback_capacity_is_strict() {
        let processor = |_: &mut [f32]| Ok::<_, ()>(());
        assert!(matches!(
            OutputCallback::try_new(processor, 0),
            Err(OutputCallbackConfigError::InvalidMaximumBlockFrames {
                actual: 0,
                maximum: MAXIMUM_BLOCK_FRAMES
            })
        ));
        let processor = |_: &mut [f32]| Ok::<_, ()>(());
        assert!(matches!(
            OutputCallback::try_new(processor, MAXIMUM_BLOCK_FRAMES + 1),
            Err(OutputCallbackConfigError::InvalidMaximumBlockFrames {
                actual,
                maximum: MAXIMUM_BLOCK_FRAMES
            }) if actual == MAXIMUM_BLOCK_FRAMES + 1
        ));
    }

    #[test]
    fn callback_can_borrow_thread_local_processor_state() {
        let mut invocation_count = 0_u8;
        {
            let mut callback = OutputCallback::try_new(
                |output: &mut [f32]| -> Result<(), ()> {
                    invocation_count = invocation_count.saturating_add(1);
                    output.fill(f32::from(invocation_count));
                    Ok(())
                },
                2,
            )
            .unwrap();
            let mut output = [0.0_f32; 4];

            assert_eq!(
                callback.process(&mut output),
                OutputCallbackStatus::Rendered
            );
            assert_samples(&output, 1.0);
        }
        assert_eq!(invocation_count, 1);
    }
}
