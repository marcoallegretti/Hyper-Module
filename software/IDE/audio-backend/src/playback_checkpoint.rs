use core::hint::spin_loop;
use core::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering, fence};
use hyper_mixer_runtime::{LocateState, TransportCommand, TransportCycle, TransportState};
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
}

impl PlaybackRecoverySnapshot {
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

    pub(crate) const fn restore_command(self) -> TransportCommand {
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
struct PlaybackRecoveryState {
    sequence: AtomicU64,
    position_frame: AtomicUsize,
    transport_state: AtomicU8,
}

/// Cloneable observer for the last transport cycle committed by the audio thread.
///
/// Meter delivery is deliberately independent and may be coalesced or dropped.
/// This checkpoint therefore publishes the recovery position directly from a
/// successful transport cycle without allocation or locking on the audio thread.
#[derive(Clone, Debug, Default)]
pub struct PlaybackRecoveryCheckpoint {
    inner: Arc<PlaybackRecoveryState>,
}

impl PlaybackRecoveryCheckpoint {
    pub(crate) fn from_snapshot(snapshot: PlaybackRecoverySnapshot) -> Self {
        Self {
            inner: Arc::new(PlaybackRecoveryState {
                sequence: AtomicU64::new(snapshot.committed_cycle_count * 2),
                position_frame: AtomicUsize::new(snapshot.position_frame),
                transport_state: AtomicU8::new(snapshot.state as u8),
            }),
        }
    }

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
            fence(Ordering::Acquire);
            let after = self.inner.sequence.load(Ordering::Relaxed);
            if before == after {
                return PlaybackRecoverySnapshot {
                    state,
                    position_frame,
                    committed_cycle_count: after / 2,
                };
            }
            spin_loop();
        }
    }

    pub(crate) fn publish_cycle(&self, cycle: TransportCycle) {
        self.publish(
            match cycle.state() {
                TransportState::Stopped => PlaybackRecoveryTransportState::Stopped,
                TransportState::Rolling => PlaybackRecoveryTransportState::Rolling,
            },
            cycle.position_frame(),
        );
    }

    fn publish(&self, state: PlaybackRecoveryTransportState, position_frame: usize) {
        let odd = self.inner.sequence.fetch_add(1, Ordering::Acquire) | 1;
        fence(Ordering::Release);
        self.inner
            .position_frame
            .store(position_frame, Ordering::Relaxed);
        self.inner
            .transport_state
            .store(state as u8, Ordering::Relaxed);
        self.inner
            .sequence
            .store(odd.wrapping_add(1), Ordering::Release);
    }
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

    #[test]
    fn checkpoint_starts_stopped_at_origin() {
        assert_eq!(
            PlaybackRecoveryCheckpoint::default().snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 0,
                committed_cycle_count: 0,
            }
        );
    }

    #[test]
    fn publication_updates_state_position_and_count() {
        let checkpoint = PlaybackRecoveryCheckpoint::default();
        checkpoint.publish(PlaybackRecoveryTransportState::Rolling, 256);
        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 256,
                committed_cycle_count: 1,
            }
        );
        assert_eq!(
            checkpoint.snapshot().restore_command(),
            TransportCommand::Locate {
                frame: 256,
                state: LocateState::Rolling,
            }
        );
        checkpoint.publish(PlaybackRecoveryTransportState::Stopped, 256);
        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Stopped,
                position_frame: 256,
                committed_cycle_count: 2,
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
    fn concurrent_snapshots_do_not_mix_publications() {
        const PUBLICATIONS: usize = 20_000;
        let checkpoint = PlaybackRecoveryCheckpoint::default();
        let writer_checkpoint = checkpoint.clone();
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
                writer_checkpoint.publish(state, generation);
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
        }
        writer.join().unwrap();
    }

    #[test]
    fn checkpoint_can_resume_an_existing_publication_sequence() {
        let checkpoint = PlaybackRecoveryCheckpoint::from_snapshot(PlaybackRecoverySnapshot {
            state: PlaybackRecoveryTransportState::Rolling,
            position_frame: 512,
            committed_cycle_count: 7,
        });

        assert_eq!(
            checkpoint.snapshot(),
            PlaybackRecoverySnapshot {
                state: PlaybackRecoveryTransportState::Rolling,
                position_frame: 512,
                committed_cycle_count: 7,
            }
        );
        checkpoint.publish(PlaybackRecoveryTransportState::Rolling, 640);
        assert_eq!(checkpoint.snapshot().committed_cycle_count(), 8);
    }
}
