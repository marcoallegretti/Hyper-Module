#![doc = "Serving one panel to a runtime control owner: arguments, the default layout, the link, and the accept loop."]
#![forbid(unsafe_code)]

use core::fmt;
use core::num::NonZeroU32;
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use hyper_control_runtime::{
    BooleanControlTarget, ControlBindingUnavailableReason, PanelActionTarget, PanelBinding,
    PanelButtonBehavior, PanelConnectError, PanelConnection, PanelControl, PanelDispatchError,
    PanelDispatchOutcome, PanelReleaseFailure, UnboundReason, ValueControlTarget,
};
use hyper_panel_protocol::{LinkConfig, LinkFault, Message, PanelDescriptor};
use hyper_runtime_panel_session::{PanelEvent, PanelSession};

/// How far one encoder detent moves a channel gain, in millionths.
///
/// This is a constant step in *amplitude*, so its perceptual size is not
/// constant across the range: a detent near silence is a far larger musical
/// change than one near unity. A hundred detents cover unity, which is a usable
/// throw on a physical encoder. A perceptually even law belongs with a gain
/// curve the mixer does not yet have.
pub const DEFAULT_GAIN_STEP_MILLIONTHS: u32 = 10_000;

/// The same step, in the form a binding takes.
const DEFAULT_GAIN_STEP: NonZeroU32 = match NonZeroU32::new(DEFAULT_GAIN_STEP_MILLIONTHS) {
    Some(step) => step,
    None => panic!("the default gain step is a non-zero constant"),
};

/// The largest read this host takes from a panel in one step.
const READ_CHUNK: usize = 1024;

/// How many times a teardown retries a release the sink refuses.
///
/// Bounded rather than persistent: a sink that will not take a release is a
/// fault in its own right, and spinning on it stops the process from ever
/// getting on with anything else.
const RELEASE_ATTEMPTS: usize = 3;

/// How long a serving loop waits on a quiet panel before letting link upkeep
/// and the stop flag have a turn.
pub const PANEL_READ_TIMEOUT: Duration = Duration::from_millis(20);

/// How long the accept loop waits between polls when no panel is connected.
pub const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What the runtime was asked to host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeArguments {
    project_root: PathBuf,
    panel_listen: Option<SocketAddr>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentError {
    MissingProjectRoot,
    UnknownOption(String),
    MissingValue(&'static str),
    UnreadableAddress(String),
    Repeated(&'static str),
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingProjectRoot => formatter.write_str("no project directory was given"),
            Self::UnknownOption(option) => write!(formatter, "unknown option {option}"),
            Self::MissingValue(option) => write!(formatter, "{option} needs a value"),
            Self::UnreadableAddress(value) => {
                write!(
                    formatter,
                    "{value} is not an address the host can listen on"
                )
            }
            Self::Repeated(option) => write!(formatter, "{option} was given more than once"),
        }
    }
}

impl Error for ArgumentError {}

impl RuntimeArguments {
    /// Reads the arguments following the program name.
    ///
    /// # Errors
    ///
    /// Returns [`ArgumentError`] rather than exiting, so the caller decides how
    /// a bad invocation is reported.
    pub fn parse<I>(arguments: I) -> Result<Self, ArgumentError>
    where
        I: IntoIterator<Item = String>,
    {
        let mut project_root = None;
        let mut panel_listen = None;
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--panel-listen" => {
                    if panel_listen.is_some() {
                        return Err(ArgumentError::Repeated("--panel-listen"));
                    }
                    let value = arguments
                        .next()
                        .ok_or(ArgumentError::MissingValue("--panel-listen"))?;
                    panel_listen = Some(
                        value
                            .parse::<SocketAddr>()
                            .map_err(|_| ArgumentError::UnreadableAddress(value))?,
                    );
                }
                option if option.starts_with("--") => {
                    return Err(ArgumentError::UnknownOption(option.to_owned()));
                }
                _ if project_root.is_some() => {
                    return Err(ArgumentError::Repeated("the project directory"));
                }
                _ => project_root = Some(PathBuf::from(argument)),
            }
        }
        Ok(Self {
            project_root: project_root.ok_or(ArgumentError::MissingProjectRoot)?,
            panel_listen,
        })
    }

    #[must_use]
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    #[must_use]
    pub const fn panel_listen(&self) -> Option<SocketAddr> {
        self.panel_listen
    }
}

/// The runtime control owner, as the panel link needs to see it.
pub trait PanelHost {
    type Error;

