#[path = "../../../test-support/realtime_allocator.rs"]
mod realtime_allocator;

use hyper_native_device::{
    DeviceChain, NativeCompressorState, NativeDeviceChainProcessor, NativeDeviceInstance,
    NativeFilterMode, NativeFilterState, PROJECT_SAMPLE_RATE_HZ,
};
use realtime_allocator::HeapMeasurement;

#[test]
fn native_device_chain_processing_performs_zero_heap_allocations() {
    let state = NativeFilterState::try_new(NativeFilterMode::LowPass, 1_000_000, 707_107).unwrap();
    let device = NativeDeviceInstance::try_new_filter(
        "018f47a2-6b2d-7c11-8a44-001122334491".to_owned(),
        true,
        state,
    )
    .unwrap();
    let compressor = NativeDeviceInstance::try_new_compressor(
        "018f47a2-6b2d-7c11-8a44-001122334492".to_owned(),
        true,
        NativeCompressorState::try_new(-18_000, 4_000_000, 10_000, 100_000, 0).unwrap(),
    )
    .unwrap();
    let chain = DeviceChain::try_new(
        "018f47a2-6b2d-7c11-8a44-001122334490".to_owned(),
        "018f47a2-6b2d-7c11-8a44-001122334455".to_owned(),
        "018f47a2-6b2d-7c11-8a44-001122334456".to_owned(),
        vec![device, compressor],
    )
    .unwrap();
    let mut processor =
        NativeDeviceChainProcessor::try_new(&chain, PROJECT_SAMPLE_RATE_HZ).unwrap();
    let mut left = [0.25_f32; 128];
    let mut right = [-0.25_f32; 128];

    let measurement = HeapMeasurement::start_verified();
    let result = processor.process_in_place(&mut left, &mut right);
    processor.reset();
    let heap_operations = measurement.finish();

    assert!(result.is_ok());
    assert_eq!(heap_operations, 0);
}
