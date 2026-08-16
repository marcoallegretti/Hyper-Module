use hyper_arrangement_renderer::RenderScratchSlot;
use hyper_audio_engine::StereoOutput;
use hyper_audio_source::DecodedAudio;
use hyper_desktop_audio::{
    OfflineExportError, OfflineExportSpec, export_project_wav, render_project_offline,
    render_project_offline_with_mixer,
};
use hyper_media_cache::CacheLimits;
use hyper_mixer_runtime::{RoutedMainRuntime, RoutedTransportRuntime, TransportCommand};
use hyper_playback_compiler::graph::GraphLimits;
use hyper_playback_compiler::scene::SceneLimits;
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::{
    DecodedPlaybackAsset, PlaybackDocumentBytes, PlaybackSessionAssemblyInput,
    PlaybackSessionAssemblyLimits, PreparedPlaybackSession, assemble_playback_session,
};

const PROJECT_ID: &str = "018f47a2-6b2d-7c11-8a44-001122337500";
const TIMELINE: &[u8] = br#"{
    "format":"hyper-timeline","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337501",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","ticks_per_quarter":1920,
    "tempo_events":[{"tick":0,"microseconds_per_quarter":1000000}],
    "meter_events":[{"tick":0,"numerator":4,"denominator":4}],
    "extensions":{}
}"#;
const TRACK: &[u8] = br#"{
    "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337502",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Audio","kind":"audio","order":0,
    "active":true,"clip_ids":["018f47a2-6b2d-7c11-8a44-001122337503"],"extensions":{}
}"#;
const CLIP: &[u8] = br#"{
    "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337503",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","name":"Clip","enabled":true,
    "duration_ticks":4,
    "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337504","start_frame":0,"frame_count":100},
    "loop":{"enabled":false,"start_tick":0,"end_tick":4},
    "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
    "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
    "warp_markers":[
        {"tick":0,"source_frame_offset":0},
        {"tick":4,"source_frame_offset":100}
    ],
    "extensions":{}
}"#;
const ARRANGEMENT: &[u8] = br#"{
    "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
    "placements":[{
        "id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503",
        "start_tick":0,"clip_start_tick":0,"duration_ticks":4,"enabled":true
    }],
    "scene_launches":[],"extensions":{}
}"#;
const MIXER: &[u8] = br#"{
    "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
    "channels":[{
        "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":500000,
        "pan_millionths":-600000,"mute":false,"solo":false,
        "destination":{"kind":"main"},"sends":[]
    }],
    "return_buses":[],
    "main":{"gain_millionths":1000000,"mute":false},
    "cue":{"source":"main","gain_millionths":1000000,"mute":false},
    "extensions":{}
}"#;
const AUTOMATION: &[u8] = br#"{
    "format":"hyper-automation-take","format_version":1,
    "id":"018f47a2-6b2d-7c11-8a44-001122337508","project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Pass","enabled":true,
    "captured_at_utc":"2026-08-16T12:05:00.000Z",
    "start_tick":0,"end_tick":8,
    "lanes":[{
        "id":"018f47a2-6b2d-7c11-8a44-001122337509",
        "target":{"kind":"mixer-channel","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","parameter":"gain"},
        "interpolation":"linear",
        "points":[{"tick":0,"value":1000000},{"tick":3,"value":0}]
    }],
    "extensions":{}
}"#;

fn session(arrangement: Option<&[u8]>, automation: Vec<Vec<u8>>) -> PreparedPlaybackSession {
    assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: PlaybackDocumentBytes {
            project_id: PROJECT_ID.to_owned(),
            sample_rate_hz: 48_000,
            timeline: TIMELINE.to_vec(),
            tracks: vec![TRACK.to_vec()],
            clips: vec![CLIP.to_vec()],
            scenes: Vec::new(),
            arrangement: arrangement.map(<[u8]>::to_vec),
            mixer: MIXER.to_vec(),
            automation_takes: automation,
        },
        decoded_assets: vec![DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, (0..100_i16).map(|n| f32::from(n) / 200.0).collect()).unwrap(),
        )],
        limits: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 2,
                max_frames_per_asset: 100,
                max_decoded_sample_bytes: 800,
            },
        },
    })
    .unwrap()
}

