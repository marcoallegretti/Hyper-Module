#![allow(unsafe_code)]

use core::alloc::{GlobalAlloc, Layout};
use std::alloc::System;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hyper_audio_engine::{
    ChannelParameters, ControlledInputBlock, InputBlock, InterleavedStereoCallback, LinearGain,
    Pan, StereoMixer, StereoOutput,
};

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
fn processing_performs_zero_heap_allocations() {
    let parameters = [
        ChannelParameters::new(LinearGain::UNITY, Pan::CENTER, true),
        ChannelParameters::new(LinearGain::UNITY, Pan::HARD_RIGHT, true),
    ];
    let mut mixer = StereoMixer::try_new(&parameters, LinearGain::UNITY).unwrap();
    let mono = [0.25_f32; 128];
    let stereo_left = [0.5_f32; 128];
    let stereo_right = [-0.5_f32; 128];
    let inputs = [
        InputBlock::Mono(&mono),
        InputBlock::Stereo {
            left: &stereo_left,
            right: &stereo_right,
        },
    ];
    let controlled_parameters = [parameters[0]; 128];
    let controlled_master = [LinearGain::UNITY; 128];
    let controlled_inputs = [ControlledInputBlock {
        input: inputs[0],
        parameters: &controlled_parameters,
    }];
    let mut controlled_mixer = StereoMixer::try_new(&parameters[..1], LinearGain::UNITY).unwrap();
    let mut output_left = [0.0_f32; 128];
    let mut output_right = [0.0_f32; 128];
    let mut callback = InterleavedStereoCallback::try_new(128).unwrap();
    let mut interleaved = [0.0_f32; 256];
    let mut failed_interleaved = [1.0_f32; 256];
    let mut invalid_interleaved = [1.0_f32; 3];

    ALLOCATIONS.store(0, Ordering::SeqCst);
    COUNTING.store(true, Ordering::SeqCst);
    let result = mixer.process(
        &inputs,
        &parameters,
        LinearGain::UNITY,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let controlled_result = controlled_mixer.process_controlled(
        &controlled_inputs,
        &controlled_master,
        StereoOutput {
            left: &mut output_left,
            right: &mut output_right,
        },
    );
    let callback_result = callback.process(&mut interleaved, |output| {
        output.left.fill(0.25);
        output.right.fill(-0.25);
        Ok::<_, u8>(())
    });
    let callback_failure = callback.process(&mut failed_interleaved, |output| {
        output.left.fill(0.5);
        output.right.fill(-0.5);
        Err::<(), _>(7_u8)
    });
    let callback_validation = callback.process(&mut invalid_interleaved, |_| Ok::<_, u8>(()));
    COUNTING.store(false, Ordering::SeqCst);

    assert!(result.is_ok());
    assert!(controlled_result.is_ok());
    assert!(callback_result.is_ok());
    assert!(callback_failure.is_err());
    assert!(callback_validation.is_err());
    assert_eq!(ALLOCATIONS.load(Ordering::SeqCst), 0);
}
