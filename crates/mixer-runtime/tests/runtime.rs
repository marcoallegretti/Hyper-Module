mod common;

use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::{MAX_PROCESS_FRAMES, StereoOutput};
use hyper_mixer_runtime::{
    FlatMainRuntime, GraphMainRuntime, RuntimeCreationError, RuntimeProcessError,
};

use common::{fixture, graph_fixture};

fn render_automated_graph(
    runtime: &mut GraphMainRuntime<'_, '_, '_>,
    block_size: usize,
) -> ([f32; 100], [f32; 100]) {
    let mut scratch_left = [[9.0_f32; 100]; 5];
    let mut scratch_right = [[9.0_f32; 100]; 5];
    let mut output_left = [9.0_f32; 100];
    let mut output_right = [9.0_f32; 100];
    let mut start = 0;
    while start < output_left.len() {
        let end = start.saturating_add(block_size).min(output_left.len());
        let frame_count = end - start;
        let [left_0, left_1, left_2, left_3, left_4] = &mut scratch_left;
        let [right_0, right_1, right_2, right_3, right_4] = &mut scratch_right;
        let mut scratch = [
            RenderScratchSlot::new(&mut left_0[..frame_count], &mut right_0[..frame_count]),
            RenderScratchSlot::new(&mut left_1[..frame_count], &mut right_1[..frame_count]),
            RenderScratchSlot::new(&mut left_2[..frame_count], &mut right_2[..frame_count]),
            RenderScratchSlot::new(&mut left_3[..frame_count], &mut right_3[..frame_count]),
            RenderScratchSlot::new(&mut left_4[..frame_count], &mut right_4[..frame_count]),
        ];
        runtime
            .process_window(
                start,
                &mut scratch,
                StereoOutput {
                    left: &mut output_left[start..end],
                    right: &mut output_right[start..end],
                },
            )
            .unwrap();
        start = end;
    }
    (output_left, output_right)
}

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
    assert_eq!(rendered.active_slot_count, 2);
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
fn automated_graph_is_sample_exact_and_block_size_independent() {
    let fixture = graph_fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let mut whole = GraphMainRuntime::try_new_automated(
        &prepared,
        &fixture.mixer,
        fixture.automation.clone(),
        100,
    )
    .unwrap();
    let mut split =
        GraphMainRuntime::try_new_automated(&prepared, &fixture.mixer, fixture.automation, 100)
            .unwrap();
    let (whole_left, whole_right) = render_automated_graph(&mut whole, 100);
    let (split_left, split_right) = render_automated_graph(&mut split, 37);

    for (whole, split) in whole_left.iter().zip(split_left) {
        assert!((*whole - split).abs() <= f32::EPSILON);
    }
    for (whole, split) in whole_right.iter().zip(split_right) {
        assert!((*whole - split).abs() <= f32::EPSILON);
    }
    assert!((whole_left[25] - 0.223_861_44).abs() <= f32::EPSILON);
    assert!((whole_right[25] - 0.176_986_44).abs() <= f32::EPSILON);
    assert!(whole_left[50].abs() <= f32::EPSILON);
    assert!(whole_right[50].abs() <= f32::EPSILON);
    assert!((whole_left[75] - 0.421_875).abs() <= f32::EPSILON);
    assert!((whole_right[75] - 0.187_5).abs() <= f32::EPSILON);
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