/// Drives the same runtime the desktop output uses, one block at a time, so the
/// comparison is against playback rather than against a second renderer.
fn render_through_playback(session: &PreparedPlaybackSession, block_frames: usize) -> Vec<(f32, f32)> {
    let (project, mixer, automation, media) = session.parts();
    let facts: Vec<_> = media.media_asset_facts().collect();
    let graph = project
        .compile_playback(&facts, GraphLimits::VERSION_1, SceneLimits::VERSION_1)
        .unwrap();
    let prepared = media.prepare_playback(&graph).unwrap();
    let runtime = match automation {
        Some(automation) => RoutedMainRuntime::try_new_prepared_playback_automated(
            &prepared,
            mixer,
            automation.clone(),
            block_frames,
        ),
        None => RoutedMainRuntime::try_new_prepared_playback(&prepared, mixer, block_frames),
    }
    .unwrap();
    let slot_count = mixer.slots().len();
    let mut left_planes = vec![vec![0.0_f32; block_frames]; slot_count];
    let mut right_planes = vec![vec![0.0_f32; block_frames]; slot_count];
    let mut scratch: Vec<RenderScratchSlot<'_>> = left_planes
        .iter_mut()
        .zip(&mut right_planes)
        .map(|(left, right)| RenderScratchSlot::new(left, right))
        .collect();
    let (mut sender, mut transport) = RoutedTransportRuntime::with_command_queue(runtime);
    sender.try_send(TransportCommand::Start).unwrap();
    let mut block_left = vec![0.0_f32; block_frames];
    let mut block_right = vec![0.0_f32; block_frames];
    let mut frames = Vec::new();
    for _ in 0..8 {
        transport
            .process_cycle(
                &mut scratch,
                StereoOutput {
                    left: &mut block_left,
                    right: &mut block_right,
                },
            )
            .unwrap();
        frames.extend(block_left.iter().copied().zip(block_right.iter().copied()));
    }
    frames
}

#[test]
fn offline_export_matches_the_playback_runtime_sample_for_sample() {
    let session = session(Some(ARRANGEMENT), vec![AUTOMATION.to_vec()]);
    let rendered = render_project_offline(&session, OfflineExportSpec { block_frames: 32 }).unwrap();

    assert_eq!(rendered.sample_rate_hz(), 48_000);
    assert_eq!(rendered.frame_count(), 100);

    let expected = render_through_playback(&session, 32);
    let left = rendered.plane(0).unwrap();
    let right = rendered.plane(1).unwrap();
    for (index, (expected_left, expected_right)) in expected.iter().take(100).enumerate() {
        assert!(
            (left[index] - expected_left).abs() < f32::EPSILON,
            "left frame {index}: export {} playback {expected_left}",
            left[index]
        );
        assert!(
            (right[index] - expected_right).abs() < f32::EPSILON,
            "right frame {index}: export {} playback {expected_right}",
            right[index]
        );
    }
    assert!(
        left.iter().any(|sample| sample.abs() > 0.0),
        "the export is not silent"
    );
    assert!(
        left.iter().zip(right.iter()).any(|(l, r)| (l - r).abs() > f32::EPSILON),
        "the panned fixture distinguishes the two channels"
    );
}

#[test]
fn offline_export_is_independent_of_the_block_size() {
    let session = session(Some(ARRANGEMENT), vec![AUTOMATION.to_vec()]);
    let coarse = render_project_offline(&session, OfflineExportSpec { block_frames: 64 }).unwrap();
    let fine = render_project_offline(&session, OfflineExportSpec { block_frames: 16 }).unwrap();
    assert_eq!(coarse.frame_count(), fine.frame_count());
    for plane in 0..2 {
        assert_eq!(
            coarse.plane(plane).unwrap(),
            fine.plane(plane).unwrap(),
            "plane {plane} differs between block sizes"
        );
    }
}

#[test]
fn offline_export_refuses_a_project_that_renders_nothing() {
    let session = session(None, Vec::new());
    assert!(
        matches!(
            session.extent(),
            hyper_playback_session::PlaybackExtent::Bounded { end_frame: 0 }
        ),
        "a project with no arrangement is bounded at zero rather than unbounded"
    );
    assert!(matches!(
        render_project_offline(&session, OfflineExportSpec { block_frames: 32 }),
        Err(OfflineExportError::EmptyExtent)
    ));
}

#[test]
fn offline_export_rejects_an_unusable_block_size() {
    let session = session(Some(ARRANGEMENT), Vec::new());
    for block_frames in [0, hyper_audio_engine::MAX_PROCESS_FRAMES + 1] {
        assert!(matches!(
            render_project_offline(&session, OfflineExportSpec { block_frames }),
            Err(OfflineExportError::InvalidBlockFrames)
        ));
    }
}