    fn mixer_channel_count(&self) -> usize;

    /// Installs the bindings for a panel that just identified itself.
    ///
    /// # Errors
    ///
    /// Returns the refusal to accept a panel at all.
    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError>;

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>>;

    fn panel_connected(&self) -> bool;

    /// Dispatches one panel message.
    ///
    /// # Errors
    ///
    /// Returns the dispatch rejection.
    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>>;
}

/// Something worth telling the operator about, which would otherwise be
/// discarded.
///
/// ADR-124 and ADR-125 went to some trouble to make refusals, unavailability,
/// and release failures reportable. Collecting them here is what stops the
/// first caller from throwing all of it away.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelNotice {
    /// The panel offered controls the owner would not bind.
    BindingsRefused { count: usize },
    /// A bound control was pressed while its target was unavailable.
    Unavailable {
        control: PanelControl,
        reason: ControlBindingUnavailableReason,
    },
    /// A control moved but its target's present state could not be read.
    StateUnavailable { control: PanelControl },
    /// The owner rejected the command a control produced.
    Rejected { control: PanelControl },
    /// The panel sent a control identifier no descriptor can name.
    ControlOutOfRange,
    /// A control could not be released at teardown and stays held.
    ReleaseFailed { control: PanelControl },
}

/// Builds the layout a panel gets when nothing has configured one.
///
/// Encoders take channel gains and buttons take channel mutes, both in index
/// order and both stopping at whichever of the panel and the project runs out
/// first. One button past the last channel, if the panel has one, toggles the
/// transport.
///
/// Deliberately dull: it names nothing the panel lacks and nothing the project
/// lacks, so every binding it produces installs. It also binds no momentary
/// button, so a panel served by this layout never holds a control.
#[must_use]
pub fn default_layout(
    descriptor: PanelDescriptor,
    mixer_channel_count: usize,
) -> Vec<PanelBinding> {
    let channels = u8::try_from(mixer_channel_count).unwrap_or(u8::MAX);
    let encoders = descriptor.encoders.min(channels);
    let buttons = descriptor.buttons.min(channels);
    let mut bindings =
        Vec::with_capacity(usize::from(encoders) + usize::from(buttons).saturating_add(1));
    for index in 0..encoders {
        bindings.push(PanelBinding::Encoder {
            index,
            target: ValueControlTarget::ChannelGain {
                channel_index: usize::from(index),
            },
            step_millionths: DEFAULT_GAIN_STEP,
        });
    }
    for index in 0..buttons {
        bindings.push(PanelBinding::Button {
            index,
            target: BooleanControlTarget::ChannelMute {
                channel_index: usize::from(index),
            },
            behavior: PanelButtonBehavior::Toggle,
        });
    }
    if descriptor.buttons > buttons {
        bindings.push(PanelBinding::ActionButton {
            index: buttons,
            action: PanelActionTarget::TransportToggle,
        });
    }
    bindings
}

/// Why one panel connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelClosure {
    /// The panel closed the stream.
    PeerClosed,
    /// The link failed, timed out, or the panel rejected every offered version.
    LinkLost,
    /// The host was asked to stop while the panel was still connected.
    HostStopping,
}

/// What one step of the link produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelStep {
    Live,
    Closed(PanelClosure),
}

/// One panel connection, driven by bytes and injected time.
///
/// Separate from the socket loop so all of it — handshake, default layout,
/// dispatch, and teardown — can be exercised without a device and without a
/// second thread.
pub struct PanelLink {
    session: PanelSession,
    bound: bool,
    notices: Vec<PanelNotice>,
}

impl PanelLink {
    #[must_use]
    pub fn new(config: LinkConfig) -> Self {
        Self {
            session: PanelSession::new(config),
            bound: false,
            notices: Vec::new(),
        }
    }

    /// Offers the runtime's supported versions to the panel.
    ///
    /// # Errors
    ///
    /// Returns the link's refusal to start.
    pub fn start(&mut self, session_id: u64, now_ms: u64) -> Result<(), LinkFault> {
        self.session.start(session_id, now_ms)
    }

    /// Takes the bytes the caller should write to the panel.
    #[must_use]
    pub fn take_transmit(&mut self) -> Vec<u8> {
        self.session.take_transmit()
    }

    /// Whether this link currently owns the host's panel bindings.
    ///
    /// Distinct from the host's own `panel_connected`, which stays true when a
    /// teardown could not release everything.
    #[must_use]
    pub const fn holds_bindings(&self) -> bool {
        self.bound
    }

