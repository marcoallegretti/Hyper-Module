#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{
    ClipOutput, ClipRenderPlan, FrameRange, NativeClipSpec, PlacementSpec, TimeMapping,
};

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
fn clip_block_rendering_performs_zero_heap_allocations() {
    let source = DecodedAudio::from_stereo(48_000, vec![0.25; 128], vec![-0.25; 128]).unwrap();
    let plan = ClipRenderPlan::try_new(
        &source,
        NativeClipSpec {
            enabled: true,
            source: FrameRange { start: 0, end: 128 },
            loop_range: Some(FrameRange { start: 32, end: 96 }),
            reverse: true,
            gain_decibels: -3.0,
            fade_in_frames: 16,
            fade_out_frames: 16,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
    )
    .unwrap();
    let placement = plan
        .place(PlacementSpec {
            clip_start_frame: 0,
            frame_count: 256,
        })
        .unwrap();
    let mut left = [0.0_f32; 128];
    let mut right = [0.0_f32; 128];

    assert_allocator_positive_control();
    let measurement = HeapMeasurement::start();
    let result = plan.render_block(
        placement,
        64,
        ClipOutput::Stereo {
            left: &mut left,
            right: &mut right,
        },
    );
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
