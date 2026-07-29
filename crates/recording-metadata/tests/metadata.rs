use hyper_recording_journal::{JournalConfig, JournalWriter, publish_wav};
use hyper_recording_metadata::{
    FinalizedRecordingMetadata, RecordingCaptureKind, RecordingMetadataContext,
    RecordingMetadataDocument, RecordingMetadataError, RecordingMetadataParseErrorKind,
};
use hyper_strict_json::StrictJsonErrorKind;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST: AtomicU64 = AtomicU64::new(0);
const VALID_METADATA: &[u8] =
    include_bytes!("../../../contracts/fixtures/valid/recording-metadata.json");
const INVALID_CHANNELS: &[u8] =
    include_bytes!("../../../contracts/fixtures/invalid/recording-metadata-channel-mismatch.json");

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hyper-recording-metadata-{}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn publication(&self, sample_rate: u32) -> hyper_recording_journal::PublishedRecording {
        let journal = self.0.join("take.journal");
        let wav = self.0.join("take.wav");
        let mut writer = JournalWriter::create(
            &journal,
            JournalConfig {
                sample_rate,
                max_chunk_frames: 2,
                max_total_frames: 2,
            },
        )
        .unwrap();
        writer.append(&[0.25, -0.25, 0.5, -0.5]).unwrap();
        drop(writer);
        publish_wav(journal, wav).unwrap()
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn context() -> RecordingMetadataContext {
    RecordingMetadataContext {
        id: "018f47a2-6b2d-7c11-8a44-001122334477".to_owned(),
        project_id: "018f47a2-6b2d-7c11-8a44-001122334455".to_owned(),
        asset_id: "018f47a2-6b2d-7c11-8a44-001122334478".to_owned(),
        track_id: "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
        capture_kind: RecordingCaptureKind::Overdub,
        captured_at_utc: "2026-07-27T12:00:00.000Z".to_owned(),
        timeline_start_tick: 7_680,
        source_input_indices: vec![0, 1],
    }
}

fn digest_hex(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in digest {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

#[test]
fn verified_publication_produces_canonical_contract_json() {
    let directory = TestDirectory::new();
    let metadata =
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), context()).unwrap();
    let json = String::from_utf8(metadata.to_canonical_json().unwrap()).unwrap();

    assert_eq!(
        json,
        "{\"asset_id\":\"018f47a2-6b2d-7c11-8a44-001122334478\",\"capture_kind\":\"overdub\",\"captured_at_utc\":\"2026-07-27T12:00:00.000Z\",\"channels\":2,\"extensions\":{},\"format\":\"hyper-recording-metadata\",\"format_version\":1,\"frame_count\":2,\"id\":\"018f47a2-6b2d-7c11-8a44-001122334477\",\"project_id\":\"018f47a2-6b2d-7c11-8a44-001122334455\",\"sample_rate_hz\":48000,\"source_input_indices\":[0,1],\"timeline_start_tick\":7680,\"track_id\":\"018f47a2-6b2d-7c11-8a44-001122334456\"}"
    );
}

#[test]
fn verified_publication_produces_canonical_storage_handoff() {
    let directory = TestDirectory::new();
    let publication = directory.publication(48_000);
    let expected_path = publication.final_wav_path().to_string_lossy().into_owned();
    let expected_length = publication.byte_length();
    let expected_sha256 = digest_hex(publication.wav_sha256());
    let metadata = FinalizedRecordingMetadata::try_new(publication, context()).unwrap();
    let bytes = metadata.to_handoff_json().unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    let handoff: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert!(text.starts_with(
        "{\"extensions\":{},\"format\":\"hyper-finalized-recording-handoff\",\"format_version\":1,"
    ));
    assert_eq!(
        handoff["format"],
        serde_json::Value::String("hyper-finalized-recording-handoff".to_owned())
    );
    assert_eq!(handoff["format_version"], 1);
    assert_eq!(handoff["recording"]["path"], expected_path);
    assert_eq!(handoff["recording"]["media_type"], "audio/wav");
    assert_eq!(handoff["recording"]["byte_length"], expected_length);
    assert_eq!(handoff["recording"]["sha256"], expected_sha256);
    assert_eq!(handoff["metadata"]["frame_count"], 2);
    assert_eq!(handoff["metadata"]["sample_rate_hz"], 48_000);
    assert_eq!(handoff["metadata"]["asset_id"], context().asset_id);
}

#[test]
fn rejects_invalid_provenance_and_media_domains() {
    let directory = TestDirectory::new();
    let mut invalid_id = context();
    invalid_id.id.make_ascii_uppercase();
    let rejected =
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), invalid_id).unwrap_err();
    assert_eq!(rejected.reason(), RecordingMetadataError::InvalidId("id"));
    let (reason, publication, rejected_context) = rejected.into_parts();
    assert_eq!(reason, RecordingMetadataError::InvalidId("id"));
    assert_eq!(publication.frames(), 2);
    assert_eq!(rejected_context.id, "018F47A2-6B2D-7C11-8A44-001122334477");

    let directory = TestDirectory::new();
    let mut invalid_date = context();
    invalid_date.captured_at_utc = "2026-02-29T12:00:00.000Z".to_owned();
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), invalid_date)
            .unwrap_err()
            .reason(),
        RecordingMetadataError::InvalidCapturedAt
    );

    let directory = TestDirectory::new();
    let mut duplicate_input = context();
    duplicate_input.source_input_indices = vec![0, 0];
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), duplicate_input)
            .unwrap_err()
            .reason(),
        RecordingMetadataError::DuplicateSourceInput { index: 0 }
    );

    let directory = TestDirectory::new();
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(44_100), context())
            .unwrap_err()
            .reason(),
        RecordingMetadataError::UnsupportedSampleRate { actual: 44_100 }
    );

    let directory = TestDirectory::new();
    let mut oversized_timeline = context();
    oversized_timeline.timeline_start_tick = 9_007_199_254_740_992;
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), oversized_timeline)
            .unwrap_err()
            .reason(),
        RecordingMetadataError::TimelineStartOutOfRange
    );

    let directory = TestDirectory::new();
    let mut missing_input = context();
    missing_input.source_input_indices = vec![0];
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), missing_input)
            .unwrap_err()
            .reason(),
        RecordingMetadataError::InvalidSourceInputCount {
            actual: 1,
            expected: 2
        }
    );

    let directory = TestDirectory::new();
    let mut unavailable_input = context();
    unavailable_input.source_input_indices = vec![0, 2];
    assert_eq!(
        FinalizedRecordingMetadata::try_new(directory.publication(48_000), unavailable_input)
            .unwrap_err()
            .reason(),
        RecordingMetadataError::InvalidSourceInput { index: 2 }
    );
}

