use hyper_playback_compiler::scene::{
    LaunchQuantization, PlayingSceneClip, RecordedSceneExtent, SceneClipResource,
    SceneCompileError, SceneLaunchState, SceneLimits, ScenePerformanceInput, SceneProcessError,
    SceneResource, SceneRuntimeError, SceneSlotAction, SceneSlotResource, SceneTrackResource,
    compile_scene_performance,
};
use hyper_playback_compiler::{TempoEvent, Timeline};

fn timeline() -> Timeline<'static> {
    static TEMPOS: [TempoEvent; 1] = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 500_000,
    }];
    Timeline::try_new(48_000, &TEMPOS).unwrap()
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario keeps sparse continuation, replacement, expiry, and locate expectations together"
)]
fn scene_authority_is_sparse_sample_exact_and_seekable() {
    let tracks = [
        SceneTrackResource {
            id: "track-a",
            active: true,
        },
        SceneTrackResource {
            id: "track-b",
            active: true,
        },
    ];
    let clips = [
        SceneClipResource {
            id: "loop-a",
            track_id: "track-a",
            enabled: true,
            duration_ticks: 4,
            loop_enabled: true,
        },
        SceneClipResource {
            id: "one-shot-b",
            track_id: "track-b",
            enabled: true,
            duration_ticks: 2,
            loop_enabled: false,
        },
        SceneClipResource {
            id: "one-shot-a",
            track_id: "track-a",
            enabled: true,
            duration_ticks: 1,
            loop_enabled: false,
        },
    ];
    let opening_slots = [
        SceneSlotResource {
            track_id: "track-a",
            action: SceneSlotAction::Play { clip_id: "loop-a" },
        },
        SceneSlotResource {
            track_id: "track-b",
            action: SceneSlotAction::Play {
                clip_id: "one-shot-b",
            },
        },
    ];
    let stop_slots = [SceneSlotResource {
        track_id: "track-b",
        action: SceneSlotAction::Stop,
    }];
    let replacement_slots = [SceneSlotResource {
        track_id: "track-a",
        action: SceneSlotAction::Play {
            clip_id: "one-shot-a",
        },
    }];
    let scenes = [
        SceneResource {
            id: "opening",
            enabled: true,
            order: 0,
            launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
            slots: &opening_slots,
        },
        SceneResource {
            id: "stop-b",
            enabled: true,
            order: 1,
            launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
            slots: &stop_slots,
        },
        SceneResource {
            id: "replace-a",
            enabled: true,
            order: 2,
            launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
            slots: &replacement_slots,
        },
    ];
    let launches = [
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-opening",
            tick: 0,
            scene_id: "opening",
        },
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-stop",
            tick: 4,
            scene_id: "stop-b",
        },
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-replacement",
            tick: 8,
            scene_id: "replace-a",
        },
    ];
    let compiled = compile_scene_performance(
        timeline(),
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
    assert_eq!(compiled.track_count(), 2);
    assert_eq!(compiled.track_id(0), Some("track-a"));
    assert_eq!(compiled.track_id(2), None);
    assert_eq!(
        compiled.recorded_extent(),
        RecordedSceneExtent::Finite { end_frame: 113 }
    );

    let open_ended = compile_scene_performance(
        timeline(),
        ScenePerformanceInput {
            enabled: true,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &launches[..1],
        },
        SceneLimits::VERSION_1,
    )
    .unwrap();
    assert_eq!(open_ended.recorded_extent(), RecordedSceneExtent::Unbounded);
    let empty = compile_scene_performance(
        timeline(),
        ScenePerformanceInput {
            enabled: true,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &[],
        },
        SceneLimits::VERSION_1,
    )
    .unwrap();
    assert_eq!(empty.recorded_extent(), RecordedSceneExtent::Empty);

    let mut runtime = compiled.runtime_at(0).unwrap();
    let mut boundaries = Vec::new();
    let segment_count = runtime
        .process_window(0, 120, |segment| {
            boundaries.push((
                segment.offset_frames,
                segment.frame_count,
                segment.track_states[0].map(PlayingSceneClip::clip_id),
                segment.track_states[1].map(PlayingSceneClip::clip_id),
            ));
        })
        .unwrap();

    assert_eq!(segment_count, 5);
    assert_eq!(
        boundaries,
        [
            (0, 25, Some("loop-a"), Some("one-shot-b")),
            (25, 25, Some("loop-a"), None),
            (50, 50, Some("loop-a"), None),
            (100, 13, Some("one-shot-a"), None),
            (113, 7, None, None),
        ]
    );
    assert_eq!(runtime.position(), 120);
    assert!(matches!(
        runtime.process_window(121, 1, |_| {}),
        Err(SceneRuntimeError::WindowNotContiguous {
            expected: 120,
            actual: 121
        })
    ));

    runtime.seek(60);
    assert_eq!(
        runtime.track_states()[0].map(PlayingSceneClip::clip_id),
        Some("loop-a")
    );
    assert_eq!(runtime.track_states()[1], None);
    let mut located_boundaries = Vec::new();
    runtime
        .process_window(60, 60, |segment| {
            located_boundaries.push((segment.offset_frames, segment.frame_count));
        })
        .unwrap();
    assert_eq!(located_boundaries, [(0, 40), (40, 13), (53, 7)]);
    runtime.seek(usize::MAX);
    assert_eq!(
        runtime.process_window(usize::MAX, 1, |_| {}),
        Err(SceneRuntimeError::FrameOverflow)
    );

    runtime.seek(0);
    let before = runtime.track_states().to_vec();
    let mut observations = 0;
    let failed = runtime.try_process_window(0, 120, |_| {
        observations += 1;
        if observations == 2 {
            Err("render failed")
        } else {
            Ok(())
        }
    });
    assert!(matches!(
        failed,
        Err(SceneProcessError::Observer("render failed"))
    ));
    assert_eq!(runtime.position(), 0);
    assert_eq!(runtime.track_states(), before);
}

