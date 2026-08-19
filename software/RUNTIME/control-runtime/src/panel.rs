//! Bindings from a discovered panel's controls to runtime targets.
//!
//! A panel describes itself at handshake instead of being declared in the
//! project, so these bindings are runtime state validated against the
//! descriptor the connected panel sent rather than compiled into the mapping
//! table.

use core::num::NonZeroU32;

use hyper_control_mappings::{ChannelParameter, MainParameter, SendParameter};
use hyper_panel_protocol::{Message, PanelDescriptor};

use crate::{
    BooleanControlTarget, BooleanShadow, ControlBindingAvailability,
    ControlBindingUnavailableReason, ControlRuntime, RuntimeControlCommand, RuntimeControlSink,
    RuntimeControlTarget, ValueControlTarget, ValueShadow,
};

/// One physical control on the connected panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelControl {
    Encoder { index: u8 },
    Button { index: u8 },
}

/// How a button's press and release pair behaves. Declared with the binding,
/// as Ardour declares its `CtlType`, rather than inferred from the messages a
/// panel happens to send.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelButtonBehavior {
    Momentary,
    Toggle,
}

/// One target a button invokes rather than one it sets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelActionTarget {
    TransportStart,
    TransportStop,
    TransportToggle,
    TransportLocateStart,
    SceneLaunch { scene_index: usize },
}

/// One binding from a panel control to a target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelBinding {
    /// An encoder moves a continuous target by `step_millionths` per detent.
    Encoder {
        index: u8,
        target: ValueControlTarget,
        step_millionths: NonZeroU32,
    },
    /// A button sets a boolean target, momentarily or by toggling it.
    Button {
        index: u8,
        target: BooleanControlTarget,
        behavior: PanelButtonBehavior,
    },
    /// A button invokes an action on press.
    ActionButton {
        index: u8,
        action: PanelActionTarget,
    },
}

impl PanelBinding {
    #[must_use]
    pub const fn control(self) -> PanelControl {
        match self {
            Self::Encoder { index, .. } => PanelControl::Encoder { index },
            Self::Button { index, .. } | Self::ActionButton { index, .. } => {
                PanelControl::Button { index }
            }
        }
    }

    const fn runtime_target(self) -> RuntimeControlTarget {
        match self {
            Self::Encoder { target, .. } => value_runtime_target(target),
            Self::Button { target, .. } => boolean_runtime_target(target),
            Self::ActionButton { action, .. } => match action {
                PanelActionTarget::TransportStart
                | PanelActionTarget::TransportStop
                | PanelActionTarget::TransportToggle
                | PanelActionTarget::TransportLocateStart => RuntimeControlTarget::Transport,
                PanelActionTarget::SceneLaunch { scene_index } => {
                    RuntimeControlTarget::Scene { scene_index }
                }
            },
        }
    }
}

/// Why one binding did not install against the connected panel.
#[derive(Debug, Eq, PartialEq)]
pub enum PanelBindingRefusalReason<E> {
    /// The descriptor the panel sent has no such encoder or button.
    ControlAbsent,
    /// An earlier binding already claimed the same control.
    ControlAlreadyBound { first_binding_index: usize },
    /// The sink rejected the target the binding names.
    TargetRejected(E),
}

/// One binding the connected panel refused, reported rather than dropped.
#[derive(Debug, Eq, PartialEq)]
pub struct PanelBindingRefusal<E> {
    binding_index: usize,
    reason: PanelBindingRefusalReason<E>,
}

impl<E> PanelBindingRefusal<E> {
    #[must_use]
    pub const fn binding_index(&self) -> usize {
        self.binding_index
    }

    #[must_use]
    pub const fn reason(&self) -> &PanelBindingRefusalReason<E> {
        &self.reason
    }
}

/// Why a panel's bindings could not be installed at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelConnectError {
    /// A panel is already connected. Disconnect it first so anything held is
    /// released rather than stranded.
    AlreadyConnected,
    /// Shadow state for the bindings could not be allocated.
    CapacityUnavailable,
}

/// The outcome of installing one panel's bindings.
///
/// Carries the bindings the panel refused. It is `#[must_use]` because
/// reporting a refusal rather than silently ignoring it is the point of the
/// refusal — a caller that drops this has turned the guarantee back into
/// silent ignoring.
#[must_use = "a refused binding is silently ignored unless the caller reads it"]
#[derive(Debug, Eq, PartialEq)]
pub struct PanelConnection<E> {
    refused: Vec<PanelBindingRefusal<E>>,
    installed: usize,
}