#[test]
fn exported_wav_carries_the_rendered_samples_without_quantization() {
    let session = session(Some(ARRANGEMENT), vec![AUTOMATION.to_vec()]);
    let spec = OfflineExportSpec { block_frames: 32 };
    let rendered = render_project_offline(&session, spec).unwrap();

    let directory = std::env::temp_dir().join(format!(
        "hyper-offline-export-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let journal = directory.join("export.journal");
    let wav = directory.join("export.wav");

    let report = export_project_wav(&session, spec, &journal, &wav).unwrap();
    assert_eq!(report.frame_count, rendered.frame_count());
    assert_eq!(report.sample_rate_hz, 48_000);
    assert!(report.sample_peak > 0.0, "the export is not silent");

    let expected_sample_peak = rendered
        .plane(0)
        .unwrap()
        .iter()
        .chain(rendered.plane(1).unwrap())
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    assert!(
        (report.sample_peak - expected_sample_peak).abs() < f32::EPSILON,
        "the reported sample peak is the rendered peak"
    );
    let mut reference_true_peak_meter =
        hyper_audio_engine::StereoTruePeakMeter::try_new(48_000).unwrap();
    let mut reference_loudness_meter =
        hyper_audio_engine::StereoLoudnessMeter::try_new(48_000).unwrap();
    let mut reference_true_peak = 0.0_f32;
    let rendered_left = rendered.plane(0).unwrap();
    let rendered_right = rendered.plane(1).unwrap();
    for start in (0..rendered.frame_count()).step_by(32) {
        let end = (start + 32).min(rendered.frame_count());
        let block = reference_true_peak_meter
            .process(&rendered_left[start..end], &rendered_right[start..end])
            .unwrap();
        let block_true_peak = block.left_linear().max(block.right_linear());
        reference_true_peak = reference_true_peak.max(block_true_peak);
        reference_loudness_meter
            .process(
                &rendered_left[start..end],
                &rendered_right[start..end],
                block_true_peak,
            )
            .unwrap();
    }
    assert!(
        (report.true_peak - reference_true_peak).abs() < f32::EPSILON,
        "the reported true peak is the oversampled meter's: report {} meter {}",
        report.true_peak,
        reference_true_peak
    );
    assert_eq!(
        report.integrated_lufs,
        reference_loudness_meter.snapshot().integrated_lufs(),
        "the reported loudness is the running EBU meter's"
    );
    assert!(
        report.integrated_lufs.is_none(),
        "100 frames is far below the EBU 400 ms gate, so no integrated value exists yet"
    );

    let published = std::fs::read(&wav).unwrap();
    assert_eq!(&published[0..4], b"RIFF");
    assert_eq!(&published[8..12], b"WAVE");

    let decoded = hyper_audio_source::decode_bytes(
        published.clone(),
        hyper_audio_source::AudioContainer::Wav,
        48_000,
        hyper_audio_source::DecodeLimits::new(1_048_576, 4_096, 65_536).unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.frame_count(), rendered.frame_count());
    for plane in 0..2 {
        assert_eq!(
            decoded.plane(plane).unwrap(),
            rendered.plane(plane).unwrap(),
            "published plane {plane} differs from the rendered audio"
        );
    }
    let _ = std::fs::remove_dir_all(&directory);
}

const LONG_CLIP: &[u8] = br#"{
    "format":"hyper-audio-clip","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337503",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","name":"Clip","enabled":true,
    "duration_ticks":1920,
    "source":{"asset_id":"018f47a2-6b2d-7c11-8a44-001122337504","start_frame":0,"frame_count":48000},
    "loop":{"enabled":false,"start_tick":0,"end_tick":1920},
    "playback":{"gain_decibels":0,"reverse":false,"transpose_cents":0},
    "fades":{"curve":"linear-amplitude","in_ticks":0,"out_ticks":0},
    "warp_markers":[
        {"tick":0,"source_frame_offset":0},
        {"tick":1920,"source_frame_offset":48000}
    ],
    "extensions":{}
}"#;
const LONG_ARRANGEMENT: &[u8] = br#"{
    "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
    "placements":[{
        "id":"018f47a2-6b2d-7c11-8a44-001122337506","track_id":"018f47a2-6b2d-7c11-8a44-001122337502","clip_id":"018f47a2-6b2d-7c11-8a44-001122337503",
        "start_tick":0,"clip_start_tick":0,"duration_ticks":1920,"enabled":true
    }],
    "scene_launches":[],"extensions":{}
}"#;

