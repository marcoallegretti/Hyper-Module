use hyper_audio_source::DecodedAudio;
use hyper_media_cache::{CacheLimits, DecodedMediaCache, DecodedMediaCacheBuilder};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::{
    CompiledAutomation, CompiledMainMixer, CompiledMixerGraph, CompiledPlaybackGraph,
    LoadedPlaybackProject, LoaderLimits, MediaAssetFact, OwnedCompiledSchedule, PlaybackJsonInput,
    load,
};

pub struct Fixture {
    pub cache: DecodedMediaCache,
    pub schedule: OwnedCompiledSchedule,
    pub mixer: CompiledMainMixer,
    pub samples: Vec<f32>,
}

pub struct GraphFixture {
    pub cache: DecodedMediaCache,
    #[allow(
        dead_code,
        reason = "the functional runtime binary composes native devices with the loaded graph"
    )]
    pub project: LoadedPlaybackProject,
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

impl GraphFixture {
    #[allow(
        dead_code,
        reason = "the functional runtime binary composes native devices with the loaded graph"
    )]
    pub fn compile_playback(&self) -> CompiledPlaybackGraph<'_> {
        let facts = [MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122336404",
            sample_rate_hz: 48_000,
            frame_count: 100,
        }];
        self.project
            .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap()
    }
}

pub struct PlaybackFixture {
    pub cache: DecodedMediaCache,
    pub project: LoadedPlaybackProject,
    pub mixer: CompiledMixerGraph,
    #[allow(
        dead_code,
        reason = "compiled automation is consumed by the allocation test binary"
    )]
    pub automation: CompiledAutomation,
    pub arrangement_samples: Vec<f32>,
    pub scene_samples: Vec<f32>,
}

impl PlaybackFixture {
    pub fn compile_playback(&self) -> CompiledPlaybackGraph<'_> {
        let asset_facts = [
            MediaAssetFact {
                id: "018f47a2-6b2d-7c11-8a44-001122336444",
                sample_rate_hz: 48_000,
                frame_count: 100,
            },
            MediaAssetFact {
                id: "018f47a2-6b2d-7c11-8a44-001122336445",
                sample_rate_hz: 48_000,
                frame_count: 100,
            },
        ];
        self.project
            .compile_playback(&asset_facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
            .unwrap()
    }
}