impl<E> PanelConnection<E> {
    #[must_use]
    pub fn refused(&self) -> &[PanelBindingRefusal<E>] {
        &self.refused
    }

    /// Whether every offered binding installed.
    #[must_use]
    pub fn is_fully_installed(&self) -> bool {
        self.refused.is_empty()
    }

    /// How many bindings the connected panel can actually serve. Zero means the
    /// panel is connected but inert.
    #[must_use]
    pub const fn installed(&self) -> usize {
        self.installed
    }

    #[must_use]
    pub fn into_refused(self) -> Vec<PanelBindingRefusal<E>> {
        self.refused
    }
}

/// One held control the sink refused to release at disconnect.
#[derive(Debug, Eq, PartialEq)]
pub struct PanelReleaseFailure<E> {
    control: PanelControl,
    command: RuntimeControlCommand,
    source: E,
}

impl<E> PanelReleaseFailure<E> {
    #[must_use]
    pub const fn control(&self) -> PanelControl {
        self.control
    }

    #[must_use]
    pub const fn command(&self) -> RuntimeControlCommand {
        self.command
    }

    #[must_use]
    pub const fn source(&self) -> &E {
        &self.source
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum PanelDispatchError<E> {
    /// The target's present state is not readable, so a toggle or a relative
    /// move has nothing to work from.
    StateUnavailable { control: PanelControl },
    Sink {
        control: PanelControl,
        command: RuntimeControlCommand,
        source: E,
    },
}

/// Why a message commanded nothing, distinguished so an operator debugging a
/// silent panel is not told the same thing in every case.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnboundReason {
    /// Link upkeep, an input kind nothing binds, or a message addressed to the
    /// panel rather than sent by it.
    MessageBindsNothing,
    /// A control id no descriptor can name.
    ControlOutOfRange,
    /// No panel is connected.
    NoPanelConnected,
    /// The panel is connected but nothing claims this control.
    NoBinding { control: PanelControl },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelDispatchOutcome {
    /// Nothing claims this message.
    Unbound(UnboundReason),
    /// A binding claims the control but this edge commands nothing.
    Idle { control: PanelControl },
    /// A relative move that the target's domain absorbed, so the position did
    /// not change. Distinct from [`Self::Idle`] because a control sitting at
    /// its limit is a state worth showing.
    AtLimit { control: PanelControl },
    Unavailable {
        control: PanelControl,
        reason: ControlBindingUnavailableReason,
    },
    Sent {
        control: PanelControl,
        command: RuntimeControlCommand,
    },
}

/// One thing a panel can show about a target it is bound to.
///
/// A ring value is normalised over the whole `u16` range: the panel reports its
/// own `ring_segments` at handshake precisely because resolution is its
/// business, so the runtime sends a position rather than a segment count.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelDisplay {
    Ring { index: u8, value: u16 },
    Led { index: u8, enabled: bool },
}

#[derive(Clone, Copy, Debug)]
struct InstalledBinding {
    binding: PanelBinding,
    availability: ControlBindingAvailability,
    held: bool,
}

#[derive(Debug)]
pub(crate) struct PanelBindings {
    bindings: Vec<InstalledBinding>,
}

#[derive(Clone, Copy)]
enum PanelEdge {
    Turn(i16),
    Press,
    Release,
}

enum PanelCommand {
    Idle,
    AtLimit,
    StateUnavailable,
    Send(RuntimeControlCommand),
}

impl<S: RuntimeControlSink> ControlRuntime<S> {
    /// Installs bindings against the descriptor the connected panel sent.
    ///
    /// Bindings naming a control the panel does not have, a control an earlier
    /// binding already claimed, or a target the sink rejects are refused and
    /// returned. The rest install, so a panel exchanged for one with fewer
    /// controls keeps the bindings whose controls it still has.
    ///
    /// # Errors
    ///
    /// Returns [`PanelAlreadyConnected`] when a panel is still connected.
    pub fn connect_panel(
        &mut self,
        descriptor: PanelDescriptor,
        bindings: &[PanelBinding],
    ) -> Result<PanelConnection<S::Error>, PanelConnectError> {
        if self.panel.is_some() {
            return Err(PanelConnectError::AlreadyConnected);
        }
        let mut installed: Vec<InstalledBinding> = Vec::new();
        let mut refused = Vec::new();
        let mut claimed: Vec<(PanelControl, usize)> = Vec::new();
        for (binding_index, binding) in bindings.iter().copied().enumerate() {
            let control = binding.control();
            if !descriptor_has(descriptor, control) {
                refused.push(PanelBindingRefusal {
                    binding_index,
                    reason: PanelBindingRefusalReason::ControlAbsent,
                });
                continue;
            }
            if let Some((_, first_binding_index)) =
                claimed.iter().find(|(claim, _)| *claim == control)
            {
                refused.push(PanelBindingRefusal {
                    binding_index,
                    reason: PanelBindingRefusalReason::ControlAlreadyBound {
                        first_binding_index: *first_binding_index,
                    },
                });
                continue;
            }
            let target = binding.runtime_target();
            let availability = self.sink.target_availability(target);
            if availability == ControlBindingAvailability::Live
                && let Err(source) = self.sink.validate_target(target)
            {
                refused.push(PanelBindingRefusal {
                    binding_index,
                    reason: PanelBindingRefusalReason::TargetRejected(source),
                });
                continue;
            }
            claimed.push((control, binding_index));
            installed.push(InstalledBinding {
                binding,
                availability,
                held: false,
            });
        }
        for installed in &installed {
            if self.seed_shadow(installed.binding).is_err() {
                self.value_shadows.retain(|shadow| !shadow.panel_seeded);
                self.boolean_shadows.retain(|shadow| !shadow.panel_seeded);
                return Err(PanelConnectError::CapacityUnavailable);
            }
        }
        let count = installed.len();
        self.panel = Some(PanelBindings {
            bindings: installed,
        });
        Ok(PanelConnection {
            refused,
            installed: count,
        })
    }

