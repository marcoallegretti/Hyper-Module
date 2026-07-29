#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::cell::Cell;

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static HEAP_OPERATIONS: Cell<usize> = const { Cell::new(0) };
}

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
    if COUNTING.get() {
        HEAP_OPERATIONS.set(HEAP_OPERATIONS.get().saturating_add(1));
    }
}

pub struct HeapMeasurement {
    active: bool,
}

impl HeapMeasurement {
    pub fn start_verified() -> Self {
        HEAP_OPERATIONS.set(0);
        COUNTING.set(true);
        let value = Box::new(7_u8);
        std::hint::black_box(&value);
        drop(value);
        COUNTING.set(false);
        assert!(HEAP_OPERATIONS.get() >= 2);
        HEAP_OPERATIONS.set(0);
        COUNTING.set(true);
        Self { active: true }
    }

    pub fn finish(mut self) -> usize {
        COUNTING.set(false);
        self.active = false;
        HEAP_OPERATIONS.get()
    }
}

impl Drop for HeapMeasurement {
    fn drop(&mut self) {
        if self.active {
            COUNTING.set(false);
        }
    }
}