#[test]
fn strict_reader_preserves_the_published_recording_contract() {
    let metadata = RecordingMetadataDocument::parse(VALID_METADATA).unwrap();
    assert_eq!(metadata.id(), "018f47a2-6b2d-7c11-8a44-001122334477");
    assert_eq!(
        metadata.project_id(),
        "018f47a2-6b2d-7c11-8a44-001122334455"
    );
    assert_eq!(metadata.asset_id(), "018f47a2-6b2d-7c11-8a44-001122334478");
    assert_eq!(metadata.track_id(), "018f47a2-6b2d-7c11-8a44-001122334456");
    assert_eq!(metadata.capture_kind(), RecordingCaptureKind::Overdub);
    assert_eq!(metadata.captured_at_utc(), "2026-07-27T12:00:00.000Z");
    assert_eq!(metadata.timeline_start_tick(), 7_680);
    assert_eq!(metadata.source_input_indices(), &[0, 1]);
    assert_eq!(metadata.sample_rate_hz(), 48_000);
    assert_eq!(metadata.channels(), 2);
    assert_eq!(metadata.frame_count(), 96_000);
    assert!(metadata.extensions().is_empty());
}

#[test]
fn strict_reader_rejects_schema_and_domain_failures() {
    assert_eq!(
        RecordingMetadataDocument::parse(INVALID_CHANNELS)
            .unwrap_err()
            .kind(),
        RecordingMetadataParseErrorKind::InvalidDomain
    );

    let unknown = String::from_utf8(VALID_METADATA.to_vec()).unwrap().replace(
        r#""extensions": {}"#,
        r#""unknown": true, "extensions": {}"#,
    );
    assert_eq!(
        RecordingMetadataDocument::parse(unknown.as_bytes())
            .unwrap_err()
            .kind(),
        RecordingMetadataParseErrorKind::Json(StrictJsonErrorKind::TypedDocumentMismatch)
    );

    let duplicate = String::from_utf8(VALID_METADATA.to_vec()).unwrap().replace(
        r#""format_version": 1,"#,
        r#""format_version": 1, "format_version": 1,"#,
    );
    assert_eq!(
        RecordingMetadataDocument::parse(duplicate.as_bytes())
            .unwrap_err()
            .kind(),
        RecordingMetadataParseErrorKind::Json(StrictJsonErrorKind::DuplicateObjectName)
    );

    let mut invalid_extension: serde_json::Value = serde_json::from_slice(VALID_METADATA).unwrap();
    invalid_extension["extensions"]["not_namespaced"] = true.into();
    assert_eq!(
        RecordingMetadataDocument::parse(&serde_json::to_vec(&invalid_extension).unwrap())
            .unwrap_err()
            .kind(),
        RecordingMetadataParseErrorKind::InvalidDomain
    );
}
