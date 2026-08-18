#![doc = "Hosting one project from a runtime process: arguments, the default panel layout, and the panel link."]
#![forbid(unsafe_code)]

use core::fmt;
use core::num::NonZeroU32;
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use hyper_control_runtime::{
    BooleanControlTarget, PanelActionTarget, PanelBinding, PanelButtonBehavior, PanelConnectError,
    PanelConnection, PanelDispatchError, PanelDispatchOutcome, PanelReleaseFailure,
    ValueControlTarget,
};
use hyper_panel_protocol::{LinkConfig, LinkFault, Message, PanelDescriptor};
use hyper_runtime_panel_session::{PanelEvent, PanelSession};

/// How far one encoder detent moves a channel gain, in millionths. A hundred
/// detents cover unity, which is a usable throw on a physical encoder without
/// making one accidental knock audible.
pub const DEFAULT_GAIN_STEP_MILLIONTHS: u32 = 10_000;

/// The same step, in the form a binding takes.
const DEFAULT_GAIN_STEP: NonZeroU32 = match NonZeroU32::new(DEFAULT_GAIN_STEP_MILLIONTHS) {
    Some(step) => step,
    None => panic!("the default gain step is a non-zero constant"),
};

/// The largest read this host takes from a panel in one step.
const READ_CHUNK: usize = 1024;

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

/// Builds the layout a panel gets when nothing has configured one.
///
/// Encoders take channel gains and buttons take channel mutes, both in index
/// order and both stopping at whichever of the panel and the project runs out
/// first. One button past the last channel, if the panel has one, toggles the
/// transport.
///
/// Deliberately dull: it names nothing the panel lacks and nothing the project
/// lacks, so every binding it produces installs.
#[must_use]
pub fn default_layout(descriptor: PanelDescriptor, mixer_channel_count: usize) -> Vec<PanelBinding> {
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
/// dispatch, and teardown — can be exercised against a real panel without a
/// device and without a second thread.
pub struct PanelLink {
    session: PanelSession,
    connected: bool,
}

impl PanelLink {
    #[must_use]
    pub fn new(config: LinkConfig) -> Self {
        Self {
            session: PanelSession::new(config),
            connected: false,
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

    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.connected
    }

    /// Feeds bytes received from the panel and advances link upkeep.
    ///
    /// A panel that identifies itself gets the default layout; one that goes
    /// away takes its bindings down with it, so nothing it held stays held.
    pub fn step<H: PanelHost>(&mut self, received: &[u8], now_ms: u64, host: &mut H) -> PanelStep {
        let mut events = self.session.receive(received, now_ms);
        if let Some(event) = self.session.poll(now_ms) {
            events.push(event);
        }
        for event in events {
            match event {
                PanelEvent::Connected(descriptor) => {
                    let bindings = default_layout(descriptor, host.mixer_channel_count());
                    if host.connect_panel(descriptor, &bindings).is_err() {
                        return self.close(host, PanelClosure::LinkLost);
                    }
                    self.connected = true;
                }
                PanelEvent::Input(message) => {
                    // A panel can outrun the first mixer snapshot, so a target
                    // that cannot be read yet is a startup race rather than a
                    // reason to drop the link.
                    let _ = host.dispatch_panel(message);
                }
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

    /// Ends the connection, releasing anything the panel still holds.
    pub fn close<H: PanelHost>(&mut self, host: &mut H, closure: PanelClosure) -> PanelStep {
        if self.connected {
            // A refused release keeps the panel installed and keeps the control
            // held, so retry until the owner takes it or reports none left.
            while !host.disconnect_panel().is_empty() && host.panel_connected() {}
            self.connected = false;
        }
        PanelStep::Closed(closure)
    }
}

/// Drives one panel connection over a stream until it closes.
///
/// # Errors
///
/// Returns the stream's own failure. A read that would block is not a failure;
/// it is how link upkeep gets a turn.
pub fn serve_panel<S, H, C>(
    stream: &mut S,
    host: &mut H,
    config: LinkConfig,
    session_id: u64,
    mut clock: C,
) -> io::Result<PanelClosure>
where
    S: Read + Write,
    H: PanelHost,
    C: FnMut() -> u64,
{
    let mut link = PanelLink::new(config);
    if link.start(session_id, clock()).is_err() {
        return Ok(PanelClosure::LinkLost);
    }
    let mut buffer = [0_u8; READ_CHUNK];
    loop {
        let outbound = link.take_transmit();
        if !outbound.is_empty() {
            stream.write_all(&outbound)?;
        }
        let read = match stream.read(&mut buffer) {
            Ok(0) => {
                link.close(host, PanelClosure::PeerClosed);
                return Ok(PanelClosure::PeerClosed);
            }
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                0
            }
            Err(error) => {
                link.close(host, PanelClosure::LinkLost);
                return Err(error);
            }
        };
        if let PanelStep::Closed(closure) = link.step(&buffer[..read], clock(), host) {
            return Ok(closure);
        }
    }
}

impl PanelHost for hyper_desktop_audio::DesktopPlaybackSession {
    type Error = hyper_desktop_audio::DesktopControlSinkError;

    fn mixer_channel_count(&self) -> usize {
        Self::mixer_channel_count(self)
    }

    fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<Self::Error>, PanelConnectError> {
        Self::connect_panel(self, descriptor, bindings)
    }

    fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<Self::Error>> {
        Self::disconnect_panel(self)
    }

    fn panel_connected(&self) -> bool {
        Self::panel_connected(self)
    }

    fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<Self::Error>> {
        Self::dispatch_panel(self, message)
    }
}
