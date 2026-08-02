use hyper_recording_journal::{JournalConfig, PublicationOutcome, recover};
use hyper_recording_transport::{RecordingPushError, try_recording_transport};
use hyper_recording_worker::{
    RecordingDisposition, RecordingDispositionError, RecordingWorker, RecordingWorkerCompletion,
    RecordingWorkerConfig, RecordingWorkerFailure, RecordingWorkerWaitError,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
const WAIT: Duration = Duration::from_secs(2);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hyper-recording-worker-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn config(&self, drain_frames: usize) -> RecordingWorkerConfig {
        RecordingWorkerConfig {
            journal_path: self.0.join("take.journal"),
            final_wav_path: self.0.join("take.wav"),
            journal: JournalConfig {
                sample_rate: 48_000,
                max_chunk_frames: 8,
                max_total_frames: 64,
            },
            drain_frames,
        }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn commit_drains_every_frame_before_verified_publication() {
    let directory = TestDirectory::new();
    let config = directory.config(2);
    let wav = config.final_wav_path.clone();
    let journal = config.journal_path.clone();
    let (mut producer, consumer) = try_recording_transport(4).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();

    producer.push(&[0.25, -0.25, 0.5, -0.5]).unwrap();
    producer.push(&[0.75, -0.75]).unwrap();
    drop(producer);
    worker.request_commit().unwrap();

    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Published(recording)
            if recording.frames() == 3
                && recording.outcome() == PublicationOutcome::Published
    ));
    assert_eq!(recover(journal).unwrap().frames, 3);
    let mut reader = hound::WavReader::open(wav).unwrap();
    assert_eq!(
        reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![0.25, -0.25, 0.5, -0.5, 0.75, -0.75]
    );
}

#[test]
fn abort_waits_for_producer_closure_and_preserves_only_the_journal() {
    let directory = TestDirectory::new();
    let config = directory.config(2);
    let wav = config.final_wav_path.clone();
    let journal = config.journal_path.clone();
    let (mut producer, consumer) = try_recording_transport(4).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();
    producer.push(&[0.25, -0.25]).unwrap();

    worker.request_abort().unwrap();
    assert!(matches!(
        worker.wait(Duration::from_millis(20)),
        Err(RecordingWorkerWaitError::TimedOut)
    ));
    producer.push(&[0.5, -0.5]).unwrap();
    drop(producer);

    let completion = worker.wait(WAIT).unwrap();
    assert!(
        matches!(
            completion,
            RecordingWorkerCompletion::Aborted { frames: 2, .. }
        ),
        "unexpected completion: {completion:?}"
    );
    assert_eq!(recover(journal).unwrap().frames, 2);
    assert!(!wav.exists());
}

#[test]
fn commit_refuses_to_publish_after_any_transport_rejection() {
    let directory = TestDirectory::new();
    let config = directory.config(1);
    let wav = config.final_wav_path.clone();
    let journal = config.journal_path.clone();
    let (mut producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();

    assert!(matches!(
        producer.push(&[0.0; 4]),
        Err(RecordingPushError::BlockExceedsCapacity { .. })
    ));
    producer.push(&[0.25, -0.25]).unwrap();
    drop(producer);
    worker.request_commit().unwrap();

    let completion = worker.wait(WAIT).unwrap();
    assert!(matches!(
        &completion,
        RecordingWorkerCompletion::Failed {
            frames: 1,
            journal_path: Some(path),
            error: RecordingWorkerFailure::TransportIntegrity { .. },
        } if path == &journal
    ));
    assert_eq!(recover(journal).unwrap().frames, 1);
    assert!(!wav.exists());
}

#[test]
fn disposition_is_single_assignment() {
    let directory = TestDirectory::new();
    let (producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(directory.config(1), consumer).unwrap();
    worker.request_abort().unwrap();
    assert_eq!(
        worker.request_commit(),
        Err(RecordingDispositionError::AlreadyRequested(
            RecordingDisposition::Abort
        ))
    );
    drop(producer);
    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Aborted { .. }
    ));
}