    /// Releases any momentary control still held, then forgets the panel's
    /// bindings, so a link lost mid-press cannot strand its target.
    ///
    /// A binding whose release the sink refuses stays installed and stays held,
    /// so the panel is *not* forgotten and a later call retries it. Check the
    /// returned failures rather than assuming the panel is gone —
    /// [`Self::panel_connected`] still reports true while any release is
    /// outstanding.
    #[must_use]
    pub fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<S::Error>> {
        let mut failures = Vec::new();
        let Some(panel) = self.panel.as_ref() else {
            return failures;
        };
        let held: Vec<(usize, PanelControl, BooleanControlTarget)> = panel
            .bindings
            .iter()
            .enumerate()
            .filter_map(
                |(position, installed)| match (installed.binding, installed.held) {
                    (PanelBinding::Button { target, .. }, true) => {
                        Some((position, installed.binding.control(), target))
                    }
                    _ => None,
                },
            )
            .collect();
        for (position, control, target) in held {
            let command = boolean_command(target, false);
            match self.sink.try_send(command) {
                Ok(()) => {
                    self.commit_shadow(command);
                    if let Some(panel) = self.panel.as_mut() {
                        panel.bindings[position].held = false;
                    }
                }
                Err(source) => failures.push(PanelReleaseFailure {
                    control,
                    command,
                    source,
                }),
            }
        }
        if !failures.is_empty() {
            return failures;
        }
        self.panel = None;
        self.value_shadows.retain(|shadow| !shadow.panel_seeded);
        self.boolean_shadows.retain(|shadow| !shadow.panel_seeded);
        failures
    }

    /// Whether a panel's bindings are installed.
    #[must_use]
    pub const fn panel_connected(&self) -> bool {
        self.panel.is_some()
    }

