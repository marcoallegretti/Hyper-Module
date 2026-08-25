use super::{
    LocateState, MAX_MAIN_MIXER_SLOTS, MAX_MIXER_SENDS_PER_SLOT, MixerChannelParameterState,
    MixerParameterStateSnapshot, MixerSendParameterState, NativeDeviceStateSnapshot,
    TransportCommand, TransportCycle, TransportLoopRange, TransportState,
};
use hyper_native_device::MAX_DEVICES_PER_CHAIN;
use core::hint::spin_loop;
use core::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence,
};
use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum PlaybackRecoveryTransportState {
    Stopped,
    Rolling,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackRecoverySnapshot {
    state: PlaybackRecoveryTransportState,
    position_frame: usize,
    committed_cycle_count: u64,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
    mixer_parameter_state: Option<MixerParameterStateSnapshot>,
    native_device_state: Option<NativeDeviceStateSnapshot>,
}

impl PlaybackRecoverySnapshot {
    /// Builds a bounded synthetic recovery observation for consumer tests.
    ///
    /// This is unavailable to normal builds, so only tests can model a
    /// checkpoint without obtaining it from the sole audio publisher.
    #[cfg(feature = "test-support")]
    #[must_use]
    pub const fn for_test(state: PlaybackRecoveryTransportState, position_frame: usize) -> Self {
        Self {
            state,
            position_frame,
            committed_cycle_count: 0,
            loop_range: None,
            loop_enabled: false,
            mixer_parameter_state: None,
            native_device_state: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> PlaybackRecoveryTransportState {
        self.state
    }

    #[must_use]
    pub const fn position_frame(&self) -> usize {
        self.position_frame
    }

    #[must_use]
    pub const fn committed_cycle_count(&self) -> u64 {
        self.committed_cycle_count
    }

    #[must_use]
    pub const fn loop_range(&self) -> Option<TransportLoopRange> {
        self.loop_range
    }

    #[must_use]
    pub const fn loop_enabled(&self) -> bool {
        self.loop_enabled
    }

    #[must_use]
    pub const fn mixer_parameter_state(&self) -> Option<MixerParameterStateSnapshot> {
        self.mixer_parameter_state
    }

    /// Returns recovered session-only native-device bypass state.
    #[must_use]
    pub const fn native_device_state(&self) -> Option<NativeDeviceStateSnapshot> {
        self.native_device_state
    }

    /// Creates the transport command that resumes this authenticated checkpoint.
    #[must_use]
    pub const fn restore_command(&self) -> TransportCommand {
        TransportCommand::Locate {
            frame: self.position_frame,
            state: match self.state {
                PlaybackRecoveryTransportState::Stopped => LocateState::Stopped,
                PlaybackRecoveryTransportState::Rolling => LocateState::Rolling,
            },
        }
    }
}

/// Coherent state from the last audio cycle accepted by the output owner.
///
/// Interactive scene feedback is observation-only. Reopening playback restores
/// transport and mixer recovery authority but deliberately clears interactive
/// scene state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackCycleObservation {
    state: PlaybackRecoveryTransportState,
    position_frame: usize,
    committed_cycle_count: u64,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
    metronome_available: bool,
    metronome_enabled: bool,
    queued_scene_index: Option<usize>,
    queued_scene_launch_frame: Option<usize>,
    last_launched_scene_index: Option<usize>,
}

impl PlaybackCycleObservation {
    #[must_use]
    pub const fn state(self) -> PlaybackRecoveryTransportState {
        self.state
    }

    #[must_use]
    pub const fn position_frame(self) -> usize {
        self.position_frame
    }

    #[must_use]
    pub const fn committed_cycle_count(self) -> u64 {
        self.committed_cycle_count
    }

    #[must_use]
    pub const fn loop_range(self) -> Option<TransportLoopRange> {
        self.loop_range
    }

    #[must_use]
    pub const fn loop_enabled(self) -> bool {
        self.loop_enabled
    }

    /// Whether this playback session owns a prepared metronome plan.
    #[must_use]
    pub const fn metronome_available(self) -> bool {
        self.metronome_available
    }

    /// Whether the audio thread committed metronome playback for this cycle.
    #[must_use]
    pub const fn metronome_enabled(self) -> bool {
        self.metronome_enabled
    }

    #[must_use]
    pub const fn queued_scene_index(self) -> Option<usize> {
        self.queued_scene_index
    }

    #[must_use]
    pub const fn queued_scene_launch_frame(self) -> Option<usize> {
        self.queued_scene_launch_frame
    }

    #[must_use]
    pub const fn last_launched_scene_index(self) -> Option<usize> {
        self.last_launched_scene_index
    }
}

#[derive(Debug, Default)]
struct AtomicChannelParameters {
    gain: AtomicU32,
    pan: AtomicU32,
    audible: AtomicBool,
}

impl AtomicChannelParameters {
    fn store(&self, parameters: ChannelParameters) {
        self.gain
            .store(parameters.gain.value().to_bits(), Ordering::Relaxed);
        self.pan
            .store(parameters.pan.value().to_bits(), Ordering::Relaxed);
        self.audible.store(parameters.audible, Ordering::Relaxed);
    }

    fn load(&self) -> ChannelParameters {
        let gain = LinearGain::new(f32::from_bits(self.gain.load(Ordering::Relaxed)))
            .expect("a recovery checkpoint stores only validated gain values");
        let pan = Pan::new(f32::from_bits(self.pan.load(Ordering::Relaxed)))
            .expect("a recovery checkpoint stores only validated pan values");
        ChannelParameters::new(gain, pan, self.audible.load(Ordering::Relaxed))
    }
}

#[derive(Debug, Default)]
struct AtomicMixerSendParameterState {
    parameters: AtomicChannelParameters,
    enabled: AtomicBool,
}

impl AtomicMixerSendParameterState {
    fn store(&self, state: MixerSendParameterState) {
        self.parameters.store(state.parameters());
        self.enabled.store(state.enabled(), Ordering::Relaxed);
    }

    fn load(&self) -> MixerSendParameterState {
        MixerSendParameterState::from_recovery_parts(
            self.parameters.load(),
            self.enabled.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Default)]
struct AtomicMixerChannelParameterState {
    parameters: AtomicChannelParameters,
    muted: AtomicBool,
    solo: AtomicBool,
    sends: [AtomicMixerSendParameterState; MAX_MIXER_SENDS_PER_SLOT],
    send_count: AtomicUsize,
}

impl AtomicMixerChannelParameterState {
    fn store(&self, state: MixerChannelParameterState) {
        self.parameters.store(state.parameters());
        self.muted.store(state.muted(), Ordering::Relaxed);
        self.solo.store(state.solo(), Ordering::Relaxed);
        for index in 0..state.send_count() {
            self.sends[index].store(
                state
                    .send(index)
                    .expect("an in-range recovery send must exist"),
            );
        }
        self.send_count.store(state.send_count(), Ordering::Relaxed);
    }

    fn load(&self) -> MixerChannelParameterState {
        let send_count = self.send_count.load(Ordering::Relaxed);
        let mut sends = [default_send_state(); MAX_MIXER_SENDS_PER_SLOT];
        for (index, send) in sends[..send_count].iter_mut().enumerate() {
            *send = self.sends[index].load();
        }
        MixerChannelParameterState::from_recovery_parts(
            self.parameters.load(),
            self.muted.load(Ordering::Relaxed),
            self.solo.load(Ordering::Relaxed),
            sends,
            send_count,
        )
        .expect("a recovery checkpoint stores only bounded send counts")
    }
}

#[derive(Debug)]
struct AtomicMixerParameterState {
    present: AtomicBool,
    layout_signature: [AtomicU64; 4],
    channels: [AtomicMixerChannelParameterState; MAX_MAIN_MIXER_SLOTS],
    channel_count: AtomicUsize,
    main_gain: AtomicU32,
    main_muted: AtomicBool,
}

impl Default for AtomicMixerParameterState {
    fn default() -> Self {
        Self {
            present: AtomicBool::new(false),
            layout_signature: std::array::from_fn(|_| AtomicU64::new(0)),
            channels: std::array::from_fn(|_| AtomicMixerChannelParameterState::default()),
            channel_count: AtomicUsize::new(0),
            main_gain: AtomicU32::new(LinearGain::UNITY.value().to_bits()),
            main_muted: AtomicBool::new(false),
        }
    }
}

impl AtomicMixerParameterState {
    fn store(&self, snapshot: &MixerParameterStateSnapshot) {
        for (target, value) in self
            .layout_signature
            .iter()
            .zip(signature_words(snapshot.layout_signature()))
        {
            target.store(value, Ordering::Relaxed);
        }
        for index in 0..snapshot.channel_count() {
            self.channels[index].store(
                snapshot
                    .channel(index)
                    .expect("an in-range recovery channel must exist"),
            );
        }
        self.channel_count
            .store(snapshot.channel_count(), Ordering::Relaxed);
        self.main_gain
            .store(snapshot.main_gain().value().to_bits(), Ordering::Relaxed);
        self.main_muted
            .store(snapshot.main_muted(), Ordering::Relaxed);
        self.present.store(true, Ordering::Relaxed);
    }

    fn load(&self) -> Option<MixerParameterStateSnapshot> {
        if !self.present.load(Ordering::Relaxed) {
            return None;
        }
        let signature_words = self
            .layout_signature
            .each_ref()
            .map(|word| word.load(Ordering::Relaxed));
        let channel_count = self.channel_count.load(Ordering::Relaxed);
        let mut channels = [default_channel_state(); MAX_MAIN_MIXER_SLOTS];
        for (index, channel) in channels[..channel_count].iter_mut().enumerate() {
            *channel = self.channels[index].load();
        }
        let main_gain = LinearGain::new(f32::from_bits(self.main_gain.load(Ordering::Relaxed)))
            .expect("a recovery checkpoint stores only validated gain values");
        MixerParameterStateSnapshot::from_recovery_parts(
            signature_bytes(signature_words),
            &channels,
            channel_count,
            main_gain,
            self.main_muted.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug)]
struct AtomicNativeDeviceState {
    present: AtomicBool,
    layout_signature: [AtomicU64; 4],
    enabled: [[AtomicBool; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS],
    device_counts: [AtomicUsize; MAX_MAIN_MIXER_SLOTS],
}

impl Default for AtomicNativeDeviceState {
    fn default() -> Self {
        Self {
            present: AtomicBool::new(false),
            layout_signature: std::array::from_fn(|_| AtomicU64::new(0)),
            enabled: std::array::from_fn(|_| {
                std::array::from_fn(|_| AtomicBool::new(false))
            }),
            device_counts: std::array::from_fn(|_| AtomicUsize::new(0)),
        }
    }
}

impl AtomicNativeDeviceState {
    fn store(&self, snapshot: &NativeDeviceStateSnapshot) {
        for (target, value) in self
            .layout_signature
            .iter()
            .zip(signature_words(snapshot.layout_signature()))
        {
            target.store(value, Ordering::Relaxed);
        }
        for slot in 0..MAX_MAIN_MIXER_SLOTS {
            let count = snapshot
                .device_count(slot)
                .expect("a native device state has fixed slot cardinality");
            for device in 0..count {
                self.enabled[slot][device].store(
                    snapshot
                        .enabled(slot, device)
                        .expect("an in-range native device state exists"),
                    Ordering::Relaxed,
                );
            }
            self.device_counts[slot].store(count, Ordering::Relaxed);
        }
        self.present.store(true, Ordering::Relaxed);
    }

    fn load(&self) -> Option<NativeDeviceStateSnapshot> {
        if !self.present.load(Ordering::Relaxed) {
            return None;
        }
        let signature_words = self
            .layout_signature
            .each_ref()
            .map(|word| word.load(Ordering::Relaxed));
        let mut enabled = [[false; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS];
        let mut device_counts = [0; MAX_MAIN_MIXER_SLOTS];
        for slot in 0..MAX_MAIN_MIXER_SLOTS {
            let count = self.device_counts[slot].load(Ordering::Relaxed);
            if count > MAX_DEVICES_PER_CHAIN {
                return None;
            }
            device_counts[slot] = count;
            for (device, state) in enabled[slot].iter_mut().take(count).enumerate() {
                *state = self.enabled[slot][device].load(Ordering::Relaxed);
            }
        }
        NativeDeviceStateSnapshot::from_recovery_parts(
            signature_bytes(signature_words),
            enabled,
            device_counts,
        )
    }
}

const NO_SCENE_INDEX: u8 = u8::MAX;

#[derive(Debug)]
struct AtomicSceneLaunchObservation {
    queued_scene_index: AtomicU8,
    queued_launch_frame: AtomicUsize,
    last_launched_scene_index: AtomicU8,
}

impl AtomicSceneLaunchObservation {
    fn store(
        &self,
        queued_scene_index: Option<usize>,
        queued_launch_frame: Option<usize>,
        last_launched_scene_index: Option<usize>,
    ) {
        self.queued_scene_index.store(
            queued_scene_index.map_or(NO_SCENE_INDEX, |index| {
                u8::try_from(index).expect("a compiled scene index fits in u8")
            }),
            Ordering::Relaxed,
        );
        self.queued_launch_frame
            .store(queued_launch_frame.unwrap_or(0), Ordering::Relaxed);
        self.last_launched_scene_index.store(
            last_launched_scene_index.map_or(NO_SCENE_INDEX, |index| {
                u8::try_from(index).expect("a compiled scene index fits in u8")
            }),
            Ordering::Relaxed,
        );
    }

    fn load(&self) -> (Option<usize>, Option<usize>, Option<usize>) {
        let queued_scene_index =
            decode_scene_index(self.queued_scene_index.load(Ordering::Relaxed));
        let queued_launch_frame =
            queued_scene_index.map(|_| self.queued_launch_frame.load(Ordering::Relaxed));
        let last_launched_scene_index =
            decode_scene_index(self.last_launched_scene_index.load(Ordering::Relaxed));
        (
            queued_scene_index,
            queued_launch_frame,
            last_launched_scene_index,
        )
    }
}

impl Default for AtomicSceneLaunchObservation {
    fn default() -> Self {
        Self {
            queued_scene_index: AtomicU8::new(NO_SCENE_INDEX),
            queued_launch_frame: AtomicUsize::new(0),
            last_launched_scene_index: AtomicU8::new(NO_SCENE_INDEX),
        }
    }
}

#[derive(Debug, Default)]
struct AtomicLoopObservation {
    start_frame: AtomicUsize,
    end_frame: AtomicUsize,
    present: AtomicBool,
    enabled: AtomicBool,
}

impl AtomicLoopObservation {
    fn store(&self, range: Option<TransportLoopRange>, enabled: bool) {
        if let Some(range) = range {
            self.start_frame
                .store(range.start_frame(), Ordering::Relaxed);
            self.end_frame.store(range.end_frame(), Ordering::Relaxed);
        }
        self.present.store(range.is_some(), Ordering::Relaxed);
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn load(&self) -> (Option<TransportLoopRange>, bool) {
        let range = self.present.load(Ordering::Relaxed).then(|| {
            TransportLoopRange::new(
                self.start_frame.load(Ordering::Relaxed),
                self.end_frame.load(Ordering::Relaxed),
            )
            .expect("a recovery checkpoint stores only validated loop ranges")
        });
        (range, self.enabled.load(Ordering::Relaxed))
    }
}

#[derive(Debug, Default)]
struct AtomicMetronomeObservation {
    available: AtomicBool,
    enabled: AtomicBool,
}

impl AtomicMetronomeObservation {
    const fn new(available: bool, enabled: bool) -> Self {
        Self {
            available: AtomicBool::new(available),
            enabled: AtomicBool::new(enabled),
        }
    }

    fn store(&self, available: bool, enabled: bool) {
        self.available.store(available, Ordering::Relaxed);
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    fn load(&self) -> (bool, bool) {
        (
            self.available.load(Ordering::Relaxed),
            self.enabled.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Default)]
struct PlaybackRecoveryState {
    sequence: AtomicU64,
    position_frame: AtomicUsize,
    transport_state: AtomicU8,
    loop_observation: AtomicLoopObservation,
    metronome_observation: AtomicMetronomeObservation,
    scene_launch: AtomicSceneLaunchObservation,
    mixer_parameters: AtomicMixerParameterState,
    native_device_state: AtomicNativeDeviceState,
}

/// Cloneable observer for the last transport cycle committed by the audio thread.
///
/// Meter delivery is deliberately independent and may be coalesced or dropped.
/// This checkpoint therefore publishes the recovery position directly from a
/// successful transport cycle without allocation or locking on the audio thread.
///
/// The observer does not expose publication authority:
///
/// ```compile_fail
/// use hyper_mixer_runtime::playback_recovery_channel;
///
/// let (_, observer) = playback_recovery_channel(None, None);
/// observer.publish_cycle(todo!(), None);
/// ```
#[derive(Clone, Debug, Default)]
pub struct PlaybackRecoveryCheckpoint {
    inner: Arc<PlaybackRecoveryState>,
}

impl PlaybackRecoveryCheckpoint {
    /// Returns one coherent recovery snapshot.
    #[must_use]
    pub fn snapshot(&self) -> PlaybackRecoverySnapshot {
        loop {
            let before = self.inner.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                spin_loop();
                continue;
            }
            let position_frame = self.inner.position_frame.load(Ordering::Relaxed);
            let state = decode_state(self.inner.transport_state.load(Ordering::Relaxed));
            let (loop_range, loop_enabled) = self.inner.loop_observation.load();
            let mixer_parameter_state = self.inner.mixer_parameters.load();
            let native_device_state = self.inner.native_device_state.load();
            fence(Ordering::Acquire);
            let after = self.inner.sequence.load(Ordering::Relaxed);
            if before == after {
                return PlaybackRecoverySnapshot {
                    state,
                    position_frame,
                    committed_cycle_count: after / 2,
                    loop_range,
                    loop_enabled,
                    mixer_parameter_state,
                    native_device_state,
                };
            }
            spin_loop();
        }
    }

    /// Returns one coherent accepted-cycle observation including transient
    /// interactive scene feedback.
    #[must_use]
    pub fn observe_cycle(&self) -> PlaybackCycleObservation {
        loop {
            let before = self.inner.sequence.load(Ordering::Acquire);
            if before & 1 != 0 {
                spin_loop();
                continue;
            }
            let position_frame = self.inner.position_frame.load(Ordering::Relaxed);
            let state = decode_state(self.inner.transport_state.load(Ordering::Relaxed));
            let (loop_range, loop_enabled) = self.inner.loop_observation.load();
            let (metronome_available, metronome_enabled) = self.inner.metronome_observation.load();
            let (queued_scene_index, queued_scene_launch_frame, last_launched_scene_index) =
                self.inner.scene_launch.load();
            fence(Ordering::Acquire);
            let after = self.inner.sequence.load(Ordering::Relaxed);
            if before == after {
                return PlaybackCycleObservation {
                    state,
                    position_frame,
                    committed_cycle_count: after / 2,
                    loop_range,
                    loop_enabled,
                    metronome_available,
                    metronome_enabled,
                    queued_scene_index,
                    queued_scene_launch_frame,
                    last_launched_scene_index,
                };
            }
            spin_loop();
        }
    }
}

/// Sole publisher for recovery cycles accepted by the native audio output.
///
/// This capability is intentionally not cloneable. Its mutable publication
/// method structurally preserves the checkpoint's single-writer seqlock
/// invariant while observers remain freely cloneable.
///
/// ```compile_fail
/// use hyper_mixer_runtime::playback_recovery_channel;
///
/// let (publisher, _) = playback_recovery_channel(None, None);
/// let duplicate = publisher.clone();
/// ```
#[derive(Debug)]
pub struct PlaybackRecoveryPublisher {
    inner: Arc<PlaybackRecoveryState>,
}

#[derive(Clone, Copy)]
struct PlaybackCyclePublication {
    state: PlaybackRecoveryTransportState,
    position_frame: usize,
    loop_range: Option<TransportLoopRange>,
    loop_enabled: bool,
    metronome_available: bool,
    metronome_enabled: bool,
    queued_scene_index: Option<usize>,
    queued_scene_launch_frame: Option<usize>,
    last_launched_scene_index: Option<usize>,
}

impl PlaybackRecoveryPublisher {
    /// Publishes one committed transport cycle and its optional mixer state.
    pub fn publish_cycle(
        &mut self,
        cycle: TransportCycle,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    ) {
        self.publish_cycle_with_native_device_state(cycle, mixer_parameter_state, None);
    }

    /// Publishes one committed transport cycle with any newly committed
    /// session-only native-device bypass state.
    pub fn publish_cycle_with_native_device_state(
        &mut self,
        cycle: TransportCycle,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
        native_device_state: Option<&NativeDeviceStateSnapshot>,
    ) {
        let scene = cycle.scene_launch_state();
        self.publish_observation(
            PlaybackCyclePublication {
                state: match cycle.state() {
                TransportState::Stopped => PlaybackRecoveryTransportState::Stopped,
                TransportState::Rolling => PlaybackRecoveryTransportState::Rolling,
                },
                position_frame: cycle.position_frame(),
                loop_range: cycle.loop_range(),
                loop_enabled: cycle.loop_enabled(),
                metronome_available: cycle.metronome_available(),
                metronome_enabled: cycle.metronome_enabled(),
                queued_scene_index: scene.queued_scene_index(),
                queued_scene_launch_frame: scene.queued_launch_frame(),
                last_launched_scene_index: scene.last_launched_scene_index(),
            },
            mixer_parameter_state,
            native_device_state,
        );
    }

    #[cfg(test)]
    fn publish(
        &mut self,
        state: PlaybackRecoveryTransportState,
        position_frame: usize,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    ) {
        self.publish_observation(
            PlaybackCyclePublication {
                state,
                position_frame,
                loop_range: None,
                loop_enabled: false,
                metronome_available: false,
                metronome_enabled: false,
                queued_scene_index: None,
                queued_scene_launch_frame: None,
                last_launched_scene_index: None,
            },
            mixer_parameter_state,
            None,
        );
    }

    fn publish_observation(
        &mut self,
        publication: PlaybackCyclePublication,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
        native_device_state: Option<&NativeDeviceStateSnapshot>,
    ) {
        let odd = self.inner.sequence.fetch_add(1, Ordering::Acquire) | 1;
        fence(Ordering::Release);
        self.inner
            .position_frame
            .store(publication.position_frame, Ordering::Relaxed);
        self.inner
            .transport_state
            .store(publication.state as u8, Ordering::Relaxed);
        self.inner
            .loop_observation
            .store(publication.loop_range, publication.loop_enabled);
        self.inner
            .metronome_observation
            .store(publication.metronome_available, publication.metronome_enabled);
        self.inner.scene_launch.store(
            publication.queued_scene_index,
            publication.queued_scene_launch_frame,
            publication.last_launched_scene_index,
        );
        if let Some(mixer_parameter_state) = mixer_parameter_state {
            self.inner.mixer_parameters.store(mixer_parameter_state);
        }
        if let Some(native_device_state) = native_device_state {
            self.inner.native_device_state.store(native_device_state);
        }
        self.inner
            .sequence
            .store(odd.wrapping_add(1), Ordering::Release);
    }
}

/// Creates one single-writer recovery publisher and its cloneable observer.
///
/// A mixer state retained by `snapshot` takes precedence. Otherwise the
/// supplied initial state is observable without counting a synthetic transport
/// cycle, allowing controls to inspect the runtime's compiled base values before
/// the first native callback.
#[must_use]
pub fn playback_recovery_channel(
    snapshot: Option<&PlaybackRecoverySnapshot>,
    initial_mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
) -> (PlaybackRecoveryPublisher, PlaybackRecoveryCheckpoint) {
    playback_recovery_channel_with_initial_loop(
        snapshot,
        initial_mixer_parameter_state,
        None,
        false,
    )
}

/// Creates a recovery channel whose zero-cycle observation carries the
/// transport's already-validated initial loop state.
#[must_use]
pub fn playback_recovery_channel_with_initial_loop(
    snapshot: Option<&PlaybackRecoverySnapshot>,
    initial_mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    initial_loop_range: Option<TransportLoopRange>,
    initial_loop_enabled: bool,
) -> (PlaybackRecoveryPublisher, PlaybackRecoveryCheckpoint) {
    playback_recovery_channel_with_initial_loop_and_metronome(
        snapshot,
        initial_mixer_parameter_state,
        initial_loop_range,
        initial_loop_enabled,
        false,
        false,
    )
}

/// Creates a recovery channel whose zero-cycle observation also carries the
/// session-only metronome capability and state.
#[must_use]
pub fn playback_recovery_channel_with_initial_loop_and_metronome(
    snapshot: Option<&PlaybackRecoverySnapshot>,
    initial_mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    initial_loop_range: Option<TransportLoopRange>,
    initial_loop_enabled: bool,
    initial_metronome_available: bool,
    initial_metronome_enabled: bool,
) -> (PlaybackRecoveryPublisher, PlaybackRecoveryCheckpoint) {
    playback_recovery_channel_with_initial_loop_metronome_and_native_device_state(
        snapshot,
        initial_mixer_parameter_state,
        None,
        initial_loop_range,
        initial_loop_enabled,
        initial_metronome_available,
        initial_metronome_enabled,
    )
}

/// Creates a recovery channel with session-only initial native-device bypass
/// state. A previously committed recovery snapshot takes precedence over the
/// supplied initial state.
#[must_use]
pub fn playback_recovery_channel_with_initial_loop_metronome_and_native_device_state(
    snapshot: Option<&PlaybackRecoverySnapshot>,
    initial_mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    initial_native_device_state: Option<&NativeDeviceStateSnapshot>,
    initial_loop_range: Option<TransportLoopRange>,
    initial_loop_enabled: bool,
    initial_metronome_available: bool,
    initial_metronome_enabled: bool,
) -> (PlaybackRecoveryPublisher, PlaybackRecoveryCheckpoint) {
    let mixer_parameters = AtomicMixerParameterState::default();
    if let Some(state) = snapshot
        .and_then(|snapshot| snapshot.mixer_parameter_state.as_ref())
        .or(initial_mixer_parameter_state)
    {
        mixer_parameters.store(state);
    }
    let native_device_state = AtomicNativeDeviceState::default();
    if let Some(state) = snapshot
        .and_then(|snapshot| snapshot.native_device_state.as_ref())
        .or(initial_native_device_state)
    {
        native_device_state.store(state);
    }
    let inner = Arc::new(PlaybackRecoveryState {
        sequence: AtomicU64::new(snapshot.map_or(0, |snapshot| snapshot.committed_cycle_count * 2)),
        position_frame: AtomicUsize::new(snapshot.map_or(0, |snapshot| snapshot.position_frame)),
        transport_state: AtomicU8::new(
            snapshot.map_or(PlaybackRecoveryTransportState::Stopped as u8, |snapshot| {
                snapshot.state as u8
            }),
        ),
        loop_observation: {
            let observation = AtomicLoopObservation::default();
            let (range, enabled) = snapshot.map_or(
                (initial_loop_range, initial_loop_enabled),
                |snapshot| (snapshot.loop_range, snapshot.loop_enabled),
            );
            observation.store(range, enabled);
            observation
        },
        metronome_observation: AtomicMetronomeObservation::new(
            initial_metronome_available,
            initial_metronome_enabled,
        ),
        scene_launch: AtomicSceneLaunchObservation::default(),
        mixer_parameters,
        native_device_state,
    });
    (
        PlaybackRecoveryPublisher {
            inner: Arc::clone(&inner),
        },
        PlaybackRecoveryCheckpoint { inner },
    )
}

fn default_send_state() -> MixerSendParameterState {
    MixerSendParameterState::from_recovery_parts(
        ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
        false,
    )
}

fn default_channel_state() -> MixerChannelParameterState {
    MixerChannelParameterState::from_recovery_parts(
        ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
        false,
        false,
        [default_send_state(); MAX_MIXER_SENDS_PER_SLOT],
        0,
    )
    .expect("the default recovery channel has a bounded send count")
}

fn signature_words(signature: [u8; 32]) -> [u64; 4] {
    std::array::from_fn(|index| {
        let start = index * 8;
        u64::from_le_bytes(
            signature[start..start + 8]
                .try_into()
                .expect("a signature word is eight bytes"),
        )
    })
}

fn signature_bytes(words: [u64; 4]) -> [u8; 32] {
    let mut signature = [0_u8; 32];
    for (index, word) in words.into_iter().enumerate() {
        let start = index * 8;
        signature[start..start + 8].copy_from_slice(&word.to_le_bytes());
    }
    signature
}

fn decode_state(value: u8) -> PlaybackRecoveryTransportState {
    if value == PlaybackRecoveryTransportState::Rolling as u8 {
        PlaybackRecoveryTransportState::Rolling
    } else {
        PlaybackRecoveryTransportState::Stopped
    }
}

fn decode_scene_index(value: u8) -> Option<usize> {
    (value != NO_SCENE_INDEX).then_some(usize::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    fn mixer_snapshot(marker: u8, main_muted: bool) -> MixerParameterStateSnapshot {
        let channels = std::array::from_fn(|channel| {
            let channel_value = f32::from(u8::try_from(channel).unwrap());
            let sends = std::array::from_fn(|send| {
                let send_value = f32::from(u8::try_from(send).unwrap());
                let gain = LinearGain::new(
                    0.25 + f32::from(marker) * 0.01 + channel_value * 0.01 + send_value * 0.001,
                )
                .unwrap();
                let pan = Pan::new(-0.5 + channel_value * 0.05 + send_value * 0.01).unwrap();
                MixerSendParameterState::from_recovery_parts(
                    ChannelParameters::new(gain, pan, (channel + send).is_multiple_of(2)),
                    (usize::from(marker) + channel + send).is_multiple_of(2),
                )
            });
            MixerChannelParameterState::from_recovery_parts(
                ChannelParameters::new(
                    LinearGain::new(0.5 + f32::from(marker) * 0.01 + channel_value * 0.01).unwrap(),
                    Pan::new(-0.25 + channel_value * 0.05).unwrap(),
                    channel.is_multiple_of(2),
                ),
                (usize::from(marker) + channel).is_multiple_of(2),
                (usize::from(marker) + channel).is_multiple_of(3),
                sends,
                MAX_MIXER_SENDS_PER_SLOT,
            )
            .unwrap()
        });
        MixerParameterStateSnapshot::from_recovery_parts(
            [marker; 32],
            &channels,
            MAX_MAIN_MIXER_SLOTS,
            LinearGain::new(0.75 + f32::from(marker) * 0.01).unwrap(),
            main_muted,
        )
        .unwrap()
    }

    fn native_device_state(enabled: bool) -> NativeDeviceStateSnapshot {
        let mut states = [[false; MAX_DEVICES_PER_CHAIN]; MAX_MAIN_MIXER_SLOTS];
        states[0][0] = enabled;
        NativeDeviceStateSnapshot::from_recovery_parts(
            [9; 32],
            states,
            [1, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap()
    }

    fn transport_cycle_for_test() -> TransportCycle {
        TransportCycle {
            state: TransportState::Rolling,
            start_frame: 0,
            position_frame: 128,
            frame_count: 128,
            loop_wrap_count: 0,
            loop_range: None,
            loop_enabled: false,
            metronome_available: false,
            metronome_enabled: false,
            active_slot_count: 0,
            scene_launch_state: hyper_playback_compiler::scene::SceneLaunchState::default(),
            applied_command_count: 0,
            command_sender_disconnected: false,
            applied_mixer_parameter_command_count: 0,
            mixer_parameter_command_sender_disconnected: false,
            applied_native_device_command_count: 1,
            native_device_command_sender_disconnected: false,
        }
    }

    #[test]
    fn checkpoint_starts_stopped_at_origin() {
        assert_eq!(
            PlaybackRecoveryCheckpoint::default().snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 0,
                committed_cycle_count: 0,
                loop_range: None,
                loop_enabled: false,
                mixer_parameter_state: None,
                native_device_state: None,
            }
        );
        assert_eq!(
            PlaybackRecoveryCheckpoint::default().observe_cycle(),
            PlaybackCycleObservation {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 0,
                committed_cycle_count: 0,
                loop_range: None,
                loop_enabled: false,
                metronome_available: false,
                metronome_enabled: false,
                queued_scene_index: None,
                queued_scene_launch_frame: None,
                last_launched_scene_index: None,
            }
        );
    }

    #[test]
    fn zero_cycle_observation_exposes_session_only_metronome_capability() {
        let (_publisher, checkpoint) = playback_recovery_channel_with_initial_loop_and_metronome(
            None,
            None,
            None,
            false,
            true,
            false,
        );

        let observation = checkpoint.observe_cycle();
        assert_eq!(observation.committed_cycle_count(), 0);
        assert!(observation.metronome_available());
        assert!(!observation.metronome_enabled());
        assert!(!checkpoint.snapshot().loop_enabled());
    }

    #[test]
    fn publication_updates_state_position_and_count() {
        let (mut publisher, checkpoint) = playback_recovery_channel(None, None);
        publisher.publish(PlaybackRecoveryTransportState::Rolling, 256, None);
        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 256,
                committed_cycle_count: 1,
                loop_range: None,
                loop_enabled: false,
                mixer_parameter_state: None,
                native_device_state: None,
            }
        );
        assert_eq!(
            checkpoint.snapshot().restore_command(),
            TransportCommand::Locate {
                frame: 256,
                state: LocateState::Rolling,
            }
        );
        publisher.publish(PlaybackRecoveryTransportState::Stopped, 256, None);
        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 256,
                committed_cycle_count: 2,
                loop_range: None,
                loop_enabled: false,
                mixer_parameter_state: None,
                native_device_state: None,
            }
        );
        assert_eq!(
            checkpoint.snapshot().restore_command(),
            TransportCommand::Locate {
                frame: 256,
                state: LocateState::Stopped,
            }
        );
    }

    #[test]
    fn mixer_state_updates_only_with_a_committed_change_and_then_remains_current() {
        let (mut publisher, checkpoint) = playback_recovery_channel(None, None);
        let mixer = mixer_snapshot(7, true);

        publisher.publish(PlaybackRecoveryTransportState::Rolling, 128, Some(&mixer));
        assert_eq!(checkpoint.snapshot().mixer_parameter_state(), Some(mixer));

        publisher.publish(PlaybackRecoveryTransportState::Rolling, 256, None);
        let unchanged = checkpoint.snapshot();
        assert_eq!(unchanged.position_frame(), 256);
        assert_eq!(unchanged.mixer_parameter_state(), Some(mixer));
    }

    #[test]
    fn native_device_state_updates_only_with_a_committed_change_and_recovers_initial_state() {
        let initial = native_device_state(true);
        let (mut publisher, checkpoint) =
            playback_recovery_channel_with_initial_loop_metronome_and_native_device_state(
                None,
                None,
                Some(&initial),
                None,
                false,
                false,
                false,
            );
        assert_eq!(checkpoint.snapshot().native_device_state(), Some(initial));

        let changed = native_device_state(false);
        publisher.publish_cycle_with_native_device_state(
            transport_cycle_for_test(),
            None,
            Some(&changed),
        );
        assert_eq!(checkpoint.snapshot().native_device_state(), Some(changed));

        publisher.publish(PlaybackRecoveryTransportState::Rolling, 256, None);
        assert_eq!(checkpoint.snapshot().native_device_state(), Some(changed));
    }

    #[test]
    fn concurrent_snapshots_do_not_mix_publications() {
        const PUBLICATIONS: usize = 20_000;
        let (mut publisher, checkpoint) = playback_recovery_channel(None, None);
        let barrier = Arc::new(Barrier::new(2));
        let writer_barrier = barrier.clone();
        let writer = thread::spawn(move || {
            writer_barrier.wait();
            for generation in 1..=PUBLICATIONS {
                let state = if generation.is_multiple_of(2) {
                    PlaybackRecoveryTransportState::Stopped
                } else {
                    PlaybackRecoveryTransportState::Rolling
                };
                let mixer = mixer_snapshot(
                    u8::from(generation.is_multiple_of(2)),
                    generation.is_multiple_of(2),
                );
                let scene = (!generation.is_multiple_of(2)).then_some((
                    generation % 8,
                    generation * 2,
                    (generation + 7) % 8,
                ));
                let loop_range = (!generation.is_multiple_of(4))
                    .then(|| TransportLoopRange::new(generation, generation + 10).unwrap());
                let loop_enabled = generation % 4 == 1;
                publisher.publish_observation(
                    PlaybackCyclePublication {
                        state,
                        position_frame: generation,
                        loop_range,
                        loop_enabled,
                        metronome_available: !generation.is_multiple_of(3),
                        metronome_enabled: generation.is_multiple_of(5),
                        queued_scene_index: scene.map(|value| value.0),
                        queued_scene_launch_frame: scene.map(|value| value.1),
                        last_launched_scene_index: scene.map(|value| value.2),
                    },
                    Some(&mixer),
                    None,
                );
            }
        });

        barrier.wait();
        while checkpoint.snapshot().committed_cycle_count() < PUBLICATIONS as u64 {
            let observation = checkpoint.observe_cycle();
            if observation.committed_cycle_count() > 0 {
                let generation = observation.position_frame();
                assert_eq!(observation.committed_cycle_count(), generation as u64);
                let expected_state = if generation.is_multiple_of(2) {
                    PlaybackRecoveryTransportState::Stopped
                } else {
                    PlaybackRecoveryTransportState::Rolling
                };
                assert_eq!(observation.state(), expected_state);
                let expected_loop = (!generation.is_multiple_of(4))
                    .then(|| TransportLoopRange::new(generation, generation + 10).unwrap());
                assert_eq!(observation.loop_range(), expected_loop);
                assert_eq!(observation.loop_enabled(), generation % 4 == 1);
                assert_eq!(
                    observation.metronome_available(),
                    !generation.is_multiple_of(3)
                );
                assert_eq!(observation.metronome_enabled(), generation.is_multiple_of(5));
                let expected_scene = (!generation.is_multiple_of(2)).then_some((
                    generation % 8,
                    generation * 2,
                    (generation + 7) % 8,
                ));
                assert_eq!(
                    observation.queued_scene_index(),
                    expected_scene.map(|value| value.0)
                );
                assert_eq!(
                    observation.queued_scene_launch_frame(),
                    expected_scene.map(|value| value.1)
                );
                assert_eq!(
                    observation.last_launched_scene_index(),
                    expected_scene.map(|value| value.2)
                );
            }
            let snapshot = checkpoint.snapshot();
            let generation = snapshot.position_frame();
            let expected = if generation.is_multiple_of(2) {
                PlaybackRecoveryTransportState::Stopped
            } else {
                PlaybackRecoveryTransportState::Rolling
            };
            assert_eq!(snapshot.state(), expected);
            if snapshot.committed_cycle_count() > 0 {
                assert_eq!(snapshot.committed_cycle_count(), generation as u64);
                let expected_loop = (!generation.is_multiple_of(4))
                    .then(|| TransportLoopRange::new(generation, generation + 10).unwrap());
                assert_eq!(snapshot.loop_range(), expected_loop);
                assert_eq!(snapshot.loop_enabled(), generation % 4 == 1);
                let mixer = snapshot.mixer_parameter_state().unwrap();
                let even = generation.is_multiple_of(2);
                assert_eq!(mixer, mixer_snapshot(u8::from(even), even));
            }
        }
        writer.join().unwrap();
    }

    #[test]
    fn checkpoint_can_resume_an_existing_publication_sequence() {
        let mixer = mixer_snapshot(9, true);
        let snapshot = PlaybackRecoverySnapshot {
            state: PlaybackRecoveryTransportState::Rolling,
            position_frame: 512,
            committed_cycle_count: 7,
            loop_range: Some(TransportLoopRange::new(128, 640).unwrap()),
            loop_enabled: true,
            mixer_parameter_state: Some(mixer),
            native_device_state: None,
        };
        let different_initial = mixer_snapshot(3, false);
        let (mut publisher, checkpoint) =
            playback_recovery_channel(Some(&snapshot), Some(&different_initial));

        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 512,
                committed_cycle_count: 7,
                loop_range: Some(TransportLoopRange::new(128, 640).unwrap()),
                loop_enabled: true,
                mixer_parameter_state: Some(mixer),
                native_device_state: None,
            }
        );
        publisher.publish(PlaybackRecoveryTransportState::Rolling, 640, None);
        assert_eq!(checkpoint.snapshot().committed_cycle_count(), 8);
    }

    #[test]
    fn checkpoint_can_expose_initial_mixer_state_without_a_committed_cycle() {
        let mixer = mixer_snapshot(5, false);
        let (_publisher, checkpoint) = playback_recovery_channel(None, Some(&mixer));

        let snapshot = checkpoint.snapshot();
        assert_eq!(snapshot.state(), PlaybackRecoveryTransportState::Stopped);
        assert_eq!(snapshot.position_frame(), 0);
        assert_eq!(snapshot.committed_cycle_count(), 0);
        assert_eq!(snapshot.mixer_parameter_state(), Some(mixer));
    }
}
