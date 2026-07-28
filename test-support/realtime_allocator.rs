#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static HEAP_OPERATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_operation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_operation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_operation();
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_operation();
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

fn record_operation() {
    if COUNTING.load(Ordering::Relaxed) {
        HEAP_OPERATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct HeapMeasurement {
    active: bool,
}

impl HeapMeasurement {
    pub fn start() -> Self {
        HEAP_OPERATIONS.store(0, Ordering::SeqCst);
        COUNTING.store(true, Ordering::SeqCst);
        Self { active: true }
    }

    pub fn finish(mut self) -> usize {
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

pub fn assert_allocator_positive_control() {
    let measurement = HeapMeasurement::start();
    let value = Box::new(7_u8);
    std::hint::black_box(&value);
    drop(value);
    assert!(measurement.finish() >= 2);
}
