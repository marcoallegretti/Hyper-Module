mod common;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{MAX_PROCESS_FRAMES, StereoOutput};
use hyper_mixer_runtime::{
    FlatMainRuntime, GraphMainRuntime, RuntimeCreationError, RuntimeProcessError,
};

use common::{fixture, graph_fixture};

#[test]
fn verified_documents_render_through_fixed_main_mixer_slots() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [9.0_f32; 64];
    let mut scratch_right = [9.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [9.0_f32; 64];
    let mut output_right = [9.0_f32; 64];

    let rendered = runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    assert_eq!(runtime.slot_count(), 1);
    assert_eq!(rendered.active_slot_count, 1);
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
fn output_shape_failure_precedes_scratch_or_output_mutation() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = FlatMainRuntime::try_new(&prepared, &fixture.mixer).unwrap();
    let mut scratch_left = [7.0_f32; 64];
    let mut scratch_right = [7.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(
        &mut scratch_left,
        &mut scratch_right,
    )];
    let mut output_left = [8.0_f32; 64];
    let mut output_right = [8.0_f32; 63];

    assert_eq!(
        runtime.process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        ),
        Err(RuntimeProcessError::OutputLengthMismatch {
            left: 64,
            right: 63,
        })
    );
    assert_eq!(scratch[0].left(), &[7.0; 64]);
    assert_eq!(scratch[0].right(), &[7.0; 64]);
    assert!(
        output_left
            .iter()
            .all(|sample| (*sample - 8.0).abs() <= f32::EPSILON)
    );
    assert!(
        output_right
            .iter()
            .all(|sample| (*sample - 8.0).abs() <= f32::EPSILON)
    );
}

#[test]
fn verified_graph_executes_pre_post_returns_groups_solo_and_main() {
    let fixture = graph_fixture();
    assert_eq!(fixture.mixer.processing_order(), &[0, 1, 3, 4, 2]);
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut runtime = GraphMainRuntime::try_new(&prepared, &fixture.mixer, 64).unwrap();
    let mut left_0 = [9.0_f32; 64];
    let mut right_0 = [9.0_f32; 64];
    let mut left_1 = [9.0_f32; 64];
    let mut right_1 = [9.0_f32; 64];
    let mut left_2 = [9.0_f32; 64];
    let mut right_2 = [9.0_f32; 64];
    let mut left_3 = [9.0_f32; 64];
    let mut right_3 = [9.0_f32; 64];
    let mut left_4 = [9.0_f32; 64];
    let mut right_4 = [9.0_f32; 64];
    let mut scratch = [
        RenderScratchSlot::new(&mut left_0, &mut right_0),
        RenderScratchSlot::new(&mut left_1, &mut right_1),
        RenderScratchSlot::new(&mut left_2, &mut right_2),
        RenderScratchSlot::new(&mut left_3, &mut right_3),
        RenderScratchSlot::new(&mut left_4, &mut right_4),
    ];
    let mut output_left = [9.0_f32; 64];
    let mut output_right = [9.0_f32; 64];

    let rendered = runtime
        .process_window(
            0,
            &mut scratch,
            StereoOutput {
                left: &mut output_left,
                right: &mut output_right,
            },
        )
        .unwrap();

    assert_eq!(runtime.slot_count(), 5);
    assert_eq!(runtime.max_block_frames(), 64);
    assert_eq!(rendered.active_slot_count, 1);
    for ((left, right), source) in output_left
        .iter()
        .zip(&output_right)
        .zip(&fixture.samples[..64])
    {
        assert!((*left - *source * 0.375).abs() <= f32::EPSILON);
        assert!((*right - *source * 0.25).abs() <= f32::EPSILON);
    }
}

#[test]
fn graph_runtime_rejects_invalid_callback_capacity_before_allocation() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();

    assert!(matches!(
        GraphMainRuntime::try_new(&prepared, &fixture.mixer, 0),
        Err(RuntimeCreationError::InvalidMaximumBlockFrames {
            actual: 0,
            maximum: MAX_PROCESS_FRAMES,
        })
    ));
    assert!(matches!(
        GraphMainRuntime::try_new(&prepared, &fixture.mixer, MAX_PROCESS_FRAMES + 1),
        Err(RuntimeCreationError::InvalidMaximumBlockFrames {
            actual,
            maximum: MAX_PROCESS_FRAMES,
        }) if actual == MAX_PROCESS_FRAMES + 1
    ));
}
