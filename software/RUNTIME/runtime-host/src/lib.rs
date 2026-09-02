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
    PanelDispatchOutcome, PanelDisplay, PanelReleaseFailure, UnboundReason, ValueControlTarget,
};
use hyper_panel_protocol::{LinkConfig, Message, PanelDescriptor};
use hyper_runtime_panel_session::{PanelEvent, PanelSession, PanelSessionError};

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

/// How many display messages leave in one quiet moment.
///
/// Bounded so a burst of change cannot starve input or the stop flag; whatever
/// is not sent this tick is sent on the next. With a 20 ms tick this still
/// refreshes an entire panel several times a second.
pub const FEEDBACK_PER_TICK: usize = 4;

/// How many panels waiting behind the active one may be refused in one host
/// tick.
///
/// The refusal path performs an `accept` system call for every panel. Giving it
/// the same four-item budget as feedback keeps a quiet 20 ms tick finite while
/// still rejecting up to two hundred stale connections per second. Any excess
/// remains queued for the next quiet tick.
pub const PANEL_REFUSALS_PER_TICK: usize = 4;

/// How long a serving loop waits on a quiet panel before letting link upkeep
/// and the stop flag have a turn.
pub const PANEL_READ_TIMEOUT: Duration = Duration::from_millis(20);

/// How long a serving loop waits on a panel that has stopped reading before it
/// gives up on the write and lets the stop flag have a turn.
///
/// Deliberately longer than [`PANEL_READ_TIMEOUT`] and no longer than the link's
/// own heartbeat timeout: a write that is merely slow must not be mistaken for a
/// panel that has gone, and a panel that has gone is the link's call, not the
/// socket's.
pub const PANEL_WRITE_TIMEOUT: Duration = Duration::from_millis(500);

