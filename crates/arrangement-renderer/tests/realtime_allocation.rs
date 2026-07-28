#![allow(unsafe_code)]

mod common;

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_arrangement_renderer::{ArrangementRenderer, RenderScratchSlot};

use common::fixture;

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static HEAP_OPERATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            HEAP_OPERATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            HEAP_OPERATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            HEAP_OPERATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            HEAP_OPERATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

struct HeapMeasurement {
    active: bool,
}

impl HeapMeasurement {
    fn start() -> Self {
        HEAP_OPERATIONS.store(0, Ordering::SeqCst);
        COUNTING.store(true, Ordering::SeqCst);
        Self { active: true }
    }

    fn finish(mut self) -> usize {
        COUNTING.store(false, Ordering::SeqCst);
        self.active = false;
        HEAP_OPERATIONS.load(Ordering::SeqCst)
    }
}

impl Drop for HeapMeasurement {
    fn drop(&mut self) {
        if self.active {
            COUNTING.store(false, Ordering::SeqCst);
        }
    }
}

fn assert_allocator_positive_control() {
    let measurement = HeapMeasurement::start();
    let value = Box::new(7_u8);
    std::hint::black_box(&value);
    drop(value);
    assert!(measurement.finish() >= 2);
}

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

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
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
    let renderer =
        ArrangementRenderer::try_new_for_tracks(&prepared, &["track-a", "track-b", "track-c"])
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

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
    let result = renderer.render_track_window(190, 160, &mut slots);
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
