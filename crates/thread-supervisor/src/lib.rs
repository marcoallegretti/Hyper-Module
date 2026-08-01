#![doc = "Bounded ownership and completion-aware reaping for native threads."]

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadSupervisorSnapshot {
    pub capacity: usize,
    pub active: usize,
    pub pending: usize,
    pub reaped: usize,
    pub panicked: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThreadCapacityExhausted {
    pub capacity: usize,
}

impl fmt::Display for ThreadCapacityExhausted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "native thread capacity of {} is exhausted",
            self.capacity
        )
    }
}

impl std::error::Error for ThreadCapacityExhausted {}

struct PendingThread<T> {
    handle: JoinHandle<T>,
    _permit: ThreadPermit<T>,
}

struct SupervisorState<T> {
    capacity: usize,
    active: AtomicUsize,
    pending_count: AtomicUsize,
    reaped: AtomicUsize,
    panicked: AtomicUsize,
    shutdown_requested: AtomicBool,
    pending: Mutex<Vec<PendingThread<T>>>,
    wake: Condvar,
}

pub struct ThreadSupervisor<T> {
    state: Arc<SupervisorState<T>>,
    reaper: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> fmt::Debug for ThreadSupervisor<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadSupervisor")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl<T: Send + 'static> ThreadSupervisor<T> {
    /// Starts one bounded, completion-aware thread owner.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when `capacity` is zero or the supervisor thread
    /// cannot be created.
    pub fn new(
        thread_name: impl Into<String>,
        capacity: usize,
        poll_interval: Duration,
    ) -> io::Result<Self> {
        if capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread supervisor capacity must be positive",
            ));
        }
        if poll_interval.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "thread supervisor poll interval must be positive",
            ));
        }
        let state = Arc::new(SupervisorState {
            capacity,
            active: AtomicUsize::new(0),
            pending_count: AtomicUsize::new(0),
            reaped: AtomicUsize::new(0),
            panicked: AtomicUsize::new(0),
            shutdown_requested: AtomicBool::new(false),
            pending: Mutex::new(Vec::with_capacity(capacity)),
            wake: Condvar::new(),
        });
        let supervisor_state = Arc::clone(&state);
        let reaper = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || supervise(&supervisor_state, poll_interval))?;
        Ok(Self {
            state,
            reaper: Some(reaper),
        })
    }

    /// Reserves one slot before a native thread is created.
    ///
    /// # Errors
    ///
    /// Returns [`ThreadCapacityExhausted`] when every bounded slot is active.
    pub fn acquire(&self) -> Result<ThreadPermit<T>, ThreadCapacityExhausted> {
        self.state
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.state.capacity).then_some(active + 1)
            })
            .map_err(|_| ThreadCapacityExhausted {
                capacity: self.state.capacity,
            })?;
        Ok(ThreadPermit {
            state: Arc::clone(&self.state),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> ThreadSupervisorSnapshot {
        ThreadSupervisorSnapshot {
            capacity: self.state.capacity,
            active: self.state.active.load(Ordering::Acquire),
            pending: self.state.pending_count.load(Ordering::Acquire),
            reaped: self.state.reaped.load(Ordering::Acquire),
            panicked: self.state.panicked.load(Ordering::Acquire),
        }
    }

    /// Stops admission, drains every issued permit and retained thread, and
    /// joins the supervisor thread.
    ///
    /// # Errors
    ///
    /// Returns the supervisor thread's panic payload if the reaper panicked.
    pub fn shutdown(mut self) -> thread::Result<ThreadSupervisorSnapshot> {
        self.shutdown_and_join()?;
        Ok(self.snapshot())
    }
}

impl<T> ThreadSupervisor<T> {
    fn shutdown_and_join(&mut self) -> thread::Result<()> {
        self.state.shutdown_requested.store(true, Ordering::Release);
        self.state.wake.notify_all();
        match self.reaper.take() {
            Some(reaper) => reaper.join(),
            None => Ok(()),
        }
    }
}

impl<T> Drop for ThreadSupervisor<T> {
    fn drop(&mut self) {
        let _ = self.shutdown_and_join();
    }
}

pub struct ThreadPermit<T> {
    state: Arc<SupervisorState<T>>,
}

impl<T> fmt::Debug for ThreadPermit<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ThreadPermit")
            .field("capacity", &self.state.capacity)
            .finish_non_exhaustive()
    }
}