/// How long the accept loop waits between polls when no panel is connected.
pub const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// What the runtime was asked to host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeArguments {
    deployment_root: PathBuf,
    panel_listen: Option<SocketAddr>,
    performance_updates: Option<PathBuf>,
    mixer_updates: Option<PathBuf>,
    audio_output: Option<String>,
    operator_ui: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArgumentError {
    MissingDeploymentRoot,
    UnknownOption(String),
    MissingValue(&'static str),
    UnreadableAddress(String),
    Repeated(&'static str),
}

impl fmt::Display for ArgumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDeploymentRoot => {
                formatter.write_str("no Runtime deployment target directory was given")
            }
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
        let mut deployment_root = None;
        let mut panel_listen = None;
        let mut performance_updates = None;
        let mut mixer_updates = None;
        let mut audio_output = None;
        let mut operator_ui = false;
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--operator-ui" => {
                    if operator_ui {
                        return Err(ArgumentError::Repeated("--operator-ui"));
                    }
                    operator_ui = true;
                }
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
                "--performance-updates" => {
                    if performance_updates.is_some() {
                        return Err(ArgumentError::Repeated("--performance-updates"));
                    }
                    let value = arguments
                        .next()
                        .ok_or(ArgumentError::MissingValue("--performance-updates"))?;
                    performance_updates = Some(PathBuf::from(value));
                }
                "--mixer-updates" => {
                    if mixer_updates.is_some() {
                        return Err(ArgumentError::Repeated("--mixer-updates"));
                    }
                    let value = arguments
                        .next()
                        .ok_or(ArgumentError::MissingValue("--mixer-updates"))?;
                    mixer_updates = Some(PathBuf::from(value));
                }
                "--audio-output" => {
                    if audio_output.is_some() {
                        return Err(ArgumentError::Repeated("--audio-output"));
                    }
                    audio_output = Some(
                        arguments
                            .next()
                            .ok_or(ArgumentError::MissingValue("--audio-output"))?,
                    );
                }
                option if option.starts_with("--") => {
                    return Err(ArgumentError::UnknownOption(option.to_owned()));
                }
                _ if deployment_root.is_some() => {
                    return Err(ArgumentError::Repeated(
                        "the Runtime deployment target directory",
                    ));
                }
                _ => deployment_root = Some(PathBuf::from(argument)),
            }
        }
        Ok(Self {
            deployment_root: deployment_root.ok_or(ArgumentError::MissingDeploymentRoot)?,
            panel_listen,
            performance_updates,
            mixer_updates,
            audio_output,
            operator_ui,
        })
    }

    #[must_use]
    pub fn deployment_root(&self) -> &Path {
        &self.deployment_root
    }

    #[must_use]
    pub const fn panel_listen(&self) -> Option<SocketAddr> {
        self.panel_listen
    }

    /// Returns the explicit Runtime scene-performance update directory, when enabled.
    #[must_use]
    pub fn performance_updates(&self) -> Option<&Path> {
        self.performance_updates.as_deref()
    }

    /// Returns the explicit Runtime mixer-update directory, when enabled.
    #[must_use]
    pub fn mixer_updates(&self) -> Option<&Path> {
        self.mixer_updates.as_deref()
    }

    /// Returns the opaque desktop output routing identity requested for this
    /// Runtime launch, if one was explicitly selected.
    #[must_use]
    pub fn audio_output(&self) -> Option<&str> {
        self.audio_output.as_deref()
    }

    #[must_use]
    pub const fn operator_ui(&self) -> bool {
        self.operator_ui
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    #[test]
    fn parses_independent_external_update_directories_and_audio_output() {
        let arguments = RuntimeArguments::parse([
            "--performance-updates".to_owned(),
            "scene-updates".to_owned(),
            "--mixer-updates".to_owned(),
            "mixer-updates".to_owned(),
            "--audio-output".to_owned(),
            "studio-right".to_owned(),
            "deployment".to_owned(),
        ])
        .unwrap();

        assert_eq!(arguments.deployment_root(), Path::new("deployment"));
        assert_eq!(
            arguments.performance_updates(),
            Some(Path::new("scene-updates"))
        );
        assert_eq!(arguments.mixer_updates(), Some(Path::new("mixer-updates")));
        assert_eq!(arguments.audio_output(), Some("studio-right"));
    }

    #[test]
    fn refuses_repeated_or_value_less_value_options() {
        assert_eq!(
            RuntimeArguments::parse([
                "--mixer-updates".to_owned(),
                "first".to_owned(),
                "--mixer-updates".to_owned(),
                "second".to_owned(),
                "deployment".to_owned(),
            ]),
            Err(ArgumentError::Repeated("--mixer-updates"))
        );
        assert_eq!(
            RuntimeArguments::parse(["--mixer-updates".to_owned(), "deployment".to_owned()]),
            Err(ArgumentError::MissingDeploymentRoot),
            "an option's immediate next token is its value, even if it resembles a path"
        );
        assert_eq!(
            RuntimeArguments::parse(["--mixer-updates".to_owned()]),
            Err(ArgumentError::MissingValue("--mixer-updates"))
        );
        assert_eq!(
            RuntimeArguments::parse([
                "--audio-output".to_owned(),
                "first".to_owned(),
                "--audio-output".to_owned(),
                "second".to_owned(),
                "deployment".to_owned(),
            ]),
            Err(ArgumentError::Repeated("--audio-output"))
        );
        assert_eq!(
            RuntimeArguments::parse(["--audio-output".to_owned()]),
            Err(ArgumentError::MissingValue("--audio-output"))
        );
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

    /// Collects what the connected panel should currently be showing.
    fn panel_display(&mut self, out: &mut Vec<PanelDisplay>);
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
    descriptor: Option<PanelDescriptor>,
    /// What the panel was last told, so an unchanged value costs nothing. With
    /// a 20 ms tick and up to 32 LEDs, this deduplication is what makes
    /// feedback affordable at all.
    shown: Vec<PanelDisplay>,
    wanted: Vec<PanelDisplay>,
    /// The next display candidate to inspect. The position survives refreshes
    /// so controls near the front cannot consume every bounded tick.
    feedback_cursor: usize,
}

impl PanelLink {
    #[must_use]
    pub fn new(config: LinkConfig) -> Self {
        Self {
            session: PanelSession::new(config),
            bound: false,
            notices: Vec::new(),
            descriptor: None,
            shown: Vec::new(),
            wanted: Vec::new(),
            feedback_cursor: 0,
        }
    }

    /// Offers the runtime's supported versions to the panel.
    ///
    /// # Errors
    ///
    /// Returns the link's refusal to start.
    pub fn start(&mut self, session_id: u64, now_ms: u64) -> Result<(), PanelSessionError> {
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
                    // Nothing is bound yet, so there is nothing to release.
                    let Ok(connection) = host.connect_panel(descriptor, &bindings) else {
                        return PanelStep::Closed(PanelClosure::LinkLost);
                    };
                    if !connection.is_fully_installed() {
                        self.notices.push(PanelNotice::BindingsRefused {
                            count: connection.refused().len(),
                        });
                    }
                    self.descriptor = Some(descriptor);
                    self.bound = true;
                }
                PanelEvent::Input(message) => self.deliver(message, host),
                PanelEvent::Fault(_)
                | PanelEvent::TimedOut
                | PanelEvent::VersionRejected
                | PanelEvent::Undecodable(_)
                | PanelEvent::TransmitFailed(_) => {
                    self.release(host);
                    return PanelStep::Closed(PanelClosure::LinkLost);
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

    /// Sends what has changed on the panel's display, bounded per call.
    ///
    /// Only differences leave: a value the panel was already shown costs
    /// nothing. Nothing is addressed that the descriptor did not declare, for
    /// the same reason a binding to an absent control is refused.
    ///
    /// Returns how many messages were queued, so a caller can tell a quiet
    /// panel from a busy one.
    pub fn refresh_display<H: PanelHost>(&mut self, host: &mut H, now_ms: u64) -> usize {
        if !self.bound {
            return 0;
        }
        let Some(descriptor) = self.descriptor else {
            return 0;
        };
        host.panel_display(&mut self.wanted);
        if self.wanted.is_empty() {
            self.feedback_cursor = 0;
            return 0;
        }
        let mut sent = 0;
        let candidates = self.wanted.len();
        let mut inspected = 0;
        while inspected < candidates && sent < FEEDBACK_PER_TICK {
            let index = self.feedback_cursor % candidates;
            self.feedback_cursor = (index + 1) % candidates;
            inspected += 1;
            let display = self.wanted[index];
            if !descriptor_shows(descriptor, display) {
                continue;
            }
            if self.shown.contains(&display) {
                continue;
            }
            if self.session.send(message_for(display), now_ms).is_err() {
                break;
            }
            self.shown.retain(|known| !same_control(*known, display));
            self.shown.push(display);
            sent += 1;
        }
        sent
    }

    /// Releases anything the panel still holds and forgets its bindings.
    ///
    /// Releases are retried a bounded number of times. A control the sink keeps
    /// refusing is reported and left held rather than spun on — the host's own
    /// `panel_connected` stays true in that case, which the caller must treat
    /// as fatal rather than as a panel it can replace.
    pub fn release<H: PanelHost>(&mut self, host: &mut H) {
        if self.bound {
            let mut outstanding = Vec::new();
            for _ in 0..RELEASE_ATTEMPTS {
                outstanding = host.disconnect_panel();
                if outstanding.is_empty() {
                    break;
                }
            }
            // Reported once, after the retries are spent: a release that
            // succeeded on a later attempt is not a control left held.
            for failure in &outstanding {
                self.notices.push(PanelNotice::ReleaseFailed {
                    control: failure.control(),
                });
            }
            self.bound = false;
        }
    }
}

/// Drives one panel connection over a stream until it closes.
///
/// The link is torn down on **every** exit, including a failed write: the
/// teardown happens once, after the loop, rather than at each early return, so
/// a later branch cannot forget it.
///
/// Notices come back on **both** paths. An error is exactly when the operator
/// most needs to know a control was left held, so the notices are returned
/// beside the result rather than inside it.
pub fn serve_panel<S, H, C, I>(
    stream: &mut S,
    host: &mut H,
    config: LinkConfig,
    session_id: u64,
    stop: &AtomicBool,
    clock: C,
    mut while_idle: I,
) -> (io::Result<PanelClosure>, Vec<PanelNotice>)
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
    I: FnMut(),
{
    serve_panel_with_host_idle(stream, host, config, session_id, stop, clock, |_| {
        while_idle();
    })
}

fn serve_panel_with_host_idle<S, H, C, I>(
    stream: &mut S,
    host: &mut H,
    config: LinkConfig,
    session_id: u64,
    stop: &AtomicBool,
    mut clock: C,
    mut while_idle: I,
) -> (io::Result<PanelClosure>, Vec<PanelNotice>)
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
    I: FnMut(&mut H),
{
    let mut link = PanelLink::new(config);
    let outcome = drive_panel(
        &mut link,
        stream,
        host,
        session_id,
        stop,
        &mut clock,
        &mut while_idle,
    );
    link.release(host);
    let notices = link.take_notices();
    (outcome, notices)
}

/// The loop itself, which never tears down — [`serve_panel`] does that once for
/// every way out of here.
fn drive_panel<S, H, C, I>(
    link: &mut PanelLink,
    stream: &mut S,
    host: &mut H,
    session_id: u64,
    stop: &AtomicBool,
    clock: &mut C,
    while_idle: &mut I,
) -> io::Result<PanelClosure>
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
    I: FnMut(&mut H),
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
            // A quiet panel is the common case, not a failure. Which kind a
            // quiet read reports is platform-dependent: a socket with
            // `SO_RCVTIMEO` set reports `TimedOut`, a non-blocking one reports
            // `WouldBlock`. Both mean the same thing here.
            // A quiet moment is the only safe place to do anything other than
            // serve this panel, so it is where a second one gets turned away.
            Err(error) if is_quiet(error.kind()) => {
                while_idle(host);
                link.refresh_display(host, clock());
                0
            }
            Err(error) => return Err(error),
        };
        if let PanelStep::Closed(closure) = link.step(&buffer[..read], clock(), host) {
            return Ok(closure);
        }
    }
}

