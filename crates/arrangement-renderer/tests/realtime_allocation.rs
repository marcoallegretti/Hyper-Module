mod common;
#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use std::sync::Mutex;

use hyper_arrangement_renderer::{ArrangementRenderer, RenderScratchSlot};

use common::fixture;
use realtime_allocator::HeapMeasurement;

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn prepared_window_rendering_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut left_a = [0.0_f32; 80];
    let mut right_a = [0.0_f32; 80];
    let mut left_b = [0.0_f32; 80];
    let mut right_b = [0.0_f32; 80];
    let mut slots = [
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut left_b, &mut right_b),
    ];

    let measurement = HeapMeasurement::start_verified();
    let result = renderer.render_window(90, 80, &mut slots);
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}

#[test]
fn fixed_track_window_rendering_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new_for_tracks(
        &prepared,
        &[
            "018f47a2-6b2d-7c11-8a44-001122335002",
            "018f47a2-6b2d-7c11-8a44-001122335003",
            "018f47a2-6b2d-7c11-8a44-001122335004",
        ],
    )
    .unwrap();
    let mut left_a = [0.0_f32; 160];
    let mut right_a = [0.0_f32; 160];
    let mut left_b = [0.0_f32; 160];
    let mut right_b = [0.0_f32; 160];
    let mut left_c = [0.0_f32; 160];
    let mut right_c = [0.0_f32; 160];
    let mut slots = [
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut left_b, &mut right_b),
        RenderScratchSlot::new(&mut left_c, &mut right_c),
    ];

    let measurement = HeapMeasurement::start_verified();
    let result = renderer.render_track_window(190, 160, &mut slots);
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
