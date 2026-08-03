#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::alloc::System;

use hyper_arrangement_renderer::{RenderScratchSlot, SceneRenderer};
use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{CacheLimits, DecodedMediaCacheBuilder, SceneAudioBinding};
use hyper_playback_compiler::scene::{
    LaunchQuantization, SceneClipResource, SceneLimits, ScenePerformanceInput, ScenePlaybackSource,
    SceneResource, SceneSlotAction, SceneSlotResource, SceneTrackResource,
    compile_scene_performance,
};
use hyper_playback_compiler::{
    ClipCompileSpec, TempoEvent, TickRange, Timeline, WarpMarker, compile_scene_audio_clip,
};

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
#[allow(
    clippy::too_many_lines,
    reason = "allocation measurement requires the complete prepared scene path before counting begins"
)]
fn prepared_scene_rendering_performs_zero_heap_allocations() {
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
        loop_enabled: true,
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
        tick: 0,
        scene_id: "scene",
    }];
    let authority = compile_scene_performance(
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
    let markers = [
        WarpMarker {
            tick: 0,
            source_frame_offset: 0,
        },
        WarpMarker {
            tick: 2,
            source_frame_offset: 25,
        },
    ];
    let compiled = compile_scene_audio_clip(
        timeline,
        ClipCompileSpec {
            enabled: true,
            duration_ticks: 2,
            source_start_frame: 0,
            source_frame_count: 25,
            asset_frame_count: 25,
            source_sample_rate_hz: 48_000,
            loop_enabled: true,
            loop_range: TickRange { start: 0, end: 2 },
            reverse: false,
            gain_decibels: 0.0,
            transpose_cents: 0,
            fade_in_ticks: 0,
            fade_out_ticks: 0,
            warp_markers: &markers,
        },
        0,
    )
    .unwrap();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 25,
        max_decoded_sample_bytes: 100,
    })
    .unwrap();
    builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, vec![0.25; 25]).unwrap(),
        )
        .unwrap();
    let cache = builder.seal();
    let bindings = [SceneAudioBinding {
        track_id: "track",
        source: ScenePlaybackSource::Recorded {
            launch_id: "launch",
        },
        scene_id: "scene",
        clip_id: "clip",
        asset_id: "asset",
        compiled,
    }];
    let prepared = cache.prepare_scene_audio(&authority, &bindings).unwrap();
    let mut renderer = SceneRenderer::try_new(&authority, &prepared, 0).unwrap();
    let mut left = [0.0_f32; 64];
    let mut right = [0.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let result = renderer.render_window(0, 64, &mut scratch);
    COUNTING.store(false, Ordering::SeqCst);

    assert!(result.is_ok());
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
}