/// Whether the panel declared the control this display addresses.
const fn descriptor_shows(descriptor: PanelDescriptor, display: PanelDisplay) -> bool {
    match display {
        PanelDisplay::Ring { index, .. } => index < descriptor.rings,
        PanelDisplay::Led { index, .. } => index < descriptor.leds,
    }
}

/// Whether two displays address the same physical control, so the newer one
/// replaces the older rather than accumulating beside it.
const fn same_control(left: PanelDisplay, right: PanelDisplay) -> bool {
    match (left, right) {
        (PanelDisplay::Ring { index: a, .. }, PanelDisplay::Ring { index: b, .. })
        | (PanelDisplay::Led { index: a, .. }, PanelDisplay::Led { index: b, .. }) => a == b,
        _ => false,
    }
}

const fn message_for(display: PanelDisplay) -> Message {
    match display {
        PanelDisplay::Ring { index, value } => Message::SetRingValue {
            ring_id: index,
            value,
        },
        PanelDisplay::Led { index, enabled } => Message::SetLed {
            control_id: index as u16,
            enabled,
        },
    }
}

/// Whether a read reporting this kind means "no bytes yet" rather than "this
/// connection is broken".
#[must_use]
pub const fn is_quiet(kind: io::ErrorKind) -> bool {
    matches!(kind, io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
}

/// What one accepted panel connection produced.
#[derive(Debug)]
pub struct ServedPanel {
    pub peer: SocketAddr,
    /// Which connection this was, so a run of failures is distinguishable from
    /// one failure reported repeatedly.
    pub session_id: u64,
    /// How the connection ended, or the stream failure that ended it. One bad
    /// panel costs one panel: this is reported, and the runtime accepts the
    /// next.
    pub outcome: io::Result<PanelClosure>,
    pub notices: Vec<PanelNotice>,
}

/// Why the accept loop stopped serving.
#[derive(Debug)]
pub enum ServeError {
    /// The listener itself failed. Nothing further can be accepted.
    Listener(io::Error),
    /// A teardown left controls held, so the owner cannot take another panel.
    /// Serving on would produce an endless run of immediately-refused panels.
    ControlsStillHeld,
}

impl fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Listener(error) => write!(formatter, "the panel listener failed: {error}"),
            Self::ControlsStillHeld => formatter.write_str(
                "a panel teardown left controls held, so no further panel can be served",
            ),
        }
    }
}