impl<T> ThreadPermit<T> {
    pub fn retain(self, handle: JoinHandle<T>) {
        let state = Arc::clone(&self.state);
        let mut pending = state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.len() >= state.capacity {
            std::process::abort();
        }
        pending.push(PendingThread {
            handle,
            _permit: self,
        });
        state.pending_count.fetch_add(1, Ordering::Release);
        drop(pending);
        state.wake.notify_one();
    }
}

impl<T> Drop for ThreadPermit<T> {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::AcqRel);
        self.state.wake.notify_all();
    }
}

fn supervise<T: Send + 'static>(state: &SupervisorState<T>, poll_interval: Duration) {
    loop {
        let mut pending = state
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while pending.is_empty() {
            if state.shutdown_requested.load(Ordering::Acquire)
                && state.active.load(Ordering::Acquire) == 0
            {
                return;
            }
            pending = state
                .wake
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let (locked, _) = state
            .wake
            .wait_timeout(pending, poll_interval)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pending = locked;

        let mut completed = Vec::new();
        let mut index = 0;
        while index < pending.len() {
            if pending[index].handle.is_finished() {
                completed.push(pending.swap_remove(index));
            } else {
                index += 1;
            }
        }
        state
            .pending_count
            .fetch_sub(completed.len(), Ordering::AcqRel);
        drop(pending);

        for completed_thread in completed {
            if completed_thread.handle.join().is_err() {
                state.panicked.fetch_add(1, Ordering::Relaxed);
            }
            state.reaped.fetch_add(1, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;
    use std::time::Instant;

    fn wait_for(
        supervisor: &ThreadSupervisor<()>,
        predicate: impl Fn(ThreadSupervisorSnapshot) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if predicate(supervisor.snapshot()) {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "supervisor state did not converge: {:?}",
            supervisor.snapshot()
        );
    }

    #[test]
    fn capacity_is_reserved_before_threads_are_started() {
        let supervisor = ThreadSupervisor::<()>::new(
            "hyper-supervisor-capacity-test",
            2,
            Duration::from_millis(1),
        )
        .unwrap();
        let first = supervisor.acquire().unwrap();
        let second = supervisor.acquire().unwrap();
        assert!(matches!(
            supervisor.acquire(),
            Err(ThreadCapacityExhausted { capacity: 2 })
        ));
        drop(first);
        assert!(supervisor.acquire().is_ok());
        drop(second);
    }

    #[test]
    fn blocked_thread_does_not_prevent_later_reaping() {
        let supervisor = ThreadSupervisor::<()>::new(
            "hyper-supervisor-progress-test",
            2,
            Duration::from_millis(1),
        )
        .unwrap();
        let blocked_permit = supervisor.acquire().unwrap();
        let completed_permit = supervisor.acquire().unwrap();
        let (release_sender, release_receiver) = sync_channel(0);
        let blocked = thread::spawn(move || {
            let _ = release_receiver.recv();
        });
        let completed = thread::spawn(|| {});
        blocked_permit.retain(blocked);
        completed_permit.retain(completed);

        wait_for(&supervisor, |snapshot| {
            snapshot.reaped == 1 && snapshot.active == 1 && snapshot.pending == 1
        });
        release_sender.send(()).unwrap();
        wait_for(&supervisor, |snapshot| {
            snapshot.reaped == 2 && snapshot.active == 0 && snapshot.pending == 0
        });
    }

    #[test]
    fn explicit_shutdown_drains_and_joins_the_reaper() {
        let supervisor = ThreadSupervisor::<()>::new(
            "hyper-supervisor-shutdown-test",
            1,
            Duration::from_millis(1),
        )
        .unwrap();
        let permit = supervisor.acquire().unwrap();
        permit.retain(thread::spawn(|| {}));

        let snapshot = supervisor.shutdown().unwrap();
        assert_eq!(snapshot.active, 0);
        assert_eq!(snapshot.pending, 0);
        assert_eq!(snapshot.reaped, 1);
        assert_eq!(snapshot.panicked, 0);
    }

    #[test]
    fn drop_waits_for_an_issued_permit_to_be_released() {
        let supervisor =
            ThreadSupervisor::<()>::new("hyper-supervisor-drop-test", 1, Duration::from_millis(1))
                .unwrap();
        let permit = supervisor.acquire().unwrap();
        let (release_sender, release_receiver) = sync_channel(0);
        let (dropped_sender, dropped_receiver) = sync_channel(1);
        let permit_owner = thread::spawn(move || {
            release_receiver.recv().unwrap();
            drop(permit);
            dropped_sender.send(()).unwrap();
        });
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            release_sender.send(()).unwrap();
        });

        drop(supervisor);
        dropped_receiver.recv().unwrap();
        release.join().unwrap();
        permit_owner.join().unwrap();
    }
}
