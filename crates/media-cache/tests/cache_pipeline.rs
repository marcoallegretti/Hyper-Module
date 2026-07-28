mod common;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::ClipOutput;
use hyper_media_cache::{CacheLimits, DecodedMediaCacheBuilder, PrepareScheduleError};
use hyper_playback_compiler::PrepareError;

use common::one_clip_schedule;

fn builder() -> DecodedMediaCacheBuilder {
    DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 4,
        max_frames_per_asset: 1_000,
        max_decoded_sample_bytes: 32_000,
    })
    .unwrap()
}

#[test]
fn cache_binds_compiled_identity_and_renders_prepared_audio() {
    let schedule = one_clip_schedule();
    let source_samples: Vec<f32> = (0_u16..100)
        .map(|sample| f32::from(sample) / 100.0)
        .collect();
    let source = DecodedAudio::from_mono(48_000, source_samples.clone()).unwrap();
    let mut builder = builder();
    builder.try_insert("asset", source).unwrap();
    let cache = builder.seal();

    let prepared = cache.prepare_schedule(&schedule).unwrap();
    let entry = &prepared.entries()[0];
    let mut output = [0.0_f32; 100];
    entry
        .render_block(0, ClipOutput::Mono(&mut output))
        .unwrap();

    assert_eq!(entry.schedule_entry().placement_id, "placement");
    assert_eq!(
        entry.schedule_entry().compiled.arrangement_start_frame(),
        100
    );
    assert_eq!(output.as_slice(), source_samples.as_slice());
}

#[test]
fn missing_asset_rejects_the_whole_schedule() {
    let schedule = one_clip_schedule();
    let cache = builder().seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::MissingAsset { schedule_index: 0 }
    );
}

#[test]
fn decoded_rate_mismatch_is_rejected_during_binding() {
    let schedule = one_clip_schedule();
    let mut builder = builder();
    builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(44_100, vec![0.0; 100]).unwrap(),
        )
        .unwrap();
    let cache = builder.seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::InvalidAsset {
            schedule_index: 0,
            source: PrepareError::SourceSampleRateMismatch {
                expected: 48_000,
                actual: 44_100,
            },
        }
    );
}

#[test]
fn decoded_frame_count_mismatch_is_rejected_during_binding() {
    let schedule = one_clip_schedule();
    let mut builder = builder();
    builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, vec![0.0; 101]).unwrap(),
        )
        .unwrap();
    let cache = builder.seal();

    assert_eq!(
        cache.prepare_schedule(&schedule).unwrap_err(),
        PrepareScheduleError::InvalidAsset {
            schedule_index: 0,
            source: PrepareError::SourceFrameCountMismatch {
                expected: 100,
                actual: 101,
            },
        }
    );
}