#[test]
fn invalid_scene_authority_is_rejected_before_publication() {
    let tracks = [SceneTrackResource {
        id: "track",
        active: true,
    }];
    let clips = [SceneClipResource {
        id: "clip",
        track_id: "track",
        enabled: true,
        duration_ticks: 4,
        loop_enabled: true,
    }];
    let duplicate_slots = [
        SceneSlotResource {
            track_id: "track",
            action: SceneSlotAction::Stop,
        },
        SceneSlotResource {
            track_id: "track",
            action: SceneSlotAction::Play { clip_id: "clip" },
        },
    ];
    let scenes = [SceneResource {
        id: "scene",
        enabled: true,
        order: 0,
        launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
        slots: &duplicate_slots,
    }];
    let launches = [hyper_playback_compiler::scene::SceneLaunchResource {
        id: "launch",
        tick: 4,
        scene_id: "scene",
    }];

    let duplicate_slot = compile_scene_performance(
        timeline(),
        ScenePerformanceInput {
            enabled: true,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &launches,
        },
        SceneLimits::VERSION_1,
    );
    assert!(matches!(
        duplicate_slot,
        Err(SceneCompileError::DuplicateSlotTrack {
            scene_index: 0,
            slot_index: 1
        })
    ));

    let valid_slots = [duplicate_slots[0]];
    let valid_scene = SceneResource {
        slots: &valid_slots,
        ..scenes[0]
    };
    let off_grid_launch = [hyper_playback_compiler::scene::SceneLaunchResource {
        tick: 3,
        ..launches[0]
    }];
    assert!(matches!(
        compile_scene_performance(
            timeline(),
            ScenePerformanceInput {
                enabled: true,
                tracks: &tracks,
                clips: &clips,
                scenes: &[valid_scene],
                launches: &off_grid_launch,
            },
            SceneLimits::VERSION_1,
        ),
        Err(SceneCompileError::LaunchOffGrid { launch_index: 0 })
    ));

    let unsorted_launches = [
        launches[0],
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-2",
            ..launches[0]
        },
    ];
    assert!(matches!(
        compile_scene_performance(
            timeline(),
            ScenePerformanceInput {
                enabled: true,
                tracks: &tracks,
                clips: &clips,
                scenes: &[valid_scene],
                launches: &unsorted_launches,
            },
            SceneLimits::VERSION_1,
        ),
        Err(SceneCompileError::LaunchesNotStrictlyIncreasing { launch_index: 1 })
    ));
}