impl Error for ServeError {}

/// Accepts and serves panels, one at a time, until asked to stop.
///
/// The listener must be non-blocking so the stop flag is observed while idle.
///
/// **One bad panel costs one panel.** A stream that fails to prepare or fails
/// mid-connection is reported through `served` and the loop goes back to
/// accepting: a control surface is an accessory, and losing one must not stop
/// the project the runtime is hosting.
///
/// The backlog is drained immediately after a panel is accepted **and** on every
/// quiet moment of the session that follows, so a second panel is turned away
/// promptly rather than left with an established connection and total silence —
/// which a reconnect state machine cannot tell apart from a slow runtime.
///
/// A panel that resets and redials inside the link's timeout is refused too,
/// because until that timeout expires the runtime genuinely still holds a
/// panel. It can redial once the session ends; draining only after the session
/// would instead leave every mid-session arrival queued for its whole duration.
///
/// # Errors
///
/// Returns [`ServeError`] only for what genuinely ends the service: the
/// listener failing, or a teardown leaving controls held.
pub fn serve_panels<H, C, R>(
    listener: &TcpListener,
    host: &mut H,
    config: LinkConfig,
    stop: &AtomicBool,
    clock: C,
    served: R,
) -> Result<(), ServeError>
where
    H: PanelHost,
    C: FnMut() -> u64,
    R: FnMut(&ServedPanel),
{
    serve_panels_with_idle(listener, host, config, stop, clock, served, |_| {})
}

