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

    #[must_use]
    pub fn into_reason(self) -> PanelBindingRefusalReason<E> {
        self.reason
    }
}

/// A panel is already connected. Disconnect it first so anything held is
/// released rather than stranded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PanelAlreadyConnected;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelDispatchOutcome {
    /// No binding claims the control, or the message binds nothing.
    Unbound,
    /// A binding claims the control but this edge commands nothing.
    Idle { control: PanelControl },
    Unavailable {
        control: PanelControl,
        reason: ControlBindingUnavailableReason,
    },
    Sent {
        control: PanelControl,
        command: RuntimeControlCommand,
    },
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
    ) -> Result<Vec<PanelBindingRefusal<S::Error>>, PanelAlreadyConnected> {
        if self.panel.is_some() {
            return Err(PanelAlreadyConnected);
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
            self.seed_shadow(installed.binding);
        }
        self.panel = Some(PanelBindings {
            bindings: installed,
        });
        Ok(refused)
    }

    /// Forgets the panel's bindings, releasing any momentary control still held
    /// so a link lost mid-press cannot strand its target.
    #[must_use]
    pub fn disconnect_panel(&mut self) -> Vec<PanelReleaseFailure<S::Error>> {
        let mut failures = Vec::new();
        let Some(panel) = self.panel.take() else {
            return failures;
        };
        for installed in &panel.bindings {
            if !installed.held {
                continue;
            }
            let PanelBinding::Button { target, .. } = installed.binding else {
                continue;
            };
            let command = boolean_command(target, false);
            match self.sink.try_send(command) {
                Ok(()) => self.commit_shadow(command),
                Err(source) => failures.push(PanelReleaseFailure {
                    control: installed.binding.control(),
                    command,
                    source,
                }),
            }
        }
        self.value_shadows.clear();
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
        let Some((control, edge)) = panel_input(message) else {
            return Ok(PanelDispatchOutcome::Unbound);
        };
        let Some(panel) = self.panel.as_ref() else {
            return Ok(PanelDispatchOutcome::Unbound);
        };
        let Some(position) = panel
            .bindings
            .iter()
            .position(|installed| installed.binding.control() == control)
        else {
            return Ok(PanelDispatchOutcome::Unbound);
        };
        let installed = panel.bindings[position];
        if let ControlBindingAvailability::Unavailable(reason) = installed.availability {
            return Ok(PanelDispatchOutcome::Unavailable { control, reason });
        }
        self.refresh_missing_state();
        let command = match self.panel_command(installed.binding, edge) {
            PanelCommand::Idle => return Ok(PanelDispatchOutcome::Idle { control }),
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

    fn seed_shadow(&mut self, binding: PanelBinding) {
        match binding {
            PanelBinding::Encoder { target, .. } => {
                if !self
                    .value_shadows
                    .iter()
                    .any(|shadow| shadow.target == target)
                {
                    let value = self.sink.value_state(target);
                    self.value_shadows.push(ValueShadow { target, value });
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
                    let value = self.sink.boolean_state(target);
                    self.boolean_shadows.push(BooleanShadow { target, value });
                }
            }
            PanelBinding::Button { .. } | PanelBinding::ActionButton { .. } => {}
        }
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
                let (minimum, maximum) = target.domain();
                let moved =
                    i64::from(current) + i64::from(delta) * i64::from(step_millionths.get());
                let next = moved.clamp(i64::from(minimum), i64::from(maximum));
                let next = i32::try_from(next)
                    .expect("a value clamped into its target's domain fits that domain's type");
                if next == current {
                    return PanelCommand::Idle;
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

fn panel_input(message: Message) -> Option<(PanelControl, PanelEdge)> {
    match message {
        Message::EncoderDelta {
            control_id, delta, ..
        } => Some((
            PanelControl::Encoder {
                index: u8::try_from(control_id).ok()?,
            },
            PanelEdge::Turn(delta),
        )),
        Message::ButtonPress { control_id, .. } => Some((
            PanelControl::Button {
                index: u8::try_from(control_id).ok()?,
            },
            PanelEdge::Press,
        )),
        Message::ButtonRelease { control_id, .. } => Some((
            PanelControl::Button {
                index: u8::try_from(control_id).ok()?,
            },
            PanelEdge::Release,
        )),
        _ => None,
    }
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
    let gain =
        || u32::try_from(value).expect("a value clamped into the gain domain is not negative");
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
