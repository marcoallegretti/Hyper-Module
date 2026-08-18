//! The runtime process: hosts one project and serves one panel.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hyper_audio_source::DecodeLimits;
use hyper_desktop_audio::{DesktopOutputConfig, DesktopPlaybackSession};
use hyper_media_cache::CacheLimits;
use hyper_panel_protocol::LinkConfig;
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_runtime_host::{PanelClosure, RuntimeArguments, serve_panel};

/// The runtime's own hosting policy. Stated here rather than borrowed from the
/// IDE: these are the limits this process accepts, and they are software
/// limits, not evidence of embedded hardware capacity.
const MAXIMUM_BLOCK_FRAMES: usize = 1_024;
const SAMPLE_RATE_HZ: u32 = 48_000;
const MAX_AUDIO_FILE_BYTES: u64 = 512 * 1_024 * 1_024;
const MAX_AUDIO_FRAMES: u64 = 43_200_000;
const MAX_AUDIO_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_RESOURCE_BYTES: usize = 1_024 * 1_024 * 1_024;
const MAX_PROJECT_DECODED_BYTES: usize = 1_024 * 1_024 * 1_024;

const HEARTBEAT_INTERVAL_MS: u32 = 50;
const HEARTBEAT_TIMEOUT_MS: u32 = 500;
const PANEL_READ_TIMEOUT: Duration = Duration::from_millis(20);
const IDLE_POLL: Duration = Duration::from_millis(50);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("hyper-runtime: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let arguments = RuntimeArguments::parse(std::env::args().skip(1)).map_err(|error| {
        format!("{error}\nusage: hyper-runtime <project-directory> [--panel-listen ADDRESS]")
    })?;

    let output = DesktopOutputConfig::try_new(SAMPLE_RATE_HZ, MAXIMUM_BLOCK_FRAMES)
        .map_err(|error| format!("output configuration is invalid: {error}"))?;
    let mut session =
        DesktopPlaybackSession::open_project(output, arguments.project_root(), limits()?).map_err(
            |error| {
                format!(
                    "{} could not be hosted: {error}",
                    arguments.project_root().display()
                )
            },
        )?;
    println!(
        "hosting {} with {} mixer channels",
        arguments.project_root().display(),
        session.mixer_channel_count()
    );

    let stop = Arc::new(AtomicBool::new(false));
    watch_for_quit(Arc::clone(&stop));

    let listener = if let Some(address) = arguments.panel_listen() {
        let listener = TcpListener::bind(address)
            .map_err(|error| format!("cannot listen on {address}: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot poll {address}: {error}"))?;
        println!(
            "panel link listening on {}",
            listener.local_addr().unwrap_or(address)
        );
        Some(listener)
    } else {
        println!("no panel link requested");
        None
    };

    let started = Instant::now();
    let config = LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS)
        .map_err(|error| format!("panel link configuration is invalid: {error:?}"))?;
    let mut session_id = 1_u64;
    while !stop.load(Ordering::Acquire) {
        let Some(listener) = listener.as_ref() else {
            std::thread::sleep(IDLE_POLL);
            continue;
        };
        match listener.accept() {
            Ok((mut stream, peer)) => {
                println!("panel connected from {peer}");
                if let Err(error) = stream.set_read_timeout(Some(PANEL_READ_TIMEOUT)) {
                    eprintln!("hyper-runtime: cannot pace {peer}: {error}");
                    continue;
                }
                let closure = serve_panel(&mut stream, &mut session, config, session_id, || {
                    millis_since(started)
                });
                session_id = session_id.wrapping_add(1);
                match closure {
                    Ok(PanelClosure::PeerClosed) => println!("panel {peer} disconnected"),
                    Ok(PanelClosure::LinkLost) => println!("panel {peer} link lost"),
                    Err(error) => eprintln!("hyper-runtime: panel {peer} failed: {error}"),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(IDLE_POLL);
            }
            Err(error) => return Err(format!("cannot accept a panel: {error}")),
        }
    }

    println!("stopping");
    Ok(())
}

fn limits() -> Result<ProjectPlaybackLimits, String> {
    let audio = DecodeLimits::new(
        MAX_AUDIO_FILE_BYTES,
        MAX_AUDIO_FRAMES,
        MAX_AUDIO_DECODED_BYTES,
    )
    .map_err(|error| format!("decode policy is invalid: {error}"))?;
    let max_frames_per_asset = usize::try_from(MAX_AUDIO_FRAMES)
        .map_err(|_| "decode policy exceeds this platform's addressable frames".to_owned())?;
    Ok(ProjectPlaybackLimits {
        max_total_resource_bytes: MAX_PROJECT_RESOURCE_BYTES,
        audio,
        session: PlaybackSessionAssemblyLimits {
            documents: LoaderLimits::VERSION_1,
            media_cache: CacheLimits {
                max_assets: LoaderLimits::VERSION_1.max_clip_documents,
                max_frames_per_asset,
                max_decoded_sample_bytes: MAX_PROJECT_DECODED_BYTES,
            },
        },
    })
}

/// Ends the process when standard input closes or asks it to.
///
/// `std` has no signal handling, so the process is stopped the way a headless
/// process is driven: through the stream it was started with.
fn watch_for_quit(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut line = String::new();
        let mut input = std::io::stdin().lock();
        loop {
            line.clear();
            let Ok(read) = input.read_line(&mut line) else {
                break;
            };
            if read == 0 || line.trim() == "quit" {
                break;
            }
        }
        stop.store(true, Ordering::Release);
    });
}

fn millis_since(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