pub fn fixture() -> Fixture {
    let timeline = br#"{
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336401",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336402",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Audio","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122336403"],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336403",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","track_id":"018f47a2-6b2d-7c11-8a44-001122336402","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122336404","start_frame":0,"frame_count":100},
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
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336405",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122336406","track_id":"018f47a2-6b2d-7c11-8a44-001122336402","clip_id":"018f47a2-6b2d-7c11-8a44-001122336403",
            "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
        }],
        "scene_launches":[],"extensions":{}
    }"#;
    let mixer = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336407",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400",
        "channels":[{
            "track_id":"018f47a2-6b2d-7c11-8a44-001122336402","gain_millionths":500000,
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
            project_id: "018f47a2-6b2d-7c11-8a44-001122336400",
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
    let facts = [MediaAssetFact {
        id: "018f47a2-6b2d-7c11-8a44-001122336404",
        sample_rate_hz: 48_000,
        frame_count: 100,
    }];
    let schedule = loaded.compile(&facts, GraphLimits::VERSION_1).unwrap();
    let mixer = loaded
        .load_mixer(mixer, &facts, LoaderLimits::VERSION_1)
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
            "018f47a2-6b2d-7c11-8a44-001122336404",
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
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336401",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let arrangement_track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336440",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Arrangement","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122336442"],"extensions":{}
    }"#;
    let scene_track = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336441",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Scene","kind":"audio","order":1,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122336443"],"extensions":{}
    }"#;
    let arrangement_clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336442",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","track_id":"018f47a2-6b2d-7c11-8a44-001122336440","name":"Arrangement Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122336444","start_frame":0,"frame_count":100},
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
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336443",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","track_id":"018f47a2-6b2d-7c11-8a44-001122336441","name":"Scene Clip",
        "enabled":true,"duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122336445","start_frame":0,"frame_count":100},
        "loop":{"enabled":false,"start_tick":0,"end_tick":4},
        "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
        "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
        "warp_markers":[
            {"tick":0,"source_frame_offset":0},
            {"tick":4,"source_frame_offset":100}
        ],
        "extensions":{}
    }"#;
    let play_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336446",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Play","order":0,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122336441","action":"play","clip_id":"018f47a2-6b2d-7c11-8a44-001122336443"}],
        "extensions":{}
    }"#;
    let stop_scene = br#"{
        "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336447",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Stop","order":1,"enabled":true,
        "launch_quantization":{"mode":"immediate"},
        "slots":[{"track_id":"018f47a2-6b2d-7c11-8a44-001122336441","action":"stop"}],
        "extensions":{}
    }"#;
    let arrangement = br#"{
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336405",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Main","enabled":true,
        "placements":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122336448","track_id":"018f47a2-6b2d-7c11-8a44-001122336440",
            "clip_id":"018f47a2-6b2d-7c11-8a44-001122336442","start_tick":0,"clip_start_tick":0,
            "duration_ticks":4,"enabled":true
        }],
        "scene_launches":[
            {"id":"018f47a2-6b2d-7c11-8a44-001122336449","tick":1,"scene_id":"018f47a2-6b2d-7c11-8a44-001122336446"},
            {"id":"018f47a2-6b2d-7c11-8a44-001122336450","tick":3,"scene_id":"018f47a2-6b2d-7c11-8a44-001122336447"}
        ],
        "extensions":{}
    }"#;
    let mixer_json = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336407",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336400",
        "channels":[
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336440","gain_millionths":1000000,
                "pan_millionths":-1000000,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336441","gain_millionths":1000000,
                "pan_millionths":1000000,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            }
        ],
        "return_buses":[],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"main","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    let automation_json = br#"{
        "format":"hyper-automation-take","format_version":1,
        "id":"018f47a2-6b2d-7c11-8a44-001122336408","project_id":"018f47a2-6b2d-7c11-8a44-001122336400","name":"Unity","enabled":true,
        "captured_at_utc":"2026-07-28T14:00:00.000Z",
        "start_tick":0,"end_tick":5,
        "lanes":[{
            "id":"018f47a2-6b2d-7c11-8a44-001122336409",
            "target":{"kind":"mixer-main","parameter":"gain"},
            "interpolation":"linear",
            "points":[
                {"tick":0,"value":1000000},
                {"tick":4,"value":1000000}
            ]
        }],
        "extensions":{}
    }"#;
    let tracks: [&[u8]; 2] = [arrangement_track, scene_track];
    let clips: [&[u8]; 2] = [arrangement_clip, scene_clip];
    let scenes: [&[u8]; 2] = [play_scene, stop_scene];
    let loaded = load(
        PlaybackJsonInput {
            project_id: "018f47a2-6b2d-7c11-8a44-001122336400",
            sample_rate_hz: 48_000,
            timeline: timeline_json,
            tracks: &tracks,
            clips: &clips,
            scenes: &scenes,
            arrangement: Some(arrangement),
        },
        LoaderLimits::VERSION_1,
    )
    .unwrap();
    let facts = [
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122336444",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
        MediaAssetFact {
            id: "018f47a2-6b2d-7c11-8a44-001122336445",
            sample_rate_hz: 48_000,
            frame_count: 100,
        },
    ];
    let loaded_mixer = loaded
        .load_mixer(mixer_json, &facts, LoaderLimits::VERSION_1)
        .unwrap();
    let loaded_automation = loaded
        .load_automation_takes(
            &loaded_mixer,
            &facts,
            &[automation_json],
            LoaderLimits::VERSION_1,
        )
        .unwrap();
    let mixer = loaded_mixer.compile_main_graph().unwrap();
    let automation = loaded
        .compile_main_automation(&mixer, &loaded_automation)
        .unwrap();

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
            "018f47a2-6b2d-7c11-8a44-001122336444",
            DecodedAudio::from_mono(48_000, arrangement_samples.clone()).unwrap(),
        )
        .unwrap();
    builder
        .try_insert(
            "018f47a2-6b2d-7c11-8a44-001122336445",
            DecodedAudio::from_mono(48_000, scene_samples.clone()).unwrap(),
        )
        .unwrap();

    PlaybackFixture {
        cache: builder.seal(),
        project: loaded,
        mixer,
        automation,
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
        "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336401",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","ticks_per_quarter":1920,
        "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
        "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
        "extensions":{}
    }"#;
    let source = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336421",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Source","kind":"audio","order":0,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122336403"],"extensions":{}
    }"#;
    let silent = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336422",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Silent","kind":"audio","order":1,
        "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122336427"],"extensions":{}
    }"#;
    let group = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336423",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Group","kind":"group","order":2,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let return_a = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336424",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Return A","kind":"return","order":3,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let return_b = br#"{
        "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336425",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Return B","kind":"return","order":4,
        "active":true,"clip_ids":[],"extensions":{}
    }"#;
    let clip = br#"{
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336403",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","track_id":"018f47a2-6b2d-7c11-8a44-001122336421","name":"Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122336404","start_frame":0,"frame_count":100},
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
        "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336427",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","track_id":"018f47a2-6b2d-7c11-8a44-001122336422","name":"Silent Clip","enabled":true,
        "duration_ticks":4,
        "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122336404","start_frame":0,"frame_count":100},
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
        "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336405",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Main","enabled":true,
        "placements":[
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336431","track_id":"018f47a2-6b2d-7c11-8a44-001122336421","clip_id":"018f47a2-6b2d-7c11-8a44-001122336403",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336430","track_id":"018f47a2-6b2d-7c11-8a44-001122336422","clip_id":"018f47a2-6b2d-7c11-8a44-001122336427",
                "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
            }
        ],
        "scene_launches":[],"extensions":{}
    }"#;
    let mixer = br#"{
        "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122336407",
        "project_id":"018f47a2-6b2d-7c11-8a44-001122336420",
        "channels":[
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336421","gain_millionths":500000,
                "pan_millionths":-1000000,"mute":false,"solo":true,
                "destination":{"kind":"track","track_id":"018f47a2-6b2d-7c11-8a44-001122336423"},
                "sends":[
                    {
                        "id":"018f47a2-6b2d-7c11-8a44-001122336433","bus":"a","tap":"pre-fader",
                        "gain_millionths":1000000,"pan_millionths":1000000,
                        "enabled":true
                    },
                    {
                        "id":"018f47a2-6b2d-7c11-8a44-001122336434","bus":"b","tap":"post-fader",
                        "gain_millionths":1000000,"pan_millionths":0,
                        "enabled":true
                    }
                ]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336422","gain_millionths":1000000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336423","gain_millionths":500000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336424","gain_millionths":250000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"main"},"sends":[]
            },
            {
                "track_id":"018f47a2-6b2d-7c11-8a44-001122336425","gain_millionths":500000,
                "pan_millionths":0,"mute":false,"solo":false,
                "destination":{"kind":"track","track_id":"018f47a2-6b2d-7c11-8a44-001122336423"},"sends":[]
            }
        ],
        "return_buses":[
            {"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-001122336424"},
            {"bus":"b","track_id":"018f47a2-6b2d-7c11-8a44-001122336425"}
        ],
        "main":{"gain_millionths":1000000,"mute":false},
        "cue":{"source":"solo","gain_millionths":1000000,"mute":false},
        "extensions":{}
    }"#;
    let automation = br#"{
        "format":"hyper-automation-take","format_version":1,
        "id":"018f47a2-6b2d-7c11-8a44-001122336408","project_id":"018f47a2-6b2d-7c11-8a44-001122336420","name":"Main pass","enabled":true,
        "captured_at_utc":"2026-07-28T14:00:00.000Z",
        "start_tick":0,"end_tick":5,
        "lanes":[
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336436",
                "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122336421","parameter":"gain"},
                "interpolation":"linear",
                "points":[{"tick":0,"value":0},{"tick":4,"value":1000000}]
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336437",
                "target":{"kind":"mixer-main","parameter":"mute"},
                "interpolation":"hold",
                "points":[
                    {"tick":0,"value":false},
                    {"tick":2,"value":true},
                    {"tick":3,"value":false}
                ]
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336438",
                "target":{"kind":"mixer-send","send_id":"018f47a2-6b2d-7c11-8a44-001122336433","parameter":"enabled"},
                "interpolation":"hold",
                "points":[
                    {"tick":0,"value":true},
                    {"tick":1,"value":false},
                    {"tick":2,"value":true}
                ]
            },
            {
                "id":"018f47a2-6b2d-7c11-8a44-001122336439",
                "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122336421","parameter":"solo"},
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
            project_id: "018f47a2-6b2d-7c11-8a44-001122336420",
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
    let facts = [MediaAssetFact {
        id: "018f47a2-6b2d-7c11-8a44-001122336404",
        sample_rate_hz: 48_000,
        frame_count: 100,
    }];
    let schedule = loaded.compile(&facts, GraphLimits::VERSION_1).unwrap();
    let loaded_mixer = loaded
        .load_mixer(mixer, &facts, LoaderLimits::VERSION_1)
        .unwrap();
    let loaded_automation = loaded
        .load_automation_takes(
            &loaded_mixer,
            &facts,
            &[automation],
            LoaderLimits::VERSION_1,
        )
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
            "018f47a2-6b2d-7c11-8a44-001122336404",
            DecodedAudio::from_mono(48_000, samples.clone()).unwrap(),
        )
        .unwrap();

    GraphFixture {
        cache: builder.seal(),
        project: loaded,
        schedule,
        mixer,
        automation,
        samples,
    }
}