/// One second of steady tone, which clears the EBU 400 ms gate.
fn long_session() -> PreparedPlaybackSession {
    let samples: Vec<f32> = (0..48_000_i32)
        .map(|n| (f32::from(i16::try_from(n % 480).unwrap()) / 480.0).mul_add(2.0, -1.0) * 0.5)
        .collect();
    assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: PlaybackDocumentBytes {
            project_id: PROJECT_ID.to_owned(),
            sample_rate_hz: 48_000,
            timeline: TIMELINE.to_vec(),
            tracks: vec![TRACK.to_vec()],
            clips: vec![LONG_CLIP.to_vec()],
            scenes: Vec::new(),
            arrangement: Some(LONG_ARRANGEMENT.to_vec()),
            mixer: MIXER.to_vec(),
            automation_takes: Vec::new(),
        },
        decoded_assets: vec![DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, samples).unwrap(),
        )],
        limits: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 2,
                max_frames_per_asset: 48_000,
                max_decoded_sample_bytes: 400_000,
            },
        },
    })
    .unwrap()
}

#[test]
fn a_programme_past_the_gate_reports_integrated_loudness_and_a_true_peak() {
    let session = long_session();
    let spec = OfflineExportSpec { block_frames: 256 };
    let directory = std::env::temp_dir().join(format!(
        "hyper-offline-loudness-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();

    let report = export_project_wav(
        &session,
        spec,
        &directory.join("export.journal"),
        &directory.join("export.wav"),
    )
    .unwrap();

    assert_eq!(report.frame_count, 48_000);
    assert_eq!(
        hyper_audio_engine::StereoLoudnessMeter::try_new(48_000)
            .unwrap()
            .state(),
        hyper_audio_engine::LoudnessMeasurementState::Running,
        "the export relies on a new loudness meter measuring by default"
    );
    let integrated = report
        .integrated_lufs
        .expect("one second of tone clears the EBU 400 ms gate");
    assert!(
        (-70.0..0.0).contains(&integrated),
        "integrated loudness is a plausible LUFS value: {integrated}"
    );
    assert!(report.true_peak > 0.0);
    assert!(
        report.true_peak >= report.sample_peak,
        "true peak {} is not below sample peak {}",
        report.true_peak,
        report.sample_peak
    );
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_project_shorter_than_one_block_still_exports() {
    let session = session(Some(ARRANGEMENT), Vec::new());
    let directory = std::env::temp_dir().join(format!(
        "hyper-offline-short-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let wav = directory.join("short.wav");

    // The desktop passes 1024, far beyond this project's 100 frames.
    let report = export_project_wav(
        &session,
        OfflineExportSpec {
            block_frames: 1_024,
        },
        &directory.join("short.journal"),
        &wav,
    )
    .expect("a block larger than the extent must not fail the export");

    assert_eq!(report.frame_count, 100);
    assert!(wav.exists());
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn re_exporting_to_the_same_destination_replaces_it_and_leaves_no_journal() {
    let session = session(Some(ARRANGEMENT), Vec::new());
    let directory = std::env::temp_dir().join(format!(
        "hyper-offline-reexport-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let journal = directory.join("bounce.journal");
    let wav = directory.join("bounce.wav");
    let spec = OfflineExportSpec { block_frames: 32 };

    export_project_wav(&session, spec, &journal, &wav).unwrap();
    let first = std::fs::read(&wav).unwrap();
    assert!(!journal.exists(), "a successful export leaves no staging journal");

    // Tweak the mix, as a bounce-tweak-bounce loop does, so the second export
    // is genuinely different content rather than an idempotent republication.
    let (project, _, _, media) = session.parts();
    let facts: Vec<_> = media.media_asset_facts().collect();
    let quieter = String::from_utf8(MIXER.to_vec())
        .unwrap()
        .replace("\"gain_millionths\":500000", "\"gain_millionths\":125000");
    let tweaked = project
        .load_mixer(quieter.as_bytes(), &facts, LoaderLimits::VERSION_1)
        .unwrap()
        .compile_main_graph()
        .unwrap();

    let second_report =
        hyper_desktop_audio::export_project_wav_with_mixer(
            &session,
            spec,
            Some(&tweaked),
            &journal,
            &wav,
        )
        .expect("bounce, tweak, bounce again must not fail on the second bounce");
    assert_eq!(second_report.frame_count, 100);
    assert_ne!(
        std::fs::read(&wav).unwrap(),
        first,
        "the destination carries the newer render, not the first one"
    );

    // A journal orphaned by an earlier failure must not poison later exports.
    std::fs::write(&journal, b"stale").unwrap();
    export_project_wav(&session, spec, &journal, &wav)
        .expect("a stale staging journal must not permanently block export");
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_live_mixer_override_changes_the_render_and_must_match_the_prepared_layout() {
    let session = session(Some(ARRANGEMENT), Vec::new());
    let spec = OfflineExportSpec { block_frames: 32 };
    let prepared_render = render_project_offline(&session, spec).unwrap();

    let (project, _, _, media) = session.parts();
    let facts: Vec<_> = media.media_asset_facts().collect();
    let quieter = String::from_utf8(MIXER.to_vec())
        .unwrap()
        .replace("\"gain_millionths\":500000", "\"gain_millionths\":125000");
    let live = project
        .load_mixer(quieter.as_bytes(), &facts, LoaderLimits::VERSION_1)
        .unwrap()
        .compile_main_graph()
        .unwrap();

    let live_render = render_project_offline_with_mixer(&session, spec, Some(&live)).unwrap();
    assert_eq!(live_render.frame_count(), prepared_render.frame_count());
    assert!(
        live_render
            .plane(0)
            .unwrap()
            .iter()
            .zip(prepared_render.plane(0).unwrap())
            .any(|(live, prepared)| (live - prepared).abs() > f32::EPSILON),
        "a different live gain must produce a different render"
    );

}

const GROUP_TRACK: &[u8] = br#"{
    "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-00112233750a",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Group","kind":"group","order":1,
    "active":true,"clip_ids":[],"extensions":{}
}"#;
const RETURN_TRACK: &[u8] = br#"{
    "format":"hyper-track","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-00112233750b",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Return","kind":"return","order":2,
    "active":true,"clip_ids":[],"extensions":{}
}"#;
const ROUTED_MIXER: &[u8] = br#"{
    "format":"hyper-mixer","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337507",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500",
    "channels":[
        {
            "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","gain_millionths":700000,
            "pan_millionths":-400000,"mute":false,"solo":false,
            "destination":{"kind":"track","track_id":"018f47a2-6b2d-7c11-8a44-00112233750a"},
            "sends":[{
                "id":"018f47a2-6b2d-7c11-8a44-00112233750c","bus":"a","tap":"post-fader",
                "gain_millionths":800000,"pan_millionths":300000,"enabled":true
            }]
        },
        {
            "track_id":"018f47a2-6b2d-7c11-8a44-00112233750a","gain_millionths":900000,
            "pan_millionths":200000,"mute":false,"solo":false,
            "destination":{"kind":"main"},"sends":[]
        },
        {
            "track_id":"018f47a2-6b2d-7c11-8a44-00112233750b","gain_millionths":600000,
            "pan_millionths":-200000,"mute":false,"solo":false,
            "destination":{"kind":"main"},"sends":[]
        }
    ],
    "return_buses":[{"bus":"a","track_id":"018f47a2-6b2d-7c11-8a44-00112233750b"}],
    "main":{"gain_millionths":900000,"mute":false},
    "cue":{"source":"main","gain_millionths":1000000,"mute":false},
    "extensions":{}
}"#;

/// A group destination, a post-fader send, and a return bus — three of the
/// things ADR-117 cites when it rejects the flat aligned-source renderer.
fn routed_session() -> PreparedPlaybackSession {
    assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: PlaybackDocumentBytes {
            project_id: PROJECT_ID.to_owned(),
            sample_rate_hz: 48_000,
            timeline: TIMELINE.to_vec(),
            tracks: vec![TRACK.to_vec(), GROUP_TRACK.to_vec(), RETURN_TRACK.to_vec()],
            clips: vec![CLIP.to_vec()],
            scenes: Vec::new(),
            arrangement: Some(ARRANGEMENT.to_vec()),
            mixer: ROUTED_MIXER.to_vec(),
            automation_takes: Vec::new(),
        },
        decoded_assets: vec![DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, (0..100_i16).map(|n| f32::from(n) / 200.0).collect())
                .unwrap(),
        )],
        limits: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 2,
                max_frames_per_asset: 100,
                max_decoded_sample_bytes: 800,
            },
        },
    })
    .unwrap()
}

