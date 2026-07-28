use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{
    CacheLimits, DecodedMediaCache, DecodedMediaCacheBuilder, SceneAudioBinding,
};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::{
    CompiledScenePerformance, LaunchQuantization, SceneClipResource, SceneLimits,
    ScenePerformanceInput, SceneResource, SceneSlotAction, SceneSlotResource, SceneTrackResource,
    compile_scene_performance,
};
use hyper_playback_compiler::{
    ClipCompileSpec, TempoEvent, TickRange, Timeline, WarpMarker, compile_scene_audio_clip,
};
use hyper_playback_json::{
    CompiledAutomation, CompiledMainMixer, CompiledMixerGraph, LoaderLimits, MediaAssetFact,
    OwnedCompiledSchedule, PlaybackJsonInput, load,
};

pub struct Fixture {
    pub cache: DecodedMediaCache,
    pub schedule: OwnedCompiledSchedule,
    pub mixer: CompiledMainMixer,
    pub samples: Vec<f32>,
}

pub struct GraphFixture {
    pub cache: DecodedMediaCache,
    pub schedule: OwnedCompiledSchedule,
    pub mixer: CompiledMixerGraph,
    #[allow(
        dead_code,
        reason = "compiled automation is consumed by the functional and allocation binaries"
    )]
    pub automation: CompiledAutomation,
    #[allow(
        dead_code,
        reason = "sample vector is asserted by the functional test binary"
    )]
    pub samples: Vec<f32>,
}

pub struct PlaybackFixture {
    pub cache: DecodedMediaCache,
    pub schedule: OwnedCompiledSchedule,
    pub mixer: CompiledMixerGraph,
    pub authority: CompiledScenePerformance<'static>,
    pub scene_bindings: Vec<SceneAudioBinding<'static>>,
    pub arrangement_samples: Vec<f32>,
    pub scene_samples: Vec<f32>,
}

static PLAYBACK_SCENE_TRACKS: [SceneTrackResource<'static>; 2] = [
    SceneTrackResource {
        id: "scene-track",
        active: true,
    },
    SceneTrackResource {
        id: "arrangement-track",
        active: true,
    },
];
static PLAYBACK_SCENE_CLIPS: [SceneClipResource<'static>; 1] = [SceneClipResource {
    id: "scene-clip",
    track_id: "scene-track",
    enabled: true,
    duration_ticks: 4,
    loop_enabled: false,
}];
static PLAYBACK_PLAY_SLOTS: [SceneSlotResource<'static>; 1] = [SceneSlotResource {
    track_id: "scene-track",
    action: SceneSlotAction::Play {
        clip_id: "scene-clip",
    },
}];
static PLAYBACK_STOP_SLOTS: [SceneSlotResource<'static>; 1] = [SceneSlotResource {
    track_id: "scene-track",
    action: SceneSlotAction::Stop,
}];
static PLAYBACK_SCENES: [SceneResource<'static, 'static>; 2] = [
    SceneResource {
        id: "play",
        enabled: true,
        order: 0,
        launch_quantization: LaunchQuantization::Immediate,
        slots: &PLAYBACK_PLAY_SLOTS,
    },
    SceneResource {
        id: "stop",
        enabled: true,
        order: 1,
        launch_quantization: LaunchQuantization::Immediate,
        slots: &PLAYBACK_STOP_SLOTS,
    },
];
static PLAYBACK_LAUNCHES: [hyper_playback_compiler::scene::SceneLaunchResource<'static>; 2] = [
    hyper_playback_compiler::scene::SceneLaunchResource {
        id: "play-launch",
        tick: 1,
        scene_id: "play",
    },
    hyper_playback_compiler::scene::SceneLaunchResource {
        id: "stop-launch",
        tick: 3,
        scene_id: "stop",
    },
];

