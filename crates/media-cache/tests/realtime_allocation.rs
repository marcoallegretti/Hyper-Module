#![allow(unsafe_code)]

mod common;

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::ClipOutput;
use hyper_media_cache::{CacheLimits, DecodedMediaCacheBuilder};

use common::one_clip_schedule;

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static HEAP_OPERATIONS: AtomicUsize = AtomicUsize::new(0);

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
fn prepared_cache_rendering_performs_zero_heap_allocations() {
    let schedule = one_clip_schedule();
    let source = DecodedAudio::from_mono(48_000, vec![0.25; 100]).unwrap();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 400,
    })
    .unwrap();
    builder.try_insert("asset", source).unwrap();
    let cache = builder.seal();
    let prepared = cache.prepare_schedule(&schedule).unwrap();
    let mut output = [0.0_f32; 100];

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
    let result = prepared.entries()[0].render_block(0, ClipOutput::Mono(&mut output));
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