#[test]
fn a_grouped_and_sent_project_exports_exactly_as_it_plays() {
    let session = routed_session();
    assert_eq!(
        session.parts().1.slots().len(),
        3,
        "the fixture exercises more than one mixer slot"
    );

    let rendered = render_project_offline(&session, OfflineExportSpec { block_frames: 32 }).unwrap();
    let expected = render_through_playback(&session, 32);
    let left = rendered.plane(0).unwrap();
    let right = rendered.plane(1).unwrap();
    for (index, (expected_left, expected_right)) in expected.iter().take(100).enumerate() {
        assert!(
            (left[index] - expected_left).abs() < f32::EPSILON,
            "left frame {index}: export {} playback {expected_left}",
            left[index]
        );
        assert!(
            (right[index] - expected_right).abs() < f32::EPSILON,
            "right frame {index}: export {} playback {expected_right}",
            right[index]
        );
    }
    assert!(
        left.iter().any(|sample| sample.abs() > 0.0),
        "the routed graph reaches the main output"
    );
    assert!(
        left.iter().zip(right.iter()).any(|(l, r)| (l - r).abs() > f32::EPSILON),
        "the send pan and channel pan distinguish the channels"
    );
}

const SCENE: &[u8] = br#"{
    "format":"hyper-scene","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-00112233750d",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Verse","order":0,"enabled":true,
    "launch_quantization":{"mode":"immediate"},
    "slots":[{
        "track_id":"018f47a2-6b2d-7c11-8a44-001122337502","action":"play",
        "clip_id":"018f47a2-6b2d-7c11-8a44-001122337503"
    }],
    "extensions":{}
}"#;
const SCENE_ARRANGEMENT: &[u8] = br#"{
    "format":"hyper-arrangement","format_version":1,"id":"018f47a2-6b2d-7c11-8a44-001122337505",
    "project_id":"018f47a2-6b2d-7c11-8a44-001122337500","name":"Main","enabled":true,
    "placements":[],
    "scene_launches":[{
        "id":"018f47a2-6b2d-7c11-8a44-00112233750e","tick":0,
        "scene_id":"018f47a2-6b2d-7c11-8a44-00112233750d"
    }],
    "extensions":{}
}"#;

