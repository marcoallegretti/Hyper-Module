use hyper_arrangement_renderer::{RenderScratchSlot, SceneRenderError, SceneRenderer};
use hyper_audio_engine::{ChannelParameters, LinearGain, Pan, StereoMixer, StereoOutput};
use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{
    CacheLimits, DecodedMediaCacheBuilder, PrepareSceneAudioError, SceneAudioBinding,
};
use hyper_playback_compiler::scene::{
    LaunchQuantization, SceneClipResource, SceneLimits, ScenePerformanceInput, ScenePlaybackSource,
    SceneResource, SceneSlotAction, SceneSlotResource, SceneTrackResource,
    compile_scene_performance,
};
use hyper_playback_compiler::{
    ClipCompileSpec, TempoEvent, TickRange, Timeline, WarpMarker, compile_scene_audio_clip,
};

const EPSILON: f32 = 1.0e-6;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end scenario keeps authority, binding, rendering, and mixer evidence together"
)]
fn prepared_scene_audio_launches_loops_stops_and_reaches_the_mixer() {
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
    let play_slots = [SceneSlotResource {
        track_id: "track",
        action: SceneSlotAction::Play { clip_id: "clip" },
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
            launch_quantization: LaunchQuantization::Immediate,
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
    let launches = [
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-play",
            tick: 1,
            scene_id: "play",
        },
        hyper_playback_compiler::scene::SceneLaunchResource {
            id: "launch-stop",
            tick: 4,
            scene_id: "stop",
        },
    ];
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
    let compiled_audio = compile_scene_audio_clip(
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
        1,
    )
    .unwrap();
    assert_eq!(compiled_audio.launch_frame(), 13);
    assert!(compiled_audio.is_looping());

    let source: Vec<f32> = (0_u8..25).map(|sample| f32::from(sample) / 100.0).collect();
    let mut cache_builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 25,
        max_decoded_sample_bytes: 100,
    })
    .unwrap();
    cache_builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, source.clone()).unwrap(),
        )
        .unwrap();
    let cache = cache_builder.seal();
    let bindings = [SceneAudioBinding {
        track_id: "track",
        source: ScenePlaybackSource::Recorded {
            launch_id: "launch-play",
        },
        scene_id: "play",
        clip_id: "clip",
        asset_id: "asset",
        compiled: compiled_audio,
    }];
    let mismatched_bindings = [SceneAudioBinding {
        source: ScenePlaybackSource::Recorded {
            launch_id: "unknown-launch",
        },
        ..bindings[0]
    }];
    assert!(matches!(
        cache.prepare_scene_audio(&authority, &mismatched_bindings),
        Err(PrepareSceneAudioError::AuthorityMismatch { binding_index: 0 })
    ));

    let prepared = cache.prepare_scene_audio(&authority, &bindings).unwrap();
    let mut renderer = SceneRenderer::try_new(&authority, &prepared, 0).unwrap();
    let mut left = [9.0_f32; 64];
    let mut right = [9.0_f32; 64];
    let mut scratch = [RenderScratchSlot::new(&mut left, &mut right)];

    let report = renderer.render_window(0, 64, &mut scratch).unwrap();

    assert_eq!(report.active_slot_count, 1);
    let expected: Vec<f32> = (0..64)
        .map(|frame| {
            if (13..50).contains(&frame) {
                source[(frame - 13) % source.len()]
            } else {
                0.0
            }
        })
        .collect();
    assert_samples(scratch[0].left(), &expected);
    assert_samples(scratch[0].right(), &[0.0; 64]);

    let parameters = [ChannelParameters::new(
        LinearGain::UNITY,
        Pan::HARD_RIGHT,
        true,
    )];
    let mut mixer = StereoMixer::try_new(&parameters, LinearGain::UNITY).unwrap();
    let inputs = [scratch[0].input_block()];
    let mut mixed_left = [9.0_f32; 64];
    let mut mixed_right = [9.0_f32; 64];
    mixer
        .process(
            &inputs,
            &parameters,
            LinearGain::UNITY,
            StereoOutput {
                left: &mut mixed_left,
                right: &mut mixed_right,
            },
        )
        .unwrap();
    assert_samples(&mixed_left, &[0.0; 64]);
    assert_samples(&mixed_right, &expected);

    assert_eq!(
        renderer.render_window(64, 1, &mut []),
        Err(SceneRenderError::ScratchSlotCountMismatch {
            expected: 1,
            actual: 0,
        })
    );
    assert_eq!(renderer.position(), 64);
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
