use hyper_recording_journal::{JournalConfig, JournalWriter};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hyper-recording-recover-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn command() -> Command {
    Command::new(env!("CARGO_BIN_EXE_hyper-recording-recover"))
}

#[test]
fn recovers_committed_audio_and_retries_without_replacing_it() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("salvaged.WAV");
    let mut writer = JournalWriter::create(
        &journal,
        JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 8,
            max_total_frames: 64,
        },
    )
    .unwrap();
    writer.append(&[0.25, -0.25, 0.5, -0.5]).unwrap();
    drop(writer);
    OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b"incomplete tail")
        .unwrap();

    let first = command().args([&journal, &wav]).output().unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("Published "));
    let mut reader = hound::WavReader::open(&wav).unwrap();
    assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
    assert_eq!(
        reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        [0.25, -0.25, 0.5, -0.5]
    );
    let first_bytes = fs::read(&wav).unwrap();

    let second = command().args([&journal, &wav]).output().unwrap();
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("Already published "));
    assert_eq!(fs::read(&wav).unwrap(), first_bytes);

    let conflicting = directory.join("foreign.wav");
    fs::write(&conflicting, b"foreign media").unwrap();
    let failure = command().args([&journal, &conflicting]).output().unwrap();
    assert!(!failure.status.success());
    assert_eq!(fs::read(&conflicting).unwrap(), b"foreign media");
    assert!(journal.exists());
}

#[test]
fn refuses_invalid_journal_without_publishing_media() {
    let directory = TestDirectory::new();
    let journal = directory.join("invalid.journal");
    let wav = directory.join("invalid.wav");
    fs::write(&journal, b"not a journal").unwrap();
    let output = command().args([&journal, &wav]).output().unwrap();
    assert!(!output.status.success());
    assert!(!wav.exists());
    assert_eq!(fs::read(&journal).unwrap(), b"not a journal");
}

#[test]
fn refuses_a_live_recording_owner() {
    let directory = TestDirectory::new();
    let journal = directory.join("active.journal");
    let wav = directory.join("active.wav");
    let mut writer = JournalWriter::create(
        &journal,
        JournalConfig {
            sample_rate: 48_000,
            max_chunk_frames: 8,
            max_total_frames: 64,
        },
    )
    .unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    let output = command().args([&journal, &wav]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already in use"));
    assert!(!wav.exists());
    assert_eq!(writer.committed_frames(), 1);
    drop(writer);
    let settled = command().args([&journal, &wav]).output().unwrap();
    assert!(settled.status.success());
}
