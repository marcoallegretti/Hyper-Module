#![allow(unsafe_code)]

mod common;

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{InterleavedStereoCallback, StereoOutput};
use hyper_mixer_runtime::{
    FlatMainRuntime, GraphMainRuntime, GraphTransportRuntime, TransportCommand,
    output_meter_channel,
};

use common::{fixture, graph_fixture};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: Mutex<()> = Mutex::new(());

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
fn prepared_arrangement_to_main_mix_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [0.0_f32; 64];
    let mut scratch_right = [0.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let result = runtime.process_window(
        0,
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    COUNTING.store(false, Ordering::SeqCst);

    assert!(result.is_ok());
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
    for (actual, source) in output_left.iter().zip(&fixture.samples[..64]) {
        assert!((*actual - *source * 0.25).abs() <= f32::EPSILON);
    }
    assert!(
        output_right
            .iter()
            .all(|sample| sample.abs() <= f32::EPSILON)
    );
}

#[test]
fn prepared_routing_graph_performs_zero_heap_allocations() {
    let _guard = TEST_LOCK.lock().unwrap();
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 64).unwrap();
    let automated =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 64)
            .unwrap();
    let (mut sender, mut transport) = GraphTransportRuntime::with_command_queue(automated);
    let mut left_0 = [0.0_f32; 64];
    let mut right_0 = [0.0_f32; 64];
    let mut left_1 = [0.0_f32; 64];
    let mut right_1 = [0.0_f32; 64];
    let mut left_2 = [0.0_f32; 64];
    let mut right_2 = [0.0_f32; 64];
    let mut left_3 = [0.0_f32; 64];
    let mut right_3 = [0.0_f32; 64];
    let mut left_4 = [0.0_f32; 64];
    let mut right_4 = [0.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut left_0, &mut right_0),
        RenderScratchSlot::new(&mut left_1, &mut right_1),
        RenderScratchSlot::new(&mut left_2, &mut right_2),
        RenderScratchSlot::new(&mut left_3, &mut right_3),
        RenderScratchSlot::new(&mut left_4, &mut right_4),
    ];
    let mut output_left = [0.0_f32; 64];
    let mut output_right = [0.0_f32; 64];
    let mut callback = InterleavedStereoCallback::try_new(64).unwrap();
    let (mut meter_sender, _meter_receiver) = output_meter_channel();
    let mut interleaved = [0.0_f32; 128];

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let result = runtime.process_window(
        0,
        &mut scratch,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let start_result = sender.try_send(TransportCommand::Start);
    let automated_result = transport.process_interleaved_cycle(
        &mut callback,
        &mut scratch,
        &mut interleaved,
        &mut meter_sender,
    );
    let stop_result = sender.try_send(TransportCommand::Stop);
    let stopped_result = transport.process_interleaved_cycle(
        &mut callback,
        &mut scratch,
        &mut interleaved,
        &mut meter_sender,
    );
    COUNTING.store(false, Ordering::SeqCst);

    assert!(result.is_ok());
    assert!(start_result.is_ok());
    assert!(automated_result.is_ok());
    assert!(stop_result.is_ok());
    assert!(stopped_result.is_ok());
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
}