    /// Dispatches one panel message against the installed bindings.
    ///
    /// # Errors
    ///
    /// Returns the sink's original rejection, or reports that the target's
    /// present state could not be read.
    pub fn dispatch_panel(
        &mut self,
        message: Message,
    ) -> Result<PanelDispatchOutcome, PanelDispatchError<S::Error>> {
        let (control, edge) = match panel_input(message) {
            Ok(input) => input,
            Err(reason) => return Ok(PanelDispatchOutcome::Unbound(reason)),
        };
        let Some(panel) = self.panel.as_ref() else {
            return Ok(PanelDispatchOutcome::Unbound(
                UnboundReason::NoPanelConnected,
            ));
        };
        let Some(position) = panel
            .bindings
            .iter()
            .position(|installed| installed.binding.control() == control)
        else {
            return Ok(PanelDispatchOutcome::Unbound(UnboundReason::NoBinding {
                control,
            }));
        };
        let installed = panel.bindings[position];
        if let ControlBindingAvailability::Unavailable(_) = installed.availability {
            // A panel connection outlasts the conditions checked at connect: a
            // scene gets prepared, a full queue drains. Re-ask for the control
            // the operator just pressed rather than reporting the answer from
            // when they plugged the panel in.
            let availability = self
                .sink
                .target_availability(installed.binding.runtime_target());
            if let ControlBindingAvailability::Unavailable(reason) = availability {
                return Ok(PanelDispatchOutcome::Unavailable { control, reason });
            }
            if let Some(panel) = self.panel.as_mut() {
                panel.bindings[position].availability = availability;
            }
        }
        self.refresh_missing_state();
        let command = match self.panel_command(installed.binding, edge) {
            PanelCommand::Idle => return Ok(PanelDispatchOutcome::Idle { control }),
            PanelCommand::AtLimit => return Ok(PanelDispatchOutcome::AtLimit { control }),
            PanelCommand::StateUnavailable => {
                return Err(PanelDispatchError::StateUnavailable { control });
            }
            PanelCommand::Send(command) => command,
        };
        self.sink
            .try_send(command)
            .map_err(|source| PanelDispatchError::Sink {
                control,
                command,
                source,
            })?;
        self.commit_shadow(command);
        if let Some(panel) = self.panel.as_mut()
            && matches!(
                installed.binding,
                PanelBinding::Button {
                    behavior: PanelButtonBehavior::Momentary,
                    ..
                }
            )
        {
            panel.bindings[position].held = matches!(edge, PanelEdge::Press);
        }
        Ok(PanelDispatchOutcome::Sent { control, command })
    }

    /// Makes room for one binding's shadow without reading it.
    ///
    /// The value is left unknown so [`Self::refresh_missing_state`] reads it at
    /// first use rather than at connect — the sink's snapshot lags what the
    /// audio thread has been sent, and reading late is reading less stale. A
    /// shadow an existing binding already owns is left alone; one created here
    /// is marked so the matching disconnect removes exactly what it added.
    /// Collects what the connected panel should be showing.
    ///
    /// Reads the shadows rather than the sink: the sink's snapshot lags what
    /// the audio thread has been sent, and a display built from it would show
    /// the previous position of the very control the operator is turning.
    ///
    /// Unknown state is read from the sink first. That is the one moment the
    /// sink is the right source: nothing has been sent yet, so there is no
    /// pending command for it to lag behind — and without it a freshly
    /// connected panel would stay dark until the operator touched something,
    /// which is the opposite of what a display is for.
    pub fn panel_display(&mut self, out: &mut Vec<PanelDisplay>) {
        out.clear();
        if self.panel.is_none() {
            return;
        }
        self.refresh_missing_state();
        let Some(panel) = self.panel.as_ref() else {
            return;
        };
        for installed in &panel.bindings {
            match installed.binding {
                PanelBinding::Encoder { index, target, .. } => {
                    if let Some(value) = self
                        .value_shadows
                        .iter()
                        .find(|shadow| shadow.target == target)
                        .and_then(|shadow| shadow.value)
                    {
                        out.push(PanelDisplay::Ring {
                            index,
                            value: self.sink.value_domain(target).normalize(value),
                        });
                    }
                }
                PanelBinding::Button { index, target, .. } => {
                    if let Some(enabled) = self
                        .boolean_shadows
                        .iter()
                        .find(|shadow| shadow.target == target)
                        .and_then(|shadow| shadow.value)
                    {
                        out.push(PanelDisplay::Led { index, enabled });
                    }
                }
                PanelBinding::ActionButton { .. } => {}
            }
        }
    }

    fn seed_shadow(&mut self, binding: PanelBinding) -> Result<(), ()> {
        match binding {
            PanelBinding::Encoder { target, .. } => {
                if !self
                    .value_shadows
                    .iter()
                    .any(|shadow| shadow.target == target)
                {
                    self.value_shadows.try_reserve(1).map_err(|_| ())?;
                    self.value_shadows.push(ValueShadow {
                        target,
                        value: None,
                        panel_seeded: true,
                    });
                }
            }
            PanelBinding::Button {
                target,
                behavior: PanelButtonBehavior::Toggle,
                ..
            } => {
                if !self
                    .boolean_shadows
                    .iter()
                    .any(|shadow| shadow.target == target)
                {
                    self.boolean_shadows.try_reserve(1).map_err(|_| ())?;
                    self.boolean_shadows.push(BooleanShadow {
                        target,
                        value: None,
                        panel_seeded: true,
                    });
                }
            }
            PanelBinding::Button { .. } | PanelBinding::ActionButton { .. } => {}
        }
        Ok(())
    }

