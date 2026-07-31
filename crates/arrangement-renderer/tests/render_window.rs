mod common;

use hyper_arrangement_renderer::{
    ArrangementRenderer, RenderScratchSlot, RenderWindowError, RendererCreationError,
};
use hyper_audio_engine::MAX_PROCESS_FRAMES;

use common::fixture;

#[test]
fn renders_half_open_overlapping_mono_and_stereo_lanes_in_schedule_order() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut left_a = [9.0_f32; 80];
    let mut right_a = [9.0_f32; 80];
    let mut left_b = [9.0_f32; 80];
    let mut right_b = [9.0_f32; 80];
    let mut slots = [
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut left_b, &mut right_b),
    ];

    let outcome = renderer.render_window(90, 80, &mut slots).unwrap();

    assert_eq!(outcome.active_slot_count, 2);
    assert_eq!(renderer.track_lane_count(), 3);
    assert_eq!(slots[0].entry_index(), Some(0));
    assert_eq!(slots[0].channels(), 1);
    assert_eq!(&slots[0].left()[..10], &[0.0; 10]);
    assert_eq!(&slots[0].left()[10..], &fixture.mono_samples[..70]);
    assert_eq!(slots[0].right(), &[0.0; 80]);
    assert_eq!(slots[1].entry_index(), Some(2));
    assert_eq!(slots[1].channels(), 2);
    assert_eq!(&slots[1].left()[..35], &[0.0; 35]);
    assert_eq!(&slots[1].left()[35..], &fixture.stereo_left[..45]);
    assert_eq!(&slots[1].right()[..35], &[0.0; 35]);
    assert_eq!(&slots[1].right()[35..], &fixture.stereo_right[..45]);
}

#[test]
fn placement_end_is_exclusive_and_disabled_placements_consume_no_slot() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut left = [9.0_f32; 25];
    let mut right = [9.0_f32; 25];
    let mut slots = [RenderScratchSlot::new(&mut left, &mut right)];

    let outcome = renderer.render_window(200, 25, &mut slots).unwrap();

    assert_eq!(outcome.active_slot_count, 1);
    assert_eq!(slots[0].entry_index(), Some(2));
    assert_eq!(slots[0].left(), &fixture.stereo_left[75..]);
    assert_eq!(slots[0].right(), &fixture.stereo_right[75..]);

    let outcome = renderer.render_window(225, 0, &mut slots).unwrap();
    assert_eq!(outcome.active_slot_count, 0);
}

#[test]
fn one_window_can_render_sequential_placements_from_the_same_track() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut left_a = [0.0_f32; 160];
    let mut right_a = [0.0_f32; 160];
    let mut left_b = [0.0_f32; 160];
    let mut right_b = [0.0_f32; 160];
    let mut left_d = [0.0_f32; 160];
    let mut right_d = [0.0_f32; 160];
    let mut slots = [
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut left_b, &mut right_b),
        RenderScratchSlot::new(&mut left_d, &mut right_d),
    ];

    let outcome = renderer.render_window(190, 160, &mut slots).unwrap();

    assert_eq!(outcome.active_slot_count, 3);
    assert_eq!(slots[0].entry_index(), Some(0));
    assert_eq!(&slots[0].left()[..10], &fixture.mono_samples[90..]);
    assert_eq!(slots[1].entry_index(), Some(2));
    assert_eq!(&slots[1].left()[..35], &fixture.stereo_left[65..]);
    assert_eq!(slots[2].entry_index(), Some(3));
    assert_eq!(&slots[2].left()[..35], &[0.0; 35]);
    assert_eq!(&slots[2].left()[35..135], &fixture.sequential_samples);
    assert_eq!(&slots[2].left()[135..], &[0.0; 25]);
}

#[test]
fn fixed_track_rendering_preserves_mixer_order_and_silent_lanes() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new_for_tracks(
        &prepared,
        &[
            "018f47a2-6b2d-7c11-8a44-001122335003",
            "018f47a2-6b2d-7c11-8a44-001122335002",
            "018f47a2-6b2d-7c11-8a44-001122335004",
            "empty-track",
        ],
    )
    .unwrap();
    let mut left_b = [9.0_f32; 160];
    let mut right_b = [9.0_f32; 160];
    let mut left_a = [9.0_f32; 160];
    let mut right_a = [9.0_f32; 160];
    let mut left_c = [9.0_f32; 160];
    let mut right_c = [9.0_f32; 160];
    let mut left_empty = [9.0_f32; 160];
    let mut right_empty = [9.0_f32; 160];
    let mut slots = [
        RenderScratchSlot::new(&mut left_b, &mut right_b),
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut left_c, &mut right_c),
        RenderScratchSlot::new(&mut left_empty, &mut right_empty),
    ];

    let outcome = renderer.render_track_window(190, 160, &mut slots).unwrap();

    assert_eq!(outcome.active_slot_count, 2);
    assert_eq!(renderer.track_lane_count(), 4);
    assert_eq!(slots[0].entry_index(), Some(2));
    assert_eq!(slots[0].channels(), 2);
    assert_eq!(&slots[0].left()[..35], &fixture.stereo_left[65..]);
    assert_eq!(&slots[0].left()[35..], &[0.0; 125]);
    assert_eq!(&slots[0].right()[..35], &fixture.stereo_right[65..]);
    assert_eq!(&slots[0].right()[35..], &[0.0; 125]);
    assert_eq!(slots[1].entry_index(), Some(0));
    assert_eq!(slots[1].channels(), 1);
    assert_eq!(&slots[1].left()[..10], &fixture.mono_samples[90..]);
    assert_eq!(&slots[1].left()[10..35], &[0.0; 25]);
    assert_eq!(&slots[1].left()[35..135], &fixture.sequential_samples);
    assert_eq!(&slots[1].left()[135..], &[0.0; 25]);
    assert_eq!(slots[1].right(), &[0.0; 160]);
    for slot in &slots[2..] {
        assert_eq!(slot.entry_index(), None);
        assert_eq!(slot.channels(), 1);
        assert_eq!(slot.left(), &[0.0; 160]);
        assert_eq!(slot.right(), &[0.0; 160]);
    }
}

