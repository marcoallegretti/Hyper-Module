#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_desktop_audio::{OutputCallback, OutputCallbackStatus};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn callback_processing_performs_zero_heap_allocations() {
    let mut invocation = 0_u8;
    let mut callback = OutputCallback::try_new(
        move |output: &mut [f32]| -> Result<(), u8> {
            invocation = invocation.saturating_add(1);
            output.fill(0.25);
            if invocation == 1 { Ok(()) } else { Err(7) }
        },
        128,
    )
    .unwrap();
    let mut rendered = [0.0_f32; 256];
    let mut failed = [1.0_f32; 256];
    let mut invalid = [1.0_f32; 3];
    let mut oversized = [1.0_f32; 258];

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let rendered_status = callback.process(&mut rendered);
    let failed_status = callback.process(&mut failed);
    let invalid_status = callback.process(&mut invalid);
    let oversized_status = callback.process(&mut oversized);
    COUNTING.store(false, Ordering::SeqCst);

    assert_eq!(rendered_status, OutputCallbackStatus::Rendered);
    assert_eq!(failed_status, OutputCallbackStatus::ProcessorFailed);
    assert_eq!(
        invalid_status,
        OutputCallbackStatus::SampleCountNotStereoAligned { samples: 3 }
    );
    assert_eq!(
        oversized_status,
        OutputCallbackStatus::BlockTooLarge {
            frames: 129,
            maximum: 128
        }
    );
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
}