/// Audio driven by scene authority rather than a linear placement — the fifth
/// capability ADR-117 cites when it rejects the flat aligned-source renderer.
fn scene_session() -> PreparedPlaybackSession {
    assemble_playback_session(PlaybackSessionAssemblyInput {
        documents: PlaybackDocumentBytes {
            project_id: PROJECT_ID.to_owned(),
            sample_rate_hz: 48_000,
            timeline: TIMELINE.to_vec(),
            tracks: vec![TRACK.to_vec()],
            clips: vec![CLIP.to_vec()],
            scenes: vec![SCENE.to_vec()],
            arrangement: Some(SCENE_ARRANGEMENT.to_vec()),
            mixer: MIXER.to_vec(),
            automation_takes: Vec::new(),
        },
        decoded_assets: vec![DecodedPlaybackAsset::new(
            "018f47a2-6b2d-7c11-8a44-001122337504".to_owned(),
            DecodedAudio::from_mono(48_000, (0..100_i16).map(|n| f32::from(n) / 200.0).collect())
                .unwrap(),
        )],
        limits: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: 2,
                max_frames_per_asset: 100,
                max_decoded_sample_bytes: 800,
            },
        },
    })
    .unwrap()
}

#[test]
fn a_scene_launched_project_exports_exactly_as_it_plays() {
    let session = scene_session();
    assert!(
        session.arrangement().placements().is_empty(),
        "this project's audio comes from scene authority, not a placement"
    );
    assert_eq!(session.arrangement().scene_launches().len(), 1);
    let rendered = render_project_offline(&session, OfflineExportSpec { block_frames: 32 }).unwrap();
    assert_eq!(rendered.frame_count(), 100);
    let expected = render_through_playback(&session, 32);

    let left = rendered.plane(0).unwrap();
    let right = rendered.plane(1).unwrap();
    for (index, (expected_left, expected_right)) in
        expected.iter().take(rendered.frame_count()).enumerate()
    {
        assert!(
            (left[index] - expected_left).abs() < f32::EPSILON,
            "left frame {index}: export {} playback {expected_left}",
            left[index]
        );
        assert!(
            (right[index] - expected_right).abs() < f32::EPSILON,
            "right frame {index}: export {} playback {expected_right}",
            right[index]
        );
    }
    assert!(
        left.iter().any(|sample| sample.abs() > 0.0),
        "the scene launch reaches the main output in the export"
    );
}