#[test]
fn fixed_track_construction_and_scratch_shape_are_strict() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();

    assert_eq!(
        ArrangementRenderer::try_new_for_tracks(
            &prepared,
            &[
                "018f47a2-6b2d-7c11-8a44-001122335002",
                "018f47a2-6b2d-7c11-8a44-001122335002"
            ]
        )
        .unwrap_err(),
        RendererCreationError::DuplicateTrackId { track_index: 1 }
    );
    assert_eq!(
        ArrangementRenderer::try_new_for_tracks(
            &prepared,
            &[
                "018f47a2-6b2d-7c11-8a44-001122335002",
                "018f47a2-6b2d-7c11-8a44-001122335003"
            ]
        )
        .unwrap_err(),
        RendererCreationError::MissingScheduleTrack { entry_index: 1 }
    );

    let renderer = ArrangementRenderer::try_new_for_tracks(
        &prepared,
        &[
            "018f47a2-6b2d-7c11-8a44-001122335002",
            "018f47a2-6b2d-7c11-8a44-001122335003",
            "018f47a2-6b2d-7c11-8a44-001122335004",
        ],
    )
    .unwrap();
    let mut left = [7.0_f32; 80];
    let mut right = [7.0_f32; 80];
    let mut one_slot = [RenderScratchSlot::new(&mut left, &mut right)];
    assert_eq!(
        renderer.render_track_window(90, 80, &mut one_slot),
        Err(RenderWindowError::ScratchSlotCountMismatch {
            expected: 3,
            actual: 1,
        })
    );
    assert_eq!(one_slot[0].left(), &[7.0; 80]);
    assert_eq!(one_slot[0].right(), &[7.0; 80]);
}

#[test]
fn insufficient_or_malformed_scratch_is_rejected_before_mutation() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();
    let mut left = [7.0_f32; 80];
    let mut right = [7.0_f32; 80];
    let mut one_slot = [RenderScratchSlot::new(&mut left, &mut right)];

    assert_eq!(
        renderer.render_window(90, 80, &mut one_slot),
        Err(RenderWindowError::InsufficientScratchSlots {
            required: 2,
            available: 1,
        })
    );
    assert_eq!(one_slot[0].left(), &[7.0; 80]);
    assert_eq!(one_slot[0].right(), &[7.0; 80]);

    let mut left_a = [8.0_f32; 80];
    let mut right_a = [8.0_f32; 80];
    let mut short_left = [8.0_f32; 79];
    let mut right_b = [8.0_f32; 80];
    let mut malformed = [
        RenderScratchSlot::new(&mut left_a, &mut right_a),
        RenderScratchSlot::new(&mut short_left, &mut right_b),
    ];
    assert_eq!(
        renderer.render_window(90, 80, &mut malformed),
        Err(RenderWindowError::ScratchLengthMismatch {
            slot: 1,
            expected: 80,
            left: 79,
            right: 80,
        })
    );
    assert_eq!(malformed[0].left(), &[8.0; 80]);
    assert_eq!(malformed[0].right(), &[8.0; 80]);
}

#[test]
fn rejects_oversized_and_overflowing_windows_without_scratch() {
    let fixture = fixture();
    let prepared = fixture.cache.prepare_schedule(&fixture.schedule).unwrap();
    let renderer = ArrangementRenderer::try_new(&prepared).unwrap();

    assert_eq!(
        renderer.render_window(0, MAX_PROCESS_FRAMES + 1, &mut []),
        Err(RenderWindowError::BlockTooLarge {
            actual: MAX_PROCESS_FRAMES + 1,
            maximum: MAX_PROCESS_FRAMES,
        })
    );
    assert_eq!(
        renderer.render_window(usize::MAX, 1, &mut []),
        Err(RenderWindowError::WindowOverflow)
    );
}