    fn panel_command(&self, binding: PanelBinding, edge: PanelEdge) -> PanelCommand {
        match binding {
            PanelBinding::Encoder {
                target,
                step_millionths,
                ..
            } => {
                let PanelEdge::Turn(delta) = edge else {
                    return PanelCommand::Idle;
                };
                let Some(current) = self
                    .value_shadows
                    .iter()
                    .find(|shadow| shadow.target == target)
                    .and_then(|shadow| shadow.value)
                else {
                    return PanelCommand::StateUnavailable;
                };
                let moved =
                    i64::from(current) + i64::from(delta) * i64::from(step_millionths.get());
                let next = self.sink.value_domain(target).clamp(moved);
                if next == current {
                    return if moved == i64::from(next) {
                        PanelCommand::Idle
                    } else {
                        PanelCommand::AtLimit
                    };
                }
                PanelCommand::Send(value_command(target, next))
            }
            PanelBinding::Button {
                target, behavior, ..
            } => match (behavior, edge) {
                (PanelButtonBehavior::Momentary, PanelEdge::Press) => {
                    PanelCommand::Send(boolean_command(target, true))
                }
                (PanelButtonBehavior::Momentary, PanelEdge::Release) => {
                    PanelCommand::Send(boolean_command(target, false))
                }
                (PanelButtonBehavior::Toggle, PanelEdge::Press) => {
                    let Some(current) = self
                        .boolean_shadows
                        .iter()
                        .find(|shadow| shadow.target == target)
                        .and_then(|shadow| shadow.value)
                    else {
                        return PanelCommand::StateUnavailable;
                    };
                    PanelCommand::Send(boolean_command(target, !current))
                }
                (PanelButtonBehavior::Toggle, PanelEdge::Release) | (_, PanelEdge::Turn(_)) => {
                    PanelCommand::Idle
                }
            },
            PanelBinding::ActionButton { action, .. } => {
                if !matches!(edge, PanelEdge::Press) {
                    return PanelCommand::Idle;
                }
                match action {
                    PanelActionTarget::TransportStart => {
                        PanelCommand::Send(RuntimeControlCommand::TransportStart)
                    }
                    PanelActionTarget::TransportStop => {
                        PanelCommand::Send(RuntimeControlCommand::TransportStop)
                    }
                    PanelActionTarget::TransportToggle => {
                        let Some(rolling) = self.transport_rolling else {
                            return PanelCommand::StateUnavailable;
                        };
                        PanelCommand::Send(if rolling {
                            RuntimeControlCommand::TransportStop
                        } else {
                            RuntimeControlCommand::TransportStart
                        })
                    }
                    PanelActionTarget::TransportLocateStart => {
                        PanelCommand::Send(RuntimeControlCommand::TransportLocateStartPreserve)
                    }
                    PanelActionTarget::SceneLaunch { scene_index } => {
                        PanelCommand::Send(RuntimeControlCommand::SceneLaunch { scene_index })
                    }
                }
            }
        }
    }
}

const fn descriptor_has(descriptor: PanelDescriptor, control: PanelControl) -> bool {
    match control {
        PanelControl::Encoder { index } => index < descriptor.encoders,
        PanelControl::Button { index } => index < descriptor.buttons,
    }
}

/// Separates a panel message into the control it names and what that control
/// did, or reports why it names no bindable control.
///
/// Enumerated rather than wildcarded: the whole reason this crate depends on
/// the protocol is that the wire vocabulary and the binding vocabulary stay in
/// step, and a new panel-to-runtime input kind should stop the build here
/// rather than be silently ignored.
fn panel_input(message: Message) -> Result<(PanelControl, PanelEdge), UnboundReason> {
    let (control_id, control): (u16, fn(u8) -> PanelControl) = match message {
        Message::EncoderDelta { control_id, .. } => {
            (control_id, |index| PanelControl::Encoder { index })
        }
        Message::ButtonPress { control_id, .. } | Message::ButtonRelease { control_id, .. } => {
            (control_id, |index| PanelControl::Button { index })
        }
        Message::InputSnapshot(_)
        | Message::EncoderPress { .. }
        | Message::EncoderRelease { .. }
        | Message::ButtonLongPress { .. }
        | Message::ButtonDoublePress { .. }
        | Message::PanelError { .. }
        | Message::Heartbeat { .. }
        | Message::RuntimeHello { .. }
        | Message::PanelHello { .. }
        | Message::VersionRejected { .. }
        | Message::SetLed { .. }
        | Message::SetLedBrightness { .. }
        | Message::SetLedBlink(_)
        | Message::SetRingSegment(_)
        | Message::SetRingValue { .. }
        | Message::SetRingAnimation { .. }
        | Message::SetGlobalBrightness { .. }
        | Message::EnterBootloader { .. } => return Err(UnboundReason::MessageBindsNothing),
    };
    let index = u8::try_from(control_id).map_err(|_| UnboundReason::ControlOutOfRange)?;
    let edge = match message {
        Message::EncoderDelta { delta, .. } => PanelEdge::Turn(delta),
        Message::ButtonRelease { .. } => PanelEdge::Release,
        _ => PanelEdge::Press,
    };
    Ok((control(index), edge))
}

const fn value_runtime_target(target: ValueControlTarget) -> RuntimeControlTarget {
    match target {
        ValueControlTarget::ChannelGain { channel_index } => RuntimeControlTarget::Channel {
            channel_index,
            parameter: ChannelParameter::Gain,
        },
        ValueControlTarget::ChannelPan { channel_index } => RuntimeControlTarget::Channel {
            channel_index,
            parameter: ChannelParameter::Pan,
        },
        ValueControlTarget::SendGain {
            channel_index,
            send_index,
        } => RuntimeControlTarget::Send {
            channel_index,
            send_index,
            parameter: SendParameter::Gain,
        },
        ValueControlTarget::SendPan {
            channel_index,
            send_index,
        } => RuntimeControlTarget::Send {
            channel_index,
            send_index,
            parameter: SendParameter::Pan,
        },
        ValueControlTarget::MainGain => RuntimeControlTarget::Main {
            parameter: MainParameter::Gain,
        },
    }
}

const fn boolean_runtime_target(target: BooleanControlTarget) -> RuntimeControlTarget {
    match target {
        BooleanControlTarget::ChannelMute { channel_index } => RuntimeControlTarget::Channel {
            channel_index,
            parameter: ChannelParameter::Mute,
        },
        BooleanControlTarget::ChannelSolo { channel_index } => RuntimeControlTarget::Channel {
            channel_index,
            parameter: ChannelParameter::Solo,
        },
        BooleanControlTarget::SendEnabled {
            channel_index,
            send_index,
        } => RuntimeControlTarget::Send {
            channel_index,
            send_index,
            parameter: SendParameter::Enabled,
        },
        BooleanControlTarget::MainMute => RuntimeControlTarget::Main {
            parameter: MainParameter::Mute,
        },
    }
}

fn value_command(target: ValueControlTarget, value: i32) -> RuntimeControlCommand {
    let gain = || value.max(0).unsigned_abs();
    match target {
        ValueControlTarget::ChannelGain { channel_index } => RuntimeControlCommand::ChannelGain {
            channel_index,
            gain_millionths: gain(),
        },
        ValueControlTarget::ChannelPan { channel_index } => RuntimeControlCommand::ChannelPan {
            channel_index,
            pan_millionths: value,
        },
        ValueControlTarget::SendGain {
            channel_index,
            send_index,
        } => RuntimeControlCommand::SendGain {
            channel_index,
            send_index,
            gain_millionths: gain(),
        },
        ValueControlTarget::SendPan {
            channel_index,
            send_index,
        } => RuntimeControlCommand::SendPan {
            channel_index,
            send_index,
            pan_millionths: value,
        },
        ValueControlTarget::MainGain => RuntimeControlCommand::MainGain {
            gain_millionths: gain(),
        },
    }
}

const fn boolean_command(target: BooleanControlTarget, value: bool) -> RuntimeControlCommand {
    match target {
        BooleanControlTarget::ChannelMute { channel_index } => RuntimeControlCommand::ChannelMute {
            channel_index,
            muted: value,
        },
        BooleanControlTarget::ChannelSolo { channel_index } => RuntimeControlCommand::ChannelSolo {
            channel_index,
            solo: value,
        },
        BooleanControlTarget::SendEnabled {
            channel_index,
            send_index,
        } => RuntimeControlCommand::SendEnabled {
            channel_index,
            send_index,
            enabled: value,
        },
        BooleanControlTarget::MainMute => RuntimeControlCommand::MainMute { muted: value },
    }
}