#[test]
fn producer_closure_never_authorizes_publication_by_itself() {
    let directory = TestDirectory::new();
    let config = directory.config(1);
    let wav = config.final_wav_path.clone();
    let (producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();
    drop(producer);

    assert!(matches!(
        worker.wait(Duration::from_millis(20)),
        Err(RecordingWorkerWaitError::TimedOut)
    ));
    assert!(!wav.exists());
    worker.request_abort().unwrap();
    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Aborted { frames: 0, .. }
    ));
}

#[test]
fn dropping_the_owner_stops_the_worker_and_closes_the_consumer() {
    let directory = TestDirectory::new();
    let (mut producer, consumer) = try_recording_transport(1).unwrap();
    let worker = RecordingWorker::start(directory.config(1), consumer).unwrap();
    drop(worker);

    let deadline = Instant::now() + WAIT;
    loop {
        if matches!(
            producer.push(&[0.25, -0.25]),
            Err(RecordingPushError::ConsumerClosed { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "recording worker did not observe owner cancellation"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn invalid_config_fails_before_creating_a_journal() {
    let directory = TestDirectory::new();
    let mut config = directory.config(2);
    config.final_wav_path = config.journal_path.clone();
    let journal = config.journal_path.clone();
    let (_producer, consumer) = try_recording_transport(2).unwrap();

    assert!(RecordingWorker::start(config, consumer).is_err());
    assert!(!journal.exists());
}

#[test]
fn relative_paths_fail_before_creating_a_journal() {
    let directory = TestDirectory::new();
    let mut config = directory.config(1);
    let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let relative = PathBuf::from(format!(
        "hyper-recording-worker-relative-{}-{id}.journal",
        std::process::id()
    ));
    assert!(!relative.exists());
    config.journal_path = relative.clone();
    let (_producer, consumer) = try_recording_transport(1).unwrap();

    assert!(RecordingWorker::start(config, consumer).is_err());
    assert!(!relative.exists());
}

#[test]
fn non_wav_destination_fails_before_creating_a_journal() {
    let directory = TestDirectory::new();
    let mut config = directory.config(1);
    config.final_wav_path = directory.0.join("take.flac");
    let journal = config.journal_path.clone();
    let (_producer, consumer) = try_recording_transport(1).unwrap();

    assert!(RecordingWorker::start(config, consumer).is_err());
    assert!(!journal.exists());
}

#[test]
fn staging_overlap_fails_before_creating_a_journal() {
    let directory = TestDirectory::new();
    let mut config = directory.config(1);
    config.journal_path = directory.0.join(".take.wav.hyper-staging");
    let journal = config.journal_path.clone();
    let (_producer, consumer) = try_recording_transport(1).unwrap();

    assert!(RecordingWorker::start(config, consumer).is_err());
    assert!(!journal.exists());
}

#[test]
fn reused_wake_registration_fails_before_creating_a_journal() {
    let directory = TestDirectory::new();
    let config = directory.config(1);
    let journal = config.journal_path.clone();
    let (_producer, consumer) = try_recording_transport(1).unwrap();
    consumer.register_current_thread_waker().unwrap();

    assert!(RecordingWorker::start(config, consumer).is_err());
    assert!(!journal.exists());
}

#[cfg(windows)]
#[test]
fn worker_retains_both_recording_parent_directories_until_completion() {
    let directory = TestDirectory::new();
    let journal_parent = directory.0.join("journals");
    let media_parent = directory.0.join("media");
    fs::create_dir(&journal_parent).unwrap();
    fs::create_dir(&media_parent).unwrap();
    let config = RecordingWorkerConfig {
        journal_path: journal_parent.join("take.journal"),
        final_wav_path: media_parent.join("take.wav"),
        journal: JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 8,
            max_total_frames: 64,
        },
        drain_frames: 1,
    };
    let (mut producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();

    assert!(
        fs::rename(&journal_parent, directory.0.join("moved-journals")).is_err(),
        "the live journal parent must not be displaceable"
    );
    assert!(
        fs::rename(&media_parent, directory.0.join("moved-media")).is_err(),
        "the live media parent must not be displaceable"
    );

    producer.push(&[0.25, -0.25]).unwrap();
    drop(producer);
    worker.request_commit().unwrap();
    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Published(_)
    ));
    fs::rename(&journal_parent, directory.0.join("moved-journals")).unwrap();
    fs::rename(&media_parent, directory.0.join("moved-media")).unwrap();
}

#[cfg(unix)]
#[test]
fn worker_refuses_publication_after_recording_parent_displacement() {
    let directory = TestDirectory::new();
    let journal_parent = directory.0.join("journals");
    let media_parent = directory.0.join("media");
    let moved_journal_parent = directory.0.join("moved-journals");
    let moved_media_parent = directory.0.join("moved-media");
    fs::create_dir(&journal_parent).unwrap();
    fs::create_dir(&media_parent).unwrap();
    let config = RecordingWorkerConfig {
        journal_path: journal_parent.join("take.journal"),
        final_wav_path: media_parent.join("take.wav"),
        journal: JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 8,
            max_total_frames: 64,
        },
        drain_frames: 1,
    };
    let (mut producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();

    fs::rename(&journal_parent, &moved_journal_parent).unwrap();
    fs::rename(&media_parent, &moved_media_parent).unwrap();
    fs::create_dir(&journal_parent).unwrap();
    fs::create_dir(&media_parent).unwrap();
    fs::write(
        journal_parent.join("owner.txt"),
        b"replacement journal parent",
    )
    .unwrap();
    fs::write(media_parent.join("owner.txt"), b"replacement media parent").unwrap();

    producer.push(&[0.25, -0.25]).unwrap();
    drop(producer);
    worker.request_commit().unwrap();
    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Failed {
            frames: 1,
            journal_path: None,
            error: RecordingWorkerFailure::JournalLocationLost { .. },
        }
    ));
    assert!(moved_journal_parent.join("take.journal").is_file());
    assert!(!moved_media_parent.join("take.wav").exists());
    assert!(!journal_parent.join("take.journal").exists());
    assert!(!media_parent.join("take.wav").exists());
    assert_eq!(
        fs::read(journal_parent.join("owner.txt")).unwrap(),
        b"replacement journal parent"
    );
    assert_eq!(
        fs::read(media_parent.join("owner.txt")).unwrap(),
        b"replacement media parent"
    );
}

#[cfg(unix)]
#[test]
fn worker_never_reports_a_stale_recovery_path_after_parent_displacement() {
    let directory = TestDirectory::new();
    let journal_parent = directory.0.join("journals");
    let moved_journal_parent = directory.0.join("moved-journals");
    fs::create_dir(&journal_parent).unwrap();
    let config = RecordingWorkerConfig {
        journal_path: journal_parent.join("take.journal"),
        final_wav_path: directory.0.join("take.wav"),
        journal: JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 8,
            max_total_frames: 64,
        },
        drain_frames: 1,
    };
    let (producer, consumer) = try_recording_transport(1).unwrap();
    let mut worker = RecordingWorker::start(config, consumer).unwrap();

    fs::rename(&journal_parent, &moved_journal_parent).unwrap();
    fs::create_dir(&journal_parent).unwrap();
    drop(producer);
    worker.request_abort().unwrap();

    assert!(matches!(
        worker.wait(WAIT).unwrap(),
        RecordingWorkerCompletion::Failed {
            frames: 0,
            journal_path: None,
            error: RecordingWorkerFailure::Journal(
                hyper_recording_journal::JournalError::PathAuthorityLost(_)
            ),
        }
    ));
    assert!(moved_journal_parent.join("take.journal").is_file());
    assert!(!journal_parent.join("take.journal").exists());
}
