use hyper_recording_journal::{
    JournalConfig, JournalError, JournalWriter, PublicationOutcome, publish_wav, recover,
};
use hyper_recording_transport::try_recording_transport;
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hyper-recording-journal-{}-{id}",
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
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn config() -> JournalConfig {
    JournalConfig {
        sample_rate: 48_000,
        max_chunk_frames: 8,
        max_total_frames: 64,
    }
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

#[cfg(unix)]
fn create_link_like(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
    assert!(fs::symlink_metadata(link).unwrap().file_type().is_symlink());
}

#[cfg(windows)]
fn create_link_like(target: &Path, link: &Path) {
    junction::create(target, link).unwrap();
    assert!(junction::exists(link).unwrap());
}

#[test]
fn validates_append_inputs_without_poisoning_the_writer() {
    let directory = TestDirectory::new();
    let path = directory.join("take.journal");
    let mut writer = JournalWriter::create(&path, config()).unwrap();

    assert!(matches!(writer.append(&[]), Err(JournalError::EmptyChunk)));
    assert!(matches!(
        writer.append(&[0.0]),
        Err(JournalError::SampleCountNotStereoAligned { .. })
    ));
    assert!(matches!(
        writer.append(&[f32::NAN, 0.0]),
        Err(JournalError::NonFiniteSample)
    ));
    assert!(matches!(
        writer.append(&[0.0; 18]),
        Err(JournalError::ChunkTooLarge { .. })
    ));

    writer.append(&[0.25, -0.25]).unwrap();
    assert_eq!(writer.committed_frames(), 1);
}

#[test]
fn rejects_concurrent_recovery_and_resume_owners() {
    let directory = TestDirectory::new();
    let path = directory.join("take.journal");
    let writer = JournalWriter::create(&path, config()).unwrap();

    assert!(matches!(recover(&path), Err(JournalError::JournalInUse(_))));
    assert!(matches!(
        JournalWriter::resume(&path),
        Err(JournalError::JournalInUse(_))
    ));
    drop(writer);
    assert_eq!(recover(&path).unwrap().frames, 0);
}

#[test]
fn recovers_partial_header_and_payload_tails() {
    for tail in [&[1_u8, 2, 3][..], &[0_u8; 20][..]] {
        let directory = TestDirectory::new();
        let path = directory.join("take.journal");
        let mut writer = JournalWriter::create(&path, config()).unwrap();
        writer.append(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        drop(writer);
        let clean_length = fs::metadata(&path).unwrap().len();
        append_bytes(&path, tail);

        let report = recover(&path).unwrap();
        assert_eq!(report.frames, 2);
        assert_eq!(report.discarded_bytes, tail.len() as u64);
        assert_eq!(fs::metadata(&path).unwrap().len(), clean_length);
    }
}

#[test]
fn recovers_an_incomplete_payload_and_resumes_sequence() {
    let directory = TestDirectory::new();
    let path = directory.join("take.journal");
    let mut writer = JournalWriter::create(&path, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    writer.append(&[0.5, -0.5]).unwrap();
    drop(writer);
    let full_length = fs::metadata(&path).unwrap().len();
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(full_length - 3).unwrap();
    file.sync_all().unwrap();

    let (mut writer, report) = JournalWriter::resume(&path).unwrap();
    assert_eq!(report.frames, 1);
    assert_eq!(report.chunks, 1);
    assert_eq!(report.discarded_bytes, 37);
    writer.append(&[0.75, -0.75]).unwrap();
    drop(writer);

    let report = recover(&path).unwrap();
    assert_eq!(report.frames, 2);
    assert_eq!(report.chunks, 2);
}

#[test]
fn rejects_interior_corruption_without_truncating_it() {
    let directory = TestDirectory::new();
    let path = directory.join("take.journal");
    let mut writer = JournalWriter::create(&path, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    writer.append(&[0.5, -0.5]).unwrap();
    drop(writer);
    let original_length = fs::metadata(&path).unwrap().len();

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::Start(36 + 32)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();

    assert!(matches!(
        recover(&path),
        Err(JournalError::CorruptJournal {
            reason: "interior chunk payload checksum failed",
            ..
        })
    ));
    assert_eq!(fs::metadata(&path).unwrap().len(), original_length);
}

#[test]
fn rejects_a_checksum_failed_final_chunk_without_truncating_it() {
    let directory = TestDirectory::new();
    let path = directory.join("take.journal");
    let mut writer = JournalWriter::create(&path, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    writer.append(&[0.5, -0.5]).unwrap();
    drop(writer);
    let original_length = fs::metadata(&path).unwrap().len();

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.seek(SeekFrom::End(-1)).unwrap();
    file.write_all(&[0xff]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    assert!(matches!(
        recover(&path),
        Err(JournalError::CorruptJournal {
            reason: "final chunk payload checksum failed",
            ..
        })
    ));
    assert_eq!(fs::metadata(path).unwrap().len(), original_length);
}

#[test]
fn publishes_exact_float_wav_and_is_idempotent() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("take.wav");
    let samples = [0.0, -0.0, 0.25, -0.25, 1.0, -1.0];
    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    writer.append(&samples).unwrap();
    drop(writer);

    let published = publish_wav(&journal, &wav).unwrap();
    assert_eq!(published.journal_path(), journal);
    assert_eq!(published.final_wav_path(), wav);
    assert_eq!(published.sample_rate_hz(), 48_000);
    assert_eq!(published.channels(), 2);
    assert_eq!(published.frames(), 3);
    assert_ne!(published.content_crc32(), 0);
    assert_eq!(published.byte_length(), fs::metadata(&wav).unwrap().len());
    assert_eq!(
        published.wav_sha256(),
        &<[u8; 32]>::from(Sha256::digest(fs::read(&wav).unwrap()))
    );
    assert_eq!(published.outcome(), PublicationOutcome::Published);
    let republished = publish_wav(&journal, &wav).unwrap();
    assert_eq!(republished.outcome(), PublicationOutcome::AlreadyPublished);
    assert_eq!(republished.byte_length(), published.byte_length());
    assert_eq!(republished.wav_sha256(), published.wav_sha256());
    let mut reader = hound::WavReader::open(&wav).unwrap();
    let decoded = reader
        .samples::<f32>()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        decoded
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>(),
        samples
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    );
}

#[test]
fn resumes_a_verified_staging_file_after_interruption() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("take.wav");
    let staging = directory.join(".take.wav.hyper-staging");
    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    drop(writer);
    publish_wav(&journal, &wav).unwrap();
    fs::rename(&wav, &staging).unwrap();

    assert_eq!(
        publish_wav(&journal, &wav).unwrap().outcome(),
        PublicationOutcome::Published
    );
    assert!(wav.exists());
    assert!(!staging.exists());
}

#[test]
fn preserves_a_conflicting_staging_file_for_explicit_resolution() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("take.wav");
    let staging = directory.join(".take.wav.hyper-staging");
    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    drop(writer);
    fs::write(&staging, b"unrelated data").unwrap();

    assert!(matches!(
        publish_wav(&journal, &wav),
        Err(JournalError::StagingConflict(_))
    ));
    assert_eq!(fs::read(staging).unwrap(), b"unrelated data");
    assert!(!wav.exists());
}

#[test]
fn never_overwrites_existing_published_media() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("take.wav");
    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    drop(writer);
    fs::write(&wav, b"existing media").unwrap();

    assert!(matches!(
        publish_wav(&journal, &wav),
        Err(JournalError::PublishedRecordingConflict(_))
    ));
    assert_eq!(fs::read(&wav).unwrap(), b"existing media");
}

#[test]
fn rejects_link_like_published_and_staging_conflicts() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let target = directory.join("verified.wav");
    let linked_final = directory.join("linked.wav");
    let staged_final = directory.join("staged.wav");
    let linked_staging = directory.join(".staged.wav.hyper-staging");
    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    writer.append(&[0.25, -0.25]).unwrap();
    drop(writer);
    publish_wav(&journal, &target).unwrap();
    #[cfg(unix)]
    let link_target = target.clone();
    #[cfg(windows)]
    let link_target = {
        let path = directory.join("junction-target");
        fs::create_dir(&path).unwrap();
        path
    };

    create_link_like(&link_target, &linked_final);
    assert!(matches!(
        publish_wav(&journal, &linked_final),
        Err(JournalError::PublishedRecordingConflict(path)) if path == linked_final
    ));

    create_link_like(&link_target, &linked_staging);
    assert!(matches!(
        publish_wav(&journal, &staged_final),
        Err(JournalError::StagingConflict(path)) if path == linked_staging
    ));
    assert!(!staged_final.exists());
}

#[test]
fn transfers_transport_blocks_through_journal_to_wav() {
    let directory = TestDirectory::new();
    let journal = directory.join("take.journal");
    let wav = directory.join("take.wav");
    let (mut producer, mut consumer) = try_recording_transport(8).unwrap();
    producer.push(&[0.25, -0.25, 0.5, -0.5]).unwrap();
    drop(producer);

    let mut writer = JournalWriter::create(&journal, config()).unwrap();
    let mut buffer = [0.0_f32; 8];
    loop {
        let drained = consumer.drain(&mut buffer).unwrap();
        if drained.frames > 0 {
            writer.append(&buffer[..drained.frames * 2]).unwrap();
        }
        if drained.producer_closed {
            break;
        }
    }
    drop(writer);

    publish_wav(&journal, &wav).unwrap();
    let mut reader = hound::WavReader::open(wav).unwrap();
    assert_eq!(
        reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        vec![0.25, -0.25, 0.5, -0.5]
    );
}
