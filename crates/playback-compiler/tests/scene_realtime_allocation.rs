#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::System;

use hyper_playback_compiler::scene::{
    LaunchQuantization, SceneClipResource, SceneLimits, ScenePerformanceInput, SceneResource,
    SceneSlotAction, SceneSlotResource, SceneTrackResource, compile_scene_performance,
};
use hyper_playback_compiler::{TempoEvent, Timeline};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::SeqCst) {
            ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn scene_window_execution_performs_zero_heap_allocations() {
    let tempos = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 500_000,
    }];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
    let tracks = [SceneTrackResource {
        id: "track",
        active: true,
    }];
    let clips = [SceneClipResource {
        id: "clip",
        track_id: "track",
        enabled: true,
        duration_ticks: 2,
        loop_enabled: false,
    }];
    let slots = [SceneSlotResource {
        track_id: "track",
        action: SceneSlotAction::Play { clip_id: "clip" },
    }];
    let scenes = [SceneResource {
        id: "scene",
        enabled: true,
        order: 0,
        launch_quantization: LaunchQuantization::Immediate,
        slots: &slots,
    }];
    let launches = [hyper_playback_compiler::scene::SceneLaunchResource {
        id: "launch",
        tick: 1,
        scene_id: "scene",
    }];
    let compiled = compile_scene_performance(
        timeline,
        ScenePerformanceInput {
            enabled: true,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &launches,
        },
        SceneLimits::VERSION_1,
    )
    .unwrap();
    let mut runtime = compiled.runtime_at(0).unwrap();
    let mut observed_frames = 0;

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let result = runtime.process_window(0, 64, |segment| {
        observed_frames += segment.frame_count;
    });
    COUNTING.store(false, Ordering::SeqCst);

    assert_eq!(result, Ok(3));
    assert_eq!(observed_frames, 64);
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
}