    /// Takes everything worth reporting since the last call.
    #[must_use]
    pub fn take_notices(&mut self) -> Vec<PanelNotice> {
        core::mem::take(&mut self.notices)
    }

    /// Feeds bytes received from the panel and advances link upkeep.
    ///
    /// A panel that identifies itself gets the default layout; one that goes
    /// away takes its bindings down with it.
    pub fn step<H: PanelHost>(&mut self, received: &[u8], now_ms: u64, host: &mut H) -> PanelStep {
        let mut events = self.session.receive(received, now_ms);
        if let Some(event) = self.session.poll(now_ms) {
            events.push(event);
        }
        for event in events {
            match event {
                PanelEvent::Connected(descriptor) => {
                    let bindings = default_layout(descriptor, host.mixer_channel_count());
                    match host.connect_panel(descriptor, &bindings) {
                        Ok(connection) => {
                            if !connection.is_fully_installed() {
                                self.notices.push(PanelNotice::BindingsRefused {
                                    count: connection.refused().len(),
                                });
                            }
                            self.bound = true;
                        }
                        Err(_) => return self.close(host, PanelClosure::LinkLost),
                    }
                }
                PanelEvent::Input(message) => self.deliver(message, host),
                PanelEvent::Fault(_)
                | PanelEvent::TimedOut
                | PanelEvent::VersionRejected
                | PanelEvent::Undecodable(_) => {
                    return self.close(host, PanelClosure::LinkLost);
                }
            }
        }
        PanelStep::Live
    }

    fn deliver<H: PanelHost>(&mut self, message: Message, host: &mut H) {
        // A panel can outrun the first mixer snapshot, so a target that cannot
        // be read yet is a startup race rather than a reason to drop the link.
        match host.dispatch_panel(message) {
            Ok(PanelDispatchOutcome::Unavailable { control, reason }) => {
                self.notices
                    .push(PanelNotice::Unavailable { control, reason });
            }
            Ok(PanelDispatchOutcome::Unbound(UnboundReason::ControlOutOfRange)) => {
                self.notices.push(PanelNotice::ControlOutOfRange);
            }
            Ok(_) => {}
            Err(PanelDispatchError::StateUnavailable { control }) => {
                self.notices.push(PanelNotice::StateUnavailable { control });
            }
            Err(PanelDispatchError::Sink { control, .. }) => {
                self.notices.push(PanelNotice::Rejected { control });
            }
        }
    }

    /// Ends the connection, releasing anything the panel still holds.
    ///
    /// Releases are retried a bounded number of times. A control the sink keeps
    /// refusing is reported and left held rather than spun on — the host's own
    /// `panel_connected` stays true in that case, which the caller must treat
    /// as fatal rather than as a panel it can replace.
    pub fn close<H: PanelHost>(&mut self, host: &mut H, closure: PanelClosure) -> PanelStep {
        if self.bound {
            for _ in 0..RELEASE_ATTEMPTS {
                let failures = host.disconnect_panel();
                if failures.is_empty() {
                    break;
                }
                for failure in &failures {
                    self.notices.push(PanelNotice::ReleaseFailed {
                        control: failure.control(),
                    });
                }
            }
            self.bound = false;
        }
        PanelStep::Closed(closure)
    }
}

/// Drives one panel connection over a stream until it closes.
///
/// The link is torn down on **every** exit, including a failed write: the
/// teardown happens once, at the end, rather than at each early return, so a
/// later branch cannot forget it.
///
/// # Errors
///
/// Returns the stream's own failure. A read that would block is not a failure;
/// it is how link upkeep and the stop flag get a turn.
pub fn serve_panel<S, H, C>(
    stream: &mut S,
    host: &mut H,
    config: LinkConfig,
    session_id: u64,
    stop: &AtomicBool,
    mut clock: C,
) -> io::Result<(PanelClosure, Vec<PanelNotice>)>
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
{
    let mut link = PanelLink::new(config);
    let outcome = drive_panel(&mut link, stream, host, session_id, stop, &mut clock);
    let closure = match &outcome {
        Ok(closure) => *closure,
        Err(_) => PanelClosure::LinkLost,
    };
    link.close(host, closure);
    let notices = link.take_notices();
    outcome.map(|closure| (closure, notices))
}

