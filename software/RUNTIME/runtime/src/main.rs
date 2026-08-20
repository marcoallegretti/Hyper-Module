//! The runtime process: hosts one verified bundle and serves one panel.

#![forbid(unsafe_code)]

use std::io::BufRead;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use hyper_audio_source::DecodeLimits;
use hyper_control_runtime::{
    PanelBinding, PanelConnectError, PanelConnection, PanelControl, PanelDispatchError,
    PanelDispatchOutcome, PanelDisplay, PanelReleaseFailure,
};
use hyper_desktop_audio::{DesktopControlSinkError, DesktopOutputConfig, DesktopPlaybackSession};
use hyper_media_cache::CacheLimits;
use hyper_panel_protocol::{LinkConfig, Message, PanelDescriptor};
use hyper_playback_json::LoaderLimits;
use hyper_playback_session::PlaybackSessionAssemblyLimits;
use hyper_project_loader::ProjectPlaybackLimits;
use hyper_runtime_bundle::prepare_runtime_bundle;
use hyper_runtime_host::{PanelHost, RuntimeArguments, ServeError, ServedPanel, serve_panels};

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
const MAX_RUNTIME_BUNDLE_BYTES: u64 = 1_024 * 1_024 * 1_024;

const HEARTBEAT_INTERVAL_MS: u32 = 50;
const HEARTBEAT_TIMEOUT_MS: u32 = 500;

/// The runtime's control owner. Every method forwards; the session owns the
/// sole control runtime, so it is the only thing that can answer these.
struct SessionHost(DesktopPlaybackSession);

impl PanelHost for SessionHost {
    type Error = DesktopControlSinkError;

    fn mixer_channel_count(&self) -> usize {
        self.0.mixer_channel_count()
    }

    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
        self.0.attach_panel(descriptor, bindings)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        self.0.detach_panel()
    }

    fn panel_connected(&self) -> bool {
        self.0.has_panel()
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        self.0.deliver_panel_message(message)
    }

    fn panel_display(&mut self, out: &mut Vec<PanelDisplay>) {
        self.0.panel_display(out);
    }
}

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
        format!("{error}\nusage: hyper-runtime <runtime-bundle-directory> [--panel-listen ADDRESS]")
    })?;

    let bundle = prepare_runtime_bundle(
        arguments.bundle_root(),
        env!("CARGO_PKG_VERSION"),
        MAX_RUNTIME_BUNDLE_BYTES,
        limits()?,
    )
    .map_err(|error| {
        format!(
            "{} could not be verified and prepared: {error}",
            arguments.bundle_root().display()
        )
    })?;
    let bundle_identity = hex_digest(bundle.bundle_sha256());
    let output = DesktopOutputConfig::try_new(SAMPLE_RATE_HZ, MAXIMUM_BLOCK_FRAMES)
        .map_err(|error| format!("output configuration is invalid: {error}"))?;
    let session = DesktopPlaybackSession::start_project(output, bundle.into_project())
        .map_err(|error| format!("verified bundle could not be hosted: {error}"))?;
    let mut host = SessionHost(session);
    println!(
        "hosting bundle {bundle_identity} from {} with {} mixer channels",
        arguments.bundle_root().display(),
        host.mixer_channel_count()
    );

    let stop = Arc::new(AtomicBool::new(false));
    watch_for_quit(Arc::clone(&stop));

    let Some(address) = arguments.panel_listen() else {
        println!("no panel link requested");
        while !stop.load(Ordering::Acquire) {
            std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL);
        }
        println!("stopping");
        return Ok(());
    };

    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot poll {address}: {error}"))?;
    println!(
        "panel link listening on {}",
        listener.local_addr().unwrap_or(address)
    );

    let config = LinkConfig::new(HEARTBEAT_INTERVAL_MS, HEARTBEAT_TIMEOUT_MS)
        .map_err(|error| format!("panel link configuration is invalid: {error:?}"))?;
    let started = Instant::now();
    // Panel service ending is not the project ending. Whatever stops the
    // listener, the audio the runtime exists to host keeps running until the
    // operator asks it to stop.
    let service = serve_panels(
        &listener,
        &mut host,
        config,
        &stop,
        || millis_since(started),
        report,
    );
    let mut reporter = ConsolePanelServiceReporter;
    finish_panel_service(
        listener,
        service,
        &stop,
        || {
            host.disconnect_panel()
                .into_iter()
                .map(|failure| failure.control())
                .collect()
        },
        &mut reporter,
        || std::thread::sleep(hyper_runtime_host::ACCEPT_POLL_INTERVAL),
    );

    println!("stopping");
    Ok(())
}

