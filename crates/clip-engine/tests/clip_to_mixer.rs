use hyper_audio_engine::{ChannelParameters, LinearGain, Pan};
use hyper_audio_source::DecodedAudio;
use hyper_clip_engine::{ClipRenderPlan, FrameRange, NativeClipSpec, PlacementSpec, TimeMapping};
use hyper_offline_renderer::{AlignedInput, RenderSpec, render_aligned};

const EPSILON: f32 = 1.0e-6;

#[test]
fn rendered_looping_clip_reaches_the_shared_mixer() {
    let source = DecodedAudio::from_mono(48_000, vec![0.0, 0.25, 0.5, 0.75]).unwrap();
    let plan = ClipRenderPlan::try_new(
        &source,
        NativeClipSpec {
            enabled: true,
            source: FrameRange { start: 0, end: 4 },
            loop_range: Some(FrameRange { start: 1, end: 4 }),
            reverse: false,
            gain_decibels: 0.0,
            fade_in_frames: 0,
            fade_out_frames: 0,
            transpose_cents: 0,
            time_mapping: TimeMapping::NativeRate,
        },
    )
    .unwrap();
    let placement = plan
        .place(PlacementSpec {
            clip_start_frame: 0,
            frame_count: 7,
        })
        .unwrap();
    let clip_audio = plan.render_offline(placement).unwrap();
    let mixed = render_aligned(
        &[AlignedInput {
            source: &clip_audio,
            parameters: ChannelParameters::new(LinearGain::UNITY, Pan::HARD_RIGHT, true),
        }],
        RenderSpec {
            sample_rate_hz: 48_000,
            frame_count: 7,
            block_frames: 3,
        },
        LinearGain::UNITY,
    )
    .unwrap();

    assert_samples(mixed.plane(0).unwrap(), &[0.0; 7]);
    assert_samples(
        mixed.plane(1).unwrap(),
        &[0.0, 0.25, 0.5, 0.75, 0.25, 0.5, 0.75],
    );
}

fn assert_samples(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        assert!(
            (actual - expected).abs() <= EPSILON,
            "sample {index}: actual={actual}, expected={expected}"
        );
    }
}