#[test]
fn disabled_authority_is_silent_and_disabled_clips_replace_prior_state() {
    let tracks = [SceneTrackResource {
        id: "track",
        active: true,
    }];
    let clips = [SceneClipResource {
        id: "clip",
        track_id: "track",
        enabled: false,
        duration_ticks: 4,
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
        tick: 1,
        scene_id: "scene",
    }];
    let compiled = compile_scene_performance(
        timeline(),
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
    let runtime = compiled.runtime_at(13).unwrap();
    let playing = runtime.track_states()[0].unwrap();
    assert_eq!(playing.started_at_frame(), 13);
    assert!(!playing.enabled());

    let disabled = compile_scene_performance(
        timeline(),
        ScenePerformanceInput {
            enabled: false,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &launches,
        },
        SceneLimits::VERSION_1,
    )
    .unwrap();
    assert_eq!(disabled.runtime_at(100).unwrap().track_states(), [None]);
}

#[test]
fn scene_resource_limits_and_references_are_checked() {
    let tracks = [SceneTrackResource {
        id: "track",
        active: true,
    }];
    let foreign_clips = [SceneClipResource {
        id: "clip",
        track_id: "missing-track",
        enabled: true,
        duration_ticks: 1,
        loop_enabled: false,
    }];
    let slots = [SceneSlotResource {
        track_id: "track",
        action: SceneSlotAction::Stop,
    }];
    let scene = SceneResource {
        id: "scene",
        enabled: true,
        order: 8,
        launch_quantization: LaunchQuantization::Immediate,
        slots: &slots,
    };

    assert!(matches!(
        compile_scene_performance(
            timeline(),
            ScenePerformanceInput {
                enabled: true,
                tracks: &tracks,
                clips: &foreign_clips,
                scenes: &[],
                launches: &[],
            },
            SceneLimits::VERSION_1,
        ),
        Err(SceneCompileError::MissingClipTrack { clip_index: 0 })
    ));
    assert!(matches!(
        compile_scene_performance(
            timeline(),
            ScenePerformanceInput {
                enabled: true,
                tracks: &tracks,
                clips: &[],
                scenes: &[scene],
                launches: &[],
            },
            SceneLimits::VERSION_1,
        ),
        Err(SceneCompileError::InvalidSceneOrder { scene_index: 0 })
    ));
    assert!(matches!(
        compile_scene_performance(
            timeline(),
            ScenePerformanceInput {
                enabled: true,
                tracks: &tracks,
                clips: &[],
                scenes: &[],
                launches: &[],
            },
            SceneLimits {
                max_tracks: 0,
                ..SceneLimits::VERSION_1
            },
        ),
        Err(SceneCompileError::LimitExceeded {
            collection: hyper_playback_compiler::scene::SceneCollection::Track,
            actual: 1,
            maximum: 0
        })
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one interactive launch transaction keeps rollback, committed grid evidence, seek reset, and immediate-launch evidence together"
)]
fn interactive_launch_is_quantized_transactional_and_cleared_by_seek() {
    let tracks = [SceneTrackResource {
        id: "track",
        active: true,
    }];
    let clips = [SceneClipResource {
        id: "one-shot",
        track_id: "track",
        enabled: true,
        duration_ticks: 2,
        loop_enabled: false,
    }];
    let play_slots = [SceneSlotResource {
        track_id: "track",
        action: SceneSlotAction::Play {
            clip_id: "one-shot",
        },
    }];
    let stop_slots = [SceneSlotResource {
        track_id: "track",
        action: SceneSlotAction::Stop,
    }];
    let scenes = [
        SceneResource {
            id: "play",
            enabled: true,
            order: 0,
            launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
            slots: &play_slots,
        },
        SceneResource {
            id: "stop",
            enabled: true,
            order: 1,
            launch_quantization: LaunchQuantization::Immediate,
            slots: &stop_slots,
        },
    ];
    let compiled = compile_scene_performance(
        timeline(),
        ScenePerformanceInput {
            enabled: true,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            launches: &[],
        },
        SceneLimits::VERSION_1,
    )
    .unwrap();
    assert_eq!(compiled.scene_count(), 2);
    assert_eq!(compiled.scene_id(0), Some("play"));
    assert_eq!(compiled.interactive_clips().count(), 1);

    let mut runtime = compiled.runtime_at(1).unwrap();
    runtime.queue_scene_launch(0).unwrap();
    assert_eq!(runtime.launch_state().queued_scene_index(), Some(0));
    assert_eq!(runtime.launch_state().queued_launch_frame(), Some(50));
    assert_eq!(runtime.launch_state().last_launched_scene_index(), None);
    let checkpoint = runtime.checkpoint();
    let mut segments = Vec::new();
    let failure = runtime.try_process_window(1, 60, |segment| {
        segments.push((
            segment.offset_frames,
            segment.track_states[0].map(PlayingSceneClip::clip_id),
        ));
        (segment.offset_frames < 49).then_some(()).ok_or("render")
    });
    assert!(matches!(
        failure,
        Err(SceneProcessError::Observer("render"))
    ));
    assert_eq!(runtime.checkpoint(), checkpoint);
    assert_eq!(runtime.launch_state().queued_scene_index(), Some(0));
    assert_eq!(runtime.launch_state().last_launched_scene_index(), None);
    assert_eq!(runtime.launch_state().last_interactive_launch(), None);

    let mut rendered_segments = Vec::new();
    runtime
        .process_window(1, 130, |segment| {
            rendered_segments.push((
                segment.offset_frames,
                segment.frame_count,
                segment.track_states[0].map(PlayingSceneClip::clip_id),
            ));
        })
        .unwrap();
    assert_eq!(
        rendered_segments,
        [(0, 49, None), (49, 25, Some("one-shot")), (74, 56, None)]
    );
    assert_eq!(runtime.launch_state().queued_scene_index(), None);
    assert_eq!(runtime.launch_state().queued_launch_frame(), None);
    assert_eq!(runtime.launch_state().last_launched_scene_index(), Some(0));
    let launch = runtime.launch_state().last_interactive_launch().unwrap();
    assert_eq!(launch.scene_index(), 0);
    assert_eq!(launch.tick(), 4);
    assert_eq!(launch.frame(), 50);
    assert_eq!(launch.arrangement_tick(), Some(4));
    assert_eq!(launch.sequence(), 1);

    runtime.seek(50);
    assert_eq!(runtime.track_states(), [None]);
    assert_eq!(runtime.launch_state(), SceneLaunchState::default());
    runtime.queue_scene_launch(0).unwrap();
    runtime.queue_scene_launch(1).unwrap();
    assert_eq!(runtime.launch_state().queued_scene_index(), Some(1));
    runtime.process_window(50, 1, |_| {}).unwrap();
    assert_eq!(runtime.track_states(), [None]);
    assert_eq!(runtime.launch_state().last_launched_scene_index(), Some(1));
    let launch = runtime.launch_state().last_interactive_launch().unwrap();
    assert_eq!(launch.scene_index(), 1);
    assert_eq!(launch.tick(), 4);
    assert_eq!(launch.frame(), 50);
    assert_eq!(launch.arrangement_tick(), None);
    assert_eq!(launch.sequence(), 2);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one recorded and interactive authority fixture keeps the stop boundary auditable"
)]
fn stopping_interactive_scenes_preserves_recorded_authority_and_cancels_pending_launches() {
    let tracks = [
        SceneTrackResource {
            id: "recorded-track",
            active: true,
        },
        SceneTrackResource {
            id: "interactive-track",
            active: true,
        },
    ];
    let clips = [
        SceneClipResource {
            id: "recorded-loop",
            track_id: "recorded-track",
            enabled: true,
            duration_ticks: 16,
            loop_enabled: true,
        },
        SceneClipResource {
            id: "interactive-loop",
            track_id: "interactive-track",
            enabled: true,
            duration_ticks: 16,
            loop_enabled: true,
        },
    ];
    let recorded_slots = [SceneSlotResource {
        track_id: "recorded-track",
        action: SceneSlotAction::Play {
            clip_id: "recorded-loop",
        },
    }];
    let interactive_slots = [SceneSlotResource {
        track_id: "interactive-track",
        action: SceneSlotAction::Play {
            clip_id: "interactive-loop",
        },
    }];
    let recorded_stop_slots = [SceneSlotResource {
        track_id: "recorded-track",
        action: SceneSlotAction::Stop,
    }];
    let scenes = [
        SceneResource {
            id: "recorded",
            enabled: true,
            order: 0,
            launch_quantization: LaunchQuantization::Immediate,
            slots: &recorded_slots,
        },
        SceneResource {
            id: "interactive",
            enabled: true,
            order: 1,
            launch_quantization: LaunchQuantization::Grid { quantum_ticks: 4 },
            slots: &interactive_slots,
        },
        SceneResource {
            id: "recorded-stop",
            enabled: true,
            order: 2,
            launch_quantization: LaunchQuantization::Immediate,
            slots: &recorded_stop_slots,
        },
    ];
    let launches = [
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "recorded-launch",
            tick: 0,
            scene_id: "recorded",
        },
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "recorded-stop-launch",
            tick: 8,
            scene_id: "recorded-stop",
        },
    ];
    let compiled = compile_scene_performance(
        timeline(),
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
    let mut runtime = compiled.runtime_at(1).unwrap();

    runtime.queue_scene_launch(1).unwrap();
    runtime.process_window(1, 50, |_| {}).unwrap();
    assert_eq!(
        runtime.track_states()[0].map(PlayingSceneClip::clip_id),
        Some("recorded-loop")
    );
    assert_eq!(
        runtime.track_states()[1].map(PlayingSceneClip::clip_id),
        Some("interactive-loop")
    );
    assert_eq!(runtime.launch_state().last_launched_scene_index(), Some(1));

    runtime.queue_scene_launch(1).unwrap();
    assert_eq!(runtime.launch_state().queued_scene_index(), Some(1));
    runtime.stop_interactive_scenes();

    assert_eq!(runtime.launch_state().queued_scene_index(), None);
    assert_eq!(runtime.launch_state().last_launched_scene_index(), Some(1));
    assert_eq!(
        runtime.track_states()[0].map(PlayingSceneClip::clip_id),
        Some("recorded-loop")
    );
    assert_eq!(runtime.track_states()[1], None);
    assert_eq!(runtime.track_controlled(), [true, true]);

    runtime.process_window(51, 50, |_| {}).unwrap();
    assert_eq!(runtime.track_states(), [None, None]);
    assert_eq!(runtime.track_controlled(), [true, true]);
    assert_eq!(runtime.launch_state().last_launched_scene_index(), Some(1));
}