/// Receives the process-level events that follow the panel service ending.
trait PanelServiceReporter<E> {
    fn service_ended(&mut self, error: &E);
    fn release_failed(&mut self, control: PanelControl);
    fn project_still_hosted(&mut self);
}

struct ConsolePanelServiceReporter;

impl PanelServiceReporter<ServeError> for ConsolePanelServiceReporter {
    fn service_ended(&mut self, error: &ServeError) {
        eprintln!("hyper-runtime: panel service ended: {error}");
    }

    fn release_failed(&mut self, control: PanelControl) {
        eprintln!("hyper-runtime: {control:?} could not be released and stays held");
    }

    fn project_still_hosted(&mut self) {
        eprintln!("hyper-runtime: the project stays hosted; send quit to stop it");
    }
}

/// Releases the listener and applies the process policy after panel service.
///
/// The listener is owned so it is closed before any failure is reported: a
/// panel must never connect to a port the process no longer services. A service
/// failure gets one process-owner release attempt, then project playback stays
/// alive until the ordinary quit flag is observed.
fn finish_panel_service<L, E, D, R, W>(
    listener: L,
    service: Result<(), E>,
    stop: &AtomicBool,
    mut disconnect: D,
    reporter: &mut R,
    mut wait: W,
) where
    D: FnMut() -> Vec<PanelControl>,
    R: PanelServiceReporter<E>,
    W: FnMut(),
{
    drop(listener);
    if let Err(error) = service {
        reporter.service_ended(&error);
        for control in disconnect() {
            reporter.release_failed(control);
        }
        reporter.project_still_hosted();
        while !stop.load(Ordering::Acquire) {
            wait();
        }
    }
}

fn report(served: &ServedPanel) {
    match &served.outcome {
        Ok(closure) => println!(
            "panel {} (#{}) ended: {closure:?}",
            served.peer, served.session_id
        ),
        Err(error) => println!(
            "panel {} (#{}) failed: {error}",
            served.peer, served.session_id
        ),
    }
    for notice in &served.notices {
        println!("  {notice:?}");
    }
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
/// process is driven: through the stream it was started with. The serving loop
/// observes this flag between reads, so it takes effect while a panel is
/// connected rather than only between connections.
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

fn hex_digest(value: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in value {
        use core::fmt::Write;
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    struct DropProbe<'a>(&'a Cell<bool>);

    impl Drop for DropProbe<'_> {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Reported {
        ServiceEnded,
        ReleaseFailed(PanelControl),
        ProjectStillHosted,
    }

    #[derive(Default)]
    struct Reporter {
        events: Vec<Reported>,
    }

    impl PanelServiceReporter<()> for Reporter {
        fn service_ended(&mut self, _error: &()) {
            self.events.push(Reported::ServiceEnded);
        }

        fn release_failed(&mut self, control: PanelControl) {
            self.events.push(Reported::ReleaseFailed(control));
        }

        fn project_still_hosted(&mut self) {
            self.events.push(Reported::ProjectStillHosted);
        }
    }

    #[test]
    fn a_failed_panel_service_releases_the_port_retries_controls_and_keeps_hosting() {
        let dropped = Cell::new(false);
        let disconnects = Cell::new(0);
        let waits = Cell::new(0);
        let stop = AtomicBool::new(false);
        let mut reporter = Reporter::default();

        finish_panel_service(
            DropProbe(&dropped),
            Err(()),
            &stop,
            || {
                assert!(dropped.get(), "the unattended port is closed first");
                disconnects.set(disconnects.get() + 1);
                vec![PanelControl::Button { index: 2 }]
            },
            &mut reporter,
            || {
                waits.set(waits.get() + 1);
                stop.store(true, Ordering::Release);
            },
        );

        assert!(dropped.get());
        assert_eq!(disconnects.get(), 1, "the process owner retries once");
        assert_eq!(waits.get(), 1, "the project remains hosted until quit");
        assert_eq!(
            reporter.events,
            [
                Reported::ServiceEnded,
                Reported::ReleaseFailed(PanelControl::Button { index: 2 }),
                Reported::ProjectStillHosted,
            ]
        );
    }

    #[test]
    fn a_clean_panel_service_end_only_releases_the_listener() {
        let dropped = Cell::new(false);
        let stop = AtomicBool::new(false);
        let mut reporter = Reporter::default();

        finish_panel_service(
            DropProbe(&dropped),
            Ok(()),
            &stop,
            || panic!("a clean service end has no stranded controls"),
            &mut reporter,
            || panic!("a clean service end does not enter failure waiting"),
        );

        assert!(dropped.get());
        assert!(reporter.events.is_empty());
    }
}