/// The loop itself, which never tears down — [`serve_panel`] does that once for
/// every way out of here.
fn drive_panel<S, H, C>(
    link: &mut PanelLink,
    stream: &mut S,
    host: &mut H,
    session_id: u64,
    stop: &AtomicBool,
    clock: &mut C,
) -> io::Result<PanelClosure>
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
{
    if link.start(session_id, clock()).is_err() {
        return Ok(PanelClosure::LinkLost);
    }
    let mut buffer = [0_u8; READ_CHUNK];
    loop {
        if stop.load(Ordering::Acquire) {
            return Ok(PanelClosure::HostStopping);
        }
        let outbound = link.take_transmit();
        if !outbound.is_empty() {
            stream.write_all(&outbound)?;
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Ok(PanelClosure::PeerClosed),
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                0
            }
            Err(error) => return Err(error),
        };
        if let PanelStep::Closed(closure) = link.step(&buffer[..read], clock(), host) {
            return Ok(closure);
        }
    }
}

/// What one accepted panel connection produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServedPanel {
    pub peer: SocketAddr,
    pub closure: PanelClosure,
    pub notices: Vec<PanelNotice>,
}

/// Why the accept loop gave up.
#[derive(Debug)]
pub enum ServeError {
    /// The listener or a panel stream failed.
    Io(io::Error),
    /// A teardown left controls held, so the owner cannot take another panel.
    /// Serving on would produce an endless run of immediately-refused panels.
    ControlsStillHeld,
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "panel link failed: {error}"),
            Self::ControlsStillHeld => formatter.write_str(
                "a panel teardown left controls held, so no further panel can be served",
            ),
        }
    }
}

impl Error for ServeError {}

impl From<io::Error> for ServeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Accepts and serves panels, one at a time, until asked to stop.
///
/// The listener must be non-blocking so the stop flag is observed while idle.
/// Each accepted stream is put **back** into blocking mode before its read
/// timeout is set: on Windows an accepted socket inherits the listener's
/// non-blocking mode, which makes `SO_RCVTIMEO` inert and turns the serving
/// loop into a spin.
///
/// While one panel is being served, any further connection is accepted and
/// dropped immediately, so a second panel is refused rather than left queued in
/// the backlog to be promoted stale later.
///
/// # Errors
///
/// Returns [`ServeError`] when the listener fails or a teardown leaves controls
/// held.
pub fn serve_panels<H, C, R>(
    listener: &TcpListener,
    host: &mut H,
    config: LinkConfig,
    stop: &AtomicBool,
    mut clock: C,
    mut served: R,
) -> Result<(), ServeError>
where
    H: PanelHost,
    C: FnMut() -> u64,
    R: FnMut(&ServedPanel),
{
    let mut session_id = 1_u64;
    while !stop.load(Ordering::Acquire) {
        let (mut stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        prepare_panel_stream(&stream)?;
        let (closure, notices) = serve_panel(
            &mut RefusingStream {
                stream: &mut stream,
                listener,
            },
            host,
            config,
            session_id,
            stop,
            &mut clock,
        )?;
        session_id = session_id.wrapping_add(1);
        served(&ServedPanel {
            peer,
            closure,
            notices,
        });
        if host.panel_connected() {
            return Err(ServeError::ControlsStillHeld);
        }
    }
    Ok(())
}

/// Puts one accepted panel stream into the mode the serving loop expects.
///
/// The `set_nonblocking(false)` is the important half and is not redundant: on
/// Windows an accepted socket inherits the listening socket's non-blocking
/// mode, which makes `SO_RCVTIMEO` inert and turns every read into an immediate
/// `WouldBlock` — a spin for the whole life of the connection.
///
/// # Errors
///
/// Returns the socket's own refusal to change mode.
pub fn prepare_panel_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(PANEL_READ_TIMEOUT))
}

/// Accepts and immediately drops anything else waiting on the listener.
///
/// One panel at a time: a second one is turned away where it can see that it
/// was, rather than left in the backlog to be promoted stale once the first
/// disconnects. The listener must be non-blocking.
pub fn refuse_waiting_panels(listener: &TcpListener) {
    while let Ok((extra, _)) = listener.accept() {
        drop(extra);
    }
}

/// A panel stream that turns away any other panel each time it is read.
struct RefusingStream<'stream, 'listener> {
    stream: &'stream mut TcpStream,
    listener: &'listener TcpListener,
}

impl Read for RefusingStream<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        refuse_waiting_panels(self.listener);
        self.stream.read(buffer)
    }
}

impl Write for RefusingStream<'_, '_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
