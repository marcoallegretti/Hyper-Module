use super::{
    LocateState, MAX_MAIN_MIXER_SLOTS, MAX_MIXER_SENDS_PER_SLOT, MixerChannelParameterState,
    MixerParameterStateSnapshot, MixerSendParameterState, TransportCommand, TransportCycle,
    TransportState,
};
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
    mixer_parameter_state: Option<MixerParameterStateSnapshot>,
}

impl PlaybackRecoverySnapshot {
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
    pub const fn mixer_parameter_state(&self) -> Option<MixerParameterStateSnapshot> {
        self.mixer_parameter_state
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

#[derive(Debug, Default)]
struct PlaybackRecoveryState {
    sequence: AtomicU64,
    position_frame: AtomicUsize,
    transport_state: AtomicU8,
    mixer_parameters: AtomicMixerParameterState,
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
/// let (_, observer) = playback_recovery_channel(None);
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
            let mixer_parameter_state = self.inner.mixer_parameters.load();
            fence(Ordering::Acquire);
            let after = self.inner.sequence.load(Ordering::Relaxed);
            if before == after {
                return PlaybackRecoverySnapshot {
                    state,
                    position_frame,
                    committed_cycle_count: after / 2,
                    mixer_parameter_state,
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
/// let (publisher, _) = playback_recovery_channel(None);
/// let duplicate = publisher.clone();
/// ```
#[derive(Debug)]
pub struct PlaybackRecoveryPublisher {
    inner: Arc<PlaybackRecoveryState>,
}

impl PlaybackRecoveryPublisher {
    /// Publishes one committed transport cycle and its optional mixer state.
    pub fn publish_cycle(
        &mut self,
        cycle: TransportCycle,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    ) {
        self.publish(
            match cycle.state() {
                TransportState::Stopped => PlaybackRecoveryTransportState::Stopped,
                TransportState::Rolling => PlaybackRecoveryTransportState::Rolling,
            },
            cycle.position_frame(),
            mixer_parameter_state,
        );
    }

    fn publish(
        &mut self,
        state: PlaybackRecoveryTransportState,
        position_frame: usize,
        mixer_parameter_state: Option<&MixerParameterStateSnapshot>,
    ) {
        let odd = self.inner.sequence.fetch_add(1, Ordering::Acquire) | 1;
        fence(Ordering::Release);
        self.inner
            .position_frame
            .store(position_frame, Ordering::Relaxed);
        self.inner
            .transport_state
            .store(state as u8, Ordering::Relaxed);
        if let Some(mixer_parameter_state) = mixer_parameter_state {
            self.inner.mixer_parameters.store(mixer_parameter_state);
        }
        self.inner
            .sequence
            .store(odd.wrapping_add(1), Ordering::Release);
    }
}

/// Creates one single-writer recovery publisher and its cloneable observer.
#[must_use]
pub fn playback_recovery_channel(
    snapshot: Option<&PlaybackRecoverySnapshot>,
) -> (PlaybackRecoveryPublisher, PlaybackRecoveryCheckpoint) {
    let mixer_parameters = AtomicMixerParameterState::default();
    if let Some(state) = snapshot.and_then(|snapshot| snapshot.mixer_parameter_state.as_ref()) {
        mixer_parameters.store(state);
    }
    let inner = Arc::new(PlaybackRecoveryState {
        sequence: AtomicU64::new(snapshot.map_or(0, |snapshot| snapshot.committed_cycle_count * 2)),
        position_frame: AtomicUsize::new(snapshot.map_or(0, |snapshot| snapshot.position_frame)),
        transport_state: AtomicU8::new(
            snapshot.map_or(PlaybackRecoveryTransportState::Stopped as u8, |snapshot| {
                snapshot.state as u8
            }),
        ),
        mixer_parameters,
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

    #[test]
    fn checkpoint_starts_stopped_at_origin() {
        assert_eq!(
            PlaybackRecoveryCheckpoint::default().snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 0,
                committed_cycle_count: 0,
                mixer_parameter_state: None,
            }
        );
    }

    #[test]
    fn publication_updates_state_position_and_count() {
        let (mut publisher, checkpoint) = playback_recovery_channel(None);
        publisher.publish(PlaybackRecoveryTransportState::Rolling, 256, None);
        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 256,
                committed_cycle_count: 1,
                mixer_parameter_state: None,
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
                mixer_parameter_state: None,
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
        let (mut publisher, checkpoint) = playback_recovery_channel(None);
        let mixer = mixer_snapshot(7, true);

        publisher.publish(PlaybackRecoveryTransportState::Rolling, 128, Some(&mixer));
        assert_eq!(checkpoint.snapshot().mixer_parameter_state(), Some(mixer));

        publisher.publish(PlaybackRecoveryTransportState::Rolling, 256, None);
        let unchanged = checkpoint.snapshot();
        assert_eq!(unchanged.position_frame(), 256);
        assert_eq!(unchanged.mixer_parameter_state(), Some(mixer));
    }

    #[test]
    fn concurrent_snapshots_do_not_mix_publications() {
        const PUBLICATIONS: usize = 20_000;
        let (mut publisher, checkpoint) = playback_recovery_channel(None);
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
                publisher.publish(state, generation, Some(&mixer));
            }
        });

        barrier.wait();
        while checkpoint.snapshot().committed_cycle_count() < PUBLICATIONS as u64 {
            let snapshot = checkpoint.snapshot();
            let expected = if snapshot.position_frame().is_multiple_of(2) {
                PlaybackRecoveryTransportState::Stopped
            } else {
                PlaybackRecoveryTransportState::Rolling
            };
            assert_eq!(snapshot.state(), expected);
            if snapshot.committed_cycle_count() > 0 {
                let mixer = snapshot.mixer_parameter_state().unwrap();
                let even = snapshot.position_frame().is_multiple_of(2);
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
            mixer_parameter_state: Some(mixer),
        };
        let (mut publisher, checkpoint) = playback_recovery_channel(Some(&snapshot));

        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 512,
                committed_cycle_count: 7,
                mixer_parameter_state: Some(mixer),
            }
        );
        publisher.publish(PlaybackRecoveryTransportState::Rolling, 640, None);
        assert_eq!(checkpoint.snapshot().committed_cycle_count(), 8);
    }
}
