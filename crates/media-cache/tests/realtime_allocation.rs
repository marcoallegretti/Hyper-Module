mod common;
#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::ClipOutput;
use hyper_media_cache::{CacheLimits, DecodedMediaCacheBuilder};

use common::one_clip_schedule;
use realtime_allocator::HeapMeasurement;

#[test]
fn prepared_cache_rendering_performs_zero_heap_allocations() {
    let schedule = one_clip_schedule();
    let source = DecodedAudio::from_mono(48_000, vec![0.25; 100]).unwrap();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 400,
    })
    .unwrap();
    builder
        .try_insert("018f47a2-6b2d-7c11-8a44-001122335034", source)
        .unwrap();
    let cache = builder.seal();
    let prepared = cache.prepare_schedule(&schedule).unwrap();
    let mut output = [0.0_f32; 100];

    let measurement = HeapMeasurement::start_verified();
    let result = prepared.entries()[0].render_block(0, ClipOutput::Mono(&mut output));
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