/// Accepts and serves panels while giving other bounded owner commands one
/// turn whenever the socket service is idle.
///
/// The callback runs with the sole mutable host authority. It must perform
/// finite non-blocking work so panel upkeep and shutdown remain bounded.
///
/// # Errors
///
/// Returns the same terminal service failures as [`serve_panels`].
pub fn serve_panels_with_idle<H, C, R, I>(
    listener: &TcpListener,
    host: &mut H,
    config: LinkConfig,
    stop: &AtomicBool,
    mut clock: C,
    mut served: R,
    mut while_idle: I,
) -> Result<(), ServeError>
where
    H: PanelHost,
    C: FnMut() -> u64,
    R: FnMut(&ServedPanel),
    I: FnMut(&mut H),
{
    let mut session_id = 1_u64;
    while !stop.load(Ordering::Acquire) {
        let (mut stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                while_idle(host);
                std::thread::sleep(ACCEPT_POLL_INTERVAL);
                continue;
            }
            Err(error) => return Err(ServeError::Listener(error)),
        };
        // Anything queued behind the panel just taken is turned away now, at
        // the moment one panel is committed to. Draining after the session
        // instead would also refuse panels that arrived later — including the
        // same panel reconnecting after a reset.
        let _ = refuse_waiting_panels(listener);
        let (outcome, notices) = match prepare_panel_stream(&stream) {
            Ok(()) => serve_panel_with_host_idle(
                &mut stream,
                host,
                config,
                session_id,
                stop,
                &mut clock,
                |host| {
                    let _ = refuse_waiting_panels(listener);
                    while_idle(host);
                },
            ),
            Err(error) => (Err(error), Vec::new()),
        };
        served(&ServedPanel {
            peer,
            session_id,
            outcome,
            notices,
        });
        session_id = session_id.wrapping_add(1);
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
/// The write timeout matters for the same reason the read one does: without it
/// a panel that stops reading blocks the serving loop indefinitely, and the
/// stop flag is checked between reads.
///
/// # Errors
///
/// Returns the socket's own refusal to change mode.
pub fn prepare_panel_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(PANEL_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(PANEL_WRITE_TIMEOUT))
}

/// Accepts and immediately drops a bounded number of panels waiting on the
/// listener.
///
/// One panel at a time: a second one is turned away where it can see that it
/// was, rather than left in the backlog to be promoted stale once the first
/// disconnects.
///
/// Returns how many were refused. The listener must be non-blocking, as required
/// by [`serve_panels`]; otherwise the first empty `accept` can still block.
#[must_use]
pub fn refuse_waiting_panels(listener: &TcpListener) -> usize {
    let mut refused = 0;
    for _ in 0..PANEL_REFUSALS_PER_TICK {
        let Ok((extra, _)) = listener.accept() else {
            break;
        };
        drop(extra);
        refused += 1;
    }
    refused
}