pub fn fixture() -> Fixture {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"track",
        "project_id":"project","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["clip"],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"clip",
        "project_id":"project","track_id":"track","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"asset","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[{
            "id":"placement","track_id":"track","clip_id":"clip",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let mixer = br#"{
        "format":"hyper-mixer","format_version":1,"id":"mixer",
        "project_id":"project",
        "channels":[{
            "track_id":"track","gain_millionths":500000,
            "pan_millionths":-1000000,"mute":false,"solo":false,
            "destination":{"kind":"main"},"sends":[]
        }],
        "return_buses":[],
        "main":{"gain_millionths":500000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 1] = [track];
    let clips: [&[u8]; 1] = [clip];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "project",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            scenes: &[],
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let schedule = loaded
        .compile(
            &[MediaAssetFact {
                id: "asset",
                sample_rate_hz: 48_000,
                frame_count: 100,
            }],
            GraphLimits::VERSION_1,
        )
        .unwrap();
    let mixer = loaded
        .load_mixer(mixer, LoaderLimits::VERSION_1)
        .unwrap()
        .compile_flat_main()
        .unwrap();

    let samples: Vec<f32> = (0_u16..100).map(|frame| f32::from(frame) / 100.0).collect();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 400,
    })
    .unwrap();
    builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, samples.clone()).unwrap(),
        )
        .unwrap();

    Fixture {
        cache: builder.seal(),
        schedule,
        mixer,
        samples,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one explicit fixture keeps both playback authorities and mixer order reviewable"
)]
pub fn playback_fixture() -> PlaybackFixture {
    let timeline_json = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let arrangement_track = br#"{
        "format":"hyper-track","format_version":1,"id":"arrangement-track",
        "project_id":"project","name":"Arrangement","kind":"audio","order":0,
        "active":true,"clip_ids":["arrangement-clip"],"extensions":{}
    }"#;
    let scene_track = br#"{
        "format":"hyper-track","format_version":1,"id":"scene-track",
        "project_id":"project","name":"Scene","kind":"audio","order":1,
        "active":true,"clip_ids":["scene-clip"],"extensions":{}
    }"#;
    let arrangement_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"arrangement-clip",
        "project_id":"project","track_id":"arrangement-track","name":"Arrangement Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"arrangement-asset","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let scene_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"scene-clip",
        "project_id":"project","track_id":"scene-track","name":"Scene Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"scene-asset","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[{
            "id":"arrangement-placement","track_id":"arrangement-track",
            "clip_id":"arrangement-clip","start_tick":0,"clip_start_tick":0,
            "duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let mixer_json = br#"{
        "format":"hyper-mixer","format_version":1,"id":"mixer",
        "project_id":"project",
        "channels":[
            {
                "track_id":"arrangement-track","gain_millionths":1000000,
                "pan_millionths":-1000000,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"scene-track","gain_millionths":1000000,
                "pan_millionths":1000000,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            }
        ],
        "return_buses":[],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 2] = [arrangement_track, scene_track];
    let clips: [&[u8]; 2] = [arrangement_clip, scene_clip];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "project",
            sample_rate_hz: 48_000,
            timeline: timeline_json,
            tracks: &tracks,
            clips: &clips,
            scenes: &[],
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let asset_facts = [
        MediaAssetFact {
            id: "arrangement-asset",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "scene-asset",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let schedule = loaded
        .compile(&asset_facts, GraphLimits::VERSION_1)
        .unwrap();
    let mixer = loaded
        .load_mixer(mixer_json, LoaderLimits::VERSION_1)
        .unwrap()
        .compile_main_graph()
        .unwrap();

    let tempos = [TempoEvent {
        tick: 0,
        microseconds_per_quarter: 1_000_000,
    }];
    let timeline = Timeline::try_new(48_000, &tempos).unwrap();
    let authority = compile_scene_performance(
        timeline,
        ScenePerformanceInput {
            enabled: true,
            tracks: &PLAYBACK_SCENE_TRACKS,
            clips: &PLAYBACK_SCENE_CLIPS,
            scenes: &PLAYBACK_SCENES,
            launches: &PLAYBACK_LAUNCHES,
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
            tick: 4,
            source_frame_offset: 100,
        },
    ];
    let compiled_scene_audio = compile_scene_audio_clip(
        timeline,
        ClipCompileSpec {
            enabled: true,
            duration_ticks: 4,
            source_start_frame: 0,
            source_frame_count: 100,
            asset_frame_count: 100,
            source_sample_rate_hz: 48_000,
            loop_enabled: false,
            loop_range: TickRange { start: 0, end: 4 },
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
    let scene_bindings = vec![SceneAudioBinding {
        track_id: "scene-track",
        launch_id: "play-launch",
        scene_id: "play",
        clip_id: "scene-clip",
        asset_id: "scene-asset",
        compiled: compiled_scene_audio,
    }];

    let arrangement_samples: Vec<f32> =
        (0_u16..100).map(|frame| f32::from(frame) / 100.0).collect();
    let scene_samples: Vec<f32> = (0_u16..100)
        .map(|frame| 1.0 + f32::from(frame) / 100.0)
        .collect();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 2,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 800,
    })
    .unwrap();
    builder
        .try_insert(
            "arrangement-asset",
            DecodedAudio::from_mono(48_000, arrangement_samples.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "scene-asset",
            DecodedAudio::from_mono(48_000, scene_samples.clone()).unwrap(),
        )
        .unwrap();

    PlaybackFixture {
        cache: builder.seal(),
        schedule,
        mixer,
        authority,
        scene_bindings,
        arrangement_samples,
        scene_samples,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one explicit graph fixture keeps every route and expected gain stage reviewable"
)]
pub fn graph_fixture() -> GraphFixture {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"timeline",
        "project_id":"project","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let source = br#"{
        "format":"hyper-track","format_version":1,"id":"source",
        "project_id":"project","name":"Source","kind":"audio","order":0,
        "active":true,"clip_ids":["clip"],"extensions":{}
    }"#;
    let silent = br#"{
        "format":"hyper-track","format_version":1,"id":"silent",
        "project_id":"project","name":"Silent","kind":"audio","order":1,
        "active":true,"clip_ids":["clip-silent"],"extensions":{}
    }"#;
    let group = br#"{
        "format":"hyper-track","format_version":1,"id":"group",
        "project_id":"project","name":"Group","kind":"group","order":2,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let return_a = br#"{
        "format":"hyper-track","format_version":1,"id":"return-a",
        "project_id":"project","name":"Return A","kind":"return","order":3,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let return_b = br#"{
        "format":"hyper-track","format_version":1,"id":"return-b",
        "project_id":"project","name":"Return B","kind":"return","order":4,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"clip",
        "project_id":"project","track_id":"source","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"asset","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let silent_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"clip-silent",
        "project_id":"project","track_id":"silent","name":"Silent Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"asset","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"arrangement",
        "project_id":"project","name":"Main","enabled":true,
        "placements":[
            {
                "id":"placement-silent","track_id":"silent","clip_id":"clip-silent",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"placement","track_id":"source","clip_id":"clip",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }
        ],
        "scene_launches":[],"extensions":{}
    }"#;
    let mixer = br#"{
        "format":"hyper-mixer","format_version":1,"id":"mixer",
        "project_id":"project",
        "channels":[
            {
                "track_id":"source","gain_millionths":500000,
                "pan_millionths":-1000000,"mute":false,"solo":true,
                "destination":{"kind":"track","track_id":"group"},
                "sends":[
                    {
                        "id":"send-a","bus":"a","tap":"pre-fader",
                        "gain_millionths":1000000,"pan_millionths":1000000,
                        "enabled":true
                    },
                    {
                        "id":"send-b","bus":"b","tap":"post-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":true
                    }
                ]
            },
            {
                "track_id":"silent","gain_millionths":1000000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"group","gain_millionths":500000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"return-a","gain_millionths":250000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"return-b","gain_millionths":500000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"track","track_id":"group"},"sends":[]
            }
        ],
        "return_buses":[
            {"bus":"a","track_id":"return-a"},
            {"bus":"b","track_id":"return-b"}
        ],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"solo","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    let automation = br#"{
        "format":"hyper-automation-take","format_version":1,
        "id":"automation","project_id":"project","name":"Main pass","enabled":true,
        "captured_at_utc":"2026-07-28T14:00:00.000Z",
        "start_tick":0,"end_tick":5,
        "lanes":[
            {
                "id":"automation-gain",
                "target":{"kind":"mixer-channel","track_id":"source","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":0},{"tick":4,"value":1000000}]
            },
            {
                "id":"automation-main-mute",
                "target":{"kind":"mixer-main","parameter":"mute"},
                "interpolation":"hold",
                "points":[
                    {"tick":0,"value":false},
                    {"tick":2,"value":true},
                    {"tick":3,"value":false}
                ]
            },
            {
                "id":"automation-send",
                "target":{"kind":"mixer-send","send_id":"send-a","parameter":"enabled"},
                "interpolation":"hold",
                "points":[
                    {"tick":0,"value":true},
                    {"tick":1,"value":false},
                    {"tick":2,"value":true}
                ]
            },
            {
                "id":"automation-solo",
                "target":{"kind":"mixer-channel","track_id":"source","parameter":"solo"},
                "interpolation":"hold",
                "points":[
                    {"tick":0,"value":true},
                    {"tick":1,"value":false},
                    {"tick":3,"value":true}
                ]
            }
        ],
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 5] = [source, silent, group, return_a, return_b];
    let clips: [&[u8]; 2] = [clip, silent_clip];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "project",
            sample_rate_hz: 48_000,
            timeline,
            tracks: &tracks,
            clips: &clips,
            scenes: &[],
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let schedule = loaded
        .compile(
            &[MediaAssetFact {
                id: "asset",
                sample_rate_hz: 48_000,
                frame_count: 100,
            }],
            GraphLimits::VERSION_1,
        )
        .unwrap();
    let loaded_mixer = loaded.load_mixer(mixer, LoaderLimits::VERSION_1).unwrap();
    let loaded_automation = loaded
        .load_automation_takes(&loaded_mixer, &[automation], LoaderLimits::VERSION_1)
        .unwrap();
    let mixer = loaded_mixer.compile_main_graph().unwrap();
    let automation = loaded
        .compile_main_automation(&mixer, &loaded_automation)
        .unwrap();
    let samples: Vec<f32> = (0_u16..100).map(|frame| f32::from(frame) / 100.0).collect();
    let mut builder = DecodedMediaCacheBuilder::try_new(CacheLimits {
        max_assets: 1,
        max_frames_per_asset: 100,
        max_decoded_sample_bytes: 400,
    })
    .unwrap();
    builder
        .try_insert(
            "asset",
            DecodedAudio::from_mono(48_000, samples.clone()).unwrap(),
        )
        .unwrap();

    GraphFixture {
        cache: builder.seal(),
        schedule,
        mixer,
        automation,
        samples,
    }
}
